use librespot_core::session::Session;
use librespot_core::spotify_uri::SpotifyUri;
use librespot_metadata::Metadata;

use crate::error::BotError;
use crate::spotify::types::{SpotifyRef, SpotifyTrack, parse_spotify_ref};

pub struct SpotifyMetadata {
    session: Session,
}

impl SpotifyMetadata {
    pub fn new(session: Session) -> Self {
        Self { session }
    }

    pub fn session(&self) -> &Session {
        &self.session
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
        let track = librespot_metadata::Track::get(&self.session, uri).await
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
        let album = librespot_metadata::Album::get(&self.session, uri).await
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

    /// Fetch all tracks from a playlist via librespot-metadata.
    pub async fn get_playlist_tracks_meta(&self, uri: &SpotifyUri) -> Result<Vec<SpotifyTrack>, BotError> {
        let playlist = librespot_metadata::Playlist::get(&self.session, uri).await
            .map_err(|e| BotError::Playback(format!("Failed to fetch playlist metadata: {e}")))?;

        let mut tracks = Vec::new();

        for track_uri in playlist.tracks() {
            match self.fetch_track(track_uri).await {
                Ok(t) => tracks.push(t),
                Err(e) => tracing::warn!("Failed to fetch track {track_uri:?}: {e}"),
            }
        }

        Ok(tracks)
    }

    /// Fetch artist info including related artists and top tracks.
    pub async fn get_artist_meta(&self, uri: &SpotifyUri) -> Result<ArtistInfo, BotError> {
        let artist = librespot_metadata::Artist::get(&self.session, uri).await
            .map_err(|e| BotError::Playback(format!("Failed to fetch artist metadata: {e}")))?;

        let related: Vec<String> = artist.related.0.iter()
            .map(|a| a.name.clone())
            .collect();

        // Get top tracks for all countries (take first available)
        let top_track_uris: Vec<SpotifyUri> = artist.top_tracks.0.iter()
            .flat_map(|tt| tt.tracks.0.iter().cloned())
            .collect();

        Ok(ArtistInfo {
            name: artist.name.clone(),
            related_names: related,
            top_track_uris,
            related_artists: artist.related.0.iter()
                .map(|a| a.id.clone())
                .collect(),
        })
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

        let response = self.session.spclient()
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
        let search_uri = format!("spotify:search:{}", query.replace(' ', "+"));
        let ctx = self.session.spclient().get_context(&search_uri).await
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

    /// Resolve any query (search text, URL, URI) to a list of tracks.
    /// Uses librespot-metadata for URIs (faster), Web API for search.
    pub async fn resolve(&self, query: &str, search_limit: u8) -> Result<Vec<SpotifyTrack>, BotError> {
        if let Some(spotify_ref) = parse_spotify_ref(query) {
            return match spotify_ref {
                SpotifyRef::Track(id) => {
                    let uri_str = format!("spotify:track:{id}");
                    match SpotifyUri::from_uri(&uri_str) {
                        Ok(uri) => {
                            let track = self.get_track_meta(&uri).await?;
                            Ok(vec![track])
                        }
                        Err(_) => self.search_tracks(&id, 1).await,
                    }
                }
                SpotifyRef::Album(id) => {
                    let uri_str = format!("spotify:album:{id}");
                    match SpotifyUri::from_uri(&uri_str) {
                        Ok(uri) => self.get_album_tracks_meta(&uri).await,
                        Err(_) => Err(BotError::Playback(format!("Invalid album ID: {id}"))),
                    }
                }
                SpotifyRef::Playlist(id) => {
                    let uri_str = format!("spotify:playlist:{id}");
                    match SpotifyUri::from_uri(&uri_str) {
                        Ok(uri) => self.get_playlist_tracks_meta(&uri).await,
                        Err(_) => Err(BotError::Playback(format!("Invalid playlist ID: {id}"))),
                    }
                }
            };
        }

        // Plain text search via Web API
        self.search_tracks(query, search_limit).await
    }
}

/// Artist info returned by get_artist_meta
pub struct ArtistInfo {
    pub name: String,
    pub related_names: Vec<String>,
    pub top_track_uris: Vec<SpotifyUri>,
    pub related_artists: Vec<SpotifyUri>,
}
