use std::fmt::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

use teamtalk::Client;

use crate::bot::state::{PlaybackStatus, SharedState};
use crate::i18n::{I18n, Key};
use crate::services::Service;

/// Commands sent from the bot thread to the async command processor.
#[derive(Debug)]
#[allow(dead_code)] // user_id fields kept for consistent command protocol + debug logging
pub enum BotCommand {
    SearchAndPlay { query: String, user_id: i32, user_name: String },
    Play { user_id: i32 },
    Pause { user_id: i32 },
    Stop { user_id: i32 },
    /// `after_track`: Some(uri) when sent automatically because that track
    /// ended or failed — the handler drops it if the queue already moved past
    /// that track (races with a manual `n`). None for a user-issued skip.
    Next { user_id: i32, after_track: Option<String> },
    Prev { user_id: i32 },
    Seek { offset_ms: i32, user_id: i32 },
    SetVolume { percent: u8, user_id: i32 },
    SetMode { mode: PlaybackMode, user_id: i32 },
    SetShuffle { enable: bool, user_id: i32 },
    RadioToggle { enable: bool, user_id: i32 },
    QueueClear { user_id: i32 },
    QueueRemove { index: usize, user_id: i32 },
    SearchOnly { query: String, user_id: i32 },
    SearchPick { user_id: i32, pick: usize, user_name: String },
    JoinChannel { path: String, user_id: i32 },
    ChangeNick { name: String, user_id: i32 },
    SetGender { gender: String, user_id: i32 },
    Quit { user_id: i32 },
    Restart { user_id: i32 },
    SetService { service: Service, user_id: i32 },
    /// Admin: set the server-wide default language (glang). Persisted to config.
    SetDefaultLanguage { code: String, user_id: i32 },
    /// Internal: pre-fetch radio recommendations for the given seed track
    RadioPreFetch { seed_uri: String },
    /// Internal: preload next track for gapless playback
    PreloadNext,
    /// Internal: start whatever the queue says is current. Sent when a
    /// background load fills a queue that had run dry - the queue makes the
    /// first arrival current, but only the command loop can start audio.
    StartCurrent,
    /// Internal: a YouTube track finished (or errored). `generation` identifies
    /// which load this belongs to so a stale completion (after the user already
    /// skipped/stopped) is dropped instead of double-advancing the queue.
    /// `error` carries a short failure reason when playback did not end cleanly.
    TrackEnded { generation: u64, error: Option<String> },
}

/// What happens when a track ends. Shuffle is a separate toggle, the way it is
/// in Spotify, so it can be combined with either repeat mode.
#[derive(Debug)]
pub enum PlaybackMode {
    RepeatTrack,
    RepeatQueue,
    Off,
}

/// Maximum reply length before message-chunking kicks in.
pub const MAX_REPLY_LEN: usize = 500;

/// Split a message into chunks no larger than `max_len`, splitting on line
/// boundaries (never mid-line). A line that is itself longer than `max_len` is
/// returned as a single oversized chunk rather than truncated.
///
/// Empty input returns an empty Vec (nothing to send).
pub fn chunk_message(text: &str, max_len: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    if text.len() <= max_len {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    for line in text.lines() {
        if !chunk.is_empty() && chunk.len() + 1 + line.len() > max_len {
            chunks.push(std::mem::take(&mut chunk));
        }
        if !chunk.is_empty() {
            chunk.push('\n');
        }
        chunk.push_str(line);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

/// Send a reply to a user, splitting at line boundaries if it exceeds MAX_REPLY_LEN.
pub fn send_reply(client: &Client, user_id: i32, text: &str) {
    let uid = ::teamtalk::types::UserId(user_id);
    for chunk in chunk_message(text, MAX_REPLY_LEN) {
        let _ = client.send_to_user(uid, &chunk);
    }
}

/// Result of the first-pass classification of an incoming message, before any
/// command-specific handling. Pure and unit-tested (see tests below).
#[derive(Debug, PartialEq)]
enum Input {
    /// Empty/whitespace-only message.
    Empty,
    /// Search cancellation word (a / cancel / abort / exit).
    Cancel,
    /// Bare number (search pick). `n` is as typed (1-based; 0 is a no-op).
    Number(usize),
    /// A command word plus its (case-preserved) argument string.
    Command { name: String, args: String },
}

/// Classify raw message text: strip an optional `/`/`!` prefix, detect cancel
/// words and bare-number picks, otherwise split into a lowercased command word
/// and its trimmed argument string.
fn classify_input(text: &str) -> Input {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Input::Empty;
    }
    let stripped = trimmed
        .strip_prefix('/')
        .or_else(|| trimmed.strip_prefix('!'))
        .unwrap_or(trimmed);

    match stripped.to_lowercase().as_str() {
        "a" | "cancel" | "abort" | "exit" => return Input::Cancel,
        _ => {}
    }
    if let Ok(n) = stripped.parse::<usize>() {
        return Input::Number(n);
    }
    let (cmd, args) = stripped
        .split_once(|c: char| c.is_whitespace())
        .map(|(c, a)| (c, a.trim()))
        .unwrap_or((stripped, ""));
    Input::Command {
        name: cmd.to_lowercase(),
        args: args.to_string(),
    }
}

/// Parsed volume command. `Set` carries the raw requested percent (unbounded;
/// the caller clamps against `max_volume`).
#[derive(Debug, PartialEq)]
enum VolumeParse {
    Show,
    Set(u16),
}

/// Parse a volume command word + args, matching `v`, `volume`, `v50`, `v 50`.
/// Returns `None` if the command word is not a volume command at all.
fn parse_volume(cmd: &str, args: &str) -> Option<VolumeParse> {
    let is_vol_cmd = cmd == "v"
        || cmd == "volume"
        || (cmd.starts_with('v') && cmd.len() > 1 && cmd[1..].chars().all(|c| c.is_ascii_digit()));
    if !is_vol_cmd {
        return None;
    }
    let vol_str = if cmd.len() > 1 && cmd.starts_with('v') && cmd != "volume" {
        &cmd[1..]
    } else {
        args
    };
    match vol_str.parse::<u16>() {
        Ok(v) => Some(VolumeParse::Set(v)),
        Err(_) => Some(VolumeParse::Show),
    }
}

/// Parsed seek command. `Seconds` is signed (negative = backward).
#[derive(Debug, PartialEq)]
enum SeekParse {
    Seconds(i32),
    Usage,
}

/// Parse a seek command word + args. Matches bare `sf`/`sb` (default 10s) or
/// `sf`/`sb` immediately followed by digits (`sf10`); a non-numeric explicit
/// arg yields `Usage`. Returns `None` for anything that is not a seek command
/// (notably "sblah", which must not silently seek).
fn parse_seek(cmd: &str, args: &str) -> Option<SeekParse> {
    let is_seek = (cmd == "sf" || cmd == "sb")
        || ((cmd.starts_with("sf") || cmd.starts_with("sb"))
            && cmd.len() > 2
            && cmd[2..].chars().all(|c| c.is_ascii_digit()));
    if !is_seek {
        return None;
    }
    let direction: i32 = if cmd.starts_with("sf") { 1 } else { -1 };
    let num_str = if cmd.len() > 2 { &cmd[2..] } else { args };
    let secs: i32 = if num_str.is_empty() {
        10
    } else {
        match num_str.parse() {
            Ok(n) => n,
            Err(_) => return Some(SeekParse::Usage),
        }
    };
    Some(SeekParse::Seconds(direction * secs))
}

/// Parse the argument to `shuffle`. No argument toggles, so the answer depends
/// on `currently_on`. `None` means the argument was not understood.
fn parse_shuffle(args: &str, currently_on: bool) -> Option<bool> {
    match args.trim().to_lowercase().as_str() {
        "" => Some(!currently_on),
        "on" | "1" | "yes" | "true" => Some(true),
        "off" | "0" | "no" | "false" => Some(false),
        _ => None,
    }
}

/// Sanitize an error for display to a user: collapse to a single line and cap
/// the length, so a raw multi-line `Display` (which may embed internal detail)
/// doesn't flood a TeamTalk PM. Logs keep the full error.
pub fn user_error(e: impl std::fmt::Display) -> String {
    const MAX: usize = 200;
    let one_line: String = e
        .to_string()
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = one_line.trim();
    if trimmed.chars().count() > MAX {
        let head: String = trimmed.chars().take(MAX - 3).collect();
        format!("{head}...")
    } else {
        trimmed.to_string()
    }
}

/// Render a search-results numbered listing. Header and footer are passed in
/// (translated by the caller) so this stays a pure formatter.
pub fn format_search_results(
    tracks: &[crate::track::Track],
    header: &str,
    footer: &str,
) -> String {
    let mut msg = format!("{header}\n");
    for (i, track) in tracks.iter().enumerate() {
        let _ = writeln!(msg, "  {}: {} [{}]",
            i + 1, track.display_name(), track.duration_display());
    }
    msg.push_str(footer);
    msg
}

/// Shared resources for command dispatch.
pub struct CommandDispatcher {
    pub state: SharedState,
    pub volume: Arc<AtomicU8>,
    pub cmd_tx: UnboundedSender<BotCommand>,
    pub max_volume: u8,
    pub start_time: std::time::Instant,
    pub auth: crate::bot::auth::AdminAuth,
    pub i18n: Arc<I18n>,
}

impl CommandDispatcher {
    fn send(&self, cmd: BotCommand) {
        if let Err(e) = self.cmd_tx.send(cmd) {
            tracing::error!("Failed to send command: {e}");
        }
    }

    fn reply(&self, client: &Client, user_id: i32, text: &str) {
        send_reply(client, user_id, text);
    }

    /// Reply with a translated message, resolved for the target user's
    /// language (seeded at dispatch). Help and the language-control surface
    /// keep using plain `reply` (always English) per the i18n design.
    fn reply_t(&self, client: &Client, user_id: i32, key: Key, args: &[(&str, String)]) {
        self.reply(client, user_id, &self.i18n.tr(user_id, key, args));
    }

    /// Whether the caller may use admin-gated commands. Resolves the sender's
    /// TeamTalk user_type from the client cache (lazy; only callers that need
    /// it call this). Falls back to non-admin (0) if the user is not cached.
    fn is_caller_admin(&self, client: &Client, sender_id: i32, username: &str) -> bool {
        let user_type = client
            .get_user(::teamtalk::types::UserId(sender_id))
            .map(|u| u.user_type)
            .unwrap_or(0);
        self.auth.is_admin(username, user_type)
    }

    /// Dispatch a text message as a command. Returns true if handled, false to stop the bot.
    pub fn dispatch(&self, client: &Client, text: &str, sender_id: i32, username: &str) -> bool {
        // Resolve and cache the sender's language first: every reply in this
        // dispatch and in the async command processor reads the cache by id.
        self.i18n.seed(sender_id, username);

        let (cmd, args) = match classify_input(text) {
            Input::Empty => return true,
            Input::Cancel => {
                let mut state = self.state.lock();
                let removed = state.remove_search_results(sender_id);
                drop(state);
                if removed {
                    self.reply_t(client, sender_id, Key::SearchCancelled, &[]);
                }
                return true;
            }
            Input::Number(n) => {
                if n > 0 {
                    self.send(BotCommand::SearchPick {
                        user_id: sender_id,
                        pick: n - 1,
                        user_name: format!("User#{sender_id}"),
                    });
                }
                return true;
            }
            Input::Command { name, args } => (name, args),
        };
        let args = args.as_str();

        // Admin gate: gated commands require an admin. A non-admin gets a SILENT
        // no-op (no reply) so the command's existence is never revealed, matching
        // the repo's service-private silent no-op convention. Every other command
        // falls through untouched.
        if crate::bot::auth::is_admin_command(&cmd)
            && !self.is_caller_admin(client, sender_id, username)
        {
            return true;
        }

        tracing::info!("Command from user {sender_id}: {cmd} {args}");

        // Volume and seek use dedicated parsers so their fiddly forms (v50,
        // sf10, and rejecting "sblah") stay unit-testable.
        if let Some(vol) = parse_volume(&cmd, args) {
            match vol {
                VolumeParse::Set(v) => {
                    if v > self.max_volume as u16 {
                        self.reply_t(client, sender_id, Key::VolumeRange, &[
                            ("max", self.max_volume.to_string()),
                            ("got", v.to_string()),
                        ]);
                    } else {
                        let capped = (v as u8).min(self.max_volume);
                        self.volume.store(capped, Ordering::Relaxed);
                        self.send(BotCommand::SetVolume { percent: capped, user_id: sender_id });
                        self.reply_t(client, sender_id, Key::VolumeSet, &[
                            ("percent", capped.to_string()),
                        ]);
                    }
                }
                VolumeParse::Show => {
                    let vol = self.volume.load(Ordering::Relaxed);
                    self.reply_t(client, sender_id, Key::VolumeShow, &[
                        ("percent", vol.to_string()),
                        ("max", self.max_volume.to_string()),
                    ]);
                }
            }
            return true;
        }
        if let Some(seek) = parse_seek(&cmd, args) {
            match seek {
                SeekParse::Seconds(secs) => {
                    self.send(BotCommand::Seek { offset_ms: secs * 1000, user_id: sender_id });
                    let key = if secs >= 0 { Key::SeekForward } else { Key::SeekBackward };
                    self.reply_t(client, sender_id, key, &[
                        ("seconds", secs.abs().to_string()),
                    ]);
                }
                SeekParse::Usage => {
                    self.reply_t(client, sender_id, Key::SeekUsage, &[]);
                }
            }
            return true;
        }

        match cmd.as_str() {
            // -- Playback --
            "p" | "play" => {
                if !args.is_empty() {
                    self.send(BotCommand::SearchAndPlay {
                        query: args.to_string(),
                        user_id: sender_id,
                        user_name: format!("User#{sender_id}"),
                    });
                    self.reply_t(client, sender_id, Key::Searching, &[]);
                } else {
                    let status = self.state.lock().status;
                    match status {
                        PlaybackStatus::Loading => {
                            self.reply_t(client, sender_id, Key::LoadingTrack, &[]);
                        }
                        // No reply either way: the music stopping or starting
                        // says it faster than a message can, and every other
                        // reply the bot sends carries something the audio does
                        // not. The branches below speak because nothing audible
                        // happens in them.
                        PlaybackStatus::Paused => {
                            self.send(BotCommand::Play { user_id: sender_id });
                        }
                        PlaybackStatus::Playing => {
                            self.send(BotCommand::Pause { user_id: sender_id });
                        }
                        PlaybackStatus::Idle => {
                            self.reply_t(client, sender_id, Key::NothingToPlay, &[]);
                        }
                    }
                }
            }
            "s" | "stop" => {
                self.send(BotCommand::Stop { user_id: sender_id });
            }
            "n" | "next" => {
                self.send(BotCommand::Next { user_id: sender_id, after_track: None });
            }
            "o" | "prev" => {
                self.send(BotCommand::Prev { user_id: sender_id });
            }
            // Restart the current track from the start. Reuses Seek: a large
            // negative offset clamps to position 0 (works for both services).
            "replay" | "rp" => {
                self.send(BotCommand::Seek { offset_ms: -86_400_000, user_id: sender_id });
                self.reply_t(client, sender_id, Key::RestartingTrack, &[]);
            }
            // Spotify-only: queue the user's Liked Songs. Silently no-ops on
            // YouTube (service-private, same convention as radio).
            "liked" | "fav" => {
                if self.state.lock().active_service == Service::Spotify {
                    self.send(BotCommand::SearchAndPlay {
                        query: "spotify:collection:liked".to_string(),
                        user_id: sender_id,
                        user_name: format!("User#{sender_id}"),
                    });
                    self.reply_t(client, sender_id, Key::LoadingLiked, &[]);
                }
            }

            // -- Info --
            "c" | "current" => {
                let state = self.state.lock();
                if let Some(entry) = state.current() {
                    let pos_secs = state.position_ms / 1000;
                    let pos = format!("{}:{:02}", pos_secs / 60, pos_secs % 60);
                    let (idx, total) = state.queue.position();
                    let args = [
                        ("track", entry.track.display_name()),
                        ("index", idx.to_string()),
                        ("total", total.to_string()),
                        ("position", pos),
                        ("duration", entry.track.duration_display()),
                        ("modes", state.mode_display()),
                    ];
                    drop(state);
                    self.reply_t(client, sender_id, Key::CurrentTrack, &args);
                } else {
                    drop(state);
                    self.reply_t(client, sender_id, Key::NothingPlaying, &[]);
                }
            }

            // -- Queue --
            "queue" => {
                if args.starts_with("clear") {
                    self.send(BotCommand::QueueClear { user_id: sender_id });
                    self.reply_t(client, sender_id, Key::QueueCleared, &[]);
                } else if let Some(rest) = args.strip_prefix("rm") {
                    let rest = rest.trim();
                    if let Ok(n) = rest.parse::<usize>() {
                        if n == 0 {
                            self.reply_t(client, sender_id, Key::IndexStartsAtOne, &[]);
                        } else {
                            // `n` is the number shown by `queue`: 1 is the next
                            // track up, and the playing track has no number.
                            let state = self.state.lock();
                            let name = state.upcoming().nth(n - 1).map(|e| e.track.display_name());
                            drop(state);
                            match name {
                                Some(name) => {
                                    self.send(BotCommand::QueueRemove { index: n, user_id: sender_id });
                                    self.reply_t(client, sender_id, Key::Removed, &[("name", name)]);
                                }
                                None => self.reply_t(client, sender_id, Key::NoTrackAtPosition, &[
                                    ("position", n.to_string()),
                                ]),
                            }
                        }
                    } else {
                        self.reply_t(client, sender_id, Key::QueueRmUsage, &[]);
                    }
                } else {
                    let state = self.state.lock();
                    let display = state.queue_display();
                    drop(state);
                    self.reply(client, sender_id, &display);
                }
            }

            // -- Modes --
            "mode" => {
                match args.trim() {
                    "r" | "repeat" => {
                        self.send(BotCommand::SetMode { mode: PlaybackMode::RepeatTrack, user_id: sender_id });
                        self.reply_t(client, sender_id, Key::ModeRepeatTrack, &[]);
                    }
                    "rq" | "repeat_queue" => {
                        self.send(BotCommand::SetMode { mode: PlaybackMode::RepeatQueue, user_id: sender_id });
                        self.reply_t(client, sender_id, Key::ModeRepeatQueue, &[]);
                    }
                    "off" | "o" | "none" => {
                        self.send(BotCommand::SetMode { mode: PlaybackMode::Off, user_id: sender_id });
                        self.reply_t(client, sender_id, Key::ModeOff, &[]);
                    }
                    _ => {
                        let state = self.state.lock();
                        let display = state.mode_display();
                        drop(state);
                        self.reply_t(client, sender_id, Key::ModeUsage, &[("modes", display)]);
                    }
                }
            }

            "shuffle" => {
                // A toggle of its own rather than a mode, so it can be combined
                // with repeat.
                let currently_on = self.state.lock().shuffle;
                let Some(enable) = parse_shuffle(args, currently_on) else {
                    self.reply_t(client, sender_id, Key::ShuffleUsage, &[]);
                    return true;
                };
                self.send(BotCommand::SetShuffle { enable, user_id: sender_id });
                let key = if enable { Key::ShuffleOn } else { Key::ShuffleOff };
                self.reply_t(client, sender_id, key, &[]);
            }

            // (volume and seek are handled before this match via parse_volume /
            // parse_seek so their fiddly forms stay unit-testable.)

            // -- Search --
            "search" => {
                if !args.is_empty() {
                    self.send(BotCommand::SearchOnly {
                        query: args.to_string(),
                        user_id: sender_id,
                    });
                    self.reply_t(client, sender_id, Key::Searching, &[]);
                } else {
                    // Re-display active search results if available
                    let header = self.i18n.tr(sender_id, Key::SearchResultsHeader, &[]);
                    let footer = self.i18n.tr(sender_id, Key::SearchResultsFooter, &[]);
                    let msg = self.state.lock()
                        .get_search_results(sender_id)
                        .map(|results| format_search_results(&results, &header, &footer));
                    match msg {
                        Some(m) => self.reply(client, sender_id, &m),
                        None => self.reply_t(client, sender_id, Key::SearchUsage, &[]),
                    }
                }
            }
            "pick" => {
                let trimmed = args.trim();
                if trimmed.is_empty() {
                    self.reply_t(client, sender_id, Key::PickUsage, &[]);
                } else if let Ok(n) = trimmed.parse::<usize>() {
                    if n > 0 {
                        self.send(BotCommand::SearchPick {
                            user_id: sender_id,
                            pick: n - 1,
                            user_name: format!("User#{sender_id}"),
                        });
                    } else {
                        self.reply_t(client, sender_id, Key::PickTooLow, &[]);
                    }
                } else {
                    self.reply_t(client, sender_id, Key::PickUsage, &[]);
                }
            }

            // -- Radio (Spotify-only; silently ignored on other services) --
            "radio" => {
                if self.state.lock().active_service != Service::Spotify {
                    return true;
                }
                let arg = args.trim().to_lowercase();
                if arg.starts_with("on") {
                    if self.state.lock().radio_enabled {
                        self.reply_t(client, sender_id, Key::RadioAlreadyOn, &[]);
                    } else {
                        self.send(BotCommand::RadioToggle { enable: true, user_id: sender_id });
                        self.reply_t(client, sender_id, Key::RadioEnabled, &[]);
                    }
                } else if arg.starts_with("off") {
                    if !self.state.lock().radio_enabled {
                        self.reply_t(client, sender_id, Key::RadioAlreadyOff, &[]);
                    } else {
                        self.send(BotCommand::RadioToggle { enable: false, user_id: sender_id });
                        self.reply_t(client, sender_id, Key::RadioDisabled, &[]);
                    }
                } else {
                    let key = if self.state.lock().radio_enabled {
                        Key::RadioStatusOn
                    } else {
                        Key::RadioStatusOff
                    };
                    self.reply_t(client, sender_id, key, &[]);
                }
            }

            // -- Link --
            "link" | "url" => {
                let url = self.state.lock().current().map(|e| e.track.web_url());
                match url {
                    Some(u) => self.reply(client, sender_id, &u),
                    None => self.reply_t(client, sender_id, Key::NothingPlaying, &[]),
                }
            }

            // -- Service switching --
            "sp" | "spotify" => {
                if self.state.lock().active_service == Service::Spotify {
                    self.reply_t(client, sender_id, Key::AlreadyOnService, &[
                        ("service", "Spotify".to_string()),
                    ]);
                } else {
                    self.send(BotCommand::SetService { service: Service::Spotify, user_id: sender_id });
                    self.reply_t(client, sender_id, Key::SwitchedService, &[
                        ("service", "Spotify".to_string()),
                    ]);
                }
            }
            "yt" | "youtube" => {
                if self.state.lock().active_service == Service::YouTube {
                    self.reply_t(client, sender_id, Key::AlreadyOnService, &[
                        ("service", "YouTube".to_string()),
                    ]);
                } else {
                    self.send(BotCommand::SetService { service: Service::YouTube, user_id: sender_id });
                    self.reply_t(client, sender_id, Key::SwitchedService, &[
                        ("service", "YouTube".to_string()),
                    ]);
                }
            }

            // -- Bot management --
            "jc" => {
                if !args.is_empty() {
                    self.send(BotCommand::JoinChannel { path: args.to_string(), user_id: sender_id });
                }
            }
            "cn" => {
                if !args.is_empty() {
                    self.send(BotCommand::ChangeNick { name: args.to_string(), user_id: sender_id });
                    self.reply_t(client, sender_id, Key::Nickname, &[
                        ("name", args.to_string()),
                    ]);
                }
            }
            "gender" => {
                let g = args.trim().to_lowercase();
                if crate::config::is_valid_gender(&g) {
                    self.send(BotCommand::SetGender { gender: g.clone(), user_id: sender_id });
                    self.reply_t(client, sender_id, Key::GenderSet, &[("gender", g)]);
                } else {
                    self.reply_t(client, sender_id, Key::GenderUsage, &[]);
                }
            }
            "info" | "about" => {
                self.reply_t(client, sender_id, Key::Info, &[
                    ("version", env!("CARGO_PKG_VERSION").to_string()),
                ]);
            }

            // -- Language --
            // The language-control surface deliberately replies in English
            // (except the lang_set confirmation, which renders in the newly
            // picked language): it is the recovery hatch for anyone stuck in
            // a language they cannot read.
            "lang" => {
                let code = args.trim().to_lowercase();
                if code.is_empty() {
                    let listing = self
                        .i18n
                        .available()
                        .into_iter()
                        .map(|(code, name)| format!("  {code} - {name}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let current = self.i18n.lang_of(sender_id);
                    self.reply(client, sender_id, &format!(
                        "Available languages:\n{listing}\nYour language: {current}\n\
                         Use: lang <code>, or lang clear to follow the server default"
                    ));
                } else if code == "clear" {
                    // Remove the personal pick from the prefs list: the user
                    // follows the server default again (and future glang changes).
                    self.i18n.clear_pref(sender_id, username);
                    self.reply(client, sender_id, &format!(
                        "Language preference cleared. You now follow the server default ({})",
                        self.i18n.default_language()
                    ));
                } else if self.i18n.is_available(&code) {
                    self.i18n.set_pref(sender_id, username, &code);
                    let name = self.i18n.language_name(&code);
                    // Confirm in the just-picked language.
                    let msg = self.i18n.tr_in(&code, Key::LangSet, &[("language", name)]);
                    self.reply(client, sender_id, &msg);
                } else {
                    let codes = self
                        .i18n
                        .available()
                        .into_iter()
                        .map(|(code, _)| code)
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.reply(client, sender_id, &format!(
                        "Unknown language: {code}. Available: {codes}"
                    ));
                }
            }
            // Admin-only (gated above via ADMIN_COMMANDS): set the server
            // default language. Personal picks are not touched.
            "glang" => {
                let code = args.trim().to_lowercase();
                if code.is_empty() {
                    self.reply(client, sender_id, &format!(
                        "Default language: {}\nUse: glang <code>",
                        self.i18n.default_language()
                    ));
                } else if self.i18n.is_available(&code) {
                    self.send(BotCommand::SetDefaultLanguage {
                        code,
                        user_id: sender_id,
                    });
                } else {
                    let codes = self
                        .i18n
                        .available()
                        .into_iter()
                        .map(|(code, _)| code)
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.reply(client, sender_id, &format!(
                        "Unknown language: {code}. Available: {codes}"
                    ));
                }
            }
            "stats" => {
                let uptime = self.start_time.elapsed();
                let hours = uptime.as_secs() / 3600;
                let mins = (uptime.as_secs() % 3600) / 60;
                let state = self.state.lock();
                let tracks = state.tracks_played;
                let queue_len = state.upcoming_len();
                let vol = self.volume.load(Ordering::Relaxed);
                drop(state);
                let uptime_str = if hours > 0 {
                    format!("{hours}h {mins}m")
                } else {
                    format!("{mins}m")
                };
                self.reply_t(client, sender_id, Key::Stats, &[
                    ("uptime", uptime_str),
                    ("tracks", tracks.to_string()),
                    ("queue", queue_len.to_string()),
                    ("volume", vol.to_string()),
                ]);
            }
            "q" | "quit" => {
                self.send(BotCommand::Quit { user_id: sender_id });
                return false;
            }
            "rs" | "restart" => {
                self.send(BotCommand::Restart { user_id: sender_id });
                return false;
            }
            "h" | "help" => {
                let active = self.state.lock().active_service;
                let is_admin = self.is_caller_admin(client, sender_id, username);
                if args.is_empty() {
                    let text = help_text(&self.i18n, sender_id, active, is_admin);
                    self.reply(client, sender_id, &text);
                } else {
                    let topic = args.trim().to_lowercase();
                    // Hide gated topics from non-admins: fall to "Unknown command".
                    if !is_admin
                        && matches!(topic.as_str(), "q" | "quit" | "rs" | "restart" | "jc" | "glang")
                    {
                        self.reply_t(client, sender_id, Key::HelpUnknown, &[]);
                        return true;
                    }
                    let key = match topic.as_str() {
                        "p" | "play" => Key::HelpPlay,
                        "s" | "stop" => Key::HelpStop,
                        "n" | "next" => Key::HelpNext,
                        "o" | "prev" => Key::HelpPrev,
                        "replay" | "rp" => Key::HelpReplay,
                        "c" | "current" => Key::HelpCurrent,
                        "queue" => Key::HelpQueue,
                        "mode" => Key::HelpMode,
                        "shuffle" => Key::HelpShuffle,
                        "v" | "volume" => Key::HelpVolume,
                        "sf" | "sb" | "seek" => Key::HelpSeek,
                        "search" => Key::HelpSearch,
                        "radio" if active == Service::Spotify => Key::HelpRadio,
                        "radio" => return true, // silent on non-Spotify
                        "link" | "url" => Key::HelpLink,
                        "stats" => Key::HelpStats,
                        "jc" => Key::HelpJc,
                        "lang" => Key::HelpLang,
                        "glang" => Key::HelpGlang,
                        "cn" => Key::HelpCn,
                        "gender" => Key::HelpGender,
                        "sp" | "spotify" | "yt" | "youtube" => Key::HelpService,
                        "rs" | "restart" => Key::HelpRestart,
                        "q" | "quit" => Key::HelpQuit,
                        _ => Key::HelpUnknown,
                    };
                    let detail = self.i18n.tr(sender_id, key, &[]);
                    self.reply(client, sender_id, &detail);
                }
            }

            _ => {}
        }

        true
    }
}

/// Build help text for the currently active service, in the caller's language.
/// Spotify-only sections (radio, liked) are omitted on YouTube.
fn help_text(i18n: &I18n, user_id: i32, active: Service, is_admin: bool) -> String {
    let mut out = i18n.tr(user_id, Key::HelpOverviewPlayback, &[]);
    if active == Service::Spotify {
        out.push('\n');
        out.push_str(&i18n.tr(user_id, Key::HelpOverviewSpotify, &[]));
    }
    out.push('\n');
    out.push_str(&i18n.tr(user_id, Key::HelpOverviewRest, &[]));
    if is_admin {
        out.push('\n');
        out.push_str(&i18n.tr(user_id, Key::HelpOverviewAdmin, &[]));
    }
    out.push('\n');
    out.push_str(&i18n.tr(user_id, Key::HelpOverviewFooter, &[
        ("service", active.name().to_string()),
    ]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn cmd(name: &str, args: &str) -> Input {
        Input::Command { name: name.to_string(), args: args.to_string() }
    }

    // -- help_text admin gating --

    /// An i18n runtime over the embedded catalogs, in a scratch config dir.
    fn test_i18n(lang: &str) -> I18n {
        let dir = std::env::temp_dir().join(format!(
            "ttspotify_help_{}_{lang}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        I18n::load(&dir, lang)
    }

    #[rstest]
    #[case("en")]
    #[case("es")]
    #[case("pt")]
    #[case("ru")]
    fn every_language_has_its_own_help(#[case] lang: &str) {
        // A missing key falls back to English, so identical output means the
        // translation is absent rather than merely similar.
        let i18n = test_i18n(lang);
        let english = help_text(&test_i18n("en"), 0, Service::Spotify, true);
        let translated = help_text(&i18n, 0, Service::Spotify, true);
        assert!(!translated.is_empty());
        if lang == "en" {
            assert_eq!(translated, english);
        } else {
            assert_ne!(translated, english, "{lang} help is falling back to English");
        }
        // Command names stay in English in every language: they are typed.
        assert!(translated.contains("queue rm"), "{lang}: command names must survive translation");
        assert!(translated.contains("shuffle"), "{lang}: shuffle must be listed");
    }

    #[rstest]
    #[case("en")]
    #[case("es")]
    #[case("pt")]
    #[case("ru")]
    fn every_language_has_per_topic_help(#[case] lang: &str) {
        let i18n = test_i18n(lang);
        let english = test_i18n("en");
        for key in [Key::HelpQueue, Key::HelpMode, Key::HelpShuffle, Key::HelpPrev, Key::HelpRadio] {
            let text = i18n.tr(0, key, &[]);
            assert!(!text.is_empty(), "{lang}: {key:?} is empty");
            // The catalogs store one line per entry with \n escapes; if those
            // survive to the user the help arrives as one unreadable line.
            assert!(text.contains('\n'), "{lang}: {key:?} has no line breaks: {text}");
            assert!(!text.contains("\\n"), "{lang}: {key:?} has unescaped \\n: {text}");
            if lang != "en" {
                assert_ne!(
                    text,
                    english.tr(0, key, &[]),
                    "{lang}: {key:?} is falling back to English"
                );
            }
        }
    }

    #[test]
    fn help_hides_admin_commands_from_non_admins() {
        let i18n = test_i18n("en");
        // Admins see the gated jc/rs/q lines.
        let admin = help_text(&i18n, 0, Service::Spotify, true);
        assert!(admin.contains("jc <path>"), "admin help should list jc");
        assert!(admin.contains("Join channel"), "admin help should list jc");
        assert!(admin.contains("Restart\n"), "admin help should list rs/Restart");
        assert!(admin.contains("Quit"), "admin help should list q/Quit");
        assert!(admin.contains("glang"), "admin help should list glang");

        // Non-admins must not even see that those commands exist.
        let plain = help_text(&i18n, 0, Service::Spotify, false);
        assert!(!plain.contains("jc <path>"), "non-admin help must hide jc");
        assert!(!plain.contains("Join channel"), "non-admin help must hide jc");
        assert!(!plain.contains("Restart\n"), "non-admin help must hide rs");
        assert!(!plain.contains("Quit"), "non-admin help must hide q");
        assert!(!plain.contains("glang"), "non-admin help must hide glang");

        // Non-gated Bot lines stay visible for everyone.
        assert!(plain.contains("Change nickname"), "cn stays visible");
        assert!(plain.contains("Bot info"), "info stays visible");
        assert!(plain.contains("lang "), "lang stays visible for everyone");
    }

    // -- classify_input --

    #[test]
    fn classify_empty_and_whitespace() {
        assert_eq!(classify_input(""), Input::Empty);
        assert_eq!(classify_input("   "), Input::Empty);
    }

    #[test]
    fn classify_strips_slash_and_bang_prefix() {
        assert_eq!(classify_input("/next"), cmd("next", ""));
        assert_eq!(classify_input("!p photograph"), cmd("p", "photograph"));
    }

    #[test]
    fn classify_cancel_words_case_insensitive() {
        for w in ["a", "cancel", "abort", "exit", "CANCEL", "Exit"] {
            assert_eq!(classify_input(w), Input::Cancel, "{w}");
        }
    }

    #[test]
    fn classify_bare_number_is_pick() {
        assert_eq!(classify_input("3"), Input::Number(3));
        assert_eq!(classify_input("0"), Input::Number(0));
    }

    #[test]
    fn classify_lowercases_command_but_preserves_args() {
        assert_eq!(classify_input("PLAY Hello World"), cmd("play", "Hello World"));
        assert_eq!(classify_input("Search Photograph"), cmd("search", "Photograph"));
    }

    #[test]
    fn classify_command_without_args() {
        assert_eq!(classify_input("stop"), cmd("stop", ""));
    }

    // -- parse_volume --

    #[test]
    fn volume_forms() {
        assert_eq!(parse_volume("v", ""), Some(VolumeParse::Show));
        assert_eq!(parse_volume("volume", ""), Some(VolumeParse::Show));
        assert_eq!(parse_volume("v", "50"), Some(VolumeParse::Set(50)));
        assert_eq!(parse_volume("v50", ""), Some(VolumeParse::Set(50)));
        assert_eq!(parse_volume("volume", "30"), Some(VolumeParse::Set(30)));
        // Above-range still parses as Set; caller enforces the cap.
        assert_eq!(parse_volume("v101", ""), Some(VolumeParse::Set(101)));
        // Not a volume command.
        assert_eq!(parse_volume("view", ""), None);
        assert_eq!(parse_volume("next", ""), None);
    }

    // -- parse_seek --

    #[test]
    fn seek_forms() {
        assert_eq!(parse_seek("sf", ""), Some(SeekParse::Seconds(10)));
        assert_eq!(parse_seek("sb", ""), Some(SeekParse::Seconds(-10)));
        assert_eq!(parse_seek("sf30", ""), Some(SeekParse::Seconds(30)));
        assert_eq!(parse_seek("sb", "5"), Some(SeekParse::Seconds(-5)));
        assert_eq!(parse_seek("sf", "abc"), Some(SeekParse::Usage));
    }

    #[test]
    fn seek_rejects_non_seek_words() {
        // Regression: "sblah" must NOT be treated as a seek.
        assert_eq!(parse_seek("sblah", ""), None);
        assert_eq!(parse_seek("sfx", ""), None);
        assert_eq!(parse_seek("stop", ""), None);
    }

    // -- parse_shuffle --

    #[rstest]
    // No argument flips whatever it currently is.
    #[case("", false, Some(true))]
    #[case("", true, Some(false))]
    #[case("  ", true, Some(false))]
    // Explicit forms ignore the current setting.
    #[case("on", true, Some(true))]
    #[case("ON", false, Some(true))]
    #[case("off", false, Some(false))]
    #[case(" Off ", true, Some(false))]
    #[case("yes", false, Some(true))]
    #[case("no", true, Some(false))]
    // Anything else is a usage error rather than a silent toggle.
    #[case("maybe", false, None)]
    #[case("onn", false, None)]
    fn parse_shuffle_forms(
        #[case] args: &str,
        #[case] currently_on: bool,
        #[case] expected: Option<bool>,
    ) {
        assert_eq!(parse_shuffle(args, currently_on), expected);
    }

    #[test]
    fn user_error_collapses_and_caps() {
        assert_eq!(user_error("simple error"), "simple error");
        assert_eq!(user_error("line one\nline two\r\nthree"), "line one line two  three");
        let long = "x".repeat(500);
        let out = user_error(long);
        assert_eq!(out.chars().count(), 200);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn chunk_message_empty_returns_empty_vec() {
        assert!(chunk_message("", 500).is_empty());
    }

    #[test]
    fn chunk_message_short_returns_single_chunk() {
        let chunks = chunk_message("hello", 500);
        assert_eq!(chunks, vec!["hello".to_string()]);
    }

    #[test]
    fn chunk_message_exactly_max_len_returns_single_chunk() {
        let text = "a".repeat(500);
        let chunks = chunk_message(&text, 500);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 500);
    }

    #[test]
    fn chunk_message_multiline_under_max_returns_single_chunk() {
        let text = "line one\nline two\nline three";
        let chunks = chunk_message(text, 500);
        assert_eq!(chunks, vec![text.to_string()]);
    }

    #[test]
    fn chunk_message_splits_on_line_boundary_not_mid_line() {
        // Build a message where each line is 60 chars; with max_len 100,
        // each chunk should hold exactly one line (since 60+1+60 = 121 > 100).
        let line = "x".repeat(60);
        let text = format!("{line}\n{line}\n{line}");
        let chunks = chunk_message(&text, 100);
        assert_eq!(chunks.len(), 3);
        for chunk in &chunks {
            assert_eq!(chunk.len(), 60);
            assert!(!chunk.contains('\n'), "chunk must not span line boundaries");
        }
    }

    #[test]
    fn chunk_message_packs_multiple_lines_per_chunk_when_they_fit() {
        // Three 30-char lines, max 100. First two fit in one chunk
        // (30 + 1 + 30 = 61), third forces a new chunk
        // (61 + 1 + 30 = 92 fits actually). Use sizes that force 2 chunks:
        // 40-char lines, max 100. 40 + 1 + 40 = 81 fits;
        // 81 + 1 + 40 = 122 > 100 → second chunk.
        let line = "y".repeat(40);
        let text = format!("{line}\n{line}\n{line}");
        let chunks = chunk_message(&text, 100);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], format!("{line}\n{line}"));
        assert_eq!(chunks[1], line);
    }

    #[test]
    fn chunk_message_oversized_single_line_returned_as_one_chunk() {
        // Single line longer than max_len: current behavior is to return it as
        // one oversized chunk rather than truncate or split mid-line.
        let line = "z".repeat(700);
        let chunks = chunk_message(&line, 500);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 700);
    }

    #[test]
    fn chunk_message_short_input_is_returned_verbatim() {
        // Short inputs (≤ max) round-trip exactly, including any trailing newline.
        let text = "hello\n";
        let chunks = chunk_message(text, 500);
        assert_eq!(chunks, vec!["hello\n".to_string()]);
    }

    #[test]
    fn chunk_message_long_input_with_trailing_newline_drops_empty_final_chunk() {
        // When the message is split via `lines()`, a trailing newline does not
        // emit an empty final element — `"a\n".lines()` yields just `["a"]`.
        // Build something long enough to force the split path.
        let line = "q".repeat(200);
        let text = format!("{line}\n{line}\n{line}\n");
        let chunks = chunk_message(&text, 250);
        // Each line is 200 chars; 200+1+200=401 > 250, so each chunk = 1 line.
        assert_eq!(chunks.len(), 3);
        for c in &chunks {
            assert_eq!(c.len(), 200);
            assert!(!c.ends_with('\n'));
        }
    }

    #[test]
    fn chunk_message_blank_lines_in_middle_are_preserved() {
        let text = "alpha\n\nbeta";
        let chunks = chunk_message(text, 500);
        assert_eq!(chunks, vec!["alpha\n\nbeta".to_string()]);
    }
}
