//! Music service identity and per-service capabilities.
//!
//! `Service` tags every queue entry with which provider it came from
//! (Spotify or YouTube), and is also stored on `PlayerState` as the
//! "active service" — the one new commands like `p <query>` target.

/// Written as `"Spotify"` / `"YouTube"`, but read back from any capitalisation
/// (and from the short forms the chat commands accept). A config is a file
/// people edit by hand, and `"youtube"` used to fail the whole parse — which
/// did not report a bad value, it made the bot disappear from every command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[derive(Default)]
pub enum Service {
    #[default]
    Spotify,
    YouTube,
}

impl<'de> serde::Deserialize<'de> for Service {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "\"{raw}\" is not a service; use \"Spotify\" or \"YouTube\""
            ))
        })
    }
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

    /// Parse common spellings ("spotify", "Spotify", "yt", "youtube", etc.),
    /// or `None` when it names no service.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "spotify" | "sp" | "s" => Some(Self::Spotify),
            "youtube" | "yt" | "y" => Some(Self::YouTube),
            _ => None,
        }
    }

    /// Parse common spellings ("spotify", "Spotify", "yt", "youtube", etc.).
    /// Unrecognized input falls through to `default()`.
    pub fn parse_or_default(s: &str) -> Self {
        Self::parse(s).unwrap_or_default()
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
