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

/// Parsed YouTube URL/ID kinds we know how to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YouTubeRef {
    /// 11-char video ID from an unambiguous URL form. Resolves to a single
    /// track via `music_details`.
    Video(String),
    /// Bare 11-char string that LOOKS like a video ID but might equally be an
    /// 11-character search word (e.g. "helloworld1"). The resolver tries it as
    /// an ID first and falls back to a search when the details fetch fails.
    BareVideo(String),
    /// Playlist ID (PL..., RD..., LM, OLAK..., etc.). Resolves via
    /// `music_playlist`. Covers user playlists, mood mixes, daily mixes,
    /// "liked music" (LM, requires auth), artist radios.
    Playlist(String),
    /// Album browse ID (`MPREb_...`). Resolves via `music_album`.
    Album(String),
}

/// Recognize common YouTube / YouTube Music URL forms and bare IDs.
/// Returns `None` for anything that should be treated as a search query.
pub fn parse_youtube_ref(input: &str) -> Option<YouTubeRef> {
    let input = input.trim();

    // Bare 11-char video ID (alphanum + - _). Tagged BareVideo: could just as
    // well be an 11-letter search word, so the resolver may fall back.
    if input.len() == 11 && input.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Some(YouTubeRef::BareVideo(input.to_string()));
    }

    // Strip scheme + host so we can match against the path + query uniformly.
    let path_query = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        .unwrap_or(input);
    let path_query = path_query
        .strip_prefix("music.youtube.com/")
        .or_else(|| path_query.strip_prefix("www.youtube.com/"))
        .or_else(|| path_query.strip_prefix("youtube.com/"))
        .or_else(|| path_query.strip_prefix("m.youtube.com/"))
        .or_else(|| path_query.strip_prefix("youtu.be/"))
        .unwrap_or(path_query);

    // youtu.be/<id> short URLs land here as `<id>` (or `<id>?...`).
    if let Some(id) = path_query.split(['?', '#', '/']).next() {
        if id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            // Only treat it as a video if there's no extra path
            // (e.g. avoid matching `playlist?...` whose first split is "playlist").
            if !path_query.starts_with("playlist") && !path_query.starts_with("watch")
                && !path_query.starts_with("browse")
            {
                return Some(YouTubeRef::Video(id.to_string()));
            }
        }
    }

    // Shorts: /shorts/<id>
    if let Some(rest) = path_query.strip_prefix("shorts/") {
        let id = rest.split(['?', '#', '/']).next().unwrap_or("");
        if id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Some(YouTubeRef::Video(id.to_string()));
        }
    }

    // Album browse: /browse/MPREb_...
    if let Some(rest) = path_query.strip_prefix("browse/") {
        let id = rest.split(['?', '#', '/']).next().unwrap_or("");
        if id.starts_with("MPRE") || id.starts_with("MPREb_") {
            return Some(YouTubeRef::Album(id.to_string()));
        }
    }

    // Walk the query string. `list=` wins over `v=` when both are present —
    // matches what music.youtube.com plays when you click "watch in playlist".
    if let Some(query) = path_query.split_once('?').map(|(_, q)| q) {
        let mut video_id: Option<&str> = None;
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("list=") {
                if !value.is_empty() {
                    return Some(YouTubeRef::Playlist(value.to_string()));
                }
            } else if let Some(value) = pair.strip_prefix("v=") {
                if value.len() == 11 {
                    video_id = Some(value);
                }
            }
        }
        if let Some(id) = video_id {
            return Some(YouTubeRef::Video(id.to_string()));
        }
    }

    None
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

    #[test]
    fn parse_bare_video_id() {
        // Bare IDs are tagged separately: an 11-char search word is
        // indistinguishable from an ID, so the resolver needs to know it may
        // fall back to a search when the details fetch fails.
        assert_eq!(
            parse_youtube_ref("dQw4w9WgXcQ"),
            Some(YouTubeRef::BareVideo("dQw4w9WgXcQ".into()))
        );
    }

    #[test]
    fn parse_shorts_url() {
        assert_eq!(
            parse_youtube_ref("https://www.youtube.com/shorts/dQw4w9WgXcQ"),
            Some(YouTubeRef::Video("dQw4w9WgXcQ".into()))
        );
        assert_eq!(
            parse_youtube_ref("https://youtube.com/shorts/dQw4w9WgXcQ?feature=share"),
            Some(YouTubeRef::Video("dQw4w9WgXcQ".into()))
        );
    }

    #[test]
    fn parse_youtu_be_short_url() {
        assert_eq!(parse_youtube_ref("https://youtu.be/dQw4w9WgXcQ"), Some(YouTubeRef::Video("dQw4w9WgXcQ".into())));
    }

    #[test]
    fn parse_youtube_watch_url() {
        assert_eq!(
            parse_youtube_ref("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            Some(YouTubeRef::Video("dQw4w9WgXcQ".into()))
        );
    }

    #[test]
    fn parse_music_youtube_watch_url() {
        assert_eq!(
            parse_youtube_ref("https://music.youtube.com/watch?v=dQw4w9WgXcQ&si=abc"),
            Some(YouTubeRef::Video("dQw4w9WgXcQ".into()))
        );
    }

    #[test]
    fn parse_playlist_url() {
        assert_eq!(
            parse_youtube_ref("https://music.youtube.com/playlist?list=PLkDz3vRBiruazmPbUS0mAJzGnP6kFq0jQ"),
            Some(YouTubeRef::Playlist("PLkDz3vRBiruazmPbUS0mAJzGnP6kFq0jQ".into()))
        );
    }

    #[test]
    fn parse_radio_playlist_url() {
        // "Up next" / mood mix style playlist IDs (RDCLAK..., RDAMVM..., etc.) are still playlists.
        assert_eq!(
            parse_youtube_ref("https://music.youtube.com/playlist?list=RDCLAK5uy_kFQXdnqMaQCVx2ziFf8YkBzRv5Tn4Mfng"),
            Some(YouTubeRef::Playlist("RDCLAK5uy_kFQXdnqMaQCVx2ziFf8YkBzRv5Tn4Mfng".into()))
        );
    }

    #[test]
    fn parse_album_browse_url() {
        assert_eq!(
            parse_youtube_ref("https://music.youtube.com/browse/MPREb_O2gXCdCVGsZ"),
            Some(YouTubeRef::Album("MPREb_O2gXCdCVGsZ".into()))
        );
    }

    #[test]
    fn parse_watch_url_with_list_prefers_playlist() {
        // /watch?v=...&list=... — the playlist takes precedence so the user
        // gets the whole list queued, matching what music.youtube.com plays.
        assert_eq!(
            parse_youtube_ref("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc"),
            Some(YouTubeRef::Playlist("PLabc".into()))
        );
    }

    #[test]
    fn parse_garbage_returns_none() {
        assert_eq!(parse_youtube_ref(""), None);
        assert_eq!(parse_youtube_ref("hello world"), None);
        assert_eq!(parse_youtube_ref("some search query"), None);
        assert_eq!(parse_youtube_ref("https://example.com/foo"), None);
    }
}
