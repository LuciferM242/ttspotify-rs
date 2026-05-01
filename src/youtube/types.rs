/// A YouTube Music track in the bot's internal representation.
///
/// Mirrors the shape of `SpotifyTrack` so the two coexist cleanly in `Track`.
/// `id` is the YouTube video ID (used to build the URL via `link`).
#[derive(Debug, Clone)]
pub struct YouTubeTrack {
    pub id: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration_ms: u32,
}

impl YouTubeTrack {
    pub fn display_name(&self) -> String {
        format!("{} - {}", self.artists.join(", "), self.name)
    }

    pub fn duration_display(&self) -> String {
        let secs = self.duration_ms / 1000;
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> YouTubeTrack {
        YouTubeTrack {
            id: "vid123".to_string(),
            name: "Song".to_string(),
            artists: vec!["Artist A".to_string(), "Artist B".to_string()],
            album: "Album".to_string(),
            duration_ms: 65_000,
        }
    }

    #[test]
    fn display_name_joins_artists() {
        assert_eq!(t().display_name(), "Artist A, Artist B - Song");
    }

    #[test]
    fn duration_display_formats_mm_ss() {
        assert_eq!(t().duration_display(), "1:05");
    }
}
