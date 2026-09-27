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
    /// Playlist ID (PL..., OLAK..., RDCLAK..., LM, etc.). Resolves via
    /// `music_playlist`. Covers user playlists, album playlists, the curated
    /// mixes and "liked music" (LM, requires auth).
    Playlist(String),
    /// A radio station: `id` is the `RD...` the link names, `start` the video
    /// it opens on when the link says. YouTube builds these queues per play,
    /// so `music_playlist` cannot fetch them; they go through `music_radio`.
    /// Rebuilding a station from the video instead loses which one it was:
    /// `RDEM...` and `RDAMVM<video>` play different songs.
    Radio { id: String, start: Option<String> },
    /// Album browse ID (`MPREb_...`). Resolves via `music_album`.
    Album(String),
}

/// Whether `s` is a radio station id rather than a video id. A video id can
/// begin with `RD` too, so the shape decides: `RD...` station ids are never
/// exactly 11 characters.
pub fn is_radio_id(s: &str) -> bool {
    s.starts_with("RD") && !is_video_id(s)
}

/// Whether `s` has the shape of a YouTube video id: exactly 11 characters of
/// alphanumerics, `-` and `_`.
fn is_video_id(s: &str) -> bool {
    s.len() == 11 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Recognize common YouTube / YouTube Music URL forms and bare IDs.
/// Returns `None` for anything that should be treated as a search query.
pub fn parse_youtube_ref(input: &str) -> Option<YouTubeRef> {
    let input = input.trim();

    // Bare 11-char video ID. Tagged BareVideo: could just as well be an
    // 11-letter search word, so the resolver may fall back.
    if is_video_id(input) {
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
    // Drop any #fragment up front: it otherwise rides along inside the last
    // query value, making `watch?v=<id>#t=30` fail the 11-char check (silent
    // search instead of a play) and `list=<id>#x` a corrupted playlist id.
    let path_query = path_query.split('#').next().unwrap_or(path_query);

    // youtu.be/<id> short URLs land here as `<id>` (or `<id>?...`).
    if let Some(id) = path_query.split(['?', '#', '/']).next() {
        if is_video_id(id) {
            // Only treat it as a video if there's no extra path
            // (e.g. avoid matching `playlist?...` whose first split is "playlist").
            if !path_query.starts_with("playlist") && !path_query.starts_with("watch")
                && !path_query.starts_with("browse")
            {
                return Some(YouTubeRef::Video(id.to_string()));
            }
        }
    }

    // Path-embedded video ids: /shorts/<id>, /live/<id> (streams/premieres —
    // the link keeps working as a normal VOD afterwards), /embed/<id> and the
    // ancient /v/<id>. Without these the whole URL fell through to a music
    // search on the literal link text, queueing an unrelated top hit.
    for prefix in ["shorts/", "live/", "embed/", "v/"] {
        if let Some(rest) = path_query.strip_prefix(prefix) {
            let id = rest.split(['?', '#', '/']).next().unwrap_or("");
            if is_video_id(id) {
                return Some(YouTubeRef::Video(id.to_string()));
            }
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
        let mut list_id: Option<&str> = None;
        let mut video_id: Option<&str> = None;
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("list=").filter(|v| !v.is_empty()) {
                list_id = Some(value);
            } else if let Some(value) = pair.strip_prefix("v=").filter(|v| is_video_id(v)) {
                video_id = Some(value);
            }
        }
        if let Some(list) = list_id {
            return Some(classify_list(list, video_id));
        }
        if let Some(id) = video_id {
            return Some(YouTubeRef::Video(id.to_string()));
        }
    }

    None
}

/// Where in the track a link says to start, in seconds.
///
/// `t=` on a watch or `youtu.be` link, `start=` on an embed, or the older
/// `#t=` fragment. Seconds on their own, or YouTube's `1h2m3s` form. `None`
/// when there is no offset to honour, zero included.
pub fn parse_start_seconds(input: &str) -> Option<u32> {
    let input = input.trim();
    let (before_fragment, fragment) = match input.split_once('#') {
        Some((before, after)) => (before, Some(after)),
        None => (input, None),
    };
    let value = before_fragment
        .split_once('?')
        .map(|(_, query)| query)
        .into_iter()
        .flat_map(|query| query.split('&'))
        .chain(fragment)
        .find_map(|pair| {
            pair.strip_prefix("t=").or_else(|| pair.strip_prefix("start="))
        })?;
    parse_duration_seconds(value).filter(|secs| *secs > 0)
}

/// `90`, `90s`, `2m`, `1m30s`, `1h2m3s` as seconds. `None` on anything else,
/// so a stray `t=later` is ignored rather than starting the track at 0.
fn parse_duration_seconds(value: &str) -> Option<u32> {
    if value.is_empty() {
        return None;
    }
    if let Ok(secs) = value.parse::<u32>() {
        return Some(secs);
    }
    let mut total: u32 = 0;
    let mut digits = String::new();
    for c in value.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
        let count: u32 = digits.parse().ok()?;
        digits.clear();
        total = total.checked_add(count.checked_mul(unit)?)?;
    }
    // Trailing digits with no unit ("1m30") are not a form YouTube writes.
    digits.is_empty().then_some(total)
}

/// What a `list=` id points at, given any video id standing beside it.
///
/// `RD...` ids are radio stations YouTube builds per play, not stored
/// playlists, so fetching them as playlists fails. The curated `RDCLAK` mixes
/// are the exception: those are ordinary playlists.
fn classify_list(list_id: &str, video_id: Option<&str>) -> YouTubeRef {
    if !list_id.starts_with("RD") || list_id.starts_with("RDCLAK") {
        return YouTubeRef::Playlist(list_id.to_string());
    }
    // Where to start: the video the link names, or the one `RDAMVM<id>` and
    // `RD<id>` carry. A mix such as `RDEM...` or `RDMM` names neither, and
    // then the station's own first track is the start.
    let start = video_id.or_else(|| {
        list_id
            .strip_prefix("RDAMVM")
            .or_else(|| list_id.strip_prefix("RD"))
            .filter(|id| is_video_id(id))
    });
    YouTubeRef::Radio { id: list_id.to_string(), start: start.map(str::to_string) }
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
    fn parse_curated_mix_url() {
        // RDCLAK ids are YouTube's curated mixes: stored playlists like any other.
        assert_eq!(
            parse_youtube_ref("https://music.youtube.com/playlist?list=RDCLAK5uy_kFQXdnqMaQCVx2ziFf8YkBzRv5Tn4Mfng"),
            Some(YouTubeRef::Playlist("RDCLAK5uy_kFQXdnqMaQCVx2ziFf8YkBzRv5Tn4Mfng".into()))
        );
    }

    #[test]
    fn parse_track_radio_url_keeps_the_station_and_its_first_video() {
        // "Start radio" links. The station id is what YouTube plays from; the
        // video beside it, or the one inside `RDAMVM<id>`/`RD<id>`, is where
        // it starts.
        assert_eq!(
            parse_youtube_ref("https://music.youtube.com/watch?v=5oWyMakvQew&list=RDAMVM5oWyMakvQew"),
            Some(YouTubeRef::Radio {
                id: "RDAMVM5oWyMakvQew".into(),
                start: Some("5oWyMakvQew".into()),
            })
        );
        assert_eq!(
            parse_youtube_ref("https://www.youtube.com/watch?v=5oWyMakvQew&list=RD5oWyMakvQew&start_radio=1"),
            Some(YouTubeRef::Radio {
                id: "RD5oWyMakvQew".into(),
                start: Some("5oWyMakvQew".into()),
            })
        );
        assert_eq!(
            parse_youtube_ref("https://music.youtube.com/playlist?list=RDAMVM5oWyMakvQew"),
            Some(YouTubeRef::Radio {
                id: "RDAMVM5oWyMakvQew".into(),
                start: Some("5oWyMakvQew".into()),
            })
        );
    }

    #[test]
    fn parse_endless_mix_url_is_its_own_station() {
        // An RDEM mix is a station in its own right: it embeds no video, so
        // the station id must survive rather than being rebuilt from `v=`.
        assert_eq!(
            parse_youtube_ref(
                "https://www.youtube.com/watch?v=NrLkTZrPZA4&list=RDEMtu8TSn01ATwUIqXnxfa6zQ&start_radio=1"
            ),
            Some(YouTubeRef::Radio {
                id: "RDEMtu8TSn01ATwUIqXnxfa6zQ".into(),
                start: Some("NrLkTZrPZA4".into()),
            })
        );
    }

    #[test]
    fn parse_mix_with_no_video_starts_at_the_station() {
        // Nothing says where to start, so the station's own first track does.
        assert_eq!(
            parse_youtube_ref(
                "https://music.youtube.com/playlist?list=RDTMAK5uy_kset8DisdE7LSD4TNjEVvrKRTmG7a56sY"
            ),
            Some(YouTubeRef::Radio {
                id: "RDTMAK5uy_kset8DisdE7LSD4TNjEVvrKRTmG7a56sY".into(),
                start: None,
            })
        );
        assert_eq!(
            parse_youtube_ref("https://music.youtube.com/playlist?list=RDMM"),
            Some(YouTubeRef::Radio { id: "RDMM".into(), start: None })
        );
    }

    #[test]
    fn parse_curated_mix_stays_a_playlist_even_beside_a_video() {
        // RDCLAK mixes are real, fetchable playlists, so the list wins over the
        // video the way it does for any other playlist link.
        assert_eq!(
            parse_youtube_ref(
                "https://music.youtube.com/watch?v=5oWyMakvQew&list=RDCLAK5uy_kFQXdnqMaQCVx2ziFf8YkBzRv5Tn4Mfng"
            ),
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
    fn parse_live_embed_and_old_style_urls() {
        // /live/<id> is YouTube's canonical link for streams and premieres and
        // keeps resolving as a normal video afterwards; /embed/ and /v/ are
        // other path-embedded forms. All used to fall through to a search on
        // the literal URL text.
        assert_eq!(
            parse_youtube_ref("https://www.youtube.com/live/dQw4w9WgXcQ"),
            Some(YouTubeRef::Video("dQw4w9WgXcQ".into()))
        );
        assert_eq!(
            parse_youtube_ref("https://www.youtube.com/live/dQw4w9WgXcQ?feature=share"),
            Some(YouTubeRef::Video("dQw4w9WgXcQ".into()))
        );
        assert_eq!(
            parse_youtube_ref("https://www.youtube.com/embed/dQw4w9WgXcQ"),
            Some(YouTubeRef::Video("dQw4w9WgXcQ".into()))
        );
        assert_eq!(
            parse_youtube_ref("https://www.youtube.com/v/dQw4w9WgXcQ"),
            Some(YouTubeRef::Video("dQw4w9WgXcQ".into()))
        );
    }

    #[test]
    fn parse_urls_with_fragments_still_resolve() {
        // A #fragment used to ride inside the last query value: watch?v=<id>#t
        // failed the 11-char check (silent search) and list=<id>#x produced a
        // corrupted playlist id.
        assert_eq!(
            parse_youtube_ref("https://www.youtube.com/watch?v=dQw4w9WgXcQ#t=30"),
            Some(YouTubeRef::Video("dQw4w9WgXcQ".into()))
        );
        assert_eq!(
            parse_youtube_ref("https://music.youtube.com/playlist?list=PLabc#share"),
            Some(YouTubeRef::Playlist("PLabc".into()))
        );
    }

    #[test]
    fn parse_start_offset_reads_plain_seconds() {
        assert_eq!(parse_start_seconds("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=90"), Some(90));
        assert_eq!(parse_start_seconds("https://youtu.be/dQw4w9WgXcQ?t=42"), Some(42));
        // /embed/ links spell it `start`.
        assert_eq!(parse_start_seconds("https://www.youtube.com/embed/dQw4w9WgXcQ?start=15"), Some(15));
    }

    #[test]
    fn parse_start_offset_reads_the_hms_form() {
        assert_eq!(parse_start_seconds("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=90s"), Some(90));
        assert_eq!(parse_start_seconds("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1m30s"), Some(90));
        assert_eq!(parse_start_seconds("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1h2m3s"), Some(3723));
        assert_eq!(parse_start_seconds("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=2m"), Some(120));
    }

    #[test]
    fn parse_start_offset_reads_the_fragment_form() {
        // The old share form, and the one a browser leaves in the address bar.
        assert_eq!(parse_start_seconds("https://www.youtube.com/watch?v=dQw4w9WgXcQ#t=30"), Some(30));
        assert_eq!(parse_start_seconds("https://www.youtube.com/watch?v=dQw4w9WgXcQ#t=1m5s"), Some(65));
    }

    #[test]
    fn parse_start_offset_ignores_everything_else() {
        assert_eq!(parse_start_seconds("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(parse_start_seconds("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=0"), None);
        assert_eq!(parse_start_seconds("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=later"), None);
        assert_eq!(parse_start_seconds("https://www.youtube.com/watch?v=dQw4w9WgXcQ&si=abc"), None);
        // A radio link keeps its station and still starts where it says.
        assert_eq!(
            parse_start_seconds("https://www.youtube.com/watch?v=NrLkTZrPZA4&list=RDEMtu8&t=45"),
            Some(45)
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
