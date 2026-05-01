//! Reusable bot runner.
//!
//! Contains the full bot lifecycle: TeamTalk setup, Spotify auth,
//! audio pipeline, command processor, and event loop.
//! Used by both the standalone binary and the Windows tray manager.

use std::fmt::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use librespot_core::spotify_uri::SpotifyUri;
use librespot_playback::player::PlayerEvent;

use crate::bot::commands::{BotCommand, PlaybackMode};
use crate::bot::state::{PlaybackStatus, PlayerState, SharedState};
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
#[cfg_attr(not(windows), allow(dead_code))]
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

    let mut initial_state = PlayerState::new();
    initial_state.radio_enabled = config.radio_enabled;
    initial_state.repeat_track = config.repeat_track;
    initial_state.repeat_queue = config.repeat_queue;
    initial_state.shuffle = config.shuffle;
    let state: SharedState = Arc::new(parking_lot::Mutex::new(initial_state));
    let volume = Arc::new(AtomicU8::new(config.volume));

    let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<i16>>(256);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<BotCommand>();

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
    let pipeline_shutdown = shutdown.clone();
    std::thread::spawn(move || {
        let mut pipeline = crate::audio::pipeline::AudioPipeline::new(
            audio_rx,
            pipeline_client,
            pipeline_volume,
            pipeline_reset,
            pipeline_timing_reset,
            pipeline_pause,
            pipeline_shutdown,
            &pipeline_config,
        );
        pipeline.run();
    });

    send_event(RunnerEvent::Authenticating);
    let mut auth = crate::spotify::auth::SpotifyAuth::new();
    let session = auth.connect().await?;

    let (player, event_rx) = SpotifyPlayer::new(session.clone(), &config, audio_tx);
    let metadata = SpotifyMetadata::new(session.clone());

    // Exit signal: command_processor sets this instead of process::exit
    let exit_reason: Arc<parking_lot::Mutex<Option<BotExit>>> =
        Arc::new(parking_lot::Mutex::new(None));

    // Spawn command processor
    let bot_gender = crate::config::parse_gender(&config.bot_gender);
    let cmd_ctx = CmdContext {
        player,
        metadata,
        session,
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

    let dispatcher = crate::bot::commands::CommandDispatcher {
        state: state.clone(),
        volume: volume.clone(),
        cmd_tx,
        max_volume: config.max_volume,
        start_time: std::time::Instant::now(),
    };

    tracing::info!("Bot is ready! Listening for commands...");

    {
        let mut status = ::teamtalk::types::UserStatus::default();
        status.gender = bot_gender;
        client.set_status(status, "Idle");
    }
    send_event(RunnerEvent::Idle);

    // Track current channel for manual rejoin after reconnects.
    // SDK auto-join is disabled so admin moves are respected.
    let last_channel_id = Arc::new(parking_lot::Mutex::new(client.my_channel_id()));
    let last_channel_pw = Arc::new(parking_lot::Mutex::new(config.channel_password.clone()));

    // Event loop runs on a blocking thread.
    // Connection + login reconnect is handled by the SDK; channel rejoin is manual.
    let event_client = client.clone();
    let event_shutdown = shutdown.clone();
    let event_exit = exit_reason.clone();
    let event_event_tx = event_tx.clone();
    tokio::task::spawn_blocking(move || {
        loop {
            if event_shutdown.load(Ordering::Relaxed) {
                break;
            }
            if event_exit.lock().is_some() {
                break;
            }

            if let Some((event, message)) = event_client.poll(100) {
                match event {
                    ::teamtalk::Event::ConnectionLost => {
                        tracing::warn!("Connection lost, SDK auto-reconnect will handle recovery");
                        if let Some(ref tx) = event_event_tx {
                            let _ = tx.send(RunnerEvent::Disconnected);
                        }
                    }
                    ::teamtalk::Event::ConnectSuccess => {
                        tracing::info!("Reconnected to server");
                    }
                    ::teamtalk::Event::MySelfLoggedIn => {
                        tracing::info!("Re-logged in after reconnect");
                        // Rejoin the last channel after reconnect
                        let ch = event_client.my_channel_id();
                        if ch == ::teamtalk::types::ChannelId(0) {
                            let rejoin_ch = *last_channel_id.lock();
                            if rejoin_ch != ::teamtalk::types::ChannelId(0) {
                                let pw = last_channel_pw.lock().clone();
                                match event_client.join_channel_and_wait(rejoin_ch, &pw, 5_000) {
                                    Ok(_) => tracing::info!("Rejoined channel {} after reconnect", rejoin_ch.0),
                                    Err(e) => tracing::warn!("Failed to rejoin channel after reconnect: {e}"),
                                }
                            }
                        }
                        if let Some(ref tx) = event_event_tx {
                            let _ = tx.send(RunnerEvent::Connected);
                        }
                    }
                    ::teamtalk::Event::UserJoined => {
                        if let Some(user) = message.user() {
                            if user.id == event_client.my_id() && user.channel_id != ::teamtalk::types::ChannelId(0) {
                                *last_channel_id.lock() = user.channel_id;
                                tracing::info!("Now in channel {}", user.channel_id.0);
                            }
                        }
                    }
                    ::teamtalk::Event::TextMessage => {
                        if let Some(text_msg) = message.text() {
                            if (text_msg.msg_type as i32) != 1 {
                                continue;
                            }
                            let sender_id = text_msg.from_id.0;
                            let my_id = event_client.my_id().0;
                            if sender_id != my_id && !text_msg.text.is_empty() {
                                if !dispatcher.dispatch(&event_client, &text_msg.text, sender_id) {
                                    break;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }).await.map_err(|e| BotError::TeamTalk(format!("Event loop failed: {e}")))?;

    // Give the command processor a moment to finish do_exit() if it's
    // still running (event loop may break before the async command handler
    // has set exit_reason).
    for _ in 0..20 {
        if exit_reason.lock().is_some()
            || shutdown.load(Ordering::Relaxed)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Determine exit reason: check explicit exit_reason first (quit/restart
    // command), then fall back to external shutdown signal (tray/systemd).
    // do_exit() sets both exit_reason AND shutdown=true, so we must check
    // exit_reason first to avoid masking quit/restart as Shutdown.
    let exit = exit_reason.lock().take();
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
pub(crate) fn queue_wait_info(state: &crate::bot::state::PlayerState) -> String {
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
    session: librespot_core::session::Session,
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
    exit_reason: Arc<parking_lot::Mutex<Option<BotExit>>>,
    shutdown: Arc<AtomicBool>,
    event_tx: Option<crossbeam_channel::Sender<RunnerEvent>>,
}

/// Attempt to reconnect the Spotify session using cached credentials.
async fn try_reconnect_session(session: &librespot_core::session::Session) -> bool {
    let creds = session.cache().and_then(|c| c.credentials());
    if let Some(creds) = creds {
        tracing::info!("Spotify session error, reconnecting with cached credentials...");
        match session.connect(creds, true).await {
            Ok(()) => {
                tracing::info!("Spotify session reconnected");
                true
            }
            Err(e) => {
                tracing::error!("Spotify reconnection failed: {e}");
                false
            }
        }
    } else {
        tracing::error!("No cached credentials available for reconnection");
        false
    }
}

async fn command_processor(
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<BotCommand>,
    ctx: CmdContext,
) {
    // Destructure context for ergonomic access
    let CmdContext {
        player, metadata, session, state, client,
        search_limit, radio_batch_size, radio_delay, radio_cmd_tx,
        bot_gender, config_path, audio_reset, timing_reset, pause_flag,
        volume_for_save, exit_reason, shutdown, event_tx,
    } = ctx;

    // Macro to retry a metadata operation once after reconnecting on failure.
    macro_rules! with_reconnect {
        ($expr:expr) => {{
            let result = $expr.await;
            if result.is_err() {
                if try_reconnect_session(&session).await {
                    $expr.await
                } else {
                    result
                }
            } else {
                result
            }
        }};
    }

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
            crate::bot::commands::send_reply(&client, user_id, text);
        }
    };

    let set_status = |text: &str| {
        let mut status = ::teamtalk::types::UserStatus::default();
        status.gender = bot_gender;
        client.set_status(status, text);
    };

    let now_playing_status = |track_name: &str, st: &SharedState| -> String {
        let s = st.lock();
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
        let mut s = state.lock();
        s.status = PlaybackStatus::Idle;
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
                let mut s = state.lock();
                s.status = PlaybackStatus::Loading;
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
            let s = state.lock();
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
        *exit_reason.lock() = Some(reason);
        shutdown.store(true, Ordering::Relaxed);
    };

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            BotCommand::SearchAndPlay { query, user_id, user_name } => {
                match with_reconnect!(metadata.resolve(&query, search_limit)) {
                    Ok(tracks) => {
                        if tracks.is_empty() {
                            reply(user_id, "No results found");
                            continue;
                        }

                        let is_multi = query.contains("playlist") || query.contains("album");
                        let tracks_to_add = if is_multi {
                            tracks
                        } else {
                            vec![tracks.into_iter().next().expect("empty check above")]
                        };

                        let first_name = tracks_to_add[0].display_name();
                        let first_uri = tracks_to_add[0].uri.clone();
                        let count = tracks_to_add.len();

                        // Hold lock across idle check + enqueue to prevent race
                        let is_idle = {
                            let mut s = state.lock();
                            let idle = s.status == PlaybackStatus::Idle;
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
                                let radio_on = state.lock().radio_enabled;
                                if radio_on {
                                    schedule_radio_prefetch(&radio_cmd_tx, first_uri.clone(), radio_delay);
                                }
                            }
                        } else {
                            let msg = {
                                let s = state.lock();
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
                let mut s = state.lock();
                s.status = PlaybackStatus::Playing;
                if let Some(entry) = s.current() {
                    send_event(RunnerEvent::Playing(entry.track.display_name()));
                }
            }

            BotCommand::Pause { user_id: _ } => {
                pause_flag.store(true, Ordering::Relaxed);
                player.pause();
                crate::tt::audio_inject::flush_audio(&client);
                let mut s = state.lock();
                s.status = PlaybackStatus::Paused;
                send_event(RunnerEvent::Idle);
            }

            BotCommand::Stop { user_id: _ } => {
                stop_playback(&player, &client, &state, &audio_reset, &pause_flag);
                {
                    let mut s = state.lock();
                    s.clear();
                }
                set_status("Idle");
                send_event(RunnerEvent::Idle);
            }

            BotCommand::Next { user_id } => {
                // Capture current track info before advance() clears current_index
                let (pre_seed_uri, pre_allow_rec, pre_played_ids) = {
                    let s = state.lock();
                    let seed = s.current().map(|e| e.track.uri.clone());
                    let allow = s.current().map(|e| e.allow_recommend).unwrap_or(false);
                    let played: Vec<String> = s.queue.iter().map(|e| e.track.id.clone()).collect();
                    (seed, allow, played)
                };

                let next = {
                    let mut s = state.lock();
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
                        let s = state.lock();
                        let at_end = s.current_index.map(|i| i + 1 >= s.queue.len()).unwrap_or(true);
                        let allow = s.current().map(|e| e.allow_recommend).unwrap_or(false);
                        (s.radio_enabled, at_end, allow)
                    };
                    if radio_on && at_end && allow_rec {
                        schedule_radio_prefetch(&radio_cmd_tx, uri_str.clone(), radio_delay);
                    }
                } else {
                    let radio_on = state.lock().radio_enabled;

                    if radio_on && pre_allow_rec {
                        if let Some(seed) = pre_seed_uri {
                            if let Ok(seed_parsed) = SpotifyUri::from_uri(&seed) {
                                reply(user_id, "Radio: fetching recommendations...");
                                match with_reconnect!(metadata.get_radio_tracks(&seed_parsed, radio_batch_size as usize, &pre_played_ids)) {
                                    Ok(tracks) if !tracks.is_empty() => {
                                        let first_uri = tracks[0].uri.clone();
                                        let first_name = tracks[0].display_name();
                                        {
                                            let mut s = state.lock();
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
                    } else if user_id > 0 {
                        reply(user_id, "End of queue");
                    }
                }
            }

            BotCommand::Prev { user_id } => {
                let prev = {
                    let mut s = state.lock();
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
                    let s = state.lock();
                    let current = s.position_ms as i32;
                    (current + offset_ms).max(0) as u32
                };
                audio_reset.store(true, Ordering::Relaxed);
                player.seek(new_pos);
            }

            BotCommand::SetVolume { .. } => {
                // Debounce: only save if no further volume change within 3 seconds.
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
                let mut s = state.lock();
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
                let mut s = state.lock();
                s.radio_enabled = enable;
                drop(s);
                crate::config::BotConfig::update(&config_path, |cfg| {
                    cfg.radio_enabled = enable;
                });
            }

            BotCommand::QueueClear { user_id: _ } => {
                let mut s = state.lock();
                if let Some(idx) = s.current_index {
                    s.queue.truncate(idx + 1);
                } else {
                    s.queue.clear();
                }
            }

            BotCommand::QueueRemove { index, user_id: _ } => {
                let mut s = state.lock();
                s.remove(index);
            }

            BotCommand::SearchOnly { query, user_id } => {
                match with_reconnect!(metadata.search_tracks(&query, search_limit)) {
                    Ok(tracks) => {
                        let mut msg = String::from("Search results:\n");
                        for (i, track) in tracks.iter().enumerate() {
                            let _ = write!(msg, "  {}: {} [{}]\n",
                                i + 1, track.display_name(), track.duration_display());
                        }
                        msg.push_str("Type a number to play, or a to cancel");
                        reply(user_id, &msg);
                        let mut s = state.lock();
                        s.search_results.insert(user_id, tracks);
                    }
                    Err(e) => {
                        reply(user_id, &format!("Search failed: {e}"));
                    }
                }
            }

            BotCommand::SearchPick { user_id, pick, user_name } => {
                let picked = {
                    let mut s = state.lock();
                    let track = s.search_results.get(&user_id)
                        .and_then(|results| results.get(pick).cloned());
                    track.map(|track| {
                        s.search_results.remove(&user_id);
                        let idle = s.status == PlaybackStatus::Idle;
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

                            let radio_on = state.lock().radio_enabled;
                            if radio_on {
                                schedule_radio_prefetch(&radio_cmd_tx, uri_str.clone(), radio_delay);
                            }
                        }
                    } else {
                        let upcoming = queue_wait_info(&state.lock());
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
                let status_text = {
                    let s = state.lock();
                    match s.current() {
                        Some(entry) => {
                            let name = entry.track.display_name();
                            let total = s.queue.len();
                            if total > 1 {
                                let pos = s.current_index.map(|i| i + 1).unwrap_or(1);
                                format!("{name} [{pos}/{total}]")
                            } else {
                                name
                            }
                        }
                        None => "Idle".to_string(),
                    }
                };
                let mut status = ::teamtalk::types::UserStatus::default();
                status.gender = new_gender;
                client.set_status(status, &status_text);
                crate::config::BotConfig::update(&config_path, |cfg| {
                    cfg.bot_gender = gender;
                });
            }

            BotCommand::PreloadNext => {
                let next_uri = {
                    let s = state.lock();
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
                let (radio_on, is_active, current_uri, queue_at_end, allow_rec) = {
                    let s = state.lock();
                    let cur_uri = s.current().map(|e| e.track.uri.clone());
                    let at_end = s.current_index.map(|i| i + 1 >= s.queue.len()).unwrap_or(true);
                    let allow = s.current().map(|e| e.allow_recommend).unwrap_or(false);
                    (s.radio_enabled, s.status != PlaybackStatus::Idle, cur_uri, at_end, allow)
                };

                if radio_on && is_active && allow_rec && current_uri.as_deref() == Some(&seed_uri) && queue_at_end {
                    if let Ok(seed_parsed) = SpotifyUri::from_uri(&seed_uri) {
                        let played_ids: Vec<String> = {
                            let s = state.lock();
                            s.queue.iter().map(|e| e.track.id.clone()).collect()
                        };
                        match metadata.get_radio_tracks(&seed_parsed, radio_batch_size as usize, &played_ids).await {
                            Ok(tracks) if !tracks.is_empty() => {
                                let count = tracks.len();
                                {
                                    let mut s = state.lock();
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
                let mut s = state.lock();
                s.status = PlaybackStatus::Playing;
                s.position_ms = position_ms;
            }
            PlayerEvent::Paused { position_ms, .. } => {
                let mut s = state.lock();
                s.status = PlaybackStatus::Paused;
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
                let mut s = state.lock();
                s.position_ms = position_ms;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::state::PlayerState;
    use crate::spotify::types::SpotifyTrack;

    fn track(id: &str, duration_ms: u32) -> SpotifyTrack {
        SpotifyTrack {
            id: id.to_string(),
            name: format!("T{id}"),
            artists: vec!["A".to_string()],
            album: "Album".to_string(),
            duration_ms,
            uri: format!("spotify:track:{id}"),
        }
    }

    fn enqueue(state: &mut PlayerState, durations_ms: &[u32]) {
        for (i, d) in durations_ms.iter().enumerate() {
            state.enqueue(track(&i.to_string(), *d), "u".into(), true);
        }
    }

    // -- empty / not-applicable cases --

    #[test]
    fn queue_wait_info_empty_when_no_current() {
        let state = PlayerState::new();
        assert_eq!(queue_wait_info(&state), "");
    }

    #[test]
    fn queue_wait_info_empty_when_only_current_track() {
        let mut state = PlayerState::new();
        enqueue(&mut state, &[180_000]);
        assert_eq!(queue_wait_info(&state), "");
    }

    // -- "next" position (1 upcoming) --

    #[test]
    fn queue_wait_info_one_upcoming_zero_position_says_next() {
        let mut state = PlayerState::new();
        // Two tracks: current full duration unplayed, one upcoming.
        // Wait = 60s remaining on current → rounds to 1 min.
        enqueue(&mut state, &[60_000, 120_000]);
        // position_ms=0 (default) → wait = 60_000 - 0 = 60_000ms → 1 min.
        assert_eq!(queue_wait_info(&state), " (next, ~1 min)");
    }

    #[test]
    fn queue_wait_info_subtracts_position_from_current_track_wait() {
        let mut state = PlayerState::new();
        enqueue(&mut state, &[180_000, 60_000]);
        state.position_ms = 150_000; // 30s left on current
        // Wait = 30s → (30000+30000)/60000 = 1 min.
        assert_eq!(queue_wait_info(&state), " (next, ~1 min)");
    }

    #[test]
    fn queue_wait_info_under_thirty_seconds_drops_minute_suffix() {
        let mut state = PlayerState::new();
        enqueue(&mut state, &[20_000, 60_000]);
        // Wait = 20s → (20000+30000)/60000 = 0 min → no "~N min".
        assert_eq!(queue_wait_info(&state), " (next)");
    }

    // -- multi-upcoming --

    #[test]
    fn queue_wait_info_multi_upcoming_uses_ahead_form() {
        let mut state = PlayerState::new();
        // queue [A=120s, B=60s, C=60s, D=60s], current=A, asking about D's wait.
        // upcoming_pos = total(4) - current_idx(0) - 1 = 3.
        // Wait = remaining(A=120s) + B(60s) + C(60s) = 240s = 4 min.
        // (D itself is not summed — wait is "until D starts".)
        enqueue(&mut state, &[120_000, 60_000, 60_000, 60_000]);
        assert_eq!(queue_wait_info(&state), " (3 ahead, ~4 min)");
    }

    #[test]
    fn queue_wait_info_does_not_count_last_upcoming_track_duration() {
        // Defensive test for the "wait until the newly-queued (last) track starts"
        // semantic: skip(current+1).take(upcoming_pos - 1) excludes the final entry.
        let mut state = PlayerState::new();
        // queue [A=60s, B=60s, C=999_999_000ms (huge)], current=A.
        // wait = 60s (remaining A) + 60s (B). C is excluded.
        enqueue(&mut state, &[60_000, 60_000, 999_999_000]);
        // Wait = 120s → (120000+30000)/60000 = 2 min.
        assert_eq!(queue_wait_info(&state), " (2 ahead, ~2 min)");
    }

    #[test]
    fn queue_wait_info_position_past_current_duration_saturates_to_zero() {
        // Edge: position_ms > current.duration_ms (shouldn't happen but
        // saturating_sub guards it). With upcoming_pos=1, only the (saturated)
        // remainder of the current track is summed → wait_ms=0 → "(next)".
        let mut state = PlayerState::new();
        enqueue(&mut state, &[10_000, 60_000]);
        state.position_ms = 99_999_999;
        assert_eq!(queue_wait_info(&state), " (next)");
    }
}
