use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;

use moka::sync::Cache;
use parking_lot::Mutex;

use crate::services::Service;
use crate::track::Track;

/// How long a user's search results stay pickable.
const SEARCH_RESULT_TTL: Duration = Duration::from_secs(600);

/// How many users can hold pickable results at once. Bounded so a busy channel
/// cannot grow this without limit.
const SEARCH_RESULT_CAPACITY: u64 = 256;

#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub track: Track,
    #[allow(dead_code)] // stored for future "who queued this" display
    pub requester: String,
    /// Only allow radio recommendations for single-track plays (not playlists/albums)
    pub allow_recommend: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackStatus {
    Idle,
    Loading,
    Playing,
    Paused,
}

/// What happens when a track ends. One setting rather than two booleans:
/// "repeat this track" and "repeat the queue" are alternatives, and holding
/// them as separate flags made the impossible both-at-once state expressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    Track,
    Queue,
}

impl RepeatMode {
    /// Read the pair of booleans the config file stores. A file with both set
    /// is not a state the bot can produce; track-repeat is the narrower of the
    /// two, so it wins.
    pub fn from_flags(track: bool, queue: bool) -> Self {
        match (track, queue) {
            (true, _) => Self::Track,
            (false, true) => Self::Queue,
            (false, false) => Self::Off,
        }
    }

    /// Back to the `(repeat_track, repeat_queue)` pair the config stores.
    pub fn to_flags(self) -> (bool, bool) {
        match self {
            Self::Off => (false, false),
            Self::Track => (true, false),
            Self::Queue => (false, true),
        }
    }
}

#[derive(Debug)]
pub struct PlayerState {
    pub queue: Vec<QueueEntry>,
    pub current_index: Option<usize>,
    pub status: PlaybackStatus,

    // Modes
    pub repeat: RepeatMode,
    pub shuffle: bool,

    // Radio
    pub radio_enabled: bool,

    /// Pickable search results per user. Expiry and the capacity bound are the
    /// cache's job: the hand-rolled version only swept on insert, so results
    /// from a user who searched and walked away outlived their TTL until
    /// somebody else happened to search.
    pub search_results: Cache<i32, Vec<Track>>,

    // Track position tracking
    pub position_ms: u32,

    // Stats
    pub tracks_played: u32,

    // The service that bare commands target (e.g. `p <query>`).
    // Switched via `/sp` or `/yt`. In-memory only — resets on restart.
    pub active_service: Service,

    /// Bumped on stop/clear and each new bulk load; a background bulk loader
    /// captures the value at spawn and dies when it no longer matches.
    pub bulk_load_generation: u64,

    /// True while an automatic advance is in flight. See
    /// `try_arm_auto_advance`.
    auto_advance_pending: bool,
}

pub type SharedState = Arc<Mutex<PlayerState>>;

/// Words that mark a re-release of the same recording rather than a new one.
/// Deliberately excludes "live", "acoustic" and "remix": those are genuinely
/// different performances and should stay separate queue entries.
const REISSUE_MARKERS: [&str; 9] = [
    "remaster", "version", "edit", "mono", "stereo", "anniversary", "deluxe", "reissue", "mix",
];

fn is_reissue_marker(segment: &str) -> bool {
    // "remix" contains "mix" but is a different recording.
    if segment.contains("remix") {
        return false;
    }
    REISSUE_MARKERS.iter().any(|m| segment.contains(m))
}

/// A loose identity for a song: the same recording released twice should share
/// one key. Case, punctuation, spacing and re-release qualifiers are dropped.
fn song_key(display_name: &str) -> String {
    let lower = display_name.to_lowercase();

    // Drop bracketed qualifiers like "(remastered 2011)" or "[radio edit]".
    let mut without_brackets = String::with_capacity(lower.len());
    let mut group = String::new();
    let mut depth = 0usize;
    for ch in lower.chars() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                if depth > 0 {
                    depth -= 1;
                    if !is_reissue_marker(&group) {
                        without_brackets.push_str(&group);
                    }
                    group.clear();
                }
            }
            _ if depth > 0 => group.push(ch),
            _ => without_brackets.push(ch),
        }
    }
    without_brackets.push_str(&group);

    // Drop trailing " - 2011 remaster" style qualifiers, keeping the leading
    // "artist - title" split intact.
    let parts: Vec<&str> = without_brackets.split(" - ").collect();
    let kept: Vec<&str> = parts
        .iter()
        .enumerate()
        .filter(|(i, part)| *i < 2 || !is_reissue_marker(part))
        .map(|(_, part)| *part)
        .collect();

    kept.join(" ")
        .chars()
        // Apostrophes vanish rather than split the word: "don't" == "dont".
        .filter(|c| !matches!(c, '\'' | '\u{2019}'))
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl Default for PlayerState {
    fn default() -> Self {
        Self::new()
    }
}

impl PlayerState {
    pub fn new() -> Self {
        Self {
            queue: Vec::new(),
            current_index: None,
            status: PlaybackStatus::Idle,
            repeat: RepeatMode::Off,
            shuffle: false,
            radio_enabled: false,
            search_results: Cache::builder()
                .max_capacity(SEARCH_RESULT_CAPACITY)
                .time_to_live(SEARCH_RESULT_TTL)
                .build(),
            position_ms: 0,
            tracks_played: 0,
            active_service: Service::default(),
            bulk_load_generation: 0,
            auto_advance_pending: false,
        }
    }

    pub fn current(&self) -> Option<&QueueEntry> {
        self.current_index.and_then(|i| self.queue.get(i))
    }

    /// Store a user's pickable search results.
    pub fn insert_search_results(&mut self, user_id: i32, tracks: Vec<Track>) {
        self.search_results.insert(user_id, tracks);
    }

    /// A user's current search results, if any.
    pub fn get_search_results(&self, user_id: i32) -> Option<Vec<Track>> {
        self.search_results.get(&user_id)
    }

    /// Remove a user's search results; returns whether an entry existed.
    pub fn remove_search_results(&mut self, user_id: i32) -> bool {
        let existed = self.search_results.get(&user_id).is_some();
        self.search_results.invalidate(&user_id);
        existed
    }

    /// Clone the `pick`-th result of a user's search, if present.
    pub fn pick_search_result(&self, user_id: i32, pick: usize) -> Option<Track> {
        self.search_results
            .get(&user_id)
            .and_then(|v| v.get(pick).cloned())
    }

    pub fn enqueue(&mut self, track: Track, requester: String, allow_recommend: bool) {
        self.queue.push(QueueEntry { track, requester, allow_recommend });
        if self.current_index.is_none() {
            self.current_index = Some(0);
        }
    }

    pub fn enqueue_all(&mut self, tracks: Vec<Track>, requester: String, allow_recommend: bool) {
        let was_empty = self.queue.is_empty();
        for track in tracks {
            self.queue.push(QueueEntry {
                track,
                requester: requester.clone(),
                allow_recommend,
            });
        }
        if was_empty && !self.queue.is_empty() {
            self.current_index = Some(0);
        }
    }

    /// Whether either repeat mode is on. Repeat means "keep playing what is
    /// already here", so radio must not append to the queue while it holds.
    pub fn repeat_active(&self) -> bool {
        self.repeat != RepeatMode::Off
    }

    pub fn repeats_track(&self) -> bool {
        self.repeat == RepeatMode::Track
    }

    pub fn repeats_queue(&self) -> bool {
        self.repeat == RepeatMode::Queue
    }

    /// Claim the right to send an automatic advance, returning false when one
    /// is already in flight.
    ///
    /// One track can raise several "this track is done" signals — a natural
    /// end-of-track, an `Unavailable`, a YouTube `TrackEnded` — and each would
    /// send its own advance. The usual defence is the stale check, which
    /// compares the ended track against the current one; under repeat-track
    /// those are always equal, so every duplicate looks fresh and the track
    /// restarts once per signal.
    pub fn try_arm_auto_advance(&mut self) -> bool {
        if self.auto_advance_pending {
            return false;
        }
        self.auto_advance_pending = true;
        true
    }

    /// Release the claim once the advance has been consumed (or abandoned).
    pub fn release_auto_advance(&mut self) {
        self.auto_advance_pending = false;
    }

    /// Append `tracks` and make the first of them the current track, returning
    /// it.
    ///
    /// For refilling a queue that has run dry: playback has walked off the end,
    /// so `current_index` is `None`, but the played entries are still in the
    /// queue. A plain `enqueue_all` only sets the index when the queue was
    /// *empty*, so the caller would start a track while the state still says
    /// nothing is playing — and the next skip then finds nothing to advance
    /// from and reports end-of-queue with tracks sitting right there.
    pub fn enqueue_all_as_current(
        &mut self,
        tracks: Vec<Track>,
        requester: String,
        allow_recommend: bool,
    ) -> Option<&QueueEntry> {
        if tracks.is_empty() {
            return None;
        }
        let first_new = self.queue.len();
        self.enqueue_all(tracks, requester, allow_recommend);
        self.current_index = Some(first_new);
        self.queue.get(first_new)
    }

    /// Advance to the next track because the current one ended. Returns the
    /// next entry if available.
    pub fn advance(&mut self) -> Option<&QueueEntry> {
        self.advance_inner(true)
    }

    /// Advance because the user explicitly skipped (`n`). Repeat-track is a
    /// rule about what happens when a track *ends*, not a lock on the queue —
    /// an explicit skip must always move. At the end of the queue repeat-track
    /// loops back to the start, matching how repeat-one behaves elsewhere.
    pub fn advance_manual(&mut self) -> Option<&QueueEntry> {
        self.advance_inner(false)
    }

    /// Shared advance logic. `honor_repeat_track` is false for manual skips.
    fn advance_inner(&mut self, honor_repeat_track: bool) -> Option<&QueueEntry> {
        if self.queue.is_empty() {
            self.current_index = None;
            return None;
        }

        if self.repeats_track() && honor_repeat_track {
            return self.current();
        }

        // A manual skip under repeat-track still loops the queue rather than
        // running off the end into silence.
        let wrap_at_end = self.repeats_queue() || (self.repeats_track() && !honor_repeat_track);

        if self.shuffle {
            use rand::Rng;
            let mut rng = rand::thread_rng();
            let current = self.current_index.unwrap_or(0);
            // Only shuffle among upcoming tracks (after current).
            let remaining: Vec<usize> = ((current + 1)..self.queue.len()).collect();
            if !remaining.is_empty() {
                let idx = remaining[rng.gen_range(0..remaining.len())];
                self.current_index = Some(idx);
                return self.queue.get(idx);
            } else if wrap_at_end && self.queue.len() > 1 {
                // All tracks played, re-shuffle from start (excluding the one that just played)
                let others: Vec<usize> = (0..self.queue.len()).filter(|&i| i != current).collect();
                if !others.is_empty() {
                    let idx = others[rng.gen_range(0..others.len())];
                    self.current_index = Some(idx);
                    return self.queue.get(idx);
                }
            }
            // Fallthrough: no more tracks
            self.current_index = None;
            return None;
        }

        if let Some(idx) = self.current_index {
            let next = idx + 1;
            if next < self.queue.len() {
                self.current_index = Some(next);
                return self.queue.get(next);
            } else if wrap_at_end {
                self.current_index = Some(0);
                return self.queue.first();
            } else {
                self.current_index = None;
                return None;
            }
        }

        None
    }

    /// Go to previous track.
    pub fn go_prev(&mut self) -> Option<&QueueEntry> {
        if self.queue.is_empty() {
            return None;
        }

        if let Some(idx) = self.current_index {
            if idx > 0 {
                self.current_index = Some(idx - 1);
            } else if self.repeats_queue() {
                self.current_index = Some(self.queue.len() - 1);
            }
        } else {
            self.current_index = Some(self.queue.len() - 1);
        }

        self.current()
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.current_index = None;
        self.status = PlaybackStatus::Idle;
        self.position_ms = 0;
        self.bulk_load_generation += 1;
        // A waiter armed for the track we just stopped will never be consumed.
        self.auto_advance_pending = false;
    }

    /// Drop everything after the current track (or the whole queue when
    /// nothing is playing). Also invalidates any in-flight background bulk
    /// loader — otherwise it would keep re-filling the queue the user just
    /// cleared.
    pub fn clear_upcoming(&mut self) {
        if let Some(idx) = self.current_index {
            self.queue.truncate(idx + 1);
        } else {
            self.queue.clear();
        }
        self.bulk_load_generation += 1;
    }

    /// Start a new bulk load: invalidates any in-flight background loader and
    /// returns the generation the new loader must carry.
    pub fn begin_bulk_load(&mut self) -> u64 {
        self.bulk_load_generation += 1;
        self.bulk_load_generation
    }

    /// Drop incoming tracks that are already in the queue (by track id), so
    /// repeating a bulk source (liked songs, a playlist) doesn't duplicate it.
    pub fn filter_unqueued(&self, tracks: Vec<Track>) -> Vec<Track> {
        let queued: std::collections::HashSet<&str> =
            self.queue.iter().map(|e| e.track.id()).collect();
        tracks
            .into_iter()
            .filter(|t| !queued.contains(t.id()))
            .collect()
    }

    /// Like `filter_unqueued`, but also drops re-releases of songs already
    /// queued (and duplicates inside `tracks` itself).
    ///
    /// Radio recommendations are excluded by track id, and the same recording
    /// carries a different id on every remaster, single and regional release —
    /// so a station seeded from one song can hand back that song again and
    /// again, each copy looking new. Matching on `song_key` catches those.
    pub fn filter_unqueued_similar(&self, tracks: Vec<Track>) -> Vec<Track> {
        let mut seen_ids: std::collections::HashSet<String> =
            self.queue.iter().map(|e| e.track.id().to_string()).collect();
        let mut seen_songs: std::collections::HashSet<String> =
            self.queue.iter().map(|e| song_key(&e.track.display_name())).collect();
        tracks
            .into_iter()
            .filter(|t| {
                seen_ids.insert(t.id().to_string()) && seen_songs.insert(song_key(&t.display_name()))
            })
            .collect()
    }

    /// Drop played entries, keeping at most `keep` of them before the current
    /// track. Upcoming tracks are never touched.
    ///
    /// Radio is an endless source: it appends a batch every time playback
    /// reaches the end, and nothing ever left the queue, so a long session grew
    /// it without bound and made every `queue` listing longer than the last.
    /// Trimming costs the ability to `p` back beyond `keep` tracks.
    pub fn trim_played_history(&mut self, keep: usize) {
        let Some(current) = self.current_index else {
            return;
        };
        if current <= keep {
            return;
        }
        let drop_count = current - keep;
        self.queue.drain(..drop_count);
        self.current_index = Some(current - drop_count);
    }

    pub fn remove(&mut self, index: usize) -> Option<QueueEntry> {
        if index >= self.queue.len() {
            return None;
        }
        let entry = self.queue.remove(index);

        // Adjust current index
        if let Some(ref mut cur) = self.current_index {
            if index < *cur {
                *cur -= 1;
            } else if index == *cur {
                if self.queue.is_empty() {
                    self.current_index = None;
                } else if *cur >= self.queue.len() {
                    *cur = self.queue.len() - 1;
                }
            }
        }

        Some(entry)
    }

    /// The current track followed by what is still to come.
    ///
    /// Played tracks are deliberately left out, the way Spotify's queue and
    /// YouTube Music's "up next" do it: the queue answers "what is coming",
    /// not "what have we heard". Upcoming entries are numbered from 1 so the
    /// numbers shown are the numbers `queue rm N` takes.
    pub fn queue_display(&self) -> String {
        if self.queue.is_empty() {
            return "Queue is empty".to_string();
        }

        let mut out = String::new();
        if let Some(entry) = self.current() {
            let _ = write!(out, "> Now playing [{}]: {} [{}]",
                entry.track.service().marker(),
                entry.track.display_name(), entry.track.duration_display());
        }

        let first_upcoming = self.current_index.map(|i| i + 1).unwrap_or(0);
        let upcoming = self.queue.get(first_upcoming..).unwrap_or(&[]);
        if upcoming.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("  Nothing upcoming");
            return out;
        }

        for (i, entry) in upcoming.iter().enumerate() {
            if !out.is_empty() {
                out.push('\n');
            }
            let _ = write!(out, "  {}. [{}]: {} [{}]",
                i + 1, entry.track.service().marker(),
                entry.track.display_name(), entry.track.duration_display());
        }
        out
    }

    pub fn mode_display(&self) -> String {
        let mut modes = Vec::new();
        match self.repeat {
            RepeatMode::Off => {}
            RepeatMode::Track => modes.push("Repeat Track"),
            RepeatMode::Queue => modes.push("Repeat Queue"),
        }
        if self.shuffle {
            modes.push("Shuffle");
        }
        if modes.is_empty() {
            "No modes active".to_string()
        } else {
            modes.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotify::types::SpotifyTrack;
    use proptest::prelude::*;
    use rstest::rstest;

    #[test]
    fn filter_unqueued_drops_tracks_already_in_queue() {
        let mut state = PlayerState::new();
        state.enqueue(track("a"), "u".into(), true);
        state.enqueue(track("b"), "u".into(), true);
        let incoming = vec![track("b"), track("c")];
        let fresh = state.filter_unqueued(incoming);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].id(), "c");
    }

    #[test]
    fn filter_unqueued_keeps_all_when_queue_empty() {
        let state = PlayerState::new();
        let fresh = state.filter_unqueued(vec![track("a"), track("b")]);
        assert_eq!(fresh.len(), 2);
    }

    #[test]
    fn begin_bulk_load_increments_and_returns_generation() {
        let mut state = PlayerState::new();
        let g1 = state.begin_bulk_load();
        let g2 = state.begin_bulk_load();
        assert_eq!(g2, g1 + 1);
        assert_eq!(state.bulk_load_generation, g2);
    }

    #[test]
    fn clear_invalidates_bulk_load_generation() {
        let mut state = PlayerState::new();
        let g = state.begin_bulk_load();
        state.clear();
        assert_ne!(state.bulk_load_generation, g);
    }

    #[test]
    fn clear_upcoming_keeps_current_and_invalidates_bulk_loader() {
        let mut state = PlayerState::new();
        state.enqueue_all(vec![track("a"), track("b"), track("c")], "u".to_string(), false);
        state.current_index = Some(0);
        let g = state.begin_bulk_load();
        state.clear_upcoming();
        // Current track stays, upcoming dropped, in-flight loader invalidated.
        assert_eq!(state.queue.len(), 1);
        assert_eq!(state.current_index, Some(0));
        assert_ne!(state.bulk_load_generation, g);
    }

    #[test]
    fn clear_upcoming_with_no_current_empties_queue_and_invalidates_loader() {
        let mut state = PlayerState::new();
        state.enqueue_all(vec![track("a"), track("b")], "u".to_string(), false);
        // Played past the end of the queue: entries remain but none is current.
        state.current_index = None;
        let g = state.begin_bulk_load();
        state.clear_upcoming();
        assert!(state.queue.is_empty());
        assert_ne!(state.bulk_load_generation, g);
    }

    fn track(id: &str) -> Track {
        Track::Spotify(SpotifyTrack {
            id: id.to_string(),
            name: format!("Track {id}"),
            artists: vec!["Artist".to_string()],
            album: "Album".to_string(),
            duration_ms: 180_000,
            uri: format!("spotify:track:{id}"),
        })
    }

    /// A track whose `display_name()` is exactly `display` ("Artists - Name").
    fn named(id: &str, display: &str) -> Track {
        let (artists, name) = display.split_once(" - ").unwrap_or(("", display));
        Track::Spotify(SpotifyTrack {
            id: id.to_string(),
            name: name.to_string(),
            artists: vec![artists.to_string()],
            album: "Album".to_string(),
            duration_ms: 180_000,
            uri: format!("spotify:track:{id}"),
        })
    }

    fn fill(state: &mut PlayerState, n: usize) {
        for i in 0..n {
            state.enqueue(track(&i.to_string()), "tester".to_string(), true);
        }
    }

    // -- search results --

    #[test]
    fn insert_and_pick_search_results() {
        let mut state = PlayerState::new();
        state.insert_search_results(7, vec![track("a"), track("b")]);
        assert_eq!(state.pick_search_result(7, 1).unwrap().id(), "b");
        assert!(state.get_search_results(7).is_some());
        assert!(state.remove_search_results(7));
        assert!(state.get_search_results(7).is_none());
    }

    #[test]
    fn one_users_results_do_not_disturb_anothers() {
        // Expiry itself is the cache's job now (and is covered in bot::cache);
        // what this file still owns is that results are kept per user.
        let mut state = PlayerState::new();
        state.insert_search_results(1, vec![track("a")]);
        state.insert_search_results(2, vec![track("b")]);
        assert_eq!(state.pick_search_result(1, 0).unwrap().id(), "a");
        assert!(state.remove_search_results(1));
        assert!(state.get_search_results(1).is_none());
        assert!(state.get_search_results(2).is_some(), "other users are untouched");
    }

    // -- enqueue / enqueue_all --

    #[test]
    fn enqueue_on_empty_queue_sets_current_index() {
        let mut state = PlayerState::new();
        assert_eq!(state.current_index, None);
        state.enqueue(track("a"), "u".into(), true);
        assert_eq!(state.current_index, Some(0));
        assert_eq!(state.queue.len(), 1);
    }

    #[test]
    fn enqueue_on_non_empty_queue_does_not_change_current_index() {
        let mut state = PlayerState::new();
        state.enqueue(track("a"), "u".into(), true);
        state.enqueue(track("b"), "u".into(), true);
        assert_eq!(state.current_index, Some(0));
        assert_eq!(state.queue.len(), 2);
    }

    #[test]
    fn enqueue_all_on_empty_queue_sets_current_index() {
        let mut state = PlayerState::new();
        state.enqueue_all(vec![track("a"), track("b")], "u".into(), false);
        assert_eq!(state.current_index, Some(0));
        assert_eq!(state.queue.len(), 2);
    }

    #[test]
    fn enqueue_all_on_non_empty_queue_keeps_current_index() {
        let mut state = PlayerState::new();
        state.enqueue(track("a"), "u".into(), true);
        state.enqueue_all(vec![track("b"), track("c")], "u".into(), false);
        assert_eq!(state.current_index, Some(0));
        assert_eq!(state.queue.len(), 3);
    }

    #[test]
    fn enqueue_all_with_empty_vec_on_empty_queue_leaves_index_none() {
        let mut state = PlayerState::new();
        state.enqueue_all(vec![], "u".into(), true);
        assert_eq!(state.current_index, None);
    }

    // -- advance: linear --

    #[test]
    fn advance_walks_queue_then_returns_none() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        assert_eq!(state.current_index, Some(0));
        assert_eq!(state.advance().map(|e| e.track.id().to_string()), Some("1".to_string()));
        assert_eq!(state.advance().map(|e| e.track.id().to_string()), Some("2".to_string()));
        assert!(state.advance().is_none());
        assert_eq!(state.current_index, None);
    }

    #[test]
    fn advance_on_empty_queue_returns_none() {
        let mut state = PlayerState::new();
        assert!(state.advance().is_none());
        assert_eq!(state.current_index, None);
    }

    // -- advance: repeat_track --

    // -- song_key / radio dedupe --

    #[test]
    fn song_key_ignores_remaster_and_edition_suffixes() {
        let base = song_key("Artist - Song");
        assert_eq!(song_key("Artist - Song - 2011 Remaster"), base);
        assert_eq!(song_key("Artist - Song (Remastered 2011)"), base);
        assert_eq!(song_key("Artist - Song (Single Version)"), base);
        assert_eq!(song_key("Artist - Song - Radio Edit"), base);
    }

    #[test]
    fn song_key_ignores_case_punctuation_and_spacing() {
        assert_eq!(song_key("Artist - Don't Stop"), song_key("ARTIST  -  Dont  Stop"));
    }

    #[test]
    fn song_key_keeps_genuinely_different_recordings_apart() {
        let base = song_key("Artist - Song");
        assert_ne!(song_key("Artist - Song (Live)"), base, "a live take is a different recording");
        assert_ne!(song_key("Artist - Song (Acoustic)"), base);
        assert_ne!(song_key("Artist - Other Song"), base);
        assert_ne!(song_key("Other Artist - Song"), base);
    }

    #[test]
    fn filter_unqueued_similar_drops_a_rerelease_of_a_queued_song() {
        let mut state = PlayerState::new();
        state.enqueue(named("a", "Artist - Song"), "u".into(), true);
        let incoming = vec![
            named("b", "Artist - Song (Remastered 2011)"),
            named("c", "Artist - Another Song"),
        ];
        let fresh = state.filter_unqueued_similar(incoming);
        assert_eq!(fresh.len(), 1, "the remaster is the same song under a new id");
        assert_eq!(fresh[0].id(), "c");
    }

    #[test]
    fn filter_unqueued_similar_drops_duplicates_within_the_incoming_batch() {
        let state = PlayerState::new();
        let fresh = state.filter_unqueued_similar(vec![
            named("a", "Artist - Song"),
            named("b", "Artist - Song - 2011 Remaster"),
        ]);
        assert_eq!(fresh.len(), 1);
    }

    // -- trim_played_history --

    #[test]
    fn trim_played_history_keeps_the_recent_past_and_repoints_current() {
        let mut state = PlayerState::new();
        fill(&mut state, 10);
        state.current_index = Some(9);
        state.trim_played_history(3);
        // 3 played entries plus the current one.
        assert_eq!(state.queue.len(), 4);
        assert_eq!(state.current_index, Some(3));
        assert_eq!(state.current().unwrap().track.id(), "9");
        assert_eq!(state.queue[0].track.id(), "6");
    }

    #[test]
    fn trim_played_history_keeps_upcoming_tracks() {
        let mut state = PlayerState::new();
        fill(&mut state, 10);
        state.current_index = Some(5);
        state.trim_played_history(1);
        // 1 played + current + the 4 still upcoming.
        assert_eq!(state.queue.len(), 6);
        assert_eq!(state.current().unwrap().track.id(), "5");
        assert_eq!(state.queue.last().unwrap().track.id(), "9");
    }

    #[test]
    fn trim_played_history_does_nothing_when_history_is_short() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.current_index = Some(1);
        state.trim_played_history(20);
        assert_eq!(state.queue.len(), 3);
        assert_eq!(state.current_index, Some(1));
    }

    #[test]
    fn trim_played_history_without_a_current_track_is_a_no_op() {
        let mut state = PlayerState::new();
        fill(&mut state, 5);
        state.current_index = None;
        state.trim_played_history(1);
        assert_eq!(state.queue.len(), 5);
    }

    // -- enqueue_all_as_current --

    #[test]
    fn appending_after_the_queue_ran_out_makes_the_first_new_track_current() {
        // Radio refills the queue once playback has run off the end. The
        // played tracks are still there, so the queue is not empty and a plain
        // append leaves current_index None - the bot then plays a track while
        // its own state says nothing is playing, and `n` reports end of queue
        // with tracks sitting right there.
        let mut state = PlayerState::new();
        fill(&mut state, 2);
        state.current_index = None; // played past the end

        let started = state.enqueue_all_as_current(
            vec![track("r1"), track("r2")], "Radio".into(), true,
        );

        assert_eq!(started.map(|e| e.track.id().to_string()), Some("r1".to_string()));
        assert_eq!(state.current_index, Some(2));
        assert_eq!(state.queue.len(), 4, "played tracks are kept");
    }

    #[test]
    fn a_skip_after_a_radio_refill_moves_to_the_next_new_track() {
        // The reported bug: two tracks in the queue, `n` said end of queue.
        let mut state = PlayerState::new();
        fill(&mut state, 1);
        state.current_index = None;
        state.enqueue_all_as_current(vec![track("r1"), track("r2")], "Radio".into(), true);

        assert_eq!(state.advance_manual().map(|e| e.track.id().to_string()), Some("r2".to_string()));
    }

    #[test]
    fn appending_as_current_to_an_empty_queue_starts_at_the_first_track() {
        let mut state = PlayerState::new();
        let started = state.enqueue_all_as_current(vec![track("a")], "Radio".into(), true);
        assert_eq!(started.map(|e| e.track.id().to_string()), Some("a".to_string()));
        assert_eq!(state.current_index, Some(0));
    }

    #[test]
    fn appending_nothing_as_current_leaves_the_queue_alone() {
        let mut state = PlayerState::new();
        fill(&mut state, 2);
        state.current_index = None;
        assert!(state.enqueue_all_as_current(vec![], "Radio".into(), true).is_none());
        assert_eq!(state.current_index, None);
        assert_eq!(state.queue.len(), 2);
    }

    // -- rendered text (snapshots) --
    //
    // These cover the exact strings a user reads. A wording or layout change
    // shows up as a snapshot diff to review rather than a rewritten assertion.

    #[test]
    fn snapshot_queue_with_played_current_and_upcoming() {
        let mut state = PlayerState::new();
        fill(&mut state, 5);
        state.current_index = Some(2);
        insta::assert_snapshot!(state.queue_display());
    }

    #[test]
    fn snapshot_queue_with_nothing_upcoming() {
        let mut state = PlayerState::new();
        fill(&mut state, 2);
        state.current_index = Some(1);
        insta::assert_snapshot!(state.queue_display());
    }

    #[test]
    fn snapshot_queue_empty() {
        insta::assert_snapshot!(PlayerState::new().queue_display());
    }

    #[rstest]
    #[case::off(RepeatMode::Off, false)]
    #[case::repeat_track(RepeatMode::Track, false)]
    #[case::repeat_queue(RepeatMode::Queue, false)]
    #[case::shuffle(RepeatMode::Off, true)]
    #[case::repeat_queue_and_shuffle(RepeatMode::Queue, true)]
    fn snapshot_mode_line(#[case] repeat: RepeatMode, #[case] shuffle: bool) {
        let mut state = PlayerState::new();
        state.repeat = repeat;
        state.shuffle = shuffle;
        insta::assert_snapshot!(
            format!("{repeat:?}-shuffle-{shuffle}"),
            state.mode_display()
        );
    }

    // -- advance, across every mode (parametrised) --

    #[rstest]
    // Off: walks forward, then stops at the end.
    #[case(RepeatMode::Off, 0, Some("1"))]
    #[case(RepeatMode::Off, 2, None)]
    // Track: stays put wherever it is.
    #[case(RepeatMode::Track, 0, Some("0"))]
    #[case(RepeatMode::Track, 2, Some("2"))]
    // Queue: walks forward, then wraps.
    #[case(RepeatMode::Queue, 0, Some("1"))]
    #[case(RepeatMode::Queue, 2, Some("0"))]
    fn auto_advance_per_repeat_mode(
        #[case] repeat: RepeatMode,
        #[case] from: usize,
        #[case] expected: Option<&str>,
    ) {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.repeat = repeat;
        state.current_index = Some(from);
        let got = state.advance().map(|e| e.track.id().to_string());
        assert_eq!(got.as_deref(), expected);
    }

    #[rstest]
    // A manual skip moves in every mode, and wraps rather than stopping when
    // any repeat mode is on.
    #[case(RepeatMode::Off, 0, Some("1"))]
    #[case(RepeatMode::Off, 2, None)]
    #[case(RepeatMode::Track, 0, Some("1"))]
    #[case(RepeatMode::Track, 2, Some("0"))]
    #[case(RepeatMode::Queue, 0, Some("1"))]
    #[case(RepeatMode::Queue, 2, Some("0"))]
    fn manual_advance_per_repeat_mode(
        #[case] repeat: RepeatMode,
        #[case] from: usize,
        #[case] expected: Option<&str>,
    ) {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.repeat = repeat;
        state.current_index = Some(from);
        let got = state.advance_manual().map(|e| e.track.id().to_string());
        assert_eq!(got.as_deref(), expected);
    }

    // -- queue invariants under arbitrary command sequences --

    /// One queue-mutating operation, as a user could trigger it.
    #[derive(Debug, Clone)]
    enum Op {
        Enqueue,
        EnqueueMany(u8),
        Advance,
        AdvanceManual,
        Prev,
        Remove(u8),
        Trim(u8),
        ClearUpcoming,
        Clear,
        SetRepeat(u8),
        SetShuffle(bool),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            Just(Op::Enqueue),
            (1u8..4).prop_map(Op::EnqueueMany),
            Just(Op::Advance),
            Just(Op::AdvanceManual),
            Just(Op::Prev),
            (0u8..8).prop_map(Op::Remove),
            (0u8..5).prop_map(Op::Trim),
            Just(Op::ClearUpcoming),
            Just(Op::Clear),
            (0u8..3).prop_map(Op::SetRepeat),
            any::<bool>().prop_map(Op::SetShuffle),
        ]
    }

    fn apply(state: &mut PlayerState, op: &Op, next_id: &mut u32) {
        match op {
            Op::Enqueue => {
                state.enqueue(track(&next_id.to_string()), "u".into(), true);
                *next_id += 1;
            }
            Op::EnqueueMany(n) => {
                let batch: Vec<Track> = (0..*n)
                    .map(|_| {
                        let t = track(&next_id.to_string());
                        *next_id += 1;
                        t
                    })
                    .collect();
                state.enqueue_all(batch, "u".into(), true);
            }
            Op::Advance => {
                state.advance();
            }
            Op::AdvanceManual => {
                state.advance_manual();
            }
            Op::Prev => {
                state.go_prev();
            }
            Op::Remove(i) => {
                state.remove(*i as usize);
            }
            Op::Trim(keep) => state.trim_played_history(*keep as usize),
            Op::ClearUpcoming => state.clear_upcoming(),
            Op::Clear => state.clear(),
            Op::SetRepeat(m) => {
                state.repeat = match m {
                    0 => RepeatMode::Off,
                    1 => RepeatMode::Track,
                    _ => RepeatMode::Queue,
                }
            }
            Op::SetShuffle(on) => state.shuffle = *on,
        }
    }

    proptest! {
        /// `current_index` must always name a real entry, or nothing at all.
        /// Every "the bot played the wrong track" bug in this file has been a
        /// violation of exactly this.
        #[test]
        fn current_index_always_points_at_a_real_entry(ops in prop::collection::vec(op_strategy(), 1..40)) {
            let mut state = PlayerState::new();
            let mut next_id = 0u32;
            for op in &ops {
                apply(&mut state, op, &mut next_id);
                if let Some(i) = state.current_index {
                    prop_assert!(
                        i < state.queue.len(),
                        "current_index {i} past queue length {} after {op:?}",
                        state.queue.len()
                    );
                }
                prop_assert!(
                    !(state.queue.is_empty() && state.current_index.is_some()),
                    "empty queue must have no current track (after {op:?})"
                );
            }
        }

        /// Trimming played history is invisible to what is still to come.
        #[test]
        fn trim_never_touches_the_current_or_upcoming_tracks(
            fill_n in 1usize..30,
            current in 0usize..30,
            keep in 0usize..10,
        ) {
            let mut state = PlayerState::new();
            fill(&mut state, fill_n);
            let current = current.min(fill_n - 1);
            state.current_index = Some(current);

            let before_current = state.current().unwrap().track.id().to_string();
            let before_upcoming: Vec<String> = state.queue[current + 1..]
                .iter().map(|e| e.track.id().to_string()).collect();

            state.trim_played_history(keep);

            let after_current = state.current().unwrap().track.id().to_string();
            let idx = state.current_index.unwrap();
            let after_upcoming: Vec<String> = state.queue[idx + 1..]
                .iter().map(|e| e.track.id().to_string()).collect();

            prop_assert_eq!(before_current, after_current);
            prop_assert_eq!(before_upcoming, after_upcoming);
            prop_assert!(idx <= keep, "kept {idx} played entries, asked for at most {}", keep);
        }
    }

    // -- RepeatMode --

    #[test]
    fn repeat_mode_is_one_setting_not_two_flags() {
        let mut state = PlayerState::new();
        assert_eq!(state.repeat, RepeatMode::Off);
        state.repeat = RepeatMode::Track;
        assert!(state.repeats_track());
        assert!(!state.repeats_queue(), "selecting one repeat mode clears the other");
        state.repeat = RepeatMode::Queue;
        assert!(state.repeats_queue());
        assert!(!state.repeats_track());
    }

    #[test]
    fn repeat_mode_round_trips_through_the_config_flags() {
        // The config file stores two booleans; a file with both set is not a
        // state the bot can reach, and must resolve to something sane.
        assert_eq!(RepeatMode::from_flags(false, false), RepeatMode::Off);
        assert_eq!(RepeatMode::from_flags(true, false), RepeatMode::Track);
        assert_eq!(RepeatMode::from_flags(false, true), RepeatMode::Queue);
        assert_eq!(RepeatMode::from_flags(true, true), RepeatMode::Track);
        assert_eq!(RepeatMode::Queue.to_flags(), (false, true));
        assert_eq!(RepeatMode::Off.to_flags(), (false, false));
    }

    // -- repeat_active --

    #[test]
    fn repeat_active_is_false_with_no_modes_or_shuffle_only() {
        let mut state = PlayerState::new();
        assert!(!state.repeat_active());
        state.shuffle = true;
        assert!(!state.repeat_active(), "shuffle alone is not a repeat mode");
    }

    #[test]
    fn repeat_active_is_true_for_either_repeat_mode() {
        let mut state = PlayerState::new();
        state.repeat = RepeatMode::Track;
        assert!(state.repeat_active());
        state.repeat = RepeatMode::Queue;
        assert!(state.repeat_active());
    }

    // -- auto-advance gate --

    #[test]
    fn auto_advance_gate_admits_one_signal_at_a_time() {
        let mut state = PlayerState::new();
        assert!(state.try_arm_auto_advance(), "first end-of-track should advance");
        assert!(!state.try_arm_auto_advance(), "a second signal for the same track is a duplicate");
    }

    #[test]
    fn releasing_an_unarmed_auto_advance_gate_is_harmless() {
        // Stop and clear both release; a release with nothing pending must not
        // leave the gate in a state that swallows the next real signal.
        let mut state = PlayerState::new();
        state.release_auto_advance();
        assert!(state.try_arm_auto_advance());
    }

    #[test]
    fn auto_advance_gate_rearms_once_the_advance_is_consumed() {
        let mut state = PlayerState::new();
        assert!(state.try_arm_auto_advance());
        state.release_auto_advance();
        assert!(state.try_arm_auto_advance(), "the next track must be able to advance");
    }

    #[test]
    fn clearing_the_queue_releases_a_pending_auto_advance() {
        // Stopping mid-track leaves a waiter armed; without a release the next
        // track ever played could never auto-advance.
        let mut state = PlayerState::new();
        assert!(state.try_arm_auto_advance());
        state.clear();
        assert!(state.try_arm_auto_advance());
    }

    // -- advance_manual: an explicit skip is never swallowed by repeat-track --

    // -- advance: repeat_queue --

    // -- advance: shuffle --

    #[test]
    fn advance_with_shuffle_picks_an_upcoming_index() {
        // With current=0 and queue [0,1,2,3], shuffle picks among indices 1..=3.
        for _ in 0..20 {
            let mut s = PlayerState::new();
            fill(&mut s, 4);
            s.shuffle = true;
            let next = s.advance().unwrap().track.id().to_string();
            let n: usize = next.parse().unwrap();
            assert!((1..=3).contains(&n), "shuffle picked {n}, expected upcoming index");
        }
    }

    #[test]
    fn advance_with_shuffle_at_end_returns_none_without_repeat_queue() {
        let mut state = PlayerState::new();
        fill(&mut state, 2);
        state.shuffle = true;
        state.current_index = Some(1); // already at last
        assert!(state.advance().is_none());
        assert_eq!(state.current_index, None);
    }

    #[test]
    fn advance_repeat_track_wins_over_shuffle() {
        // repeat_track is checked before shuffle, so it should short-circuit.
        let mut state = PlayerState::new();
        fill(&mut state, 5);
        state.repeat = RepeatMode::Track;
        state.shuffle = true;
        let id_before = state.current().unwrap().track.id().to_string();
        for _ in 0..10 {
            assert_eq!(state.advance().unwrap().track.id(), id_before);
        }
    }

    // Repeat-track and repeat-queue can no longer both be set, so the old test
    // for which one wins has nothing left to assert: `RepeatMode` makes that
    // state unrepresentable. `repeat_mode_is_one_setting_not_two_flags` covers
    // the replacement guarantee.

    #[test]
    fn advance_with_shuffle_and_repeat_queue_picks_different_track_at_end() {
        for _ in 0..20 {
            let mut s = PlayerState::new();
            fill(&mut s, 3);
            s.shuffle = true;
            s.repeat = RepeatMode::Queue;
            s.current_index = Some(2); // at end
            let next = s.advance().unwrap().track.id().to_string();
            assert_ne!(next, "2", "shuffle+repeat_queue should not repeat current");
        }
    }

    // -- go_prev --

    #[test]
    fn go_prev_walks_backward() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.current_index = Some(2);
        assert_eq!(state.go_prev().unwrap().track.id(), "1");
        assert_eq!(state.go_prev().unwrap().track.id(), "0");
    }

    #[test]
    fn go_prev_at_zero_without_repeat_stays_at_zero() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        // current_index already 0 from enqueue
        assert_eq!(state.go_prev().unwrap().track.id(), "0");
        assert_eq!(state.current_index, Some(0));
    }

    #[test]
    fn go_prev_at_zero_with_repeat_queue_wraps_to_last() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.repeat = RepeatMode::Queue;
        assert_eq!(state.go_prev().unwrap().track.id(), "2");
        assert_eq!(state.current_index, Some(2));
    }

    #[test]
    fn go_prev_from_none_jumps_to_last() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.current_index = None;
        assert_eq!(state.go_prev().unwrap().track.id(), "2");
    }

    #[test]
    fn go_prev_on_empty_queue_returns_none() {
        let mut state = PlayerState::new();
        assert!(state.go_prev().is_none());
    }

    // -- remove --

    #[test]
    fn remove_before_current_decrements_current_index() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.current_index = Some(2);
        state.remove(0);
        assert_eq!(state.current_index, Some(1));
        assert_eq!(state.queue.len(), 2);
    }

    #[test]
    fn remove_after_current_does_not_change_current_index() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.current_index = Some(0);
        state.remove(2);
        assert_eq!(state.current_index, Some(0));
        assert_eq!(state.queue.len(), 2);
    }

    #[test]
    fn remove_current_when_more_remain_keeps_index() {
        // queue [0,1,2], current=1, remove(1) → queue [0,2], current still 1
        // (now points to former index 2, the new last item)
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.current_index = Some(1);
        state.remove(1);
        assert_eq!(state.current_index, Some(1));
        assert_eq!(state.current().unwrap().track.id(), "2");
    }

    #[test]
    fn remove_current_at_end_clamps_to_new_last() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.current_index = Some(2);
        state.remove(2);
        assert_eq!(state.current_index, Some(1));
        assert_eq!(state.queue.len(), 2);
    }

    #[test]
    fn remove_last_remaining_item_clears_current_index() {
        let mut state = PlayerState::new();
        state.enqueue(track("a"), "u".into(), true);
        state.remove(0);
        assert_eq!(state.current_index, None);
        assert!(state.queue.is_empty());
    }

    #[test]
    fn remove_out_of_bounds_returns_none() {
        let mut state = PlayerState::new();
        fill(&mut state, 2);
        assert!(state.remove(99).is_none());
        assert_eq!(state.queue.len(), 2);
    }

    // -- clear --

    #[test]
    fn clear_resets_queue_index_status_and_position() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.status = PlaybackStatus::Playing;
        state.position_ms = 12_345;
        state.clear();
        assert!(state.queue.is_empty());
        assert_eq!(state.current_index, None);
        assert_eq!(state.status, PlaybackStatus::Idle);
        assert_eq!(state.position_ms, 0);
    }

    // -- queue_display --

    #[test]
    fn queue_display_shows_the_current_track_then_upcoming_only() {
        // Like Spotify and YouTube Music: the queue is what is coming, not a
        // log of what already played.
        let mut state = PlayerState::new();
        fill(&mut state, 4);
        state.current_index = Some(1);
        let display = state.queue_display();
        assert!(display.contains("Track 1"), "current track should be shown: {display}");
        assert!(display.contains("Track 2") && display.contains("Track 3"));
        assert!(!display.contains("Track 0"), "already-played tracks should not be listed: {display}");
    }

    #[test]
    fn queue_display_with_nothing_playing_lists_the_whole_queue_as_upcoming() {
        // Reachable state: playback ran off the end of the queue, so entries
        // remain but none is current. Everything left is still to come.
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.current_index = None;
        let display = state.queue_display();
        assert!(!display.contains("Now playing"), "nothing is playing: {display}");
        assert!(display.contains("1.") && display.contains("Track 0"), "got: {display}");
        assert!(display.contains("3.") && display.contains("Track 2"), "got: {display}");
    }

    #[test]
    fn queue_display_numbers_upcoming_tracks_the_way_queue_rm_counts() {
        // `queue rm 1` removes the next upcoming track, so it must be listed
        // as 1 - numbering by absolute queue position made the numbers a user
        // reads differ from the ones they type.
        let mut state = PlayerState::new();
        fill(&mut state, 4);
        state.current_index = Some(1);
        let display = state.queue_display();
        let upcoming: Vec<&str> = display.lines().filter(|l| l.trim_start().starts_with('1')
            || l.trim_start().starts_with('2')).collect();
        assert!(upcoming.iter().any(|l| l.contains("1.") && l.contains("Track 2")),
            "next upcoming should be numbered 1: {display}");
        assert!(upcoming.iter().any(|l| l.contains("2.") && l.contains("Track 3")),
            "second upcoming should be numbered 2: {display}");
    }

    #[test]
    fn queue_display_includes_service_marker() {
        let mut state = PlayerState::new();
        fill(&mut state, 1);
        let display = state.queue_display();
        // Spotify-only queue should mark every entry [SP].
        assert!(display.contains("[SP]"), "expected [SP] marker, got: {display}");
        assert!(!display.contains("[YT]"));
    }

    // -- active_service --

    #[test]
    fn active_service_defaults_to_spotify() {
        let state = PlayerState::new();
        assert_eq!(state.active_service, Service::Spotify);
    }

    // -- mode_display --

    // -- current --

    #[test]
    fn current_returns_none_when_index_is_none() {
        let state = PlayerState::new();
        assert!(state.current().is_none());
    }

    #[test]
    fn current_returns_indexed_entry() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.current_index = Some(2);
        assert_eq!(state.current().unwrap().track.id(), "2");
    }
}
