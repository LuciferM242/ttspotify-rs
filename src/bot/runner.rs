//! Reusable bot runner.
//!
//! Contains the full bot lifecycle: TeamTalk setup, Spotify auth,
//! audio pipeline, command processor, and event loop.
//! Used by both the standalone binary and the Windows tray manager.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use librespot_core::spotify_uri::SpotifyUri;
use librespot_playback::player::PlayerEvent;

use crate::bot::commands::{BotCommand, PlaybackMode};
use crate::bot::state::{PlayerState, SharedState};
use crate::config::BotConfig;
use crate::error::BotError;
use crate::spotify::metadata::SpotifyMetadata;
use crate::spotify::player::SpotifyPlayer;

/// How the bot exited.
#[derive(Debug, Clone, PartialEq)]
pub enum BotExit {
    /// Clean quit (user sent quit command).
    Quit,
    /// Restart requested (user sent restart command).
    Restart,
    /// External shutdown signal (tray stop button, systemd stop).
    Shutdown,
}

/// Status events sent to the tray (or any observer).
#[derive(Debug, Clone)]
#[allow(dead_code)] // All variants used via gui module (cfg(windows))
pub enum RunnerEvent {
    Connecting,
    Authenticating,
    Connected,
    Playing(String),
    Idle,
    Disconnected,
    Error(String),
}

/// Run a single bot instance. Returns when the bot exits.
///
/// - `config`: Bot configuration.
/// - `config_path`: Path to config file (for saving runtime changes).
/// - `shutdown`: External shutdown signal. Set to true to stop the bot.
/// - `event_tx`: Optional channel for status updates (used by tray).
pub async fn run_bot(
    config: BotConfig,
    config_path: String,
    shutdown: Arc<AtomicBool>,
    event_tx: Option<crossbeam_channel::Sender<RunnerEvent>>,
) -> Result<BotExit, BotError> {
    let send_event = {
        let tx = event_tx.clone();
        move |evt: RunnerEvent| {
            if let Some(ref tx) = tx {
                let _ = tx.send(evt);
            }
        }
    };

    tracing::info!("TeamTalk Spotify Bot starting...");
    tracing::info!("Config loaded from {}", config_path);

    // Create shared state
    let mut initial_state = PlayerState::new();
    initial_state.radio_enabled = config.radio_enabled;
    initial_state.repeat_track = config.repeat_track;
    initial_state.repeat_queue = config.repeat_queue;
    initial_state.shuffle = config.shuffle;
    let state: SharedState = Arc::new(std::sync::Mutex::new(initial_state));
    let volume = Arc::new(AtomicU8::new(config.volume));

    // Create channels
    let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<i16>>(256);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<BotCommand>();

    // Setup TeamTalk
    send_event(RunnerEvent::Connecting);
    let tt_config = config.clone();
    let client = tokio::task::spawn_blocking(move || {
        crate::tt::connection::setup_teamtalk(&tt_config)
    }).await.map_err(|e| BotError::TeamTalk(format!("TT setup task failed: {e}")))??;
    let client = Arc::new(client);

    send_event(RunnerEvent::Connected);

    // Spawn audio pipeline thread
    let pipeline_client = client.clone();
    let pipeline_volume = volume.clone();
    let pipeline_config = config.clone();
    let audio_reset = Arc::new(AtomicBool::new(false));
    let timing_reset = Arc::new(AtomicBool::new(false));
    let pause_flag = Arc::new(AtomicBool::new(false));
    let pipeline_reset = audio_reset.clone();
    let pipeline_timing_reset = timing_reset.clone();
    let pipeline_pause = pause_flag.clone();
    std::thread::spawn(move || {
        let mut pipeline = crate::audio::pipeline::AudioPipeline::new(
            audio_rx,
            pipeline_client,
            pipeline_volume,
            pipeline_reset,
            pipeline_timing_reset,
            pipeline_pause,
            &pipeline_config,
        );
        pipeline.run();
    });

    // Spotify auth
    send_event(RunnerEvent::Authenticating);
    let mut auth = crate::spotify::auth::SpotifyAuth::new();
    let session = auth.connect().await?;

    // Create Spotify player + metadata
    let (player, event_rx) = SpotifyPlayer::new(session.clone(), &config, audio_tx);
    let metadata = SpotifyMetadata::new(session.clone());

    // Exit signal: command_processor sets this instead of process::exit
    let exit_reason: Arc<std::sync::Mutex<Option<BotExit>>> =
        Arc::new(std::sync::Mutex::new(None));

    // Spawn command processor
    let bot_gender = crate::config::parse_gender(&config.bot_gender);
    let cmd_ctx = CmdContext {
        player,
        metadata,
        state: state.clone(),
        client: client.clone(),
        search_limit: config.search_limit,
        radio_batch_size: config.radio_batch_size,
        radio_delay: config.radio_delay,
        radio_cmd_tx: cmd_tx.clone(),
        bot_gender,
        config_path: config_path.clone(),
        audio_reset: audio_reset.clone(),
        timing_reset: timing_reset.clone(),
        pause_flag: pause_flag.clone(),
        volume_for_save: volume.clone(),
        exit_reason: exit_reason.clone(),
        shutdown: shutdown.clone(),
        event_tx: event_tx.clone(),
    };
    tokio::spawn(async move {
        command_processor(cmd_rx, cmd_ctx).await;
    });

    // Spawn player event loop
    let event_state = state.clone();
    let event_cmd_tx = cmd_tx.clone();
    tokio::spawn(async move {
        player_event_loop(event_rx, event_state, event_cmd_tx).await;
    });

    // Command dispatcher
    let dispatcher = crate::bot::commands::CommandDispatcher {
        state: state.clone(),
        volume: volume.clone(),
        cmd_tx,
        max_volume: config.max_volume,
        start_time: std::time::Instant::now(),
    };

    tracing::info!("Bot is ready! Listening for commands...");

    // Set initial idle status
    {
        let status = ::teamtalk::types::UserStatus {
            gender: bot_gender,
            ..Default::default()
        };
        client.set_status(status, "Idle");
    }
    send_event(RunnerEvent::Idle);

    // Event loop runs on a blocking thread
    let event_client = client.clone();
    let event_config = config.clone();
    let event_shutdown = shutdown.clone();
    let event_exit = exit_reason.clone();
    let event_event_tx = event_tx.clone();
    tokio::task::spawn_blocking(move || {
        loop {
            // Check external shutdown signal
            if event_shutdown.load(Ordering::Relaxed) {
                break;
            }
            // Check internal exit signal (quit/restart from command_processor)
            if event_exit.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                break;
            }

            if let Some((event, message)) = event_client.poll(100) {
                // Auto-reconnect on connection loss
                if event == ::teamtalk::Event::ConnectionLost {
                    tracing::warn!("Connection lost, attempting reconnect...");
                    if let Some(ref tx) = event_event_tx {
                        let _ = tx.send(RunnerEvent::Disconnected);
                    }
                    for attempt in 1..=5u64 {
                        // Check shutdown during reconnect so tray/systemd can stop us
                        if event_shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(2 * attempt));
                        tracing::info!("Reconnect attempt {attempt}/5...");
                        match event_client.reconnect(
                            &event_config.host, event_config.tcp_port, event_config.udp_port, event_config.encrypted,
                        ) {
                            Ok(()) => {
                                let mut connected = false;
                                for _ in 0..50 {
                                    if let Some((ev, _)) = event_client.poll(200) {
                                        if ev == ::teamtalk::Event::ConnectSuccess {
                                            connected = true;
                                            break;
                                        }
                                    }
                                }
                                if connected {
                                    event_client.login(&event_config.bot_name, &event_config.username, &event_config.password, "TTSpotifyBot");
                                    for _ in 0..50 {
                                        if let Some((ev, _)) = event_client.poll(200) {
                                            if ev == ::teamtalk::Event::MySelfLoggedIn { break; }
                                        }
                                    }
                                    let ch_id = event_client.get_channel_id_from_path(&event_config.channel_name);
                                    if ch_id.0 != 0 {
                                        event_client.join_channel(ch_id, &event_config.channel_password);
                                    }
                                    tracing::info!("Reconnected successfully");
                                    if let Some(ref tx) = event_event_tx {
                                        let _ = tx.send(RunnerEvent::Connected);
                                    }
                                    break;
                                }
                            }
                            Err(e) => tracing::warn!("Reconnect attempt {attempt} failed: {e}"),
                        }
                    }
                    continue;
                }

                // Dispatch private messages only
                if event == ::teamtalk::Event::TextMessage {
                    if let Some(text_msg) = message.text() {
                        if (text_msg.msg_type as i32) != 1 {
                            continue;
                        }
                        let sender_id = text_msg.from_id.0;
                        let my_id = event_client.my_id().0;
                        if sender_id != my_id && !text_msg.text.is_empty() {
                            if !dispatcher.dispatch(&event_client, &text_msg.text, sender_id) {
                                break; // quit/restart
                            }
                        }
                    }
                }
            }
        }
    }).await.map_err(|e| BotError::TeamTalk(format!("Event loop failed: {e}")))?;

    // Determine exit reason: check explicit exit_reason first (quit/restart
    // command), then fall back to external shutdown signal (tray/systemd).
    // do_exit() sets both exit_reason AND shutdown=true, so we must check
    // exit_reason first to avoid masking quit/restart as Shutdown.
    let exit = exit_reason.lock().unwrap_or_else(|e| e.into_inner()).take();
    let reason = match exit {
        Some(reason) => reason,
        None if shutdown.load(Ordering::Relaxed) => BotExit::Shutdown,
        None => BotExit::Quit,
    };
    let _ = client.disconnect();
    Ok(reason)
}

fn schedule_radio_prefetch(
    tx: &tokio::sync::mpsc::UnboundedSender<BotCommand>,
    seed_uri: String,
    delay_secs: f32,
) {
    let tx = tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs_f32(delay_secs)).await;
        let _ = tx.send(BotCommand::RadioPreFetch { seed_uri });
    });
}

/// Format queue position and estimated wait time for a newly queued track.
/// Returns a string like " (3rd up, ~8 min)" or empty if not applicable.
fn queue_wait_info(state: &crate::bot::state::PlayerState) -> String {
    let current_idx = match state.current_index {
        Some(i) => i,
        None => return String::new(),
    };
    let total = state.queue.len();
    if total <= current_idx + 1 {
        return String::new();
    }
    // Position in upcoming queue (1-based)
    let upcoming_pos = total - current_idx - 1;
    // Estimate wait: sum durations of tracks between current and the end,
    // minus elapsed time on current track
    let mut wait_ms: u64 = 0;
    if let Some(current) = state.queue.get(current_idx) {
        wait_ms += current.track.duration_ms.saturating_sub(state.position_ms) as u64;
    }
    for entry in state.queue.iter().skip(current_idx + 1).take(upcoming_pos - 1) {
        wait_ms += entry.track.duration_ms as u64;
    }
    let wait_min = (wait_ms + 30_000) / 60_000; // round to nearest minute
    let pos_str = match upcoming_pos {
        1 => "next".to_string(),
        _ => format!("{upcoming_pos} ahead"),
    };
    if wait_min > 0 {
        format!(" ({pos_str}, ~{wait_min} min)")
    } else {
        format!(" ({pos_str})")
    }
}

/// All shared context needed by the command processor, bundled to avoid parameter explosion.
struct CmdContext {
    player: SpotifyPlayer,
    metadata: SpotifyMetadata,
    state: SharedState,
    client: Arc<::teamtalk::Client>,
    search_limit: u8,
    radio_batch_size: u8,
    radio_delay: f32,
    radio_cmd_tx: tokio::sync::mpsc::UnboundedSender<BotCommand>,
    bot_gender: ::teamtalk::types::UserGender,
    config_path: String,
    audio_reset: Arc<AtomicBool>,
    timing_reset: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    volume_for_save: Arc<AtomicU8>,
    exit_reason: Arc<std::sync::Mutex<Option<BotExit>>>,
    shutdown: Arc<AtomicBool>,
    event_tx: Option<crossbeam_channel::Sender<RunnerEvent>>,
}

async fn command_processor(
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<BotCommand>,
    ctx: CmdContext,
) {
    // Destructure context for ergonomic access
    let CmdContext {
        player, metadata, state, client,
        search_limit, radio_batch_size, radio_delay, radio_cmd_tx,
        bot_gender, config_path, audio_reset, timing_reset, pause_flag,
        volume_for_save, exit_reason, shutdown, event_tx,
    } = ctx;

    // Debounce flag for volume config writes
    let pending_volume_save = Arc::new(AtomicBool::new(false));

    let send_event = {
        let tx = event_tx;
        move |evt: RunnerEvent| {
            if let Some(ref tx) = tx {
                let _ = tx.send(evt);
            }
        }
    };

    let reply = |user_id: i32, text: &str| {
        if user_id > 0 {
            client.send_to_user(::teamtalk::types::UserId(user_id), text);
        }
    };

    let set_status = |text: &str| {
        let status = ::teamtalk::types::UserStatus {
            gender: bot_gender,
            ..Default::default()
        };
        client.set_status(status, text);
    };

    let now_playing_status = |track_name: &str, st: &SharedState| -> String {
        let s = st.lock().unwrap();
        let total = s.queue.len();
        if total > 1 {
            let pos = s.current_index.map(|i| i + 1).unwrap_or(1);
            format!("{track_name} [{pos}/{total}]")
        } else {
            track_name.to_string()
        }
    };

    let stop_playback = |player: &SpotifyPlayer, client: &::teamtalk::Client, state: &SharedState, audio_reset: &AtomicBool, pause_flag: &AtomicBool| {
        pause_flag.store(false, Ordering::Relaxed);
        player.stop();
        crate::tt::audio_inject::flush_audio(client);
        client.enable_voice_transmission(false);
        audio_reset.store(true, Ordering::Relaxed);
        let mut s = state.lock().unwrap();
        s.is_playing = false;
        s.is_paused = false;
        s.is_loading = false;
    };

    let start_track = |uri_str: &str, player: &SpotifyPlayer, client: &::teamtalk::Client, state: &SharedState, audio_reset: &AtomicBool, pause_flag: &AtomicBool| -> bool {
        if let Ok(uri) = SpotifyUri::from_uri(uri_str) {
            pause_flag.store(false, Ordering::Relaxed);
            player.stop();
            crate::tt::audio_inject::flush_audio(client);
            client.enable_voice_transmission(false);
            audio_reset.store(true, Ordering::Relaxed);
            player.load_track(&uri);
            {
                let mut s = state.lock().unwrap();
                s.is_loading = true;
                s.is_playing = false;
                s.is_paused = false;
                s.tracks_played += 1;
            }
            true
        } else {
            false
        }
    };

    let do_exit = |reason: BotExit| {
        player.stop();
        set_status("Idle");
        send_event(RunnerEvent::Idle);
        {
            let s = state.lock().unwrap();
            let vol = volume_for_save.load(Ordering::Relaxed);
            let radio = s.radio_enabled;
            let repeat_track = s.repeat_track;
            let repeat_queue = s.repeat_queue;
            let shuffle = s.shuffle;
            drop(s);
            crate::config::BotConfig::update(&config_path, |cfg| {
                cfg.radio_enabled = radio;
                cfg.volume = vol;
                cfg.repeat_track = repeat_track;
                cfg.repeat_queue = repeat_queue;
                cfg.shuffle = shuffle;
            });
        }
        let _ = client.disconnect();
        *exit_reason.lock().unwrap() = Some(reason);
        shutdown.store(true, Ordering::Relaxed);
    };

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            BotCommand::SearchAndPlay { query, user_id, user_name } => {
                match metadata.resolve(&query, search_limit).await {
                    Ok(tracks) => {
                        if tracks.is_empty() {
                            reply(user_id, "No results found");
                            continue;
                        }

                        let is_multi = query.contains("playlist") || query.contains("album");
                        let tracks_to_add = if is_multi {
                            tracks
                        } else {
                            vec![tracks.into_iter().next().unwrap()]
                        };

                        let first_name = tracks_to_add[0].display_name();
                        let first_uri = tracks_to_add[0].uri.clone();
                        let count = tracks_to_add.len();

                        // Hold lock across idle check + enqueue to prevent race
                        let is_idle = {
                            let mut s = state.lock().unwrap();
                            let idle = !s.is_playing && !s.is_paused && !s.is_loading;
                            if idle {
                                s.clear();
                            }
                            s.enqueue_all(tracks_to_add, user_name, !is_multi);
                            idle
                        };

                        if is_idle {
                            start_track(&first_uri, &player, &client, &state, &audio_reset, &pause_flag);
                            if count > 1 {
                                reply(user_id, &format!("Now playing: {first_name} (+{} queued)", count - 1));
                            } else {
                                reply(user_id, &format!("Now playing: {first_name}"));
                            }
                            let status_text = now_playing_status(&first_name, &state);
                            set_status(&status_text);
                            send_event(RunnerEvent::Playing(first_name.clone()));

                            if !is_multi {
                                let radio_on = state.lock().unwrap().radio_enabled;
                                if radio_on {
                                    schedule_radio_prefetch(&radio_cmd_tx, first_uri.clone(), radio_delay);
                                }
                            }
                        } else {
                            let msg = {
                                let s = state.lock().unwrap();
                                let upcoming = queue_wait_info(&s);
                                if count > 1 {
                                    format!("Queued {count} tracks{upcoming}")
                                } else {
                                    format!("Queued: {first_name}{upcoming}")
                                }
                            };
                            reply(user_id, &msg);
                        }
                    }
                    Err(e) => {
                        reply(user_id, &format!("Search failed: {e}"));
                    }
                }
            }

            BotCommand::Play { user_id: _ } => {
                pause_flag.store(false, Ordering::Relaxed);
                timing_reset.store(true, Ordering::Relaxed);
                player.play();
                let mut s = state.lock().unwrap();
                s.is_playing = true;
                s.is_paused = false;
                // Send playing event with current track name
                if let Some(entry) = s.current() {
                    send_event(RunnerEvent::Playing(entry.track.display_name()));
                }
            }

            BotCommand::Pause { user_id: _ } => {
                pause_flag.store(true, Ordering::Relaxed);
                player.pause();
                crate::tt::audio_inject::flush_audio(&client);
                let mut s = state.lock().unwrap();
                s.is_paused = true;
                s.is_playing = false;
                send_event(RunnerEvent::Idle);
            }

            BotCommand::Stop { user_id: _ } => {
                stop_playback(&player, &client, &state, &audio_reset, &pause_flag);
                {
                    let mut s = state.lock().unwrap();
                    s.clear();
                }
                set_status("Idle");
                send_event(RunnerEvent::Idle);
            }

            BotCommand::Next { user_id } => {
                let next = {
                    let mut s = state.lock().unwrap();
                    s.advance().map(|e| (e.track.uri.clone(), e.track.display_name()))
                };
                if let Some((uri_str, name)) = next {
                    if start_track(&uri_str, &player, &client, &state, &audio_reset, &pause_flag) {
                        reply(user_id, &format!("Now playing: {name}"));
                        let status_text = now_playing_status(&name, &state);
                        set_status(&status_text);
                        send_event(RunnerEvent::Playing(name.clone()));
                    }

                    let (radio_on, at_end, allow_rec) = {
                        let s = state.lock().unwrap();
                        let at_end = s.current_index.map(|i| i + 1 >= s.queue.len()).unwrap_or(true);
                        let allow = s.current().map(|e| e.allow_recommend).unwrap_or(false);
                        (s.radio_enabled, at_end, allow)
                    };
                    if radio_on && at_end && allow_rec {
                        schedule_radio_prefetch(&radio_cmd_tx, uri_str.clone(), radio_delay);
                    }
                } else {
                    let (radio_on, allow_rec, seed_uri, played_ids) = {
                        let s = state.lock().unwrap();
                        let seed = s.current().map(|e| e.track.uri.clone());
                        let allow = s.current().map(|e| e.allow_recommend).unwrap_or(false);
                        let played: Vec<String> = s.queue.iter().map(|e| e.track.id.clone()).collect();
                        (s.radio_enabled, allow, seed, played)
                    };

                    if radio_on && allow_rec {
                        if let Some(seed) = seed_uri {
                            if let Ok(seed_parsed) = SpotifyUri::from_uri(&seed) {
                                reply(user_id, "Radio: fetching recommendations...");
                                match metadata.get_radio_tracks(&seed_parsed, radio_batch_size as usize, &played_ids).await {
                                    Ok(tracks) if !tracks.is_empty() => {
                                        let first_uri = tracks[0].uri.clone();
                                        let first_name = tracks[0].display_name();
                                        {
                                            let mut s = state.lock().unwrap();
                                            s.enqueue_all(tracks, "Radio".to_string(), true);
                                        }
                                        if start_track(&first_uri, &player, &client, &state, &audio_reset, &pause_flag) {
                                            reply(user_id, &format!("Radio: {first_name}"));
                                            let status_text = now_playing_status(&first_name, &state);
                                            set_status(&status_text);
                                            send_event(RunnerEvent::Playing(first_name.clone()));
                                        }
                                    }
                                    Ok(_) => {
                                        player.stop();
                                        reply(user_id, "Radio: no recommendations found");
                                        send_event(RunnerEvent::Idle);
                                    }
                                    Err(e) => {
                                        player.stop();
                                        reply(user_id, &format!("Radio failed: {e}"));
                                        send_event(RunnerEvent::Idle);
                                    }
                                }
                            }
                        }
                    } else {
                        player.stop();
                        let mut s = state.lock().unwrap();
                        s.is_playing = false;
                        s.is_paused = false;
                        set_status("Idle");
                        reply(user_id, "End of queue");
                        send_event(RunnerEvent::Idle);
                    }
                }
            }

            BotCommand::Prev { user_id } => {
                let prev = {
                    let mut s = state.lock().unwrap();
                    s.go_prev().map(|e| (e.track.uri.clone(), e.track.display_name()))
                };
                if let Some((uri_str, name)) = prev {
                    if start_track(&uri_str, &player, &client, &state, &audio_reset, &pause_flag) {
                        reply(user_id, &format!("Now playing: {name}"));
                        let status_text = now_playing_status(&name, &state);
                        set_status(&status_text);
                        send_event(RunnerEvent::Playing(name.clone()));
                    }
                }
            }

            BotCommand::Seek { offset_ms, user_id: _ } => {
                let new_pos = {
                    let s = state.lock().unwrap();
                    let current = s.position_ms as i32;
                    (current + offset_ms).max(0) as u32
                };
                audio_reset.store(true, Ordering::Relaxed);
                player.seek(new_pos);
            }

            BotCommand::SetVolume { .. } => {
                // Volume is set atomically in the dispatcher.
                // Debounce config write: only save if no further volume change
                // arrives within 3 seconds to avoid disk thrashing.
                if !pending_volume_save.load(Ordering::Relaxed) {
                    pending_volume_save.store(true, Ordering::Relaxed);
                    let save_flag = pending_volume_save.clone();
                    let vol_ref = volume_for_save.clone();
                    let path = config_path.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        let vol = vol_ref.load(Ordering::Relaxed);
                        crate::config::BotConfig::update(&path, |cfg| {
                            cfg.volume = vol;
                        });
                        save_flag.store(false, Ordering::Relaxed);
                    });
                }
            }

            BotCommand::SetMode { mode, user_id: _ } => {
                let mut s = state.lock().unwrap();
                match mode {
                    PlaybackMode::RepeatTrack => {
                        s.repeat_track = true;
                        s.repeat_queue = false;
                        s.shuffle = false;
                    }
                    PlaybackMode::RepeatQueue => {
                        s.repeat_track = false;
                        s.repeat_queue = true;
                        s.shuffle = false;
                    }
                    PlaybackMode::Shuffle => {
                        s.repeat_track = false;
                        s.repeat_queue = false;
                        s.shuffle = true;
                    }
                    PlaybackMode::Off => {
                        s.repeat_track = false;
                        s.repeat_queue = false;
                        s.shuffle = false;
                    }
                }
            }

            BotCommand::RadioToggle { enable, user_id: _ } => {
                let mut s = state.lock().unwrap();
                s.radio_enabled = enable;
                drop(s);
                crate::config::BotConfig::update(&config_path, |cfg| {
                    cfg.radio_enabled = enable;
                });
            }

            BotCommand::QueueClear { user_id: _ } => {
                let mut s = state.lock().unwrap();
                if let Some(idx) = s.current_index {
                    s.queue.truncate(idx + 1);
                } else {
                    s.queue.clear();
                }
            }

            BotCommand::QueueRemove { index, user_id: _ } => {
                let mut s = state.lock().unwrap();
                s.remove(index);
            }

            BotCommand::SearchOnly { query, user_id } => {
                match metadata.search_tracks(&query, search_limit).await {
                    Ok(tracks) => {
                        let mut msg = String::from("Search results:\n");
                        for (i, track) in tracks.iter().enumerate() {
                            msg.push_str(&format!(
                                "  {}: {} [{}]\n",
                                i + 1,
                                track.display_name(),
                                track.duration_display()
                            ));
                        }
                        msg.push_str("Type a number to play, or a to cancel");
                        reply(user_id, &msg);
                        let mut s = state.lock().unwrap();
                        s.search_results.insert(user_id, tracks);
                    }
                    Err(e) => {
                        reply(user_id, &format!("Search failed: {e}"));
                    }
                }
            }

            BotCommand::SearchPick { user_id, pick, user_name } => {
                let picked = {
                    let mut s = state.lock().unwrap();
                    let track = s.search_results.get(&user_id)
                        .and_then(|results| results.get(pick).cloned());
                    track.map(|track| {
                        s.search_results.remove(&user_id);
                        let idle = !s.is_playing && !s.is_paused && !s.is_loading;
                        if idle { s.clear(); }
                        let uri_str = track.uri.clone();
                        let track_name = track.display_name();
                        s.enqueue(track, user_name, true);
                        (uri_str, track_name, idle)
                    })
                };
                if let Some((uri_str, track_name, is_idle)) = picked {
                    if is_idle {
                        if start_track(&uri_str, &player, &client, &state, &audio_reset, &pause_flag) {
                            reply(user_id, &format!("Now playing: {track_name}"));
                            let status_text = now_playing_status(&track_name, &state);
                            set_status(&status_text);
                            send_event(RunnerEvent::Playing(track_name.clone()));
                        }
                    } else {
                        let upcoming = queue_wait_info(&state.lock().unwrap());
                        reply(user_id, &format!("Queued: {track_name}{upcoming}"));
                    }
                } else {
                    reply(user_id, "Invalid pick or no search results");
                }
            }

            BotCommand::JoinChannel { path, user_id } => {
                let channel_id = client.get_channel_id_from_path(&path);
                if channel_id == ::teamtalk::types::ChannelId(0) {
                    reply(user_id, &format!("Channel not found: {path}"));
                } else {
                    client.join_channel(channel_id, "");
                }
            }

            BotCommand::ChangeNick { name, user_id: _ } => {
                client.change_nickname(&name);
            }

            BotCommand::SetGender { gender, user_id: _ } => {
                let new_gender = crate::config::parse_gender(&gender);
                let status = ::teamtalk::types::UserStatus {
                    gender: new_gender,
                    ..Default::default()
                };
                client.set_status(status, "Idle");
                crate::config::BotConfig::update(&config_path, |cfg| {
                    cfg.bot_gender = gender;
                });
            }

            BotCommand::PreloadNext => {
                let next_uri = {
                    let s = state.lock().unwrap();
                    if s.repeat_track {
                        s.current().map(|e| e.track.uri.clone())
                    } else if let Some(idx) = s.current_index {
                        let next = idx + 1;
                        if next < s.queue.len() {
                            Some(s.queue[next].track.uri.clone())
                        } else if s.repeat_queue && !s.queue.is_empty() {
                            Some(s.queue[0].track.uri.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };
                if let Some(uri_str) = next_uri {
                    if let Ok(uri) = SpotifyUri::from_uri(&uri_str) {
                        player.preload(&uri);
                        tracing::debug!("Preloading next track: {uri_str}");
                    }
                }
            }

            BotCommand::RadioPreFetch { seed_uri } => {
                let (radio_on, is_playing, current_uri, queue_at_end, allow_rec) = {
                    let s = state.lock().unwrap();
                    let cur_uri = s.current().map(|e| e.track.uri.clone());
                    let at_end = s.current_index.map(|i| i + 1 >= s.queue.len()).unwrap_or(true);
                    let allow = s.current().map(|e| e.allow_recommend).unwrap_or(false);
                    (s.radio_enabled, s.is_playing || s.is_paused, cur_uri, at_end, allow)
                };

                if radio_on && is_playing && allow_rec && current_uri.as_deref() == Some(&seed_uri) && queue_at_end {
                    if let Ok(seed_parsed) = SpotifyUri::from_uri(&seed_uri) {
                        let played_ids: Vec<String> = {
                            let s = state.lock().unwrap();
                            s.queue.iter().map(|e| e.track.id.clone()).collect()
                        };
                        match metadata.get_radio_tracks(&seed_parsed, radio_batch_size as usize, &played_ids).await {
                            Ok(tracks) if !tracks.is_empty() => {
                                let count = tracks.len();
                                {
                                    let mut s = state.lock().unwrap();
                                    s.enqueue_all(tracks, "Radio".to_string(), true);
                                }
                                tracing::info!("Radio: pre-fetched {count} tracks from seed {seed_uri}");
                            }
                            Ok(_) => {
                                tracing::info!("Radio: no recommendations found for {seed_uri}");
                            }
                            Err(e) => {
                                tracing::warn!("Radio pre-fetch failed: {e}");
                            }
                        }
                    }
                }
            }

            BotCommand::Quit { user_id: _ } => {
                tracing::info!("Quit command received, shutting down...");
                do_exit(BotExit::Quit);
                return;
            }

            BotCommand::Restart { user_id: _ } => {
                tracing::info!("Restart command received...");
                do_exit(BotExit::Restart);
                return;
            }
        }
    }
}

async fn player_event_loop(
    mut events: librespot_playback::player::PlayerEventChannel,
    state: SharedState,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<BotCommand>,
) {
    while let Some(event) = events.recv().await {
        match event {
            PlayerEvent::Playing { position_ms, .. } => {
                let mut s = state.lock().unwrap();
                s.is_playing = true;
                s.is_paused = false;
                s.is_loading = false;
                s.position_ms = position_ms;
            }
            PlayerEvent::Paused { position_ms, .. } => {
                let mut s = state.lock().unwrap();
                s.is_playing = false;
                s.is_paused = true;
                s.position_ms = position_ms;
            }
            PlayerEvent::EndOfTrack { .. } => {
                tracing::info!("Track ended, advancing to next");
                let _ = cmd_tx.send(BotCommand::Next { user_id: 0 });
            }
            PlayerEvent::Unavailable { track_id, .. } => {
                tracing::warn!("Track unavailable: {track_id:?}, skipping");
                let _ = cmd_tx.send(BotCommand::Next { user_id: 0 });
            }
            PlayerEvent::TimeToPreloadNextTrack { .. } => {
                let _ = cmd_tx.send(BotCommand::PreloadNext);
            }
            PlayerEvent::PositionChanged { position_ms, .. }
            | PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::Seeked { position_ms, .. } => {
                let mut s = state.lock().unwrap();
                s.position_ms = position_ms;
            }
            _ => {}
        }
    }
}
