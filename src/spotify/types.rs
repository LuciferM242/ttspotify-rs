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
                _ => None,
            };
        }
    }

    // URL format: https://open.spotify.com/type/id(?params)
    let re = regex::Regex::new(
        r"https?://open\.spotify\.com/(track|album|playlist)/([a-zA-Z0-9]+)"
    ).ok()?;

    if let Some(caps) = re.captures(input) {
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
