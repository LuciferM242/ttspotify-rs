//! Music service identity and per-service capabilities.
//!
//! `Service` tags every queue entry with which provider it came from
//! (Spotify or YouTube), and is also stored on `PlayerState` as the
//! "active service" — the one new commands like `p <query>` target.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[derive(Default)]
pub enum Service {
    #[default]
    Spotify,
    YouTube,
}

impl Service {
    /// Short tag rendered in `queue` listings, e.g. `[SP]` / `[YT]`.
    pub fn marker(self) -> &'static str {
        match self {
            Self::Spotify => "SP",
            Self::YouTube => "YT",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Spotify => "Spotify",
            Self::YouTube => "YouTube",
        }
    }

    /// Parse common spellings ("spotify", "Spotify", "yt", "youtube", etc.).
    /// Unrecognized input falls through to `default()`.
    pub fn parse_or_default(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "spotify" | "sp" | "s" => Self::Spotify,
            "youtube" | "yt" | "y" => Self::YouTube,
            _ => Self::default(),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_returns_two_letter_code() {
        assert_eq!(Service::Spotify.marker(), "SP");
        assert_eq!(Service::YouTube.marker(), "YT");
    }

    #[test]
    fn name_is_human_readable() {
        assert_eq!(Service::Spotify.name(), "Spotify");
        assert_eq!(Service::YouTube.name(), "YouTube");
    }

    #[test]
    fn default_is_spotify() {
        assert_eq!(Service::default(), Service::Spotify);
    }
}
