//! Short-lived cache for search responses.
//!
//! Searching the same thing twice is common — a user mistypes, retries, or two
//! people in the channel look for the same song — and every one of those was a
//! fresh network round trip. Answers are cached briefly so a repeat search is
//! instant, while staying short enough that new uploads still show up.

use std::time::Duration;

use moka::sync::Cache;

use crate::services::Service;
use crate::track::Track;

/// How long a search answer stays usable. Long enough to cover a retry or a
/// second person asking, short enough that the catalogue is never stale in a
/// way anyone notices.
const SEARCH_CACHE_TTL: Duration = Duration::from_secs(300);

/// Maximum distinct queries held. Bounded so a busy channel cannot grow it.
const SEARCH_CACHE_CAPACITY: u64 = 256;

/// Cached search answers, keyed by service, query and result limit.
#[derive(Clone)]
pub struct SearchCache {
    inner: Cache<(Service, String, u8), Vec<Track>>,
}

impl SearchCache {
    pub fn new() -> Self {
        Self::with_ttl(SEARCH_CACHE_TTL)
    }

    /// Injectable TTL, so expiry can be tested without a five-minute wait.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(SEARCH_CACHE_CAPACITY)
                .time_to_live(ttl)
                .build(),
        }
    }

    /// Queries differing only in case or padding are the same search.
    fn key(service: Service, query: &str, limit: u8) -> (Service, String, u8) {
        (service, query.trim().to_lowercase(), limit)
    }

    pub fn get(&self, service: Service, query: &str, limit: u8) -> Option<Vec<Track>> {
        self.inner.get(&Self::key(service, query, limit))
    }

    pub fn insert(&self, service: Service, query: &str, limit: u8, tracks: Vec<Track>) {
        // An empty result is not worth holding: the next search should ask
        // again rather than repeat "no results" for five minutes.
        if tracks.is_empty() {
            return;
        }
        self.inner.insert(Self::key(service, query, limit), tracks);
    }
}

impl Default for SearchCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotify::types::SpotifyTrack;

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

    #[test]
    fn a_repeated_search_is_answered_from_the_cache() {
        let cache = SearchCache::new();
        assert!(cache.get(Service::Spotify, "hello", 5).is_none());
        cache.insert(Service::Spotify, "hello", 5, vec![track("a")]);
        let hit = cache.get(Service::Spotify, "hello", 5).expect("should be cached");
        assert_eq!(hit[0].id(), "a");
    }

    #[test]
    fn case_and_padding_do_not_split_the_cache_entry() {
        let cache = SearchCache::new();
        cache.insert(Service::Spotify, "Hello World", 5, vec![track("a")]);
        assert!(cache.get(Service::Spotify, "  hello world  ", 5).is_some());
    }

    #[test]
    fn the_two_services_do_not_share_answers() {
        let cache = SearchCache::new();
        cache.insert(Service::Spotify, "hello", 5, vec![track("a")]);
        assert!(cache.get(Service::YouTube, "hello", 5).is_none());
    }

    #[test]
    fn a_different_result_limit_is_a_different_search() {
        let cache = SearchCache::new();
        cache.insert(Service::Spotify, "hello", 5, vec![track("a")]);
        assert!(cache.get(Service::Spotify, "hello", 10).is_none());
    }

    #[test]
    fn empty_results_are_not_cached() {
        let cache = SearchCache::new();
        cache.insert(Service::Spotify, "nothing", 5, vec![]);
        assert!(cache.get(Service::Spotify, "nothing", 5).is_none());
    }

    #[test]
    fn entries_expire() {
        let cache = SearchCache::with_ttl(Duration::from_millis(50));
        cache.insert(Service::Spotify, "hello", 5, vec![track("a")]);
        assert!(cache.get(Service::Spotify, "hello", 5).is_some());
        std::thread::sleep(Duration::from_millis(120));
        cache.inner.run_pending_tasks();
        assert!(cache.get(Service::Spotify, "hello", 5).is_none(), "entry should have expired");
    }
}
