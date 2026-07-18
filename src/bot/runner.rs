//! Reusable bot runner.
//!
//! Contains the full bot lifecycle: TeamTalk setup, Spotify auth,
//! audio pipeline, command processor, and event loop.
//! Used by both the standalone binary and the Windows tray manager.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
/// - `last_channel`: In-memory carry of the current channel across a restart.
///   Applied only to the TT-connection config copy (never to `config` itself,
///   so ConfigStore/the config file keep the configured default). On a `rs`
///   restart it holds the channel the bot was in, so it rejoins there; `None`
///   (fresh process start) joins the configured default.
pub async fn run_bot(
    config: BotConfig,
    config_path: String,
    shutdown: Arc<AtomicBool>,
    event_tx: Option<crossbeam_channel::Sender<RunnerEvent>>,
    last_channel: Arc<parking_lot::Mutex<Option<String>>>,
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
    log_startup_versions();

    let mut initial_state = PlayerState::new();
    initial_state.radio_enabled = config.radio_enabled;
    initial_state.repeat_track = config.repeat_track;
    initial_state.repeat_queue = config.repeat_queue;
    initial_state.shuffle = config.shuffle;
    initial_state.active_service = config.default_service;
    let state: SharedState = Arc::new(parking_lot::Mutex::new(initial_state));
    let volume = Arc::new(AtomicU8::new(config.volume.min(config.max_volume)));

    let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<i16>>(256);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<BotCommand>();

    send_event(RunnerEvent::Connecting);
    // Only the TT connection copy gets the restart channel override; the real
    // `config` (and thus ConfigStore) keeps the configured default channel, so
    // the config file's channel_name is never rewritten.
    let tt_config = {
        let mut c = config.clone();
        if let Some(ch) = last_channel.lock().clone() {
            if ch != c.channel_name {
                tracing::info!("Restart: rejoining last channel {ch} (default is {})", c.channel_name);
                c.channel_name = ch;
            }
        }
        c
    };
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
    // Realtime playback position (ms injected since last reset), written by the
    // pipeline and read by the YouTube player for accurate `c`/seek positions.
    let pipeline_pos_ms = Arc::new(AtomicU32::new(0));
    let pipeline_reset = audio_reset.clone();
    let pipeline_timing_reset = timing_reset.clone();
    let pipeline_pause = pause_flag.clone();
    // Internal teardown signal set on EVERY run_bot exit (including the
    // reconnect-exhausted Err path, which must not touch the shared `shutdown`
    // — that would stop the supervisor from retrying). Keeps the pipeline
    // thread from leaking across tray restart-retries.
    let local_shutdown = Arc::new(AtomicBool::new(false));
    let pipeline_shutdown = local_shutdown.clone();
    let pipeline_pos = pipeline_pos_ms.clone();
    std::thread::spawn(move || {
        let mut pipeline = crate::audio::pipeline::AudioPipeline::new(
            audio_rx,
            pipeline_client,
            pipeline_volume,
            pipeline_reset,
            pipeline_timing_reset,
            pipeline_pause,
            pipeline_shutdown,
            pipeline_pos,
            &pipeline_config,
        );
        pipeline.run();
    });

    let auth = crate::spotify::auth::SpotifyAuth::new();
    let session = auth.new_session();

    // Connect Spotify eagerly only if credentials are already cached or Spotify
    // is the default service. A YouTube-only user with no cached credentials is
    // never sent to the browser at startup; the connection happens lazily on
    // their first Spotify command instead (see `ensure_spotify!`).
    let spotify_connected = if auth.has_cached_credentials()
        || config.default_service == crate::services::Service::Spotify
    {
        send_event(RunnerEvent::Authenticating);
        auth.connect_existing(&session).await?;
        true
    } else {
        tracing::info!("Skipping Spotify auth at startup; no cached credentials and default service is YouTube");
        false
    };

    let (player, event_rx) = SpotifyPlayer::new(session.clone(), &config, audio_tx.clone());
    let metadata = SpotifyMetadata::new(session.clone());
    let youtube_metadata = Arc::new(crate::youtube::metadata::YouTubeMetadata::new(&config)?);
    let youtube_player = crate::youtube::player::YouTubePlayer::new(
        audio_tx,
        youtube_metadata.clone(),
        cmd_tx.clone(),
        state.clone(),
        pipeline_pos_ms.clone(),
    );

    // Exit signal: command_processor sets this instead of process::exit
    let exit_reason: Arc<parking_lot::Mutex<Option<BotExit>>> =
        Arc::new(parking_lot::Mutex::new(None));

    // Single writer for all runtime config persistence.
    let config_store = Arc::new(crate::config::ConfigStore::new(
        config_path.clone(),
        config.clone(),
    ));

    // Spawn command processor
    let bot_gender = crate::config::parse_gender(&config.bot_gender);
    let cmd_ctx = CmdContext {
        player,
        metadata,
        youtube_metadata,
        youtube_player,
        session,
        auth,
        spotify_connected,
        state: state.clone(),
        client: client.clone(),
        search_limit: config.search_limit,
        radio_batch_size: config.radio_batch_size,
        radio_delay: config.radio_delay,
        radio_cmd_tx: cmd_tx.clone(),
        bot_gender,
        config_store: config_store.clone(),
        audio_reset: audio_reset.clone(),
        timing_reset: timing_reset.clone(),
        pause_flag: pause_flag.clone(),
        volume_for_save: volume.clone(),
        exit_reason: exit_reason.clone(),
        shutdown: shutdown.clone(),
        event_tx: event_tx.clone(),
    };
    let processor_handle = tokio::spawn(async move {
        command_processor(cmd_rx, cmd_ctx).await;
    });

    // Spawn player event loop
    let event_state = state.clone();
    let event_cmd_tx = cmd_tx.clone();
    let event_loop_handle = tokio::spawn(async move {
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

    // One-shot, non-blocking update check. Logs a breadcrumb if a newer release
    // exists; never blocks startup and never self-updates a running service.
    #[cfg(not(windows))]
    if crate::settings::load().check_updates_on_startup {
        tokio::spawn(async {
            if let Ok(Some(info)) = crate::update::check().await {
                tracing::info!("Update {} available - run: ttspotify --update", info.tag);
            }
        });
    }

    {
        let mut status = ::teamtalk::types::UserStatus::default();
        status.gender = bot_gender;
        let _ = client.set_status(status, "Idle");
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
    let event_last_channel = last_channel.clone();
    // If the SDK's auto-reconnect can't restore the session within this window,
    // stop spinning and return an error so the supervisor (tray restart /
    // systemd Restart=) can recover with a fresh client instead of the bot
    // becoming a silent zombie polling a dead connection forever.
    const RECONNECT_DEADLINE: Duration = Duration::from_secs(360);
    let reconnect_exhausted = tokio::task::spawn_blocking(move || -> bool {
        // `Some(instant)` while disconnected, cleared on successful re-login.
        let mut disconnected_since: Option<Instant> = None;
        loop {
            if event_shutdown.load(Ordering::Relaxed) {
                break false;
            }
            if event_exit.lock().is_some() {
                break false;
            }
            // Give up if we've been disconnected past the deadline.
            if let Some(since) = disconnected_since {
                if since.elapsed() > RECONNECT_DEADLINE {
                    tracing::error!(
                        "Auto-reconnect exhausted after {}s, giving up so the supervisor can restart",
                        RECONNECT_DEADLINE.as_secs()
                    );
                    break true;
                }
            }

            if let Some((event, message)) = event_client.poll(100) {
                match event {
                    ::teamtalk::Event::ConnectionLost => {
                        tracing::warn!("Connection lost, SDK auto-reconnect will handle recovery");
                        if disconnected_since.is_none() {
                            disconnected_since = Some(Instant::now());
                        }
                        if let Some(ref tx) = event_event_tx {
                            let _ = tx.send(RunnerEvent::Disconnected);
                        }
                    }
                    ::teamtalk::Event::ConnectSuccess => {
                        tracing::info!("Reconnected to server");
                    }
                    ::teamtalk::Event::MySelfLoggedIn => {
                        tracing::info!("Re-logged in after reconnect");
                        // Session restored: reset the disconnect watchdog.
                        disconnected_since = None;
                        // Rejoin our last channel whenever the reconnect didn't
                        // land us back in it (root, a different channel, or 0).
                        // Admin moves during a live session are still respected
                        // because UserJoined keeps last_channel_id current.
                        let ch = event_client.my_channel_id();
                        let rejoin_ch = *last_channel_id.lock();
                        if rejoin_ch != ::teamtalk::types::ChannelId(0) && ch != rejoin_ch {
                            let pw = last_channel_pw.lock().clone();
                            match event_client.join_channel_and_wait(rejoin_ch, &pw, 5_000) {
                                Ok(_) => tracing::info!("Rejoined channel {} after reconnect", rejoin_ch.0),
                                Err(e) => tracing::warn!("Failed to rejoin channel after reconnect: {e}"),
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
                                // Remember the current channel (in memory only) so a
                                // restart rejoins here instead of the configured
                                // default. The config file is never modified.
                                if let Some(path) = event_client.get_channel_path(user.channel_id) {
                                    *event_last_channel.lock() = Some(path);
                                }
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
                            if sender_id != my_id && !text_msg.text.is_empty()
                                && !dispatcher.dispatch(&event_client, &text_msg.text, sender_id) {
                                    break false;
                                }
                        }
                    }
                    _ => {}
                }
            }
        }
    }).await.map_err(|e| BotError::TeamTalk(format!("Event loop failed: {e}")))?;

    // Tear down the pipeline thread on every exit path (the shared `shutdown`
    // may be untouched — e.g. reconnect-exhausted, where the supervisor still
    // needs it clear to retry).
    local_shutdown.store(true, Ordering::Relaxed);

    if reconnect_exhausted {
        processor_handle.abort();
        event_loop_handle.abort();
        let _ = client.disconnect();
        return Err(BotError::TeamTalk(
            "Lost connection to the TeamTalk server and auto-reconnect was exhausted".into(),
        ));
    }

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
    // do_exit() has run by now (we waited for exit_reason), so config is saved;
    // abort the spawned tasks so they don't linger across a restart.
    processor_handle.abort();
    event_loop_handle.abort();
    let _ = client.disconnect();
    Ok(reason)
}

/// Log the app version plus the versions of the tools we depend on (TeamTalk
/// SDK, yt-dlp, bgutil-pot). Written to each instance's log at startup so a bug
/// report's log self-identifies exactly what was running.
fn log_startup_versions() {
    let app = env!("CARGO_PKG_VERSION");
    let sdk = std::fs::read_to_string("TEAMTALK_DLL/TEAMTALK_SDK_VERSION.txt")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let tools = crate::youtube::setup::installed_tool_versions();
    let yt = tools.yt_dlp.as_deref().unwrap_or("not installed");
    let bg = tools.bgutil.as_deref().unwrap_or("not installed");
    tracing::info!(
        "Versions — app: v{app}, TeamTalk SDK: {sdk}, yt-dlp: {yt}, bgutil-pot: {bg}"
    );
}

fn schedule_radio_prefetch(
    tx: &tokio::sync::mpsc::UnboundedSender<BotCommand>,
    seed_uri: String,
    delay_secs: f32,
    slot: &Arc<parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>>,
) {
    let tx = tx.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs_f32(delay_secs)).await;
        let _ = tx.send(BotCommand::RadioPreFetch { seed_uri });
    });
    // Replace (and cancel) any previously-scheduled prefetch so stale timers
    // for tracks the user has already moved past don't pile up.
    if let Some(old) = slot.lock().replace(handle) {
        old.abort();
    }
}

/// How many tracks each background batch fetches, and the pause between
/// batches. Pacing keeps the request stream looking like a normal client.
const BULK_BG_BATCH: usize = 25;
const BULK_BG_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// Fetch the remaining tracks of a bulk load (playlist / liked songs) in paced
/// batches, appending each batch to the queue. Dies silently the moment the
/// state's bulk_load_generation no longer matches `generation` (stop, queue
/// clear, or a newer bulk load).
fn spawn_bulk_loader(
    metadata: crate::spotify::metadata::SpotifyMetadata,
    state: crate::bot::state::SharedState,
    uris: Vec<librespot_core::spotify_uri::SpotifyUri>,
    requester: String,
    generation: u64,
) {
    tokio::spawn(async move {
        for chunk in uris.chunks(BULK_BG_BATCH) {
            if state.lock().bulk_load_generation != generation {
                return;
            }
            let tracks = metadata.fetch_tracks_meta(chunk).await;
            let batch: Vec<crate::track::Track> = tracks.into_iter().map(Into::into).collect();
            {
                let mut s = state.lock();
                if s.bulk_load_generation != generation {
                    return;
                }
                s.enqueue_all(batch, requester.clone(), false);
            }
            tokio::time::sleep(BULK_BG_DELAY).await;
        }
        tracing::info!("Background bulk load complete");
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
        wait_ms += current.track.duration_ms().saturating_sub(state.position_ms) as u64;
    }
    for entry in state.queue.iter().skip(current_idx + 1).take(upcoming_pos - 1) {
        wait_ms += entry.track.duration_ms() as u64;
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
    youtube_metadata: Arc<crate::youtube::metadata::YouTubeMetadata>,
    youtube_player: crate::youtube::player::YouTubePlayer,
    session: librespot_core::session::Session,
    auth: crate::spotify::auth::SpotifyAuth,
    spotify_connected: bool,
    state: SharedState,
    client: Arc<::teamtalk::Client>,
    search_limit: u8,
    radio_batch_size: u8,
    radio_delay: f32,
    radio_cmd_tx: tokio::sync::mpsc::UnboundedSender<BotCommand>,
    bot_gender: ::teamtalk::types::UserGender,
    config_store: Arc<crate::config::ConfigStore>,
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
        player, metadata, youtube_metadata, youtube_player, session, auth,
        spotify_connected, state, client,
        search_limit, radio_batch_size, radio_delay, radio_cmd_tx,
        bot_gender, config_store, audio_reset, timing_reset, pause_flag,
        volume_for_save, exit_reason, shutdown, event_tx,
    } = ctx;

    // Tracks whether the Spotify session has been connected yet. Starts false
    // for YouTube-only users; flipped true on first successful `ensure_spotify!`.
    let mut spotify_connected = spotify_connected;

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
    // Holds the most-recently-scheduled radio prefetch timer so a new schedule
    // cancels the previous one instead of leaking sleeping tasks.
    let radio_prefetch_slot: Arc<parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>> =
        Arc::new(parking_lot::Mutex::new(None));

    let send_event = {
        let tx = event_tx;
        move |evt: RunnerEvent| {
            if let Some(ref tx) = tx {
                let _ = tx.send(evt);
            }
        }
    };

    // Connect the Spotify session on first use. No-op once connected. For a
    // YouTube-only user this is where the OAuth browser finally opens — on their
    // first Spotify command, not at startup.
    macro_rules! ensure_spotify {
        () => {{
            if spotify_connected {
                Ok(())
            } else {
                send_event(RunnerEvent::Authenticating);
                let r = auth.connect_existing(&session).await;
                if r.is_ok() {
                    spotify_connected = true;
                }
                r
            }
        }};
    }

    let reply = |user_id: i32, text: &str| {
        if user_id > 0 {
            crate::bot::commands::send_reply(&client, user_id, text);
        }
    };

    let set_status = |text: &str| {
        let mut status = ::teamtalk::types::UserStatus::default();
        status.gender = bot_gender;
        let _ = client.set_status(status, text);
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

    // Update the TT status line and emit a Playing event for a now-playing
    // track. Callers send their own (varied) "Now playing"/"Radio" reply text.
    let announce_playing_status = |name: &str| {
        let status_text = now_playing_status(name, &state);
        set_status(&status_text);
        send_event(RunnerEvent::Playing(name.to_string()));
    };

    let stop_playback = |player: &SpotifyPlayer, youtube_player: &crate::youtube::player::YouTubePlayer, client: &::teamtalk::Client, state: &SharedState, audio_reset: &AtomicBool, pause_flag: &AtomicBool| {
        use crate::player::MediaPlayer as _;
        pause_flag.store(false, Ordering::Relaxed);
        player.stop();
        youtube_player.stop();
        crate::tt::audio_inject::flush_audio(client);
        let _ = client.enable_voice_transmission(false);
        audio_reset.store(true, Ordering::Relaxed);
        let mut s = state.lock();
        s.status = PlaybackStatus::Idle;
    };

    let start_track = |service: crate::services::Service, uri_str: &str, player: &SpotifyPlayer, youtube_player: &crate::youtube::player::YouTubePlayer, client: &::teamtalk::Client, state: &SharedState, audio_reset: &AtomicBool, pause_flag: &AtomicBool| -> bool {
        use crate::player::MediaPlayer;
        match service {
            crate::services::Service::Spotify => {
                if let Ok(uri) = SpotifyUri::from_uri(uri_str) {
                    pause_flag.store(false, Ordering::Relaxed);
                    player.stop();
                    youtube_player.stop();
                    crate::tt::audio_inject::flush_audio(client);
                    let _ = client.enable_voice_transmission(false);
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
            }
            crate::services::Service::YouTube => {
                pause_flag.store(false, Ordering::Relaxed);
                player.stop();
                youtube_player.stop();
                crate::tt::audio_inject::flush_audio(client);
                let _ = client.enable_voice_transmission(false);
                audio_reset.store(true, Ordering::Relaxed);
                youtube_player.load(uri_str);
                {
                    let mut s = state.lock();
                    s.status = PlaybackStatus::Loading;
                    s.tracks_played += 1;
                }
                true
            }
        }
    };

    let do_exit = |reason: BotExit| {
        use crate::player::MediaPlayer as _;
        player.stop();
        youtube_player.stop();
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
            config_store.update(|cfg| {
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

    // Count consecutive track-start failures so a queue full of dead entries
    // (e.g. unresolvable URIs) doesn't loop forever auto-skipping.
    const MAX_CONSECUTIVE_START_FAILURES: u32 = 3;
    let mut consec_start_failures: u32 = 0;

    // Start a track; on failure report to the requester and auto-skip to the
    // next entry, unless too many have failed in a row (then stop and go idle).
    // Expands to a bool: true = now playing, false = failed (caller skips its
    // "Now playing" replies).
    macro_rules! start_or_skip {
        ($service:expr, $uri:expr, $user_id:expr, $name:expr) => {{
            if start_track($service, $uri, &player, &youtube_player, &client, &state, &audio_reset, &pause_flag) {
                consec_start_failures = 0;
                true
            } else {
                consec_start_failures += 1;
                reply($user_id, &format!("Failed to start track: {}", $name));
                if consec_start_failures >= MAX_CONSECUTIVE_START_FAILURES {
                    consec_start_failures = 0;
                    stop_playback(&player, &youtube_player, &client, &state, &audio_reset, &pause_flag);
                    {
                        let mut s = state.lock();
                        s.clear();
                        s.position_ms = 0;
                    }
                    set_status("Idle");
                    send_event(RunnerEvent::Idle);
                } else {
                    let _ = radio_cmd_tx.send(BotCommand::Next { user_id: 0 });
                }
                false
            }
        }};
    }

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            BotCommand::SearchAndPlay { query, user_id, user_name } => {
                let active = state.lock().active_service;
                type ResolveOk = (Vec<crate::track::Track>, Vec<librespot_core::spotify_uri::SpotifyUri>);
                let result: Result<ResolveOk, BotError> = match active {
                    crate::services::Service::Spotify => {
                        if let Err(e) = ensure_spotify!() {
                            reply(user_id, &format!("Spotify unavailable: {}", crate::bot::commands::user_error(&e)));
                            continue;
                        }
                        with_reconnect!(metadata.resolve(&query, search_limit))
                            .map(|r| (r.tracks.into_iter().map(Into::into).collect(), r.remaining))
                    }
                    crate::services::Service::YouTube => {
                        youtube_metadata.resolve(&query, search_limit).await
                            .map(|v| (v.into_iter().map(Into::into).collect(), Vec::new()))
                    }
                };
                match result {
                    Ok((tracks, remaining_uris)) => {
                        if tracks.is_empty() {
                            reply(user_id, "No results found");
                            continue;
                        }

                        // A search returns the top single match; a URL/URI for
                        // a playlist or album resolves into multiple entries.
                        let is_multi = tracks.len() > 1;
                        let tracks_to_add = tracks;

                        let first_name = tracks_to_add[0].display_name();
                        let first_uri = tracks_to_add[0].uri().to_string();
                        let first_service = tracks_to_add[0].service();
                        let count = tracks_to_add.len();

                        // Hold lock across idle check + enqueue to prevent race.
                        // A generation is claimed only for loads that continue in
                        // the background, so single/album plays don't kill an
                        // in-flight bulk loader.
                        let (is_idle, loader_gen) = {
                            let mut s = state.lock();
                            let idle = s.status == PlaybackStatus::Idle;
                            if idle {
                                s.clear();
                            }
                            s.enqueue_all(tracks_to_add, user_name.clone(), !is_multi);
                            let generation = if remaining_uris.is_empty() {
                                None
                            } else {
                                Some(s.begin_bulk_load())
                            };
                            (idle, generation)
                        };

                        if let Some(generation) = loader_gen {
                            spawn_bulk_loader(
                                metadata.clone(),
                                state.clone(),
                                remaining_uris,
                                user_name.clone(),
                                generation,
                            );
                        }
                        let more = if loader_gen.is_some() { ", more loading" } else { "" };

                        if is_idle {
                            if start_or_skip!(first_service, &first_uri, user_id, &first_name) {
                                if count > 1 {
                                    reply(user_id, &format!("Now playing: {first_name} (+{} queued{more})", count - 1));
                                } else {
                                    reply(user_id, &format!("Now playing: {first_name}"));
                                }
                                announce_playing_status(&first_name);

                                if !is_multi {
                                    let radio_on = state.lock().radio_enabled;
                                    if radio_on {
                                        schedule_radio_prefetch(&radio_cmd_tx, first_uri.clone(), radio_delay, &radio_prefetch_slot);
                                    }
                                }
                            }
                        } else {
                            let msg = {
                                let s = state.lock();
                                let upcoming = queue_wait_info(&s);
                                if count > 1 {
                                    format!("Queued {count} tracks{upcoming}{more}")
                                } else {
                                    format!("Queued: {first_name}{upcoming}")
                                }
                            };
                            reply(user_id, &msg);
                        }
                    }
                    Err(e) => {
                        reply(user_id, &format!("Search failed: {}", crate::bot::commands::user_error(&e)));
                    }
                }
            }

            BotCommand::Play { user_id: _ } => {
                use crate::player::MediaPlayer as _;
                pause_flag.store(false, Ordering::Relaxed);
                timing_reset.store(true, Ordering::Relaxed);
                player.play();
                youtube_player.play();
                let mut s = state.lock();
                s.status = PlaybackStatus::Playing;
                if let Some(entry) = s.current() {
                    send_event(RunnerEvent::Playing(entry.track.display_name()));
                }
            }

            BotCommand::Pause { user_id: _ } => {
                use crate::player::MediaPlayer as _;
                pause_flag.store(true, Ordering::Relaxed);
                player.pause();
                youtube_player.pause();
                crate::tt::audio_inject::flush_audio(&client);
                let mut s = state.lock();
                s.status = PlaybackStatus::Paused;
                send_event(RunnerEvent::Idle);
            }

            BotCommand::Stop { user_id: _ } => {
                stop_playback(&player, &youtube_player, &client, &state, &audio_reset, &pause_flag);
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
                    let seed = s.current().map(|e| e.track.uri().to_string());
                    let allow = s.current().map(|e| e.allow_recommend).unwrap_or(false);
                    let played: Vec<String> = s.queue.iter().map(|e| e.track.id().to_string()).collect();
                    (seed, allow, played)
                };

                let next = {
                    let mut s = state.lock();
                    s.advance().map(|e| (e.track.service(), e.track.uri().to_string(), e.track.display_name()))
                };
                if let Some((service, uri_str, name)) = next {
                    if start_or_skip!(service, &uri_str, user_id, &name) {
                        reply(user_id, &format!("Now playing: {name}"));
                        announce_playing_status(&name);

                        let (radio_on, at_end, allow_rec) = {
                            let s = state.lock();
                            let at_end = s.current_index.map(|i| i + 1 >= s.queue.len()).unwrap_or(true);
                            let allow = s.current().map(|e| e.allow_recommend).unwrap_or(false);
                            (s.radio_enabled, at_end, allow)
                        };
                        if radio_on && at_end && allow_rec {
                            schedule_radio_prefetch(&radio_cmd_tx, uri_str.clone(), radio_delay, &radio_prefetch_slot);
                        }
                    }
                } else {
                    let radio_on = state.lock().radio_enabled;

                    // Track whether a radio track was successfully started; if not,
                    // fall through to a clean idle state below.
                    let mut resumed = false;
                    if radio_on && pre_allow_rec {
                        if let Some(seed) = pre_seed_uri {
                            if let Ok(seed_parsed) = SpotifyUri::from_uri(&seed) {
                                reply(user_id, "Radio: fetching recommendations...");
                                match with_reconnect!(metadata.get_radio_tracks(&seed_parsed, radio_batch_size as usize, &pre_played_ids)) {
                                    Ok(tracks) if !tracks.is_empty() => {
                                        let tracks: Vec<crate::track::Track> = tracks.into_iter().map(Into::into).collect();
                                        let first_uri = tracks[0].uri().to_string();
                                        let first_name = tracks[0].display_name();
                                        {
                                            let mut s = state.lock();
                                            s.enqueue_all(tracks, "Radio".to_string(), true);
                                        }
                                        if start_or_skip!(crate::services::Service::Spotify, &first_uri, user_id, &first_name) {
                                            resumed = true;
                                            reply(user_id, &format!("Radio: {first_name}"));
                                            announce_playing_status(&first_name);
                                        }
                                    }
                                    Ok(_) => {
                                        reply(user_id, "Radio: no recommendations found");
                                    }
                                    Err(e) => {
                                        reply(user_id, &format!("Radio failed: {}", crate::bot::commands::user_error(&e)));
                                    }
                                }
                            }
                        }
                    } else if user_id > 0 {
                        reply(user_id, "End of queue");
                    }

                    // Nothing left playing: reset to a clean idle state so the
                    // status line and PlaybackStatus don't stay stuck on "Playing".
                    if !resumed {
                        stop_playback(&player, &youtube_player, &client, &state, &audio_reset, &pause_flag);
                        {
                            let mut s = state.lock();
                            s.position_ms = 0;
                        }
                        set_status("Idle");
                        send_event(RunnerEvent::Idle);
                    }
                }
            }

            BotCommand::Prev { user_id } => {
                let prev = {
                    let mut s = state.lock();
                    s.go_prev().map(|e| (e.track.service(), e.track.uri().to_string(), e.track.display_name()))
                };
                if let Some((service, uri_str, name)) = prev {
                    if start_or_skip!(service, &uri_str, user_id, &name) {
                        reply(user_id, &format!("Now playing: {name}"));
                        announce_playing_status(&name);
                    }
                }
            }

            BotCommand::Seek { offset_ms, user_id: _ } => {
                use crate::player::MediaPlayer as _;
                let (new_pos, service) = {
                    let mut s = state.lock();
                    let current = s.position_ms as i32;
                    let pos = (current + offset_ms).max(0) as u32;
                    let svc = s.current().map(|e| e.track.service()).unwrap_or(s.active_service);
                    // Optimistically reflect the new position immediately so a
                    // rapid second seek computes from the intended target.
                    s.position_ms = pos;
                    (pos, svc)
                };
                audio_reset.store(true, Ordering::Relaxed);
                match service {
                    crate::services::Service::Spotify => player.seek(new_pos),
                    crate::services::Service::YouTube => youtube_player.seek(new_pos),
                }
            }

            BotCommand::SetVolume { .. } => {
                // Debounce: only save if no further volume change within 3 seconds.
                if !pending_volume_save.load(Ordering::Relaxed) {
                    pending_volume_save.store(true, Ordering::Relaxed);
                    let save_flag = pending_volume_save.clone();
                    let vol_ref = volume_for_save.clone();
                    let store = config_store.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        let vol = vol_ref.load(Ordering::Relaxed);
                        store.update(|cfg| {
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
                config_store.update(|cfg| {
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
                let active = state.lock().active_service;
                let result: Result<Vec<crate::track::Track>, BotError> = match active {
                    crate::services::Service::Spotify => {
                        if let Err(e) = ensure_spotify!() {
                            reply(user_id, &format!("Spotify unavailable: {}", crate::bot::commands::user_error(&e)));
                            continue;
                        }
                        with_reconnect!(metadata.search_tracks(&query, search_limit))
                            .map(|v| v.into_iter().map(Into::into).collect())
                    }
                    crate::services::Service::YouTube => {
                        youtube_metadata.search_tracks(&query, search_limit).await
                            .map(|v| v.into_iter().map(Into::into).collect())
                    }
                };
                match result {
                    Ok(tracks) => {
                        reply(user_id, &crate::bot::commands::format_search_results(&tracks));
                        state.lock().insert_search_results(user_id, tracks);
                    }
                    Err(e) => {
                        reply(user_id, &format!("Search failed: {}", crate::bot::commands::user_error(&e)));
                    }
                }
            }

            BotCommand::SearchPick { user_id, pick, user_name } => {
                let picked = {
                    let mut s = state.lock();
                    let track = s.pick_search_result(user_id, pick);
                    track.map(|track| {
                        s.remove_search_results(user_id);
                        let idle = s.status == PlaybackStatus::Idle;
                        if idle { s.clear(); }
                        let service = track.service();
                        let uri_str = track.uri().to_string();
                        let track_name = track.display_name();
                        s.enqueue(track, user_name, true);
                        (service, uri_str, track_name, idle)
                    })
                };
                if let Some((service, uri_str, track_name, is_idle)) = picked {
                    if is_idle {
                        if start_or_skip!(service, &uri_str, user_id, &track_name) {
                            reply(user_id, &format!("Now playing: {track_name}"));
                            announce_playing_status(&track_name);

                            let radio_on = state.lock().radio_enabled;
                            if radio_on {
                                schedule_radio_prefetch(&radio_cmd_tx, uri_str.clone(), radio_delay, &radio_prefetch_slot);
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
                    let _ = client.join_channel(channel_id, "");
                }
            }

            BotCommand::ChangeNick { name, user_id: _ } => {
                let _ = client.change_nickname(&name);
            }

            BotCommand::SetGender { gender, user_id: _ } => {
                let new_gender = crate::config::parse_gender(&gender);
                let current_name = state.lock().current().map(|e| e.track.display_name());
                let status_text = current_name
                    .map(|name| now_playing_status(&name, &state))
                    .unwrap_or_else(|| "Idle".to_string());
                let mut status = ::teamtalk::types::UserStatus::default();
                status.gender = new_gender;
                let _ = client.set_status(status, &status_text);
                config_store.update(|cfg| {
                    cfg.bot_gender = gender;
                });
            }

            BotCommand::TrackEnded { generation, error } => {
                // Drop stale end-of-track signals from a track the user has
                // already skipped or stopped (generation no longer current).
                if youtube_player.is_stale_generation(generation) {
                    tracing::debug!("Ignoring stale YouTube TrackEnded (gen {generation})");
                    continue;
                }
                if let Some(e) = error {
                    tracing::warn!("YouTube track ended with error: {e}");
                }
                // Advance exactly like a natural end-of-track.
                let _ = radio_cmd_tx.send(BotCommand::Next { user_id: 0 });
            }

            BotCommand::PreloadNext => {
                let next_uri = {
                    let s = state.lock();
                    if s.repeat_track {
                        s.current().map(|e| e.track.uri().to_string())
                    } else if let Some(idx) = s.current_index {
                        let next = idx + 1;
                        if next < s.queue.len() {
                            Some(s.queue[next].track.uri().to_string())
                        } else if s.repeat_queue && !s.queue.is_empty() {
                            Some(s.queue[0].track.uri().to_string())
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
                    let cur_uri = s.current().map(|e| e.track.uri().to_string());
                    let at_end = s.current_index.map(|i| i + 1 >= s.queue.len()).unwrap_or(true);
                    let allow = s.current().map(|e| e.allow_recommend).unwrap_or(false);
                    (s.radio_enabled, s.status != PlaybackStatus::Idle, cur_uri, at_end, allow)
                };

                if radio_on && is_active && allow_rec && current_uri.as_deref() == Some(&seed_uri) && queue_at_end {
                    if let Ok(seed_parsed) = SpotifyUri::from_uri(&seed_uri) {
                        let played_ids: Vec<String> = {
                            let s = state.lock();
                            s.queue.iter().map(|e| e.track.id().to_string()).collect()
                        };
                        match metadata.get_radio_tracks(&seed_parsed, radio_batch_size as usize, &played_ids).await {
                            Ok(tracks) if !tracks.is_empty() => {
                                let tracks: Vec<crate::track::Track> = tracks.into_iter().map(Into::into).collect();
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

            BotCommand::SetService { service, user_id: _ } => {
                state.lock().active_service = service;
                tracing::info!("Active service switched to {}", service.name());
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
            PlayerEvent::EndOfTrack { track_id, .. } => {
                // Guard against a stale EndOfTrack for a track we've already
                // moved past (e.g. the user skipped just as it ended), which
                // would otherwise double-advance the queue. Only advance if the
                // ended track is still the current one.
                let is_current = {
                    let s = state.lock();
                    match (s.current().map(|e| e.track.uri().to_string()), track_id.to_uri()) {
                        (Some(cur_uri), Ok(ended_uri)) => cur_uri == ended_uri,
                        // If we can't compare, fall back to advancing (old behavior).
                        _ => true,
                    }
                };
                if is_current {
                    tracing::info!("Track ended, advancing to next");
                    let _ = cmd_tx.send(BotCommand::Next { user_id: 0 });
                } else {
                    tracing::debug!("Ignoring stale Spotify EndOfTrack for {track_id:?}");
                }
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
    use crate::track::Track;

    fn track(id: &str, duration_ms: u32) -> Track {
        Track::Spotify(SpotifyTrack {
            id: id.to_string(),
            name: format!("T{id}"),
            artists: vec!["A".to_string()],
            album: "Album".to_string(),
            duration_ms,
            uri: format!("spotify:track:{id}"),
        })
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
