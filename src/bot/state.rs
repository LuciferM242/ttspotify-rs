use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::services::Service;
use crate::track::Track;

/// How long a user's search results stay pickable before being swept.
/// Prevents `search_results` growing unbounded when users search and walk away.
const SEARCH_RESULT_TTL: Duration = Duration::from_secs(600);

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

#[derive(Debug)]
pub struct PlayerState {
    pub queue: Vec<QueueEntry>,
    pub current_index: Option<usize>,
    pub status: PlaybackStatus,

    // Modes
    pub repeat_track: bool,
    pub repeat_queue: bool,
    pub shuffle: bool,

    // Radio
    pub radio_enabled: bool,

    // Search session (user_id → (inserted_at, results)). Access via the
    // search-result helper methods so stale entries get swept.
    pub search_results: HashMap<i32, (Instant, Vec<Track>)>,

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
}

pub type SharedState = Arc<Mutex<PlayerState>>;

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
            repeat_track: false,
            repeat_queue: false,
            shuffle: false,
            radio_enabled: false,
            search_results: HashMap::new(),
            position_ms: 0,
            tracks_played: 0,
            active_service: Service::default(),
            bulk_load_generation: 0,
        }
    }

    pub fn current(&self) -> Option<&QueueEntry> {
        self.current_index.and_then(|i| self.queue.get(i))
    }

    /// Store a user's search results, timestamped, sweeping any entries older
    /// than `SEARCH_RESULT_TTL` first.
    pub fn insert_search_results(&mut self, user_id: i32, tracks: Vec<Track>) {
        self.insert_search_results_at(user_id, tracks, Instant::now());
    }

    /// Timestamp-injectable variant for tests.
    pub fn insert_search_results_at(&mut self, user_id: i32, tracks: Vec<Track>, now: Instant) {
        self.search_results
            .retain(|_, (t, _)| now.duration_since(*t) < SEARCH_RESULT_TTL);
        self.search_results.insert(user_id, (now, tracks));
    }

    /// Borrow a user's current search results, if any.
    pub fn get_search_results(&self, user_id: i32) -> Option<&Vec<Track>> {
        self.search_results.get(&user_id).map(|(_, v)| v)
    }

    /// Remove a user's search results; returns whether an entry existed.
    pub fn remove_search_results(&mut self, user_id: i32) -> bool {
        self.search_results.remove(&user_id).is_some()
    }

    /// Clone the `pick`-th result of a user's search, if present.
    pub fn pick_search_result(&self, user_id: i32, pick: usize) -> Option<Track> {
        self.search_results
            .get(&user_id)
            .and_then(|(_, v)| v.get(pick).cloned())
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

    /// Advance to the next track. Returns the next entry if available.
    pub fn advance(&mut self) -> Option<&QueueEntry> {
        if self.queue.is_empty() {
            self.current_index = None;
            return None;
        }

        if self.repeat_track {
            return self.current();
        }

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
            } else if self.repeat_queue && self.queue.len() > 1 {
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
            } else if self.repeat_queue {
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
            } else if self.repeat_queue {
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

    pub fn queue_display(&self) -> String {
        if self.queue.is_empty() {
            return "Queue is empty".to_string();
        }

        let mut out = String::new();
        for (i, entry) in self.queue.iter().enumerate() {
            let marker = if self.current_index == Some(i) { "> " } else { "  " };
            if i > 0 { out.push('\n'); }
            let _ = write!(out, "{}{} [{}]: {} [{}]",
                marker, i + 1, entry.track.service().marker(),
                entry.track.display_name(), entry.track.duration_display());
        }
        out
    }

    pub fn mode_display(&self) -> String {
        let mut modes = Vec::new();
        if self.repeat_track {
            modes.push("Repeat Track");
        }
        if self.repeat_queue {
            modes.push("Repeat Queue");
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
    fn stale_search_results_are_swept_on_insert() {
        let mut state = PlayerState::new();
        let t0 = Instant::now();
        // Old entry for user 1.
        state.insert_search_results_at(1, vec![track("a")], t0);
        // Fresh insert for user 2 well past the TTL sweeps user 1.
        let later = t0 + SEARCH_RESULT_TTL + Duration::from_secs(1);
        state.insert_search_results_at(2, vec![track("b")], later);
        assert!(state.get_search_results(1).is_none(), "stale entry should be evicted");
        assert!(state.get_search_results(2).is_some());
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

    #[test]
    fn advance_with_repeat_track_returns_same_track() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.repeat_track = true;
        let id_before = state.current().unwrap().track.id().to_string();
        for _ in 0..5 {
            assert_eq!(state.advance().unwrap().track.id(), id_before);
        }
    }

    // -- advance: repeat_queue --

    #[test]
    fn advance_with_repeat_queue_wraps_to_first() {
        let mut state = PlayerState::new();
        fill(&mut state, 2);
        state.repeat_queue = true;
        assert_eq!(state.advance().unwrap().track.id(), "1");
        assert_eq!(state.advance().unwrap().track.id(), "0"); // wrap
        assert_eq!(state.advance().unwrap().track.id(), "1");
    }

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
        state.repeat_track = true;
        state.shuffle = true;
        let id_before = state.current().unwrap().track.id().to_string();
        for _ in 0..10 {
            assert_eq!(state.advance().unwrap().track.id(), id_before);
        }
    }

    #[test]
    fn advance_repeat_track_wins_over_repeat_queue() {
        // repeat_track is checked before the linear/repeat_queue branch.
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.repeat_track = true;
        state.repeat_queue = true;
        state.current_index = Some(2); // at end
        // Without repeat_track, repeat_queue would wrap to 0. With repeat_track,
        // we stay on index 2.
        assert_eq!(state.advance().unwrap().track.id(), "2");
        assert_eq!(state.current_index, Some(2));
    }

    #[test]
    fn advance_with_shuffle_and_repeat_queue_picks_different_track_at_end() {
        for _ in 0..20 {
            let mut s = PlayerState::new();
            fill(&mut s, 3);
            s.shuffle = true;
            s.repeat_queue = true;
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
        state.repeat_queue = true;
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
    fn queue_display_empty() {
        let state = PlayerState::new();
        assert_eq!(state.queue_display(), "Queue is empty");
    }

    #[test]
    fn queue_display_marks_current_with_arrow() {
        let mut state = PlayerState::new();
        fill(&mut state, 3);
        state.current_index = Some(1);
        let display = state.queue_display();
        let lines: Vec<&str> = display.lines().collect();
        assert!(lines[0].starts_with("  "));
        assert!(lines[1].starts_with("> "));
        assert!(lines[2].starts_with("  "));
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

    #[test]
    fn mode_display_no_modes() {
        let state = PlayerState::new();
        assert_eq!(state.mode_display(), "No modes active");
    }

    #[test]
    fn mode_display_single_mode() {
        let mut state = PlayerState::new();
        state.shuffle = true;
        assert_eq!(state.mode_display(), "Shuffle");
    }

    #[test]
    fn mode_display_multiple_modes_joined_with_comma() {
        let mut state = PlayerState::new();
        state.repeat_track = true;
        state.shuffle = true;
        assert_eq!(state.mode_display(), "Repeat Track, Shuffle");
    }

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
