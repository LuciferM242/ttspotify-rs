//! Caching the facts the tray menu shows about Spotify and YouTube.
//!
//! Both answers come from the filesystem: whether cached Spotify credentials
//! exist, and whether the YouTube tools are present. Asking on every right
//! click puts disk reads on the message loop, which is the thread that must
//! never wait for anything.
//!
//! They change rarely, and almost always because of something the tray itself
//! did, so an action that changes them marks the cache stale. The time limit is
//! only a backstop for changes made outside the app.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::gui_native::menu::MenuFacts;

/// How long a reading is trusted without being marked stale. Covers changes
/// made outside the app, such as installing yt-dlp by hand.
pub const MAX_AGE: Duration = Duration::from_secs(15);

/// Whether the facts must be read from disk again.
pub fn needs_refresh(
    last_read: Option<Instant>,
    now: Instant,
    max_age: Duration,
    marked_stale: bool,
) -> bool {
    if marked_stale {
        return true;
    }
    match last_read {
        None => true,
        // `checked_duration_since` rather than subtraction: a stamp marginally
        // ahead of `now` would panic on a plain subtraction.
        Some(t) => now
            .checked_duration_since(t)
            .is_none_or(|age| age >= max_age),
    }
}

/// The tray's cached view of those facts.
pub struct FactsCache {
    last: Option<(Instant, MenuFacts)>,
    stale: Arc<AtomicBool>,
}

impl Default for FactsCache {
    fn default() -> Self {
        Self::new()
    }
}

impl FactsCache {
    pub fn new() -> Self {
        Self {
            last: None,
            // Starts stale so the first menu reads the real state.
            stale: Arc::new(AtomicBool::new(true)),
        }
    }

    /// A handle that marks the cache stale. Cloneable and thread-safe, so a
    /// background install or sign-in can invalidate it when it finishes.
    pub fn staleness_flag(&self) -> Arc<AtomicBool> {
        self.stale.clone()
    }

    /// Mark the cached facts as needing a re-read.
    pub fn mark_stale(&self) {
        self.stale.store(true, Ordering::Relaxed);
    }

    /// The current facts, reading them through `fetch` only when needed.
    pub fn get(&mut self, fetch: impl FnOnce() -> MenuFacts) -> MenuFacts {
        let marked = self.stale.load(Ordering::Relaxed);
        let now = Instant::now();
        if needs_refresh(self.last.map(|(t, _)| t), now, MAX_AGE, marked) {
            let facts = fetch();
            self.last = Some((now, facts));
            self.stale.store(false, Ordering::Relaxed);
            facts
        } else {
            // Only reachable when `last` is set: needs_refresh returns true for
            // None.
            self.last.map(|(_, f)| f).unwrap_or_else(fetch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn facts(installed: bool) -> MenuFacts {
        MenuFacts {
            spotify_signed_in: false,
            youtube_installed: installed,
        }
    }

    #[test]
    fn the_first_read_always_hits_the_disk() {
        assert!(needs_refresh(None, Instant::now(), MAX_AGE, false));
    }

    #[test]
    fn a_fresh_reading_is_reused() {
        let now = Instant::now();
        let read_at = now - Duration::from_secs(1);
        assert!(!needs_refresh(Some(read_at), now, MAX_AGE, false));
    }

    #[test]
    fn an_old_reading_is_taken_again() {
        let now = Instant::now();
        let read_at = now - Duration::from_secs(60);
        assert!(needs_refresh(Some(read_at), now, MAX_AGE, false));
    }

    #[test]
    fn being_marked_stale_beats_freshness() {
        // An install that just finished must show immediately, not in 15s.
        let now = Instant::now();
        let read_at = now - Duration::from_millis(10);
        assert!(needs_refresh(Some(read_at), now, MAX_AGE, true));
    }

    #[test]
    fn a_stamp_from_the_future_is_re_read_rather_than_panicking() {
        let now = Instant::now();
        let ahead = now + Duration::from_secs(1);
        assert!(needs_refresh(Some(ahead), now, MAX_AGE, false));
    }

    #[test]
    fn repeated_reads_touch_the_disk_once() {
        // The point of the cache: opening the menu repeatedly must not repeat
        // the filesystem work.
        let calls = Cell::new(0);
        let mut cache = FactsCache::new();
        let fetch = || {
            calls.set(calls.get() + 1);
            facts(false)
        };
        for _ in 0..5 {
            cache.get(fetch);
        }
        assert_eq!(calls.get(), 1, "the facts were read {} times", calls.get());
    }

    #[test]
    fn marking_stale_forces_exactly_one_more_read() {
        let calls = Cell::new(0);
        let mut cache = FactsCache::new();
        let fetch = || {
            calls.set(calls.get() + 1);
            facts(false)
        };
        cache.get(fetch);
        cache.mark_stale();
        cache.get(fetch);
        cache.get(fetch);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn a_background_worker_can_mark_the_cache_stale() {
        // How an install running off the UI thread tells the menu to re-read.
        let mut cache = FactsCache::new();
        cache.get(|| facts(false));

        let flag = cache.staleness_flag();
        std::thread::spawn(move || flag.store(true, Ordering::Relaxed))
            .join()
            .unwrap();

        let calls = Cell::new(0);
        let refreshed = cache.get(|| {
            calls.set(calls.get() + 1);
            facts(true)
        });
        assert_eq!(calls.get(), 1, "the worker's change was not picked up");
        assert!(refreshed.youtube_installed);
    }

    #[test]
    fn the_cached_value_is_what_gets_returned() {
        let mut cache = FactsCache::new();
        let first = cache.get(|| facts(true));
        let second = cache.get(|| facts(false)); // would differ if re-read
        assert!(first.youtube_installed);
        assert!(
            second.youtube_installed,
            "the second call returned a fresh reading instead of the cache"
        );
    }
}
