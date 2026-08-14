use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;

use moka::sync::Cache;
use parking_lot::Mutex;

use crate::bot::queue::{PrevAction, Queue};
use crate::services::Service;
use crate::track::Track;

pub use crate::bot::queue::QueueEntry;

/// How long a user's search results stay pickable.
const SEARCH_RESULT_TTL: Duration = Duration::from_secs(600);

/// How many users can hold pickable results at once. Bounded so a busy channel
/// cannot grow this without limit.
const SEARCH_RESULT_CAPACITY: u64 = 256;

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
    pub queue: Queue,
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
            queue: Queue::new(),
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
        self.queue.current()
    }

    /// Tracks still to play, explicit picks first.
    pub fn upcoming(&self) -> impl Iterator<Item = &QueueEntry> {
        self.queue.upcoming()
    }

    pub fn upcoming_len(&self) -> usize {
        self.queue.upcoming_len()
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

    /// Queue a track the user asked for by name: it plays before whatever
    /// source is being worked through, the way "next in queue" sits ahead of
    /// "next from" in Spotify.
    pub fn enqueue_next(&mut self, track: Track, requester: String, allow_recommend: bool) {
        self.queue.push_next(QueueEntry { track, requester, allow_recommend });
    }

    /// Queue tracks from a source being played through: a playlist, an album,
    /// or radio.
    pub fn enqueue_source(&mut self, tracks: Vec<Track>, requester: String, allow_recommend: bool) {
        self.queue.push_source(tracks.into_iter().map(|track| QueueEntry {
            track,
            requester: requester.clone(),
            allow_recommend,
        }));
        // With shuffle on, a playlist added now must land shuffled too —
        // otherwise it plays in order and shuffle looks broken.
        if self.shuffle {
            self.queue.shuffle_source();
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

    /// Advance because the current track ended.
    pub fn advance(&mut self) -> Option<&QueueEntry> {
        let (repeat, shuffle) = (self.repeat, self.shuffle);
        self.queue.advance(repeat, shuffle)
    }

    /// Advance because the user asked to skip.
    pub fn advance_manual(&mut self) -> Option<&QueueEntry> {
        let (repeat, shuffle) = (self.repeat, self.shuffle);
        self.queue.advance_manual(repeat, shuffle)
    }

    /// Step back, given how far into the current track playback is.
    pub fn go_prev(&mut self, position_ms: u32) -> PrevAction {
        self.queue.go_prev(position_ms)
    }

    /// The track that would play next, for preloading.
    pub fn peek_next(&self) -> Option<&QueueEntry> {
        self.queue.peek_next(self.repeat)
    }

    /// Randomise the order of the source tier.
    pub fn shuffle_now(&mut self) {
        self.queue.shuffle_source();
    }

    pub fn clear(&mut self) {
        self.queue.clear();
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
        self.queue.clear_upcoming();
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
            self.queue.all_entries().map(|e| e.track.id()).collect();
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
            self.queue.all_entries().map(|e| e.track.id().to_string()).collect();
        let mut seen_songs: std::collections::HashSet<String> =
            self.queue.all_entries().map(|e| song_key(&e.track.display_name())).collect();
        tracks
            .into_iter()
            .filter(|t| {
                // Both inserts must run: `&&` short-circuiting skipped the
                // song-key record for id-duplicates, letting a same-song
                // different-id entry later in the batch slip through.
                let new_id = seen_ids.insert(t.id().to_string());
                let new_song = seen_songs.insert(song_key(&t.display_name()));
                new_id && new_song
            })
            .collect()
    }

    /// Remove the `n`-th upcoming track, counting from 1 the way `queue`
    /// displays them. The playing track is not removable.
    pub fn remove_upcoming(&mut self, n: usize) -> Option<QueueEntry> {
        self.queue.remove_upcoming(n)
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

        let mut any_upcoming = false;
        for (i, entry) in self.upcoming().enumerate() {
            any_upcoming = true;
            if !out.is_empty() {
                out.push('\n');
            }
            let _ = write!(out, "  {}. [{}]: {} [{}]",
                i + 1, entry.track.service().marker(),
                entry.track.display_name(), entry.track.duration_display());
        }
        if !any_upcoming {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("  Nothing upcoming");
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

    /// Queue `n` tracks from a source, the first of which starts playing.
    fn fill(state: &mut PlayerState, n: usize) {
        let tracks: Vec<Track> = (0..n).map(|i| track(&i.to_string())).collect();
        state.enqueue_source(tracks, "tester".to_string(), true);
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

    // -- bulk loading --

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
        fill(&mut state, 3);
        let g = state.begin_bulk_load();
        state.clear_upcoming();
        assert_eq!(state.current().unwrap().track.id(), "0", "current keeps playing");
        assert_eq!(state.upcoming_len(), 0);
        assert_ne!(state.bulk_load_generation, g);
    }

    #[test]
    fn clear_resets_the_queue_status_and_position() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.status = PlaybackStatus::Playing;
        state.position_ms = 42_000;
        state.clear();
        assert!(state.current().is_none());
        assert_eq!(state.upcoming_len(), 0);
        assert_eq!(state.status, PlaybackStatus::Idle);
        assert_eq!(state.position_ms, 0);
    }

    // -- duplicate filtering --

    #[test]
    fn filter_unqueued_drops_tracks_already_in_queue() {
        let mut state = PlayerState::new();
        state.enqueue_source(vec![track("a"), track("b")], "u".into(), true);
        let fresh = state.filter_unqueued(vec![track("b"), track("c")]);
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
    fn filter_unqueued_still_sees_tracks_that_already_played() {
        // Played tracks move to history; radio must not offer them straight
        // back as fresh recommendations.
        let mut state = PlayerState::new();
        fill(&mut state, 2);
        state.advance();
        let fresh = state.filter_unqueued(vec![track("0"), track("new")]);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].id(), "new");
    }

    #[test]
    fn filter_unqueued_similar_drops_a_rerelease_of_a_queued_song() {
        let mut state = PlayerState::new();
        state.enqueue_source(vec![named("a", "Artist - Song")], "u".into(), true);
        let fresh = state.filter_unqueued_similar(vec![
            named("b", "Artist - Song (Remastered 2011)"),
            named("c", "Artist - Another Song"),
        ]);
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

    // -- song_key --

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

    // -- rendered text --

    #[test]
    fn queue_display_shows_the_current_track_then_upcoming_only() {
        // Like Spotify: the queue is what is coming, not a log of what played.
        let mut state = PlayerState::new();
        fill(&mut state, 4);
        state.advance();
        let display = state.queue_display();
        assert!(display.contains("Track 1"), "current track should be shown: {display}");
        assert!(display.contains("Track 2") && display.contains("Track 3"));
        assert!(!display.contains("Track 0"), "played tracks are not listed: {display}");
    }

    #[test]
    fn queue_display_numbers_upcoming_tracks_the_way_queue_rm_counts() {
        // `queue rm 1` removes the next track up, so it must be listed as 1.
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        let display = state.queue_display();
        assert!(display.contains("1.") && display.contains("Track 1"), "got: {display}");
        assert!(display.contains("2.") && display.contains("Track 2"), "got: {display}");
    }

    #[test]
    fn queue_display_says_when_nothing_is_upcoming() {
        let mut state = PlayerState::new();
        fill(&mut state, 1);
        let display = state.queue_display();
        assert!(display.contains("Track 0"), "current track still shown: {display}");
        assert!(display.to_lowercase().contains("nothing upcoming"), "got: {display}");
    }

    #[test]
    fn queue_display_includes_service_marker() {
        let mut state = PlayerState::new();
        fill(&mut state, 1);
        let display = state.queue_display();
        assert!(display.contains("[SP]"), "expected [SP] marker, got: {display}");
        assert!(!display.contains("[YT]"));
    }

    #[test]
    fn snapshot_queue_with_played_current_and_upcoming() {
        let mut state = PlayerState::new();
        fill(&mut state, 5);
        state.advance();
        state.advance();
        insta::assert_snapshot!(state.queue_display());
    }

    #[test]
    fn snapshot_queue_with_nothing_upcoming() {
        let mut state = PlayerState::new();
        fill(&mut state, 2);
        state.advance();
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

    // -- shuffle --

    #[test]
    fn a_playlist_added_while_shuffle_is_on_lands_shuffled() {
        // Otherwise shuffle only affects what was queued before it was turned
        // on, and the next playlist plays straight through.
        let mut ordered = true;
        for _ in 0..20 {
            let mut state = PlayerState::new();
            state.shuffle = true;
            fill(&mut state, 8);
            let ids: Vec<String> = state.upcoming().map(|e| e.track.id().to_string()).collect();
            let in_order: Vec<String> = (1..8).map(|i| i.to_string()).collect();
            if ids != in_order {
                ordered = false;
                break;
            }
        }
        assert!(!ordered, "20 additions in a row all kept their original order");
    }

    #[test]
    fn shuffle_keeps_every_queued_track() {
        let mut state = PlayerState::new();
        state.shuffle = true;
        fill(&mut state, 8);
        let mut ids: Vec<String> = state.upcoming().map(|e| e.track.id().to_string()).collect();
        ids.push(state.current().unwrap().track.id().to_string());
        ids.sort();
        let expected: Vec<String> = (0..8).map(|i| i.to_string()).collect();
        assert_eq!(ids, expected, "shuffling must not lose or duplicate tracks");
    }

    // -- modes --

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
        assert_eq!(RepeatMode::from_flags(false, false), RepeatMode::Off);
        assert_eq!(RepeatMode::from_flags(true, false), RepeatMode::Track);
        assert_eq!(RepeatMode::from_flags(false, true), RepeatMode::Queue);
        assert_eq!(RepeatMode::from_flags(true, true), RepeatMode::Track);
        assert_eq!(RepeatMode::Queue.to_flags(), (false, true));
        assert_eq!(RepeatMode::Off.to_flags(), (false, false));
    }

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
        let mut state = PlayerState::new();
        assert!(state.try_arm_auto_advance());
        state.clear();
        assert!(state.try_arm_auto_advance());
    }

    // -- active service --

    #[test]
    fn active_service_defaults_to_spotify() {
        assert_eq!(PlayerState::new().active_service, Service::Spotify);
    }

    // -- the queue behaves under arbitrary command sequences --

    #[derive(Debug, Clone)]
    enum Op {
        PlayNext,
        QueueSource(u8),
        Advance,
        Skip,
        Prev(u32),
        Remove(u8),
        ClearUpcoming,
        Clear,
        SetRepeat(u8),
        SetShuffle(bool),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            Just(Op::PlayNext),
            (1u8..4).prop_map(Op::QueueSource),
            Just(Op::Advance),
            Just(Op::Skip),
            (0u32..5_000).prop_map(Op::Prev),
            (0u8..6).prop_map(Op::Remove),
            Just(Op::ClearUpcoming),
            Just(Op::Clear),
            (0u8..3).prop_map(Op::SetRepeat),
            any::<bool>().prop_map(Op::SetShuffle),
        ]
    }

    fn apply(state: &mut PlayerState, op: &Op, next_id: &mut u32) {
        match op {
            Op::PlayNext => {
                state.enqueue_next(track(&next_id.to_string()), "u".into(), true);
                *next_id += 1;
            }
            Op::QueueSource(n) => {
                let batch: Vec<Track> = (0..*n)
                    .map(|_| {
                        let t = track(&next_id.to_string());
                        *next_id += 1;
                        t
                    })
                    .collect();
                state.enqueue_source(batch, "u".into(), false);
            }
            Op::Advance => {
                state.advance();
            }
            Op::Skip => {
                state.advance_manual();
            }
            Op::Prev(position_ms) => {
                state.go_prev(*position_ms);
            }
            Op::Remove(n) => {
                state.remove_upcoming(*n as usize);
            }
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
        /// The bug this model was built to kill: the queue holding tracks while
        /// a skip reports there is nothing to skip to.
        #[test]
        fn a_skip_always_moves_while_tracks_are_queued(
            ops in prop::collection::vec(op_strategy(), 1..40)
        ) {
            let mut state = PlayerState::new();
            let mut next_id = 0u32;
            for op in &ops {
                apply(&mut state, op, &mut next_id);
                if state.upcoming_len() > 0 {
                    let before = state.upcoming_len();
                    prop_assert!(
                        state.advance_manual().is_some(),
                        "{} tracks queued but the skip found nothing (after {op:?})",
                        before
                    );
                }
            }
        }

        /// Something is always playing while the queue holds anything at all.
        #[test]
        fn nothing_is_queued_without_something_playing(
            ops in prop::collection::vec(op_strategy(), 1..40)
        ) {
            let mut state = PlayerState::new();
            let mut next_id = 0u32;
            for op in &ops {
                apply(&mut state, op, &mut next_id);
                prop_assert!(
                    !(state.current().is_none() && state.upcoming_len() > 0),
                    "tracks are queued with nothing playing (after {op:?})"
                );
            }
        }
    }
}
