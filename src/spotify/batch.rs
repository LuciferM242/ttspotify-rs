//! Metadata for many tracks in one request.
//!
//! `librespot_metadata::Track::get` asks `/extended-metadata` for one track,
//! so a playlist costs one HTTP request per track — a few thousand liked
//! songs is a few thousand requests, which Spotify rate limits long before it
//! finishes. The endpoint itself is batched: `entity_request` is a repeated
//! field, and several hundred tracks come back in a single round trip.
//!
//! Two things about the endpoint shape drive the code here:
//!
//! - It answers **one row per unique uri**. A playlist holding the same track
//!   twice gets one row back, so the answer is keyed by uri and the caller's
//!   list is rebuilt from it — a playlist with repeats must keep its length.
//! - A uri it cannot serve is **absent**, not flagged. The missing ones are
//!   found by comparing what came back against what was asked for.

use std::collections::HashMap;

use librespot_core::session::Session;
use librespot_core::spotify_uri::SpotifyUri;
use librespot_metadata::Metadata;
use librespot_protocol::extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery};
use librespot_protocol::extension_kind::ExtensionKind;
use protobuf::{EnumOrUnknown, Message};

use crate::error::BotError;
use crate::spotify::metadata::track_to_spotify;
use crate::spotify::types::SpotifyTrack;

/// How many uris go in one request. Several hundred in a single request is
/// known to work; a ceiling surely exists and an oversized request would fail
/// the whole batch rather than one track, so this keeps a margin.
pub const BATCH_SIZE: usize = 500;

/// What a batched lookup answered: one entry per uri asked for, in that
/// order, repeats included; `missing` is the uris that came back with nothing.
#[derive(Debug, Default)]
pub struct BatchedTracks {
    pub tracks: Vec<SpotifyTrack>,
    pub missing: Vec<String>,
}

/// The unique uris of `uris`, in first-seen order. The endpoint answers one
/// row per unique uri, so asking twice wastes request budget.
pub fn unique_in_order(uris: &[String]) -> Vec<String> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::with_capacity(uris.len());
    let mut unique = Vec::with_capacity(uris.len());
    for uri in uris {
        if seen.insert(uri.as_str()) {
            unique.push(uri.clone());
        }
    }
    unique
}

/// Rebuild the caller's list from a uri-keyed answer, keeping order and
/// repeats. Uris with no row are collected as `missing` (each once).
pub fn expand(uris: &[String], found: &HashMap<String, SpotifyTrack>) -> BatchedTracks {
    let mut tracks = Vec::with_capacity(uris.len());
    let mut missing = Vec::new();
    for uri in uris {
        match found.get(uri) {
            Some(meta) => tracks.push(meta.clone()),
            None => {
                if !missing.contains(uri) {
                    missing.push(uri.clone());
                }
            }
        }
    }
    BatchedTracks { tracks, missing }
}

/// One request's worth of metadata, keyed by the uri it answers for.
async fn fetch_chunk(
    session: &Session,
    uris: &[String],
) -> Result<HashMap<String, SpotifyTrack>, BotError> {
    let request = BatchedEntityRequest {
        entity_request: uris
            .iter()
            .map(|uri| EntityRequest {
                entity_uri: uri.clone(),
                query: vec![ExtensionQuery {
                    extension_kind: EnumOrUnknown::new(ExtensionKind::TRACK_V4),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };

    let reply = session
        .spclient()
        .get_extended_metadata(request)
        .await
        .map_err(|e| BotError::Playback(format!("Batched metadata fetch failed: {e}")))?;

    let mut found = HashMap::with_capacity(uris.len());
    for array in reply.extended_metadata.iter() {
        for data in array.extension_data.iter() {
            // A row that is not a 200 carries no usable payload. It is left
            // out so the uri lands in `missing` and is reported.
            if data.header.as_ref().map(|h| h.status_code).unwrap_or(0) != 200 {
                tracing::debug!("metadata row {} answered {:?}", data.entity_uri, data.header);
                continue;
            }
            let Some(payload) = data.extension_data.as_ref() else {
                continue;
            };
            let Ok(message) =
                librespot_protocol::metadata::Track::parse_from_bytes(&payload.value)
            else {
                tracing::warn!("undecodable metadata for {}", data.entity_uri);
                continue;
            };
            let Ok(uri) = SpotifyUri::from_uri(&data.entity_uri) else {
                continue;
            };
            // `Track::parse` is the same conversion `Track::get` performs, so
            // a batched row and a single lookup produce identical metadata.
            match librespot_metadata::Track::parse(&message, &uri) {
                Ok(track) => {
                    found.insert(data.entity_uri.clone(), track_to_spotify(&track, &uri));
                }
                Err(e) => tracing::warn!("unreadable metadata for {}: {e}", data.entity_uri),
            }
        }
    }
    Ok(found)
}

/// Metadata for every uri, in the order given, repeats kept.
///
/// The uris are deduplicated for the request and the answer is expanded back
/// over the original list. A whole-batch failure is an error — that is the
/// network being unusable, not a track being unavailable — while individual
/// uris with no row come back in `missing`.
pub async fn tracks_for_uris(session: &Session, uris: &[String]) -> Result<BatchedTracks, BotError> {
    if uris.is_empty() {
        return Ok(BatchedTracks::default());
    }

    let unique = unique_in_order(uris);
    let mut found: HashMap<String, SpotifyTrack> = HashMap::with_capacity(unique.len());
    for chunk in unique.chunks(BATCH_SIZE) {
        found.extend(fetch_chunk(session, chunk).await?);
    }
    Ok(expand(uris, &found))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str) -> SpotifyTrack {
        SpotifyTrack {
            id: id.to_string(),
            name: id.to_string(),
            artists: Vec::new(),
            album: String::new(),
            duration_ms: 0,
            uri: format!("spotify:track:{id}"),
        }
    }

    fn found(ids: &[&str]) -> HashMap<String, SpotifyTrack> {
        ids.iter()
            .map(|id| (format!("spotify:track:{id}"), meta(id)))
            .collect()
    }

    fn uris(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| format!("spotify:track:{id}")).collect()
    }

    #[test]
    fn a_uri_asked_for_twice_is_only_requested_once() {
        assert_eq!(unique_in_order(&uris(&["a", "b", "a"])), uris(&["a", "b"]));
    }

    #[test]
    fn unique_keeps_first_seen_order() {
        assert_eq!(
            unique_in_order(&uris(&["c", "a", "c", "b", "a"])),
            uris(&["c", "a", "b"])
        );
    }

    #[test]
    fn a_playlist_that_repeats_a_track_keeps_both_entries() {
        // The endpoint answers one row per unique uri. A 3-entry playlist
        // holding the same track twice is still a 3-entry playlist.
        let expanded = expand(&uris(&["a", "b", "a"]), &found(&["a", "b"]));
        let ids: Vec<&str> = expanded.tracks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "a"]);
        assert!(expanded.missing.is_empty());
    }

    #[test]
    fn the_order_asked_for_is_the_order_returned() {
        let expanded = expand(&uris(&["c", "a", "b"]), &found(&["a", "b", "c"]));
        let ids: Vec<&str> = expanded.tracks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "a", "b"]);
    }

    #[test]
    fn a_uri_with_no_row_is_reported_rather_than_dropped() {
        // The endpoint omits what it cannot serve. Saying nothing about it is
        // how a short playlist passes for a complete one.
        let expanded = expand(&uris(&["a", "gone", "b"]), &found(&["a", "b"]));
        assert_eq!(expanded.tracks.len(), 2);
        assert_eq!(expanded.missing, uris(&["gone"]));
    }

    #[test]
    fn a_missing_uri_repeated_is_reported_once() {
        let expanded = expand(&uris(&["gone", "gone"]), &found(&[]));
        assert!(expanded.tracks.is_empty());
        assert_eq!(expanded.missing.len(), 1);
    }

    #[test]
    fn nothing_asked_for_is_nothing_missing() {
        let expanded = expand(&[], &found(&["a"]));
        assert!(expanded.tracks.is_empty());
        assert!(expanded.missing.is_empty());
    }

    /// Live test. Needs a directory holding a completed librespot login in
    /// `credentials/` — set TTSPOTIFY_LIVE_CACHE to it and run with
    /// --ignored.
    #[tokio::test]
    #[ignore = "hits Spotify; needs TTSPOTIFY_LIVE_CACHE with a login"]
    async fn a_batch_answers_real_metadata_in_one_request() {
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
        let session =
            Session::new(librespot_core::config::SessionConfig::default(), Some(cache));
        session.connect(credentials, false).await.expect("connect");

        // Three well-known tracks plus a repeat and a malformed id: the
        // repeat must come back twice, the junk must land in `missing`.
        let uris = vec![
            "spotify:track:6rqhFgbbKwnb9MLmUQDhG6".to_string(), // Bohemian Rhapsody
            "spotify:track:0DiWol3AO6WpXZgp0goxAV".to_string(), // Around the World
            "spotify:track:6rqhFgbbKwnb9MLmUQDhG6".to_string(),
            "spotify:track:0000000000000000000000".to_string(),
        ];
        let answer = tracks_for_uris(&session, &uris).await.expect("batch");
        assert_eq!(answer.tracks.len(), 3, "repeat kept, junk missing");
        assert_eq!(answer.tracks[0].id, answer.tracks[2].id);
        assert!(!answer.tracks[0].name.is_empty());
        assert!(!answer.tracks[0].artists.is_empty());
        assert!(answer.tracks[0].duration_ms > 0);
        assert_eq!(answer.missing.len(), 1);
        session.shutdown();
    }
}
