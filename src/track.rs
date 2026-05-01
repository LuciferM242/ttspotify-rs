//! Service-agnostic track wrapper.
//!
//! Each queue entry carries a `Track` rather than a service-specific struct,
//! so the queue can mix Spotify and YouTube items freely. The wrapper
//! preserves the inner type so service-specific fields stay accessible
//! when needed (e.g. radio recommendations require a Spotify URI).

use crate::services::Service;
use crate::spotify::types::SpotifyTrack;

#[derive(Debug, Clone)]
pub enum Track {
    Spotify(SpotifyTrack),
    // YouTube(YouTubeTrack) — added in Phase 2 with rustypipe integration.
}

impl Track {
    pub fn service(&self) -> Service {
        match self {
            Self::Spotify(_) => Service::Spotify,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Spotify(t) => &t.id,
        }
    }

    pub fn uri(&self) -> &str {
        match self {
            Self::Spotify(t) => &t.uri,
        }
    }

    pub fn duration_ms(&self) -> u32 {
        match self {
            Self::Spotify(t) => t.duration_ms,
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::Spotify(t) => t.display_name(),
        }
    }

    pub fn duration_display(&self) -> String {
        match self {
            Self::Spotify(t) => t.duration_display(),
        }
    }
}

impl From<SpotifyTrack> for Track {
    fn from(t: SpotifyTrack) -> Self {
        Self::Spotify(t)
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
        }
    }
}
