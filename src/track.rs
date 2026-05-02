//! Service-agnostic track wrapper.
//!
//! Each queue entry carries a `Track` rather than a service-specific struct,
//! so the queue can mix Spotify and YouTube items freely. The wrapper
//! preserves the inner type so service-specific fields stay accessible
//! when needed (e.g. radio recommendations require a Spotify URI).

use crate::services::Service;
use crate::spotify::types::SpotifyTrack;
use crate::youtube::types::YouTubeTrack;

#[derive(Debug, Clone)]
pub enum Track {
    Spotify(SpotifyTrack),
    YouTube(YouTubeTrack),
}

impl Track {
    pub fn service(&self) -> Service {
        match self {
            Self::Spotify(_) => Service::Spotify,
            Self::YouTube(_) => Service::YouTube,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Spotify(t) => &t.id,
            Self::YouTube(t) => &t.id,
        }
    }

    /// Service-specific URI used by the player to start playback.
    /// Spotify: `spotify:track:<id>`. YouTube: the bare video ID
    /// (the YouTubePlayer resolves it to a stream URL).
    pub fn uri(&self) -> &str {
        match self {
            Self::Spotify(t) => &t.uri,
            Self::YouTube(t) => &t.id,
        }
    }

    pub fn duration_ms(&self) -> u32 {
        match self {
            Self::Spotify(t) => t.duration_ms,
            Self::YouTube(t) => t.duration_ms,
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::Spotify(t) => t.display_name(),
            Self::YouTube(t) => t.display_name(),
        }
    }

    pub fn duration_display(&self) -> String {
        match self {
            Self::Spotify(t) => t.duration_display(),
            Self::YouTube(t) => t.duration_display(),
        }
    }

    /// Shareable web URL for the track.
    pub fn web_url(&self) -> String {
        match self {
            Self::Spotify(t) => t.uri
                .replace("spotify:track:", "https://open.spotify.com/track/")
                .replace("spotify:episode:", "https://open.spotify.com/episode/"),
            Self::YouTube(t) => format!("https://music.youtube.com/watch?v={}", t.id),
        }
    }
}

impl From<SpotifyTrack> for Track {
    fn from(t: SpotifyTrack) -> Self {
        Self::Spotify(t)
    }
}

impl From<YouTubeTrack> for Track {
    fn from(t: YouTubeTrack) -> Self {
        Self::YouTube(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp_track() -> SpotifyTrack {
        SpotifyTrack {
            id: "abc".to_string(),
            name: "Photograph".to_string(),
            artists: vec!["Ed Sheeran".to_string()],
            album: "X".to_string(),
            duration_ms: 60_000,
            uri: "spotify:track:abc".to_string(),
        }
    }

    #[test]
    fn spotify_variant_returns_spotify_service() {
        let t: Track = sp_track().into();
        assert_eq!(t.service(), Service::Spotify);
    }

    #[test]
    fn accessors_delegate_to_inner_spotify_track() {
        let t: Track = sp_track().into();
        assert_eq!(t.id(), "abc");
        assert_eq!(t.uri(), "spotify:track:abc");
        assert_eq!(t.duration_ms(), 60_000);
        assert_eq!(t.display_name(), "Ed Sheeran - Photograph");
        assert_eq!(t.duration_display(), "1:00");
    }

    #[test]
    fn from_spotify_track_wraps_in_spotify_variant() {
        let t: Track = sp_track().into();
        match t {
            Track::Spotify(inner) => assert_eq!(inner.id, "abc"),
            other => panic!("expected Spotify variant, got {other:?}"),
        }
    }

    fn yt_track() -> YouTubeTrack {
        YouTubeTrack {
            id: "vid123".to_string(),
            name: "Song".to_string(),
            artists: vec!["Band".to_string()],
            album: "Album".to_string(),
            duration_ms: 90_000,
        }
    }

    #[test]
    fn youtube_variant_returns_youtube_service() {
        let t: Track = yt_track().into();
        assert_eq!(t.service(), Service::YouTube);
    }

    #[test]
    fn youtube_accessors_delegate_to_inner() {
        let t: Track = yt_track().into();
        assert_eq!(t.id(), "vid123");
        assert_eq!(t.uri(), "vid123"); // YT uri == id
        assert_eq!(t.duration_ms(), 90_000);
        assert_eq!(t.display_name(), "Band - Song");
        assert_eq!(t.duration_display(), "1:30");
    }
}
