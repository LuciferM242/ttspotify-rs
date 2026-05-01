use std::fmt::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

use teamtalk::Client;

use crate::bot::state::{PlaybackStatus, SharedState};
use crate::services::Service;

/// Commands sent from the bot thread to the async command processor.
#[derive(Debug)]
#[allow(dead_code)] // user_id fields kept for consistent command protocol + debug logging
pub enum BotCommand {
    SearchAndPlay { query: String, user_id: i32, user_name: String },
    Play { user_id: i32 },
    Pause { user_id: i32 },
    Stop { user_id: i32 },
    Next { user_id: i32 },
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
    /// Internal: pre-fetch radio recommendations for the given seed track
    RadioPreFetch { seed_uri: String },
    /// Internal: preload next track for gapless playback
    PreloadNext,
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
        client.send_to_user(uid, &chunk);
    }
}

/// Shared resources for command dispatch.
pub struct CommandDispatcher {
    pub state: SharedState,
    pub volume: Arc<AtomicU8>,
    pub cmd_tx: UnboundedSender<BotCommand>,
    pub max_volume: u8,
    pub start_time: std::time::Instant,
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

    /// Dispatch a text message as a command. Returns true if handled, false to stop the bot.
    pub fn dispatch(&self, client: &Client, text: &str, sender_id: i32) -> bool {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return true;
        }

        // Strip optional prefix (/ or !)
        let stripped = trimmed.strip_prefix('/')
            .or_else(|| trimmed.strip_prefix('!'))
            .unwrap_or(trimmed);

        tracing::info!("Command from user {sender_id}: {stripped}");

        // Search cancellation
        match stripped.to_lowercase().as_str() {
            "a" | "cancel" | "abort" | "exit" => {
                let mut state = self.state.lock();
                if state.search_results.remove(&sender_id).is_some() {
                    self.reply(client, sender_id, "Search cancelled");
                }
                return true;
            }
            _ => {}
        }

        // If the entire message is a number, treat as search pick
        if let Ok(n) = stripped.parse::<usize>() {
            if n > 0 {
                self.send(BotCommand::SearchPick {
                    user_id: sender_id,
                    pick: n - 1,
                    user_name: format!("User#{sender_id}"),
                });
            }
            return true;
        }

        // Split into command + args
        let (cmd, args) = stripped.split_once(|c: char| c.is_whitespace())
            .map(|(c, a)| (c, a.trim()))
            .unwrap_or((stripped, ""));
        let cmd = cmd.to_lowercase();

        match cmd.as_str() {
            // -- Playback --
            "p" | "play" => {
                if !args.is_empty() {
                    self.send(BotCommand::SearchAndPlay {
                        query: args.to_string(),
                        user_id: sender_id,
                        user_name: format!("User#{sender_id}"),
                    });
                    self.reply(client, sender_id, "Searching...");
                } else {
                    let status = self.state.lock().status;
                    match status {
                        PlaybackStatus::Loading => {
                            self.reply(client, sender_id, "Loading track...");
                        }
                        PlaybackStatus::Paused => {
                            self.send(BotCommand::Play { user_id: sender_id });
                            self.reply(client, sender_id, "Resuming");
                        }
                        PlaybackStatus::Playing => {
                            self.send(BotCommand::Pause { user_id: sender_id });
                            self.reply(client, sender_id, "Paused");
                        }
                        PlaybackStatus::Idle => {
                            self.reply(client, sender_id, "Nothing to play. Use: p <query>");
                        }
                    }
                }
            }
            "s" | "stop" => {
                self.send(BotCommand::Stop { user_id: sender_id });
            }
            "n" | "next" => {
                self.send(BotCommand::Next { user_id: sender_id });
            }
            "o" | "prev" => {
                self.send(BotCommand::Prev { user_id: sender_id });
            }

            // -- Info --
            "c" | "current" => {
                let state = self.state.lock();
                if let Some(entry) = state.current() {
                    let pos_secs = state.position_ms / 1000;
                    let pos = format!("{}:{:02}", pos_secs / 60, pos_secs % 60);
                    let total = state.queue.len();
                    let idx = state.current_index.map(|i| i + 1).unwrap_or(0);
                    let msg = format!(
                        "{} [{}/{}] ({}/{})\n{}",
                        entry.track.display_name(),
                        idx, total,
                        pos,
                        entry.track.duration_display(),
                        state.mode_display()
                    );
                    drop(state);
                    self.reply(client, sender_id, &msg);
                } else {
                    self.reply(client, sender_id, "Nothing playing");
                }
            }

            // -- Queue --
            "queue" => {
                if args.starts_with("clear") {
                    self.send(BotCommand::QueueClear { user_id: sender_id });
                    self.reply(client, sender_id, "Queue cleared");
                } else if let Some(rest) = args.strip_prefix("rm ") {
                    if let Ok(n) = rest.trim().parse::<usize>() {
                        if n == 0 {
                            self.reply(client, sender_id, "Index starts at 1");
                        } else {
                            // Offset from current position (rm 1 = next upcoming track)
                            let state = self.state.lock();
                            let base = state.current_index.map(|i| i + 1).unwrap_or(0);
                            let abs_idx = base + n - 1;
                            if abs_idx >= state.queue.len() {
                                self.reply(client, sender_id, &format!("No track at position {n}"));
                            } else {
                                let name = state.queue[abs_idx].track.display_name();
                                drop(state);
                                self.send(BotCommand::QueueRemove { index: abs_idx, user_id: sender_id });
                                self.reply(client, sender_id, &format!("Removed: {name}"));
                            }
                        }
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
                        self.reply(client, sender_id, "Repeat track enabled");
                    }
                    "rq" | "repeat_queue" => {
                        self.send(BotCommand::SetMode { mode: PlaybackMode::RepeatQueue, user_id: sender_id });
                        self.reply(client, sender_id, "Repeat queue enabled");
                    }
                    "s" | "shuffle" => {
                        self.send(BotCommand::SetMode { mode: PlaybackMode::Shuffle, user_id: sender_id });
                        self.reply(client, sender_id, "Shuffle enabled");
                    }
                    "off" | "o" | "none" => {
                        self.send(BotCommand::SetMode { mode: PlaybackMode::Off, user_id: sender_id });
                        self.reply(client, sender_id, "All modes disabled");
                    }
                    _ => {
                        let state = self.state.lock();
                        let display = state.mode_display();
                        drop(state);
                        self.reply(client, sender_id, &format!("{display}\nUsage: mode [r|rq|s|off]"));
                    }
                }
            }

            // -- Volume (also handles v50, v 50, volume 50) --
            cmd_str if cmd_str == "v" || cmd_str == "volume"
                || (cmd_str.starts_with('v') && cmd_str.len() > 1
                    && cmd_str[1..].chars().all(|c| c.is_ascii_digit())) =>
            {
                // Handle v50 (no space)
                let vol_str = if cmd_str.len() > 1 && cmd_str.starts_with('v') && cmd_str != "volume" {
                    &cmd_str[1..]
                } else {
                    args
                };

                if let Ok(vol) = vol_str.parse::<u16>() {
                    if vol > self.max_volume as u16 {
                        self.reply(client, sender_id,
                            &format!("Volume must be 0-{}. Got: {vol}", self.max_volume));
                    } else {
                        let capped = (vol as u8).min(self.max_volume);
                        self.volume.store(capped, Ordering::Relaxed);
                        self.send(BotCommand::SetVolume { percent: capped, user_id: sender_id });
                        self.reply(client, sender_id, &format!("Volume: {capped}%"));
                    }
                } else {
                    let vol = self.volume.load(Ordering::Relaxed);
                    self.reply(client, sender_id, &format!("Volume: {vol}% (max: {})", self.max_volume));
                }
            }

            // -- Seek (also handles sf10, sb5) --
            cmd_str if cmd_str.starts_with("sf") || cmd_str.starts_with("sb") => {
                let direction: i32 = if cmd_str.starts_with("sf") { 1 } else { -1 };
                // Try number attached to command (sf10) or in args (sf 10)
                let num_str = if cmd_str.len() > 2 { &cmd_str[2..] } else { args };
                let secs: i32 = num_str.parse().unwrap_or(10);
                self.send(BotCommand::Seek { offset_ms: direction * secs * 1000, user_id: sender_id });
                let dir_word = if direction > 0 { "forward" } else { "backward" };
                self.reply(client, sender_id, &format!("Seeking {dir_word} {secs}s"));
            }

            // -- Search --
            "search" => {
                if !args.is_empty() {
                    self.send(BotCommand::SearchOnly {
                        query: args.to_string(),
                        user_id: sender_id,
                    });
                    self.reply(client, sender_id, "Searching...");
                } else {
                    // Re-display active search results if available
                    let state = self.state.lock();
                    if let Some(results) = state.search_results.get(&sender_id) {
                        let mut msg = String::from("Search results:\n");
                        for (i, track) in results.iter().enumerate() {
                            let _ = write!(msg, "  {}: {} [{}]\n",
                                i + 1, track.display_name(), track.duration_display());
                        }
                        msg.push_str("Type a number to play, or a to cancel");
                        drop(state);
                        self.reply(client, sender_id, &msg);
                    } else {
                        drop(state);
                        self.reply(client, sender_id, "Usage: search <query>");
                    }
                }
            }
            "pick" => {
                let trimmed = args.trim();
                if trimmed.is_empty() {
                    self.reply(client, sender_id, "Usage: pick <number>");
                } else if let Ok(n) = trimmed.parse::<usize>() {
                    if n > 0 {
                        self.send(BotCommand::SearchPick {
                            user_id: sender_id,
                            pick: n - 1,
                            user_name: format!("User#{sender_id}"),
                        });
                    } else {
                        self.reply(client, sender_id, "Pick number must be 1 or higher");
                    }
                } else {
                    self.reply(client, sender_id, "Usage: pick <number>");
                }
            }

            // -- Radio --
            "radio" => {
                let arg = args.trim().to_lowercase();
                if arg.starts_with("on") {
                    if self.state.lock().radio_enabled {
                        self.reply(client, sender_id, "Radio is already on");
                    } else {
                        self.send(BotCommand::RadioToggle { enable: true, user_id: sender_id });
                        self.reply(client, sender_id, "Radio enabled");
                    }
                } else if arg.starts_with("off") {
                    if !self.state.lock().radio_enabled {
                        self.reply(client, sender_id, "Radio is already off");
                    } else {
                        self.send(BotCommand::RadioToggle { enable: false, user_id: sender_id });
                        self.reply(client, sender_id, "Radio disabled");
                    }
                } else {
                    let status = if self.state.lock().radio_enabled { "ON" } else { "OFF" };
                    self.reply(client, sender_id, &format!("Radio: {status}"));
                }
            }

            // -- Link --
            "link" | "url" => {
                let state = self.state.lock();
                if let Some(entry) = state.current() {
                    let url = entry.track.uri()
                        .replace("spotify:track:", "https://open.spotify.com/track/")
                        .replace("spotify:episode:", "https://open.spotify.com/episode/");
                    drop(state);
                    self.reply(client, sender_id, &url);
                } else {
                    self.reply(client, sender_id, "Nothing playing");
                }
            }

            // -- Service switching --
            "sp" | "spotify" => {
                self.send(BotCommand::SetService { service: Service::Spotify, user_id: sender_id });
                self.reply(client, sender_id, "Switched to Spotify");
            }
            "yt" | "youtube" => {
                self.send(BotCommand::SetService { service: Service::YouTube, user_id: sender_id });
                self.reply(client, sender_id, "Switched to YouTube");
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
                    self.reply(client, sender_id, &format!("Nickname: {args}"));
                }
            }
            "gender" => {
                let g = args.trim().to_lowercase();
                if crate::config::is_valid_gender(&g) {
                    self.send(BotCommand::SetGender { gender: g.clone(), user_id: sender_id });
                    self.reply(client, sender_id, &format!("Gender: {g}"));
                } else {
                    self.reply(client, sender_id, "Usage: gender [male|female|neutral]");
                }
            }
            "info" | "about" => {
                self.reply(client, sender_id, &format!(
                    "TeamTalk Spotify Bot (Rust) v{}",
                    env!("CARGO_PKG_VERSION")
                ));
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
                self.reply(client, sender_id, &format!(
                    "Uptime: {uptime_str}\nTracks played: {tracks}\nQueue: {queue_len} tracks\nVolume: {vol}%"
                ));
            }
            "q" | "quit" => {
                self.reply(client, sender_id, "Shutting down...");
                self.send(BotCommand::Quit { user_id: sender_id });
                return false;
            }
            "rs" | "restart" => {
                self.reply(client, sender_id, "Restarting...");
                self.send(BotCommand::Restart { user_id: sender_id });
                return false;
            }
            "h" | "help" => {
                let active = self.state.lock().active_service;
                if args.is_empty() {
                    let text = help_text(active);
                    self.reply(client, sender_id, &text);
                } else {
                    let topic = args.trim().to_lowercase();
                    let detail: &str = match topic.as_str() {
                        "p" | "play" => HELP_PLAY,
                        "s" | "stop" => "s / stop\nStop playback and clear the queue.",
                        "n" | "next" => "n / next\nSkip to the next track in the queue.\nIf radio is on and queue is empty, fetches recommendations.",
                        "o" | "prev" => "o / prev\nGo back to the previous track in the queue.",
                        "c" | "current" => "c / current\nShow the currently playing track with position, duration, and active modes.",
                        "queue" => HELP_QUEUE,
                        "mode" => HELP_MODE,
                        "v" | "volume" => HELP_VOLUME,
                        "sf" | "sb" | "seek" => HELP_SEEK,
                        "search" => HELP_SEARCH,
                        "radio" if active == Service::Spotify => HELP_RADIO,
                        "radio" => "radio is only available on Spotify. Switch with /sp.",
                        "link" | "url" => "link / url\nGet the URL for the currently playing track.\nOpen it in the service's app or share it with others.",
                        "stats" => "stats\nShow bot uptime, tracks played this session, queue length, and volume.",
                        "jc" => "jc <path>\nJoin a TeamTalk channel by path.\nExample: jc /Music Room",
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
fn help_text(active: Service) -> String {
    let mut out = String::from(
        "Playback:\n\
         \x20 p <query>      Search and play a track, playlist, or album\n\
         \x20 p               Toggle play/pause\n\
         \x20 s               Stop playback and clear queue\n\
         \x20 n               Next track\n\
         \x20 o               Previous track\n\
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
         \x20 jc <path>    Join channel\n\
         \x20 cn <name>    Change nickname\n\
         \x20 gender       Set bot gender\n\
         \x20 info         Bot info\n\
         \x20 rs           Restart\n\
         \x20 q            Quit\n\
         \n\
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
