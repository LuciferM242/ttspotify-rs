use std::sync::Arc;

use librespot_core::session::Session;
use librespot_core::spotify_uri::SpotifyUri;
use librespot_metadata::Metadata;
use parking_lot::Mutex;

use crate::error::BotError;
use crate::spotify::types::{PlaylistEntry, SpotifyRef, SpotifyTrack, parse_spotify_ref};

/// Metadata for the tracks to enqueue now, plus URIs still to be fetched by a
/// background loader (empty when the resolve was complete). `bulk` marks
/// collection sources (playlist / liked songs) so the runner deduplicates
/// against the queue even for a single track. `context` is the album or
/// playlist uri the tracks came from.
pub struct ResolvedTracks {
    pub tracks: Vec<SpotifyTrack>,
    pub remaining: Vec<SpotifyUri>,
    pub bulk: bool,
    pub context: Option<String>,
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

    /// Record that a track was played, so the cache keeps it.
    ///
    /// librespot bumps its own in-memory list when it reads a cached file, but
    /// that list is gone on restart and invisible to the other bots sharing the
    /// cache. Writing the time onto the file fixes both.
    ///
    /// Every file id the track lists is touched rather than working out which
    /// format librespot chose: only the cached one exists on disk, so the rest
    /// are misses, and guessing wrong would silently bump nothing.
    pub async fn mark_played(&self, uri: &SpotifyUri) {
        let session = self.session();
        let Some(cache) = session.cache().cloned() else {
            return;
        };
        let track = match librespot_metadata::Track::get(&session, uri).await {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!("cache: could not look up {uri} to mark it played: {e}");
                return;
            }
        };
        let mut touched = 0;
        for file_id in track.files.values() {
            if let Some(path) = cache.file_path(*file_id) {
                if crate::audio_cache::touch(&path).is_ok() {
                    touched += 1;
                }
            }
        }
        tracing::trace!("cache: marked {touched} file(s) played for {uri}");
    }

    // ---- helpers ----

    /// Fetch a Track from Spotify and convert to SpotifyTrack.
    async fn fetch_track(&self, uri: &SpotifyUri) -> Result<SpotifyTrack, BotError> {
        let track = librespot_metadata::Track::get(&self.session(), uri).await
            .map_err(|e| BotError::Playback(format!("Failed to fetch track metadata: {e}")))?;
        Ok(track_to_spotify(&track, uri))
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
        let uris: Vec<SpotifyUri> = album.tracks().cloned().collect();
        let mut tracks = self.fetch_tracks_meta(&uris).await;
        for t in tracks.iter_mut() {
            t.album = album_name.clone();
        }

        Ok(tracks)
    }

    /// Fetch metadata for every URI in one batched request per few hundred
    /// tracks (see `spotify::batch`), skipping unavailable ones with a
    /// warning. A whole-batch failure falls back to per-track lookups rather
    /// than losing the list: the batch endpoint failing outright is the odd
    /// case, and the slow path is only as slow as this function always was.
    pub async fn fetch_tracks_meta(&self, uris: &[SpotifyUri]) -> Vec<SpotifyTrack> {
        let uri_strings: Vec<String> = uris.iter().map(|u| u.to_uri()).collect();
        match crate::spotify::batch::tracks_for_uris(&self.session(), &uri_strings).await {
            Ok(found) => {
                for uri in found.missing.iter() {
                    tracing::warn!("Skipping {uri}: no metadata");
                }
                found.tracks
            }
            Err(e) => {
                tracing::warn!("Batched metadata failed ({e}); fetching per track");
                let mut tracks = Vec::with_capacity(uris.len());
                for uri in uris {
                    match self.fetch_track(uri).await {
                        Ok(t) => tracks.push(t),
                        Err(e) => tracing::warn!("Failed to fetch track {uri:?}: {e}"),
                    }
                }
                tracks
            }
        }
    }

    /// All track URIs of a playlist (metadata is fetched in batches later).
    pub async fn get_playlist_track_uris(&self, uri: &SpotifyUri) -> Result<Vec<SpotifyUri>, BotError> {
        let playlist = librespot_metadata::Playlist::get(&self.session(), uri).await
            .map_err(|e| BotError::Playback(format!("Failed to fetch playlist metadata: {e}")))?;
        Ok(playlist.tracks().cloned().collect())
    }

    /// URIs of the user's Liked Songs, most recently liked first.
    pub async fn get_liked_track_uris(&self) -> Result<Vec<SpotifyUri>, BotError> {
        let uris: Vec<SpotifyUri> = crate::spotify::collection::liked_track_uris(&self.session())
            .await?
            .iter()
            .filter_map(|uri| SpotifyUri::from_uri(uri).ok())
            .collect();
        if uris.is_empty() {
            return Err(BotError::NoResults);
        }
        Ok(uris)
    }

    /// Every playlist in the user's library, in library order.
    pub async fn get_user_playlists(&self) -> Result<Vec<PlaylistEntry>, BotError> {
        const PAGE: usize = 120;
        const MAX_PAGES: usize = 100;

        let session = self.session();
        let mut entries = Vec::new();
        let mut from = 0usize;
        for _ in 0..MAX_PAGES {
            let payload = session.spclient().get_rootlist(from, Some(PAGE)).await
                .map_err(|e| BotError::Playback(format!("Playlist library fetch failed: {e}")))?;
            let page = rootlist_page(&payload)?;
            entries.extend(page.entries);
            if !page.truncated || page.items == 0 {
                break;
            }
            from += page.items;
        }
        Ok(entries)
    }

    /// Fetch metadata for the first `BULK_FIRST_BATCH` URIs now; the rest are
    /// returned for a background loader.
    async fn split_and_fetch_first(
        &self,
        mut uris: Vec<SpotifyUri>,
        context: Option<String>,
    ) -> Result<ResolvedTracks, BotError> {
        let remaining = if uris.len() > BULK_FIRST_BATCH {
            uris.split_off(BULK_FIRST_BATCH)
        } else {
            Vec::new()
        };
        let tracks = self.fetch_tracks_meta(&uris).await;
        if tracks.is_empty() {
            return Err(BotError::NoResults);
        }
        Ok(ResolvedTracks { tracks, remaining, bulk: true, context })
    }

    /// Fetch radio recommendations using Spotify's radio-apollo endpoint.
    /// This is the same engine Spotify uses for autoplay/radio.
    ///
    /// `exclude_ids` — what has already been played or queued — is handed to
    /// Spotify itself as the station's history, so the server picks around it
    /// the way real autoplay does. Filtering only on our side threw away
    /// picks after the fact, and the server, never told what had played,
    /// recommended the same songs again batch after batch. The local filter
    /// stays as a backstop for whatever the server still repeats.
    pub async fn get_radio_tracks(
        &self,
        seed_track_uri: &SpotifyUri,
        limit: usize,
        exclude_ids: &[String],
    ) -> Result<Vec<SpotifyTrack>, BotError> {
        let uri_str = seed_track_uri.to_uri();
        let previous: Vec<librespot_core::SpotifyId> = exclude_ids
            .iter()
            .filter_map(|id| librespot_core::SpotifyId::from_base62(id).ok())
            .collect();

        // The "tracks" scope is the autoplay continuation: it answers tracks
        // directly and accepts the played history. The "stations" scope
        // ignores the history and wraps the same array in a station object.
        let response = self.session().spclient()
            .get_apollo_station("tracks", &uri_str, Some(limit), previous, true)
            .await
            .map_err(|e| BotError::Playback(format!("Radio fetch failed: {e}")))?;

        let json: serde_json::Value = serde_json::from_slice(&response)
            .map_err(|e| BotError::Playback(format!("Radio parse failed: {e}")))?;

        let uris: Vec<SpotifyUri> = station_track_uris(&json)
            .iter()
            .filter_map(|uri_text| SpotifyUri::from_uri(uri_text).ok())
            .filter(|uri| {
                let id = uri.to_id();
                !exclude_ids.iter().any(|eid| eid == &id)
            })
            .take(limit)
            .collect();

        if uris.is_empty() {
            return Err(BotError::NoResults);
        }

        let tracks = self.fetch_tracks_meta(&uris).await;
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

        // One batched lookup for the hits instead of one request per hit. A
        // couple of extras are taken so an unavailable track does not shrink
        // the result below the limit the caller asked for.
        let uris: Vec<SpotifyUri> = ctx
            .pages
            .iter()
            .flat_map(|page| page.tracks.iter())
            .filter_map(|track_ctx| SpotifyUri::from_uri(track_ctx.uri.as_deref()?).ok())
            .take(limit as usize + 3)
            .collect();

        let mut tracks = self.fetch_tracks_meta(&uris).await;
        tracks.truncate(limit as usize);

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
            context: None,
        };

        if let Some(spotify_ref) = parse_spotify_ref(query) {
            let context = radio_context(&spotify_ref);
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
                        Ok(uri) => self.get_album_tracks_meta(&uri).await.map(|tracks| ResolvedTracks {
                            context,
                            ..complete(tracks)
                        }),
                        Err(_) => Err(BotError::Playback(format!("Invalid album ID: {id}"))),
                    }
                }
                SpotifyRef::Playlist(id) => {
                    let uri_str = format!("spotify:playlist:{id}");
                    match SpotifyUri::from_uri(&uri_str) {
                        Ok(uri) => {
                            let uris = self.get_playlist_track_uris(&uri).await?;
                            self.split_and_fetch_first(uris, context).await
                        }
                        Err(_) => Err(BotError::Playback(format!("Invalid playlist ID: {id}"))),
                    }
                }
                SpotifyRef::Liked => self.liked().await,
            };
        }

        // Free-form search plays just the top hit (matching YouTube's
        // resolve); the `search` command is the multi-result picker.
        self.search_tracks(query, 1).await.map(complete)
    }

    /// The account's Liked Songs: the first batch now, the rest as `remaining`.
    pub async fn liked(&self) -> Result<ResolvedTracks, BotError> {
        let uris = self.get_liked_track_uris().await?;
        // No radio context: Spotify answers a Liked Songs seed with 400.
        self.split_and_fetch_first(uris, None).await
    }
}


/// The uri radio should seed from for tracks loaded through `spotify_ref`.
/// Liked Songs has none: Spotify answers a collection seed with 400.
fn radio_context(spotify_ref: &SpotifyRef) -> Option<String> {
    match spotify_ref {
        SpotifyRef::Album(id) => Some(format!("spotify:album:{id}")),
        SpotifyRef::Playlist(id) => Some(format!("spotify:playlist:{id}")),
        SpotifyRef::Track(_) | SpotifyRef::Liked => None,
    }
}

/// The playlists one rootlist page carries.
struct RootlistPage {
    entries: Vec<PlaylistEntry>,
    truncated: bool,
    /// Every row on the page, folder markers included; paging advances by this.
    items: usize,
}

/// A rootlist page asked for with `decorate=attributes,length`: `meta_items`
/// run parallel to `items` and carry each playlist's name and track count.
fn rootlist_page(payload: &[u8]) -> Result<RootlistPage, BotError> {
    use protobuf::Message;
    let root = librespot_protocol::playlist4_external::SelectedListContent::parse_from_bytes(payload)
        .map_err(|e| BotError::Playback(format!("Unreadable playlist library: {e}")))?;
    let contents = root.contents;

    let entries = contents
        .items
        .iter()
        .zip(contents.meta_items.iter())
        .filter(|(item, _)| item.uri().starts_with("spotify:playlist:"))
        .map(|(item, meta)| PlaylistEntry {
            uri: item.uri().to_string(),
            name: meta.attributes.name().to_string(),
            tracks: meta.length().max(0) as u32,
        })
        .collect();

    Ok(RootlistPage {
        entries,
        truncated: contents.truncated(),
        items: contents.items.len(),
    })
}

/// The track uris a station or autoplay reply carries.
///
/// The "tracks" scope answers a bare `{"tracks": ...}`, the "stations" scope
/// wraps the same array in a station object, and a seed-to-playlist reply
/// uses `mediaItems` instead. All three are read here so the caller does not
/// have to know which shape it asked for.
pub(crate) fn station_track_uris(json: &serde_json::Value) -> Vec<String> {
    json.get("tracks")
        .or_else(|| json.get("mediaItems"))
        .and_then(|items| items.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("uri").and_then(|uri| uri.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Convert a librespot Track + URI into our SpotifyTrack.
///
/// A free function rather than a method because `spotify::batch` performs the
/// same conversion on rows it decodes itself; one conversion means a batched
/// row and a single lookup can never disagree about metadata.
pub(crate) fn track_to_spotify(track: &librespot_metadata::Track, uri: &SpotifyUri) -> SpotifyTrack {
    SpotifyTrack {
        id: uri.to_id(),
        name: track.name.clone(),
        artists: track.artists.0.iter().map(|a| a.name.clone()).collect(),
        album: track.album.name.clone(),
        duration_ms: track.duration as u32,
        uri: uri.to_uri(),
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

    /// Live test, as below: the whole liked library must arrive.
    #[tokio::test]
    #[ignore = "hits Spotify; needs TTSPOTIFY_LIVE_CACHE with a login"]
    async fn the_liked_library_arrives_beyond_the_first_page() {
        use std::sync::Arc;

        let dir = std::env::var("TTSPOTIFY_LIVE_CACHE").expect("TTSPOTIFY_LIVE_CACHE");
        let dir = std::path::PathBuf::from(dir);
        let cache =
            librespot_core::cache::Cache::new(Some(dir.join("credentials")), None, None, None)
                .expect("cache");
        let credentials = cache.credentials().expect("a stored login");
        let session = librespot_core::session::Session::new(
            librespot_core::config::SessionConfig::default(),
            Some(cache),
        );
        session.connect(credentials, false).await.expect("connect");

        let metadata =
            super::SpotifyMetadata::new(Arc::new(parking_lot::Mutex::new(session.clone())));
        let uris = metadata.get_liked_track_uris().await.expect("liked");
        println!("liked library: {} tracks", uris.len());
        assert!(!uris.is_empty());
        session.shutdown();
    }

    /// Live test. Needs TTSPOTIFY_LIVE_CACHE pointing at a directory holding
    /// a completed librespot login in `credentials/`; run with --ignored.
    ///
    /// Exists because the radio moved from the "stations" scope to the
    /// "tracks" scope with a played history — the reply shape and the
    /// history handling are Spotify's, and only Spotify can confirm them.
    #[tokio::test]
    #[ignore = "hits Spotify; needs TTSPOTIFY_LIVE_CACHE with a login"]
    async fn radio_avoids_what_it_is_told_was_played() {
        use std::sync::Arc;

        let dir = std::env::var("TTSPOTIFY_LIVE_CACHE").expect("TTSPOTIFY_LIVE_CACHE");
        let dir = std::path::PathBuf::from(dir);
        let cache = librespot_core::cache::Cache::new(
            Some(dir.join("credentials")),
            None,
            None,
            None,
        )
        .expect("cache");
        let credentials = cache.credentials().expect("a stored login");
        let session = librespot_core::session::Session::new(
            librespot_core::config::SessionConfig::default(),
            Some(cache),
        );
        session.connect(credentials, false).await.expect("connect");

        let metadata =
            super::SpotifyMetadata::new(Arc::new(parking_lot::Mutex::new(session.clone())));
        let seed = librespot_core::spotify_uri::SpotifyUri::from_uri(
            "spotify:track:0DiWol3AO6WpXZgp0goxAV", // Around the World
        )
        .expect("seed uri");

        // First batch with no history.
        let first = metadata.get_radio_tracks(&seed, 5, &[]).await.expect("radio");
        assert!(!first.is_empty(), "the tracks scope answered nothing");

        // Second batch told the first was played: nothing may repeat.
        let played: Vec<String> = first.iter().map(|t| t.id.clone()).collect();
        let second = metadata
            .get_radio_tracks(&seed, 5, &played)
            .await
            .expect("radio continuation");
        assert!(!second.is_empty());
        for track in second.iter() {
            assert!(
                !played.contains(&track.id),
                "{} came back although it was reported played",
                track.name
            );
        }
        session.shutdown();
    }

    #[test]
    fn a_tracks_scope_reply_lists_its_tracks() {
        // What /radio-apollo/v3/tracks answers, trimmed.
        let reply = r#"{"tracks":[{"uri":"spotify:track:4WedBZTeFawYCBCgfj36iK","uid":"08c3"},
                                  {"uri":"spotify:track:54L0ET2WHVHKpEce8NYKLX","uid":"5bc9"}],
                        "next_page_url":"hm://radio-apollo/v3/tracks"}"#;
        let json: serde_json::Value = serde_json::from_str(reply).expect("json");
        assert_eq!(
            super::station_track_uris(&json),
            vec![
                "spotify:track:4WedBZTeFawYCBCgfj36iK".to_string(),
                "spotify:track:54L0ET2WHVHKpEce8NYKLX".to_string(),
            ]
        );
    }

    #[test]
    fn a_stations_scope_reply_lists_the_tracks_inside_the_station() {
        let reply = r#"{"uri":"spotify:station:track:54L0ET2WHVHKpEce8NYKLX","title":"orange",
                        "seeds":["spotify:track:54L0ET2WHVHKpEce8NYKLX"],
                        "tracks":[{"uri":"spotify:track:4WedBZTeFawYCBCgfj36iK"}]}"#;
        let json: serde_json::Value = serde_json::from_str(reply).expect("json");
        assert_eq!(
            super::station_track_uris(&json),
            vec!["spotify:track:4WedBZTeFawYCBCgfj36iK".to_string()]
        );
    }

    #[test]
    fn a_media_items_reply_lists_its_items() {
        let reply = r#"{"total":1,"mediaItems":[{"uri":"spotify:playlist:37i9dQZF1E8PVA1jdbapzL"}]}"#;
        let json: serde_json::Value = serde_json::from_str(reply).expect("json");
        assert_eq!(
            super::station_track_uris(&json),
            vec!["spotify:playlist:37i9dQZF1E8PVA1jdbapzL".to_string()]
        );
    }

    fn rootlist(rows: &[(&str, &str, i32)], truncated: bool) -> Vec<u8> {
        use librespot_protocol::playlist4_external::{
            Item, ListAttributes, ListItems, MetaItem, SelectedListContent,
        };
        use protobuf::{Message, MessageField};
        let mut contents = ListItems::new();
        contents.set_pos(0);
        contents.set_truncated(truncated);
        for (uri, name, length) in rows {
            let mut item = Item::new();
            item.set_uri(uri.to_string());
            contents.items.push(item);
            let mut attributes = ListAttributes::new();
            attributes.set_name(name.to_string());
            let mut meta = MetaItem::new();
            meta.attributes = MessageField::some(attributes);
            meta.set_length(*length);
            contents.meta_items.push(meta);
        }
        let mut root = SelectedListContent::new();
        root.contents = MessageField::some(contents);
        root.write_to_bytes().expect("serialise")
    }

    #[test]
    fn a_rootlist_page_names_its_playlists_and_counts() {
        let page = super::rootlist_page(&rootlist(
            &[
                ("spotify:playlist:652TD735fW0JesE9VgHhzS", "First", 12),
                ("spotify:playlist:5VA50pzLrODqcPjuTLK7YK", "Second", 300),
            ],
            false,
        ))
        .expect("parses");
        let names: Vec<(&str, u32)> = page.entries.iter().map(|e| (e.name.as_str(), e.tracks)).collect();
        assert_eq!(names, vec![("First", 12), ("Second", 300)]);
        assert_eq!(page.entries[0].uri, "spotify:playlist:652TD735fW0JesE9VgHhzS");
        assert!(!page.truncated);
    }

    #[test]
    fn folder_markers_are_not_playlists_but_count_for_paging() {
        let page = super::rootlist_page(&rootlist(
            &[
                ("spotify:start-group:abc:Rock", "", 0),
                ("spotify:playlist:652TD735fW0JesE9VgHhzS", "Inside", 7),
                ("spotify:end-group:abc", "", 0),
            ],
            true,
        ))
        .expect("parses");
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].name, "Inside");
        assert!(page.truncated);
        assert_eq!(page.items, 3);
    }

    #[test]
    fn an_empty_rootlist_is_an_empty_library() {
        let page = super::rootlist_page(&rootlist(&[], false)).expect("parses");
        assert!(page.entries.is_empty());
        assert_eq!(page.items, 0);
    }

    #[test]
    fn albums_and_playlists_are_radio_contexts_tracks_and_liked_are_not() {
        use crate::spotify::types::SpotifyRef;
        assert_eq!(
            super::radio_context(&SpotifyRef::Album("a".into())).as_deref(),
            Some("spotify:album:a")
        );
        assert_eq!(
            super::radio_context(&SpotifyRef::Playlist("p".into())).as_deref(),
            Some("spotify:playlist:p")
        );
        assert_eq!(super::radio_context(&SpotifyRef::Track("t".into())), None);
        assert_eq!(super::radio_context(&SpotifyRef::Liked), None);
    }

    #[test]
    fn a_reply_with_no_tracks_lists_nothing() {
        let json: serde_json::Value = serde_json::from_str(r#"{"correlation_id":"x"}"#).unwrap();
        assert!(super::station_track_uris(&json).is_empty());
    }

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
