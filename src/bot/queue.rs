//! The play queue.
//!
//! Modelled the way a listener thinks about it, and the way Spotify presents
//! it: what has played, what is playing, and what is coming. What is coming has
//! two tiers — tracks a user explicitly asked for ("next in queue") play before
//! the source being worked through, be that a playlist or radio ("next from").
//!
//! The previous model was a `Vec` plus a `current_index`, where the index
//! carried three meanings at once (nothing played yet / playing entry i / ran
//! off the end) and every operation was index arithmetic that had to be right
//! in a dozen places. Three separate playback bugs came out of that. Here there
//! is no index: advancing moves entries between collections, so "the queue has
//! tracks in it but nothing can be advanced to" cannot be expressed.

use std::collections::VecDeque;

use rand::seq::SliceRandom;

use crate::bot::state::RepeatMode;
use crate::track::Track;

/// Default number of played tracks kept behind the current one. Enough to step
/// back over an evening; bounded so an endless source (radio) cannot grow the
/// queue for as long as the bot runs.
pub const DEFAULT_HISTORY_CAP: usize = 20;

/// How far into a track `o` restarts it instead of stepping back, matching the
/// Previous button in Spotify.
pub const PREV_RESTART_AFTER_MS: u32 = 3_000;

#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub track: Track,
    #[allow(dead_code)] // stored for future "who queued this" display
    pub requester: String,
    /// Only allow radio recommendations for single-track plays (not
    /// playlists/albums).
    pub allow_recommend: bool,
}

/// What `go_prev` did, so the caller knows whether to restart or load a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrevAction {
    /// Far enough into the track that "previous" means "start this one again".
    RestartCurrent,
    /// Moved back; `current()` is the track to load.
    MovedBack,
    /// Nothing has played yet.
    NothingBehind,
}

#[derive(Debug)]
pub struct Queue {
    history: VecDeque<QueueEntry>,
    current: Option<QueueEntry>,
    /// Explicit picks. Always played before `upcoming`, always in the order
    /// they were asked for — shuffle does not touch these.
    next_up: VecDeque<QueueEntry>,
    /// The source being played through: a playlist, an album, radio.
    upcoming: VecDeque<QueueEntry>,
    history_cap: usize,
    /// Tracks retired since the queue was last cleared or looped. Kept
    /// separately from `history`, which is capped and so cannot answer "which
    /// number is this track" once a long playlist passes the cap.
    played_total: usize,
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

impl Queue {
    pub fn new() -> Self {
        Self::with_history_cap(DEFAULT_HISTORY_CAP)
    }

    pub fn with_history_cap(history_cap: usize) -> Self {
        Self {
            history: VecDeque::new(),
            current: None,
            next_up: VecDeque::new(),
            upcoming: VecDeque::new(),
            history_cap,
            played_total: 0,
        }
    }

    pub fn current(&self) -> Option<&QueueEntry> {
        self.current.as_ref()
    }

    /// Nothing playing and nothing to play.
    pub fn is_empty(&self) -> bool {
        self.current.is_none() && self.next_up.is_empty() && self.upcoming.is_empty()
    }

    /// Tracks still to play, explicit picks first.
    pub fn upcoming(&self) -> impl Iterator<Item = &QueueEntry> {
        self.next_up.iter().chain(self.upcoming.iter())
    }

    pub fn upcoming_len(&self) -> usize {
        self.next_up.len() + self.upcoming.len()
    }

    /// Everything the queue knows about, for duplicate checks.
    pub fn all_entries(&self) -> impl Iterator<Item = &QueueEntry> {
        self.history
            .iter()
            .chain(self.current.iter())
            .chain(self.next_up.iter())
            .chain(self.upcoming.iter())
    }

    /// Add a track the user explicitly asked for: it plays before whatever
    /// source is being worked through. Starts playing if nothing is.
    pub fn push_next(&mut self, entry: QueueEntry) {
        self.next_up.push_back(entry);
        self.start_if_idle();
    }

    /// Add tracks from a source being played through (playlist, album, radio).
    /// Starts playing if nothing is.
    pub fn push_source<I: IntoIterator<Item = QueueEntry>>(&mut self, entries: I) {
        self.upcoming.extend(entries);
        self.start_if_idle();
    }

    /// Take the next track when nothing is playing, so a refill of an
    /// exhausted queue always becomes the current track.
    fn start_if_idle(&mut self) {
        if self.current.is_none() {
            self.current = self.take_next();
        }
    }

    fn take_next(&mut self) -> Option<QueueEntry> {
        self.next_up.pop_front().or_else(|| self.upcoming.pop_front())
    }

    /// Move the current track into history.
    ///
    /// `trim` is false under repeat-queue, where history is not just a record
    /// of the past but the material the next lap is rebuilt from — dropping the
    /// front of it there would quietly shorten a playlist every time round.
    /// Nothing is appended during a lap, so history stays bounded by the size
    /// of the queue itself.
    fn retire_current(&mut self, trim: bool) {
        if let Some(entry) = self.current.take() {
            self.history.push_back(entry);
            self.played_total += 1;
        }
        if trim {
            while self.history.len() > self.history_cap {
                self.history.pop_front();
            }
        }
    }

    /// Advance because the current track ended.
    pub fn advance(&mut self, repeat: RepeatMode, shuffle: bool) -> Option<&QueueEntry> {
        self.advance_inner(repeat, shuffle, true)
    }

    /// Advance because the user asked to skip. Repeat-track describes what
    /// happens when a track *ends*, so an explicit skip always moves.
    pub fn advance_manual(&mut self, repeat: RepeatMode, shuffle: bool) -> Option<&QueueEntry> {
        self.advance_inner(repeat, shuffle, false)
    }

    fn advance_inner(
        &mut self,
        repeat: RepeatMode,
        shuffle: bool,
        honor_repeat_track: bool,
    ) -> Option<&QueueEntry> {
        if repeat == RepeatMode::Track && honor_repeat_track && self.current.is_some() {
            return self.current.as_ref();
        }

        let trim_history = repeat != RepeatMode::Queue;
        if let Some(next) = self.take_next() {
            self.retire_current(trim_history);
            self.current = Some(next);
            return self.current.as_ref();
        }

        // Nothing left. Repeat-queue starts the whole thing again; a manual
        // skip under repeat-track loops rather than stopping the music.
        let loops = repeat == RepeatMode::Queue
            || (repeat == RepeatMode::Track && !honor_repeat_track);
        if loops {
            // Everything is about to be drained back into the queue, so the
            // cap must not eat the front of it first.
            self.retire_current(false);
            self.upcoming.extend(self.history.drain(..));
            // A new lap: numbering starts from the top again.
            self.played_total = 0;
            if shuffle {
                self.shuffle_source();
            }
            self.current = self.take_next();
            return self.current.as_ref();
        }

        self.retire_current(trim_history);
        None
    }

    /// The track that would play next, without moving anything — for
    /// preloading. Mirrors what `advance` would pick, repeat mode included.
    pub fn peek_next(&self, repeat: RepeatMode) -> Option<&QueueEntry> {
        if repeat == RepeatMode::Track {
            return self.current.as_ref();
        }
        self.next_up
            .front()
            .or_else(|| self.upcoming.front())
            .or_else(|| {
                // About to lap: the next track is the front of what will be
                // drained back in. Shuffle makes this a guess, so don't guess.
                (repeat == RepeatMode::Queue).then(|| self.history.front()).flatten()
            })
    }

    /// Step back. Past `PREV_RESTART_AFTER_MS` into the track this means
    /// "play this one again", matching Spotify's Previous button.
    pub fn go_prev(&mut self, position_ms: u32) -> PrevAction {
        if self.current.is_some() && position_ms > PREV_RESTART_AFTER_MS {
            return PrevAction::RestartCurrent;
        }
        let Some(previous) = self.history.pop_back() else {
            return PrevAction::NothingBehind;
        };
        // The track being left plays next, so stepping back and forward again
        // returns where you were.
        if let Some(entry) = self.current.take() {
            self.next_up.push_front(entry);
        }
        self.current = Some(previous);
        self.played_total = self.played_total.saturating_sub(1);
        PrevAction::MovedBack
    }

    /// Randomise the order of the source tier. Explicit picks keep their order.
    pub fn shuffle_source(&mut self) {
        let mut items: Vec<QueueEntry> = self.upcoming.drain(..).collect();
        items.shuffle(&mut rand::thread_rng());
        self.upcoming = items.into();
    }

    /// Remove the `n`-th upcoming track, counting explicit picks first and
    /// starting at 1 — the numbering `queue` displays.
    pub fn remove_upcoming(&mut self, n: usize) -> Option<QueueEntry> {
        let index = n.checked_sub(1)?;
        if index < self.next_up.len() {
            return self.next_up.remove(index);
        }
        self.upcoming.remove(index - self.next_up.len())
    }

    /// Drop everything still to play, keeping the current track and history.
    pub fn clear_upcoming(&mut self) {
        self.next_up.clear();
        self.upcoming.clear();
    }

    /// Which track this is and how many are known about, as
    /// `(position, total)` — the numbers shown beside the current track. Both
    /// are 0 when nothing is playing.
    pub fn position(&self) -> (usize, usize) {
        if self.current.is_none() {
            return (0, 0);
        }
        let position = self.played_total + 1;
        (position, position + self.upcoming_len())
    }

    /// Forget everything, including what is playing.
    pub fn clear(&mut self) {
        self.played_total = 0;
        self.history.clear();
        self.current = None;
        self.next_up.clear();
        self.upcoming.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotify::types::SpotifyTrack;
    use rstest::rstest;

    fn entry(id: &str) -> QueueEntry {
        QueueEntry {
            track: Track::Spotify(SpotifyTrack {
                id: id.to_string(),
                name: format!("Track {id}"),
                artists: vec!["Artist".to_string()],
                album: "Album".to_string(),
                duration_ms: 180_000,
                uri: format!("spotify:track:{id}"),
            }),
            requester: "tester".to_string(),
            allow_recommend: true,
        }
    }

    /// A queue playing `a` with `b`, `c` queued from a source.
    fn playing_abc() -> Queue {
        let mut q = Queue::new();
        q.push_source([entry("a"), entry("b"), entry("c")]);
        q
    }

    fn current_id(q: &Queue) -> Option<String> {
        q.current().map(|e| e.track.id().to_string())
    }

    fn upcoming_ids(q: &Queue) -> Vec<String> {
        q.upcoming().map(|e| e.track.id().to_string()).collect()
    }

    // -- filling and starting --

    #[test]
    fn the_first_track_added_starts_playing() {
        let q = playing_abc();
        assert_eq!(current_id(&q).as_deref(), Some("a"));
        assert_eq!(upcoming_ids(&q), ["b", "c"]);
    }

    #[test]
    fn a_refill_after_the_queue_ran_out_becomes_the_current_track() {
        // The bug that started this: radio refilled an exhausted queue, and the
        // tracks sat there unplayable because nothing was current.
        let mut q = Queue::new();
        q.push_source([entry("a")]);
        assert!(q.advance(RepeatMode::Off, false).is_none(), "queue runs out");
        assert!(q.current().is_none());

        q.push_source([entry("r1"), entry("r2")]);

        assert_eq!(current_id(&q).as_deref(), Some("r1"), "the refill starts playing");
        assert_eq!(
            q.advance_manual(RepeatMode::Off, false).map(|e| e.track.id().to_string()),
            Some("r2".to_string()),
            "and a skip can move through it"
        );
    }

    // -- explicit picks jump ahead of the source --

    #[test]
    fn an_explicit_pick_plays_before_the_rest_of_the_source() {
        let mut q = playing_abc();
        q.push_next(entry("x"));
        assert_eq!(upcoming_ids(&q), ["x", "b", "c"]);
        assert_eq!(
            q.advance(RepeatMode::Off, false).map(|e| e.track.id().to_string()),
            Some("x".to_string())
        );
    }

    #[test]
    fn explicit_picks_keep_the_order_they_were_asked_for() {
        let mut q = playing_abc();
        q.push_next(entry("x"));
        q.push_next(entry("y"));
        assert_eq!(upcoming_ids(&q), ["x", "y", "b", "c"]);
    }

    // -- advancing --

    #[rstest]
    // Off: walks forward, then stops.
    #[case(RepeatMode::Off, Some("b"), Some("c"), None)]
    // Track: stays put on every end-of-track.
    #[case(RepeatMode::Track, Some("a"), Some("a"), Some("a"))]
    // Queue: walks forward, then starts over.
    #[case(RepeatMode::Queue, Some("b"), Some("c"), Some("a"))]
    fn auto_advance_per_repeat_mode(
        #[case] repeat: RepeatMode,
        #[case] first: Option<&str>,
        #[case] second: Option<&str>,
        #[case] third: Option<&str>,
    ) {
        let mut q = playing_abc();
        for expected in [first, second, third] {
            let got = q.advance(repeat, false).map(|e| e.track.id().to_string());
            assert_eq!(got.as_deref(), expected);
        }
    }

    #[test]
    fn a_manual_skip_moves_even_under_repeat_track() {
        let mut q = playing_abc();
        assert_eq!(
            q.advance_manual(RepeatMode::Track, false).map(|e| e.track.id().to_string()),
            Some("b".to_string())
        );
    }

    #[test]
    fn a_manual_skip_past_the_last_track_loops_under_repeat_track() {
        let mut q = Queue::new();
        q.push_source([entry("a")]);
        assert_eq!(
            q.advance_manual(RepeatMode::Track, false).map(|e| e.track.id().to_string()),
            Some("a".to_string()),
            "one-track queue loops back to itself"
        );
    }

    #[test]
    fn a_manual_skip_at_the_end_without_repeat_stops() {
        let mut q = playing_abc();
        q.advance_manual(RepeatMode::Off, false);
        q.advance_manual(RepeatMode::Off, false);
        assert!(q.advance_manual(RepeatMode::Off, false).is_none());
        assert!(q.current().is_none());
    }

    #[test]
    fn repeat_queue_replays_everything_that_was_played() {
        let mut q = playing_abc();
        q.advance(RepeatMode::Queue, false); // b
        q.advance(RepeatMode::Queue, false); // c
        q.advance(RepeatMode::Queue, false); // wraps to a
        assert_eq!(current_id(&q).as_deref(), Some("a"));
        assert_eq!(upcoming_ids(&q), ["b", "c"], "the whole queue is back");
    }

    #[test]
    fn repeat_queue_replays_a_queue_longer_than_the_history_cap() {
        // History is bounded so an endless source cannot grow the queue, but
        // repeat-queue rebuilds the queue *from* history: capping it there
        // would silently drop the front of a long playlist on every lap.
        let mut q = Queue::with_history_cap(3);
        q.push_source((0..10).map(|i| entry(&i.to_string())));
        for _ in 0..10 {
            q.advance(RepeatMode::Queue, false);
        }
        assert_eq!(current_id(&q).as_deref(), Some("0"), "back to the first track");
        assert_eq!(q.upcoming_len(), 9, "the whole playlist came back");
    }

    // -- history --

    #[test]
    fn history_is_bounded_so_an_endless_source_cannot_grow_the_queue() {
        let mut q = Queue::with_history_cap(2);
        q.push_source((0..6).map(|i| entry(&i.to_string())));
        for _ in 0..5 {
            q.advance(RepeatMode::Off, false);
        }
        assert_eq!(q.history.len(), 2, "only the recent past is kept");
        assert_eq!(current_id(&q).as_deref(), Some("5"));
    }

    // -- previous --

    #[test]
    fn previous_early_in_a_track_steps_back() {
        let mut q = playing_abc();
        q.advance(RepeatMode::Off, false); // now on b
        assert_eq!(q.go_prev(1_000), PrevAction::MovedBack);
        assert_eq!(current_id(&q).as_deref(), Some("a"));
    }

    #[test]
    fn previous_late_in_a_track_restarts_it() {
        let mut q = playing_abc();
        q.advance(RepeatMode::Off, false); // now on b
        assert_eq!(q.go_prev(30_000), PrevAction::RestartCurrent);
        assert_eq!(current_id(&q).as_deref(), Some("b"), "still on the same track");
    }

    #[test]
    fn previous_with_nothing_played_yet_does_nothing() {
        let mut q = playing_abc();
        assert_eq!(q.go_prev(0), PrevAction::NothingBehind);
        assert_eq!(current_id(&q).as_deref(), Some("a"));
    }

    #[test]
    fn stepping_back_then_forward_returns_to_where_you_were() {
        let mut q = playing_abc();
        q.advance(RepeatMode::Off, false); // b
        q.go_prev(0); // back to a
        assert_eq!(
            q.advance_manual(RepeatMode::Off, false).map(|e| e.track.id().to_string()),
            Some("b".to_string())
        );
    }

    // -- shuffle --

    #[test]
    fn shuffle_keeps_every_track_and_plays_each_once() {
        let mut q = Queue::new();
        q.push_source((0..8).map(|i| entry(&i.to_string())));
        q.shuffle_source();

        let mut played = vec![current_id(&q).unwrap()];
        while let Some(e) = q.advance(RepeatMode::Off, true) {
            played.push(e.track.id().to_string());
        }
        played.sort();
        let expected: Vec<String> = (0..8).map(|i| i.to_string()).collect();
        assert_eq!(played, expected, "every track plays exactly once");
    }

    #[test]
    fn shuffle_leaves_explicit_picks_in_order() {
        let mut q = playing_abc();
        q.push_next(entry("x"));
        q.push_next(entry("y"));
        q.shuffle_source();
        let ids = upcoming_ids(&q);
        assert_eq!(&ids[..2], ["x", "y"], "explicit picks stay put and stay first");
    }

    // -- removing --

    #[rstest]
    #[case(1, "x")]
    #[case(2, "b")]
    #[case(3, "c")]
    fn remove_upcoming_counts_the_way_the_queue_is_displayed(
        #[case] n: usize,
        #[case] removed: &str,
    ) {
        let mut q = playing_abc();
        q.push_next(entry("x")); // upcoming is now x, b, c
        let got = q.remove_upcoming(n).map(|e| e.track.id().to_string());
        assert_eq!(got.as_deref(), Some(removed));
    }

    #[test]
    fn removing_past_the_end_or_at_zero_returns_nothing() {
        let mut q = playing_abc();
        assert!(q.remove_upcoming(0).is_none());
        assert!(q.remove_upcoming(99).is_none());
        assert_eq!(q.upcoming_len(), 2, "queue untouched");
    }

    #[test]
    fn remove_cannot_touch_the_playing_track() {
        let mut q = playing_abc();
        q.remove_upcoming(1);
        q.remove_upcoming(1);
        assert_eq!(current_id(&q).as_deref(), Some("a"));
        assert_eq!(q.upcoming_len(), 0);
    }

    // -- position --

    #[test]
    fn position_counts_through_the_queue() {
        let mut q = playing_abc();
        assert_eq!(q.position(), (1, 3));
        q.advance(RepeatMode::Off, false);
        assert_eq!(q.position(), (2, 3));
        q.advance(RepeatMode::Off, false);
        assert_eq!(q.position(), (3, 3));
    }

    #[test]
    fn position_stays_right_past_the_history_cap() {
        // History is capped, so it cannot be what answers "which track is
        // this" on a playlist longer than the cap.
        let mut q = Queue::with_history_cap(2);
        q.push_source((0..10).map(|i| entry(&i.to_string())));
        for _ in 0..6 {
            q.advance(RepeatMode::Off, false);
        }
        assert_eq!(q.position(), (7, 10));
    }

    #[test]
    fn position_restarts_on_a_new_lap() {
        let mut q = playing_abc();
        for _ in 0..3 {
            q.advance(RepeatMode::Queue, false);
        }
        assert_eq!(q.position(), (1, 3), "back to the top of the queue");
    }

    #[test]
    fn position_steps_back_with_previous() {
        let mut q = playing_abc();
        q.advance(RepeatMode::Off, false);
        assert_eq!(q.position(), (2, 3));
        q.go_prev(0);
        assert_eq!(q.position(), (1, 3));
    }

    #[test]
    fn position_is_zero_when_nothing_plays() {
        let q = Queue::new();
        assert_eq!(q.position(), (0, 0));
    }

    // -- clearing --

    #[test]
    fn clear_upcoming_keeps_the_track_that_is_playing() {
        let mut q = playing_abc();
        q.push_next(entry("x"));
        q.clear_upcoming();
        assert_eq!(current_id(&q).as_deref(), Some("a"));
        assert_eq!(q.upcoming_len(), 0);
    }

    #[test]
    fn clear_forgets_everything() {
        let mut q = playing_abc();
        q.advance(RepeatMode::Off, false);
        q.clear();
        assert!(q.is_empty());
        assert!(q.current().is_none());
        assert_eq!(q.go_prev(0), PrevAction::NothingBehind, "history is gone too");
    }
}
