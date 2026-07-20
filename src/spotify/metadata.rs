use std::sync::Arc;

use librespot_core::session::Session;
use librespot_core::spotify_uri::SpotifyUri;
use librespot_metadata::Metadata;
use parking_lot::Mutex;

use crate::error::BotError;
use crate::spotify::types::{SpotifyRef, SpotifyTrack, parse_spotify_ref};

/// Metadata for the tracks to enqueue now, plus URIs still to be fetched by a
/// background loader (empty when the resolve was complete). `bulk` marks
/// collection sources (playlist / liked songs) so the runner applies bulk
/// semantics (dedup against the queue, no radio seed) even for a single track.
pub struct ResolvedTracks {
    pub tracks: Vec<SpotifyTrack>,
    pub remaining: Vec<SpotifyUri>,
    pub bulk: bool,
}

/// How many tracks a bulk source (playlist / liked songs) fetches up front
/// before handing the rest to the background loader.
pub const BULK_FIRST_BATCH: usize = 50;

/// Metadata client sharing the runner's swappable session holder.
///
/// The session lives behind a shared `Arc<Mutex<Session>>` (the same holder the
/// recovery routine swaps on a session rebuild), so after a recovery every
/// metadata call transparently uses the new session — no reconstruction needed.
/// Cloning shares the holder.
#[derive(Clone)]
pub struct SpotifyMetadata {
    session: Arc<Mutex<Session>>,
}

impl SpotifyMetadata {
    pub fn new(session: Arc<Mutex<Session>>) -> Self {
        Self { session }
    }

    /// Snapshot the current session (cheap `Arc`-backed clone) for a request.
    fn session(&self) -> Session {
        self.session.lock().clone()
    }

    // ---- helpers ----

    /// Convert a librespot Track + URI into our SpotifyTrack.
    fn track_to_spotify(track: &librespot_metadata::Track, uri: &SpotifyUri) -> SpotifyTrack {
        SpotifyTrack {
            id: uri.to_id().unwrap_or_default(),
            name: track.name.clone(),
            artists: track.artists.0.iter().map(|a| a.name.clone()).collect(),
            album: track.album.name.clone(),
            duration_ms: track.duration as u32,
            uri: uri.to_uri().unwrap_or_default(),
        }
    }

    /// Fetch a Track from Spotify and convert to SpotifyTrack.
    async fn fetch_track(&self, uri: &SpotifyUri) -> Result<SpotifyTrack, BotError> {
        let track = librespot_metadata::Track::get(&self.session(), uri).await
            .map_err(|e| BotError::Playback(format!("Failed to fetch track metadata: {e}")))?;
        Ok(Self::track_to_spotify(&track, uri))
    }

    // ---- librespot-metadata (Mercury protocol, no HTTP) ----

    /// Fetch a single track's metadata via librespot-metadata.
    pub async fn get_track_meta(&self, uri: &SpotifyUri) -> Result<SpotifyTrack, BotError> {
        self.fetch_track(uri).await
    }

    /// Fetch all tracks from an album via librespot-metadata.
    pub async fn get_album_tracks_meta(&self, uri: &SpotifyUri) -> Result<Vec<SpotifyTrack>, BotError> {
        let album = librespot_metadata::Album::get(&self.session(), uri).await
            .map_err(|e| BotError::Playback(format!("Failed to fetch album metadata: {e}")))?;

        let album_name = album.name.clone();
        let mut tracks = Vec::new();

        for track_uri in album.tracks() {
            match self.fetch_track(track_uri).await {
                Ok(mut t) => {
                    t.album = album_name.clone();
                    tracks.push(t);
                }
                Err(e) => tracing::warn!("Failed to fetch track {track_uri:?}: {e}"),
            }
        }

        Ok(tracks)
    }

    /// Fetch metadata for each URI, skipping failures with a warning.
    pub async fn fetch_tracks_meta(&self, uris: &[SpotifyUri]) -> Vec<SpotifyTrack> {
        let mut tracks = Vec::with_capacity(uris.len());
        for uri in uris {
            match self.fetch_track(uri).await {
                Ok(t) => tracks.push(t),
                Err(e) => tracing::warn!("Failed to fetch track {uri:?}: {e}"),
            }
        }
        tracks
    }

    /// All track URIs of a playlist (metadata is fetched in batches later).
    pub async fn get_playlist_track_uris(&self, uri: &SpotifyUri) -> Result<Vec<SpotifyUri>, BotError> {
        let playlist = librespot_metadata::Playlist::get(&self.session(), uri).await
            .map_err(|e| BotError::Playback(format!("Failed to fetch playlist metadata: {e}")))?;
        Ok(playlist.tracks().cloned().collect())
    }

    /// URIs of the user's Liked Songs, newest first, via the same spclient
    /// context endpoint Connect devices use for `spotify:user:<id>:collection`.
    pub async fn get_liked_track_uris(&self) -> Result<Vec<SpotifyUri>, BotError> {
        let ctx_uri = format!("spotify:user:{}:collection", self.session().username());
        let ctx = self.session().spclient().get_context(&ctx_uri).await
            .map_err(|e| BotError::Playback(format!("Liked songs fetch failed: {e}")))?;

        let mut uris = Vec::new();
        for page in ctx.pages.iter() {
            for track_ctx in page.tracks.iter() {
                if let Some(uri_str) = track_ctx.uri.as_deref() {
                    if let Ok(uri) = SpotifyUri::from_uri(uri_str) {
                        uris.push(uri);
                    }
                }
            }
        }
        if uris.is_empty() {
            return Err(BotError::NoResults);
        }
        Ok(uris)
    }

    /// Fetch metadata for the first `BULK_FIRST_BATCH` URIs now; the rest are
    /// returned for a background loader.
    async fn split_and_fetch_first(&self, mut uris: Vec<SpotifyUri>) -> Result<ResolvedTracks, BotError> {
        let remaining = if uris.len() > BULK_FIRST_BATCH {
            uris.split_off(BULK_FIRST_BATCH)
        } else {
            Vec::new()
        };
        let tracks = self.fetch_tracks_meta(&uris).await;
        if tracks.is_empty() {
            return Err(BotError::NoResults);
        }
        Ok(ResolvedTracks { tracks, remaining, bulk: true })
    }

    /// Fetch radio recommendations using Spotify's radio-apollo endpoint.
    /// This is the same engine Spotify uses for autoplay/radio.
    pub async fn get_radio_tracks(
        &self,
        seed_track_uri: &SpotifyUri,
        limit: usize,
        exclude_ids: &[String],
    ) -> Result<Vec<SpotifyTrack>, BotError> {
        let uri_str = seed_track_uri.to_uri()
            .map_err(|e| BotError::Playback(format!("Invalid seed URI: {e}")))?;

        let response = self.session().spclient()
            .get_apollo_station("stations", &uri_str, Some(limit), vec![], true)
            .await
            .map_err(|e| BotError::Playback(format!("Radio fetch failed: {e}")))?;

        let json_str = String::from_utf8_lossy(&response);
        let json: serde_json::Value = serde_json::from_str(&json_str)
            .map_err(|e| BotError::Playback(format!("Radio parse failed: {e}")))?;

        let track_uris: Vec<&str> = json["tracks"].as_array()
            .map(|arr| arr.iter().filter_map(|t| t["uri"].as_str()).collect())
            .unwrap_or_default();

        if track_uris.is_empty() {
            return Err(BotError::NoResults);
        }

        let mut tracks = Vec::new();
        for uri_str in track_uris.into_iter() {
            if tracks.len() >= limit {
                break;
            }
            let uri = match SpotifyUri::from_uri(uri_str) {
                Ok(u) => u,
                Err(_) => continue,
            };
            // Skip tracks already in the queue
            let id = uri.to_id().unwrap_or_default();
            if exclude_ids.iter().any(|eid| eid == &id) {
                continue;
            }
            match self.fetch_track(&uri).await {
                Ok(t) => tracks.push(t),
                Err(e) => tracing::warn!("Failed to fetch radio track {uri_str}: {e}"),
            }
        }

        if tracks.is_empty() {
            Err(BotError::NoResults)
        } else {
            Ok(tracks)
        }
    }

    // ---- Spotify Web API (search + recommendations) ----

    /// Search tracks via Spotify's internal spclient (no Web API token needed).
    pub async fn search_tracks(&self, query: &str, limit: u8) -> Result<Vec<SpotifyTrack>, BotError> {
        let search_uri = search_context_uri(query);
        let ctx = self.session().spclient().get_context(&search_uri).await
            .map_err(|e| BotError::Playback(format!("Search failed: {e}")))?;

        let mut tracks = Vec::new();
        for page in ctx.pages.iter() {
            for track_ctx in page.tracks.iter() {
                if tracks.len() >= limit as usize { break; }
                let uri_str = match track_ctx.uri.as_deref() {
                    Some(u) => u,
                    None => continue,
                };
                let uri = match SpotifyUri::from_uri(uri_str) {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                match self.fetch_track(&uri).await {
                    Ok(t) => tracks.push(t),
                    Err(_) => continue,
                }
            }
            if tracks.len() >= limit as usize { break; }
        }

        if tracks.is_empty() {
            return Err(BotError::NoResults);
        }
        Ok(tracks)
    }

    /// Resolve any query (search text, URL, URI) to tracks. Track/album/search
    /// resolve completely; playlists and the liked collection resolve their
    /// first `BULK_FIRST_BATCH` tracks and return the rest as `remaining` URIs
    /// for a background loader.
    pub async fn resolve(&self, query: &str, _search_limit: u8) -> Result<ResolvedTracks, BotError> {
        let complete = |tracks: Vec<SpotifyTrack>| ResolvedTracks {
            tracks,
            remaining: Vec::new(),
            bulk: false,
        };

        if let Some(spotify_ref) = parse_spotify_ref(query) {
            return match spotify_ref {
                SpotifyRef::Track(id) => {
                    let uri_str = format!("spotify:track:{id}");
                    match SpotifyUri::from_uri(&uri_str) {
                        Ok(uri) => {
                            let track = self.get_track_meta(&uri).await?;
                            Ok(complete(vec![track]))
                        }
                        Err(_) => self.search_tracks(&id, 1).await.map(complete),
                    }
                }
                SpotifyRef::Album(id) => {
                    let uri_str = format!("spotify:album:{id}");
                    match SpotifyUri::from_uri(&uri_str) {
                        Ok(uri) => self.get_album_tracks_meta(&uri).await.map(complete),
                        Err(_) => Err(BotError::Playback(format!("Invalid album ID: {id}"))),
                    }
                }
                SpotifyRef::Playlist(id) => {
                    let uri_str = format!("spotify:playlist:{id}");
                    match SpotifyUri::from_uri(&uri_str) {
                        Ok(uri) => {
                            let uris = self.get_playlist_track_uris(&uri).await?;
                            self.split_and_fetch_first(uris).await
                        }
                        Err(_) => Err(BotError::Playback(format!("Invalid playlist ID: {id}"))),
                    }
                }
                SpotifyRef::Liked => {
                    let uris = self.get_liked_track_uris().await?;
                    self.split_and_fetch_first(uris).await
                }
            };
        }

        // Free-form search plays just the top hit (matching YouTube's
        // resolve); the `search` command is the multi-result picker.
        self.search_tracks(query, 1).await.map(complete)
    }
}


/// Build a `spotify:search:` context URI from free-form query text.
///
/// Words are joined with `+` (the separator spclient expects), and everything
/// outside URI-unreserved ASCII is percent-encoded — raw UTF-8 bytes (e.g.
/// Cyrillic) or reserved ASCII like `#`/`?` in the URI make spclient reject
/// the request with 400 Bad Request.
fn search_context_uri(query: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    // Encode all non-alphanumeric ASCII except the unreserved marks -_.~
    // (never need encoding). Literal '+' IS encoded so it can't be misread
    // as a word separator.
    const QUERY_SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    // split_whitespace: collapses runs and trims edges, so double spaces
    // can't produce empty words ("a++b") or a dangling separator.
    let encoded: Vec<String> = query
        .split_whitespace()
        .map(|word| utf8_percent_encode(word, QUERY_SET).to_string())
        .collect();
    format!("spotify:search:{}", encoded.join("+"))
}

#[cfg(test)]
mod tests {
    use super::search_context_uri;

    #[test]
    fn ascii_query_uses_plus_for_spaces() {
        assert_eq!(search_context_uri("hello world"), "spotify:search:hello+world");
    }

    #[test]
    fn repeated_and_edge_whitespace_collapses() {
        // Double spaces produced "a++b" and leading/trailing spaces a
        // dangling "+"; tabs weren't treated as separators at all.
        assert_eq!(search_context_uri("  hello   world "), "spotify:search:hello+world");
        assert_eq!(search_context_uri("hello\tworld"), "spotify:search:hello+world");
    }

    #[test]
    fn cyrillic_query_is_percent_encoded() {
        // Raw UTF-8 bytes in the URI made spclient reject Russian queries
        // with 400 Bad Request; they must be percent-encoded.
        assert_eq!(
            search_context_uri("кино"),
            "spotify:search:%D0%BA%D0%B8%D0%BD%D0%BE"
        );
    }

    #[test]
    fn mixed_query_encodes_non_ascii_words_and_keeps_plus_separators() {
        assert_eq!(
            search_context_uri("гр кино"),
            "spotify:search:%D0%B3%D1%80+%D0%BA%D0%B8%D0%BD%D0%BE"
        );
    }

    #[test]
    fn uri_breaking_ascii_is_encoded() {
        // '#', '?', '&', '/' and literal '+' would corrupt the URI or be
        // misread as a space separator.
        assert_eq!(search_context_uri("a#b"), "spotify:search:a%23b");
        assert_eq!(search_context_uri("a?b"), "spotify:search:a%3Fb");
        assert_eq!(search_context_uri("a+b"), "spotify:search:a%2Bb");
        assert_eq!(search_context_uri("ac/dc"), "spotify:search:ac%2Fdc");
    }

    #[test]
    fn unreserved_ascii_stays_readable() {
        assert_eq!(search_context_uri("a-b_c.d~e"), "spotify:search:a-b_c.d~e");
    }
}
