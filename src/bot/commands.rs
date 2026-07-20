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
    /// Internal: a YouTube track finished (or errored). `generation` identifies
    /// which load this belongs to so a stale completion (after the user already
    /// skipped/stopped) is dropped instead of double-advancing the queue.
    /// `error` carries a short failure reason when playback did not end cleanly.
    TrackEnded { generation: u64, error: Option<String> },
}

#[derive(Debug)]
pub enum PlaybackMode {
    RepeatTrack,
    RepeatQueue,
    Shuffle,
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
                        PlaybackStatus::Paused => {
                            self.send(BotCommand::Play { user_id: sender_id });
                            self.reply_t(client, sender_id, Key::Resuming, &[]);
                        }
                        PlaybackStatus::Playing => {
                            self.send(BotCommand::Pause { user_id: sender_id });
                            self.reply_t(client, sender_id, Key::Paused, &[]);
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
                    let total = state.queue.len();
                    let idx = state.current_index.map(|i| i + 1).unwrap_or(0);
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
                            // Offset from current position (rm 1 = next upcoming track)
                            let state = self.state.lock();
                            let base = state.current_index.map(|i| i + 1).unwrap_or(0);
                            let abs_idx = base + n - 1;
                            if abs_idx >= state.queue.len() {
                                drop(state);
                                self.reply_t(client, sender_id, Key::NoTrackAtPosition, &[
                                    ("position", n.to_string()),
                                ]);
                            } else {
                                let name = state.queue[abs_idx].track.display_name();
                                drop(state);
                                self.send(BotCommand::QueueRemove { index: abs_idx, user_id: sender_id });
                                self.reply_t(client, sender_id, Key::Removed, &[("name", name)]);
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
                    "s" | "shuffle" => {
                        self.send(BotCommand::SetMode { mode: PlaybackMode::Shuffle, user_id: sender_id });
                        self.reply_t(client, sender_id, Key::ModeShuffle, &[]);
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
                        .map(|results| format_search_results(results, &header, &footer));
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
                let queue_len = state.queue.len();
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
                    let text = help_text(active, is_admin);
                    self.reply(client, sender_id, &text);
                } else {
                    let topic = args.trim().to_lowercase();
                    // Hide gated topics from non-admins: fall to "Unknown command".
                    if !is_admin
                        && matches!(topic.as_str(), "q" | "quit" | "rs" | "restart" | "jc" | "glang")
                    {
                        self.reply(
                            client,
                            sender_id,
                            "Unknown command. Type h for the command list.",
                        );
                        return true;
                    }
                    let detail: &str = match topic.as_str() {
                        "p" | "play" => HELP_PLAY,
                        "s" | "stop" => "s / stop\nStop playback and clear the queue.",
                        "n" | "next" => "n / next\nSkip to the next track in the queue.\nIf radio is on and queue is empty, fetches recommendations.",
                        "o" | "prev" => "o / prev\nGo back to the previous track in the queue.",
                        "replay" | "rp" => "replay / rp\nRestart the current track from the beginning.",
                        "c" | "current" => "c / current\nShow the currently playing track with position, duration, and active modes.",
                        "queue" => HELP_QUEUE,
                        "mode" => HELP_MODE,
                        "v" | "volume" => HELP_VOLUME,
                        "sf" | "sb" | "seek" => HELP_SEEK,
                        "search" => HELP_SEARCH,
                        "radio" if active == Service::Spotify => HELP_RADIO,
                        "radio" => return true, // silent on non-Spotify
                        "link" | "url" => "link / url\nGet the URL for the currently playing track.\nOpen it in the service's app or share it with others.",
                        "stats" => "stats\nShow bot uptime, tracks played this session, queue length, and volume.",
                        "jc" => "jc <path>\nJoin a TeamTalk channel by path.\nExample: jc /Music Room",
                        "lang" => "lang [code]\nShow available languages, or set your own.\nYour choice is remembered by username.\nlang clear removes your choice (follow the server default).\nExample: lang de",
                        "glang" => "glang <code>\nSet the server default language (admin).\nUsers who picked their own language with lang keep it.",
                        "cn" => "cn <name>\nChange the bot's nickname.\nExample: cn DJ Bot",
                        "gender" => "gender <male|female|neutral>\nSet the bot's gender (affects TT avatar).\nAliases: m, f, n, man, woman, nb",
                        "sp" | "spotify" | "yt" | "youtube" => HELP_SERVICE,
                        "rs" | "restart" => "rs / restart\nRestart the bot. Saves config before exit.",
                        "q" | "quit" => "q / quit\nShut down the bot. Saves config before exit.",
                        _ => "Unknown command. Type h for the command list.",
                    };
                    self.reply(client, sender_id, detail);
                }
            }

            _ => {}
        }

        true
    }
}

/// Build help text for the currently active service.
/// Spotify-only sections (radio) are omitted on YouTube.
fn help_text(active: Service, is_admin: bool) -> String {
    let mut out = String::from(
        "Playback:\n\
         \x20 p <query>      Search and play a track, playlist, or album\n\
         \x20 p               Toggle play/pause\n\
         \x20 s               Stop playback and clear queue\n\
         \x20 n               Next track\n\
         \x20 o               Previous track\n\
         \x20 replay          Restart current track\n\
         \x20 c               Show current track info\n\
         \n\
         Queue:\n\
         \x20 queue           Show the queue\n\
         \x20 queue clear     Clear upcoming tracks\n\
         \x20 queue rm <N>    Remove Nth upcoming track\n\
         \n\
         Modes:\n\
         \x20 mode [r|rq|s|off]   Set repeat/shuffle mode\n",
    );
    if active == Service::Spotify {
        out.push_str("  radio [on|off]      Toggle radio (auto-recommendations)\n");
        out.push_str("  liked               Play your Liked Songs (also: fav)\n");
    }
    out.push_str(
        "\n\
         Audio:\n\
         \x20 v [0-100]       Get or set volume\n\
         \x20 sf/sb [N]       Seek forward/backward N seconds\n\
         \n\
         Search:\n\
         \x20 search <query>  Search and pick from results\n\
         \x20 <number>        Pick a search result\n\
         \x20 a / cancel      Cancel search\n\
         \n\
         Service:\n\
         \x20 /sp             Switch to Spotify\n\
         \x20 /yt             Switch to YouTube\n\
         \n\
         Bot:\n\
         \x20 link         Get URL for current track\n\
         \x20 stats        Show bot uptime and session stats\n\
         \x20 lang         Show or set your language\n\
         \x20 cn <name>    Change nickname\n\
         \x20 gender       Set bot gender\n\
         \x20 info         Bot info\n",
    );
    if is_admin {
        out.push_str(
            "\x20 jc <path>    Join channel\n\
             \x20 glang        Set the server default language\n\
             \x20 rs           Restart\n\
             \x20 q            Quit\n",
        );
    }
    out.push_str(
        "\n\
         Active service: ",
    );
    out.push_str(active.name());
    out.push_str("\nType h <command> for detailed help (e.g. h queue)");
    out
}

const HELP_SERVICE: &str = "\
/sp / /yt
  /sp     Switch active service to Spotify.
  /yt     Switch active service to YouTube.
Commands like p, search, n, o target the active service.
Switching does not interrupt playback. Use s to stop.";

const HELP_PLAY: &str = "\
p / play
  p <query>   Search Spotify and play the first result.
              If already playing, queues the track instead.
              Accepts track names, Spotify URLs, playlist URLs, album URLs.
  p           Toggle play/pause when no query given.
              If paused: resumes. If playing: pauses.
Examples:
  p photograph
  p spotify:track:6rqhFgbbKwnb9MLmUQDhG6
  p https://open.spotify.com/playlist/...";

const HELP_QUEUE: &str = "\
queue
  queue          Show all tracks in the queue with positions.
  queue clear    Remove all upcoming tracks (keeps current).
  queue rm <N>   Remove the Nth upcoming track.
                 N=1 is the next track after the current one.
Examples:
  queue rm 1     Remove the next upcoming track
  queue rm 3     Remove the 3rd upcoming track
  queue clear    Clear everything after current track";

const HELP_MODE: &str = "\
mode [r|rq|s|off]
  mode r     Repeat current track
  mode rq    Repeat entire queue (loop)
  mode s     Shuffle (random order from upcoming tracks)
  mode off   Disable all modes
  mode       Show current mode";

const HELP_VOLUME: &str = "\
v / volume [0-100]
  v          Show current volume
  v 50       Set volume to 50%
  v50        Set volume to 50% (no space)
  volume 30  Set volume to 30%
Volume is capped by the configured max volume.";

const HELP_SEEK: &str = "\
sf / sb [seconds]
  sf         Seek forward 10 seconds (default)
  sb         Seek backward 10 seconds (default)
  sf30       Seek forward 30 seconds
  sb 5       Seek backward 5 seconds";

const HELP_SEARCH: &str = "\
search <query>
  Search Spotify and show results. Then:
  <number>   Pick a result to play/queue
  a / cancel Dismiss search results
Example:
  search photograph
  2          Play the 2nd result";

const HELP_RADIO: &str = "\
radio [on|off]
  radio on   Enable radio mode. When a single track finishes
             and the queue is empty, automatically fetches
             Spotify recommendations based on the last track.
             Does not trigger for playlists or albums.
  radio off  Disable radio mode.
  radio      Show current radio status.";

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, args: &str) -> Input {
        Input::Command { name: name.to_string(), args: args.to_string() }
    }

    // -- help_text admin gating --

    #[test]
    fn help_hides_admin_commands_from_non_admins() {
        // Admins see the gated jc/rs/q lines.
        let admin = help_text(Service::Spotify, true);
        assert!(admin.contains("jc <path>"), "admin help should list jc");
        assert!(admin.contains("Join channel"), "admin help should list jc");
        assert!(admin.contains("Restart\n"), "admin help should list rs/Restart");
        assert!(admin.contains("Quit"), "admin help should list q/Quit");
        assert!(admin.contains("glang"), "admin help should list glang");

        // Non-admins must not even see that those commands exist.
        let plain = help_text(Service::Spotify, false);
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
