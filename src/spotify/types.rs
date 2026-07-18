use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpotifyTrack {
    pub id: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration_ms: u32,
    pub uri: String,
}

impl SpotifyTrack {
    pub fn display_name(&self) -> String {
        format!("{} - {}", self.artists.join(", "), self.name)
    }

    pub fn duration_display(&self) -> String {
        let secs = self.duration_ms / 1000;
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// Parsed Spotify URL/URI types
#[derive(Debug, Clone)]
pub enum SpotifyRef {
    Track(String),
    Album(String),
    Playlist(String),
    /// The user's Liked Songs collection (internal sentinel, no ID).
    Liked,
}

/// Parse a Spotify URL or URI into a SpotifyRef.
/// Supports:
/// - spotify:track:ID
/// - spotify:album:ID
/// - spotify:playlist:ID
/// - https://open.spotify.com/track/ID?si=...
/// - https://open.spotify.com/album/ID
/// - https://open.spotify.com/playlist/ID
pub fn parse_spotify_ref(input: &str) -> Option<SpotifyRef> {
    let input = input.trim();

    // URI format: spotify:type:id
    if let Some(rest) = input.strip_prefix("spotify:") {
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() == 2 {
            let id = parts[1].to_string();
            return match parts[0] {
                "track" => Some(SpotifyRef::Track(id)),
                "album" => Some(SpotifyRef::Album(id)),
                "playlist" => Some(SpotifyRef::Playlist(id)),
                "collection" if id == "liked" => Some(SpotifyRef::Liked),
                _ => None,
            };
        }
    }

    // URL format: https://open.spotify.com/type/id(?params)
    static SPOTIFY_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"https?://open\.spotify\.com/(track|album|playlist)/([a-zA-Z0-9]+)").unwrap()
    });

    if let Some(caps) = SPOTIFY_URL_RE.captures(input) {
        let kind = caps.get(1)?.as_str();
        let id = caps.get(2)?.as_str().to_string();
        return match kind {
            "track" => Some(SpotifyRef::Track(id)),
            "album" => Some(SpotifyRef::Album(id)),
            "playlist" => Some(SpotifyRef::Playlist(id)),
            _ => None,
        };
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(name: &str, artists: Vec<&str>, duration_ms: u32) -> SpotifyTrack {
        SpotifyTrack {
            id: "abc123".to_string(),
            name: name.to_string(),
            artists: artists.into_iter().map(String::from).collect(),
            album: "Album".to_string(),
            duration_ms,
            uri: "spotify:track:abc123".to_string(),
        }
    }

    // -- display_name --

    #[test]
    fn display_name_single_artist() {
        let t = track("Photograph", vec!["Ed Sheeran"], 0);
        assert_eq!(t.display_name(), "Ed Sheeran - Photograph");
    }

    #[test]
    fn display_name_multiple_artists_joined_with_comma() {
        let t = track("Track", vec!["Artist A", "Artist B", "Artist C"], 0);
        assert_eq!(t.display_name(), "Artist A, Artist B, Artist C - Track");
    }

    #[test]
    fn display_name_no_artists() {
        let t = track("Solo", vec![], 0);
        assert_eq!(t.display_name(), " - Solo");
    }

    // -- duration_display --

    #[test]
    fn duration_display_zero() {
        let t = track("x", vec!["a"], 0);
        assert_eq!(t.duration_display(), "0:00");
    }

    #[test]
    fn duration_display_one_minute_five_seconds() {
        let t = track("x", vec!["a"], 65_000);
        assert_eq!(t.duration_display(), "1:05");
    }

    #[test]
    fn duration_display_zero_pads_seconds() {
        let t = track("x", vec!["a"], 60_000);
        assert_eq!(t.duration_display(), "1:00");
    }

    #[test]
    fn duration_display_floors_sub_second() {
        let t = track("x", vec!["a"], 1_500); // 1.5s → 1s
        assert_eq!(t.duration_display(), "0:01");
    }

    #[test]
    fn duration_display_long() {
        let t = track("x", vec!["a"], 9 * 60_000 + 59_000); // 9:59
        assert_eq!(t.duration_display(), "9:59");
    }

    #[test]
    fn duration_display_over_ten_minutes() {
        let t = track("x", vec!["a"], 65 * 60_000 + 7_000); // 65:07
        assert_eq!(t.duration_display(), "65:07");
    }

    // -- parse_spotify_ref: URI form --

    #[test]
    fn parse_uri_track() {
        match parse_spotify_ref("spotify:track:6rqhFgbbKwnb9MLmUQDhG6") {
            Some(SpotifyRef::Track(id)) => assert_eq!(id, "6rqhFgbbKwnb9MLmUQDhG6"),
            other => panic!("expected Track, got {other:?}"),
        }
    }

    #[test]
    fn parse_uri_album() {
        match parse_spotify_ref("spotify:album:1234abcd") {
            Some(SpotifyRef::Album(id)) => assert_eq!(id, "1234abcd"),
            other => panic!("expected Album, got {other:?}"),
        }
    }

    #[test]
    fn parse_uri_liked_collection() {
        assert!(matches!(
            parse_spotify_ref("spotify:collection:liked"),
            Some(SpotifyRef::Liked)
        ));
    }

    #[test]
    fn parse_uri_collection_other_id_is_none() {
        assert!(parse_spotify_ref("spotify:collection:xyz").is_none());
    }

    #[test]
    fn parse_uri_playlist() {
        match parse_spotify_ref("spotify:playlist:xyz") {
            Some(SpotifyRef::Playlist(id)) => assert_eq!(id, "xyz"),
            other => panic!("expected Playlist, got {other:?}"),
        }
    }

    #[test]
    fn parse_uri_unsupported_kind_returns_none() {
        // Unsupported URI types (artist, episode, show) → None.
        assert!(parse_spotify_ref("spotify:artist:x").is_none());
        assert!(parse_spotify_ref("spotify:episode:x").is_none());
    }

    // -- parse_spotify_ref: URL form --

    #[test]
    fn parse_url_track_https() {
        match parse_spotify_ref("https://open.spotify.com/track/6rqhFgbbKwnb9MLmUQDhG6") {
            Some(SpotifyRef::Track(id)) => assert_eq!(id, "6rqhFgbbKwnb9MLmUQDhG6"),
            other => panic!("expected Track, got {other:?}"),
        }
    }

    #[test]
    fn parse_url_track_http_accepted() {
        match parse_spotify_ref("http://open.spotify.com/album/1234abcd") {
            Some(SpotifyRef::Album(id)) => assert_eq!(id, "1234abcd"),
            other => panic!("expected Album, got {other:?}"),
        }
    }

    #[test]
    fn parse_url_strips_query_string() {
        // The id regex only matches alphanumerics, so the ?si=... is naturally excluded.
        match parse_spotify_ref(
            "https://open.spotify.com/track/abc123?si=qwerty&utm_source=copy-link",
        ) {
            Some(SpotifyRef::Track(id)) => assert_eq!(id, "abc123"),
            other => panic!("expected Track, got {other:?}"),
        }
    }

    #[test]
    fn parse_url_with_trailing_path_extracts_id_prefix() {
        // Regex `[a-zA-Z0-9]+` stops at the slash, so trailing path is ignored.
        match parse_spotify_ref("https://open.spotify.com/track/abc123/extra") {
            Some(SpotifyRef::Track(id)) => assert_eq!(id, "abc123"),
            other => panic!("expected Track, got {other:?}"),
        }
    }

    #[test]
    fn parse_url_playlist() {
        match parse_spotify_ref("https://open.spotify.com/playlist/PLAYLISTID") {
            Some(SpotifyRef::Playlist(id)) => assert_eq!(id, "PLAYLISTID"),
            other => panic!("expected Playlist, got {other:?}"),
        }
    }

    // -- parse_spotify_ref: whitespace and garbage --

    #[test]
    fn parse_trims_leading_and_trailing_whitespace() {
        match parse_spotify_ref("  spotify:track:abc  ") {
            Some(SpotifyRef::Track(id)) => assert_eq!(id, "abc"),
            other => panic!("expected Track, got {other:?}"),
        }
    }

    #[test]
    fn parse_garbage_returns_none() {
        assert!(parse_spotify_ref("").is_none());
        assert!(parse_spotify_ref("hello world").is_none());
        assert!(parse_spotify_ref("spotify:").is_none());
        assert!(parse_spotify_ref("https://example.com/track/abc").is_none());
    }

    #[test]
    fn parse_uri_with_empty_id_is_currently_accepted() {
        // Documents existing behavior: `spotify:track:` parses as Track("").
        // If we ever want to reject this, change here intentionally.
        match parse_spotify_ref("spotify:track:") {
            Some(SpotifyRef::Track(id)) => assert_eq!(id, ""),
            other => panic!("expected Track(\"\"), got {other:?}"),
        }
    }
}
