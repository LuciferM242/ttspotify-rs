use std::sync::Arc;

use rustypipe::client::RustyPipe;
use rustypipe::param::StreamFilter;

use crate::error::BotError;
use crate::youtube::types::YouTubeTrack;

/// YouTube Music metadata service backed by rustypipe.
///
/// Configured with `no_botguard()` so it uses TV/embedded clients that
/// don't require PO tokens. No external `rustypipe-botguard` binary
/// needed.
pub struct YouTubeMetadata {
    client: Arc<RustyPipe>,
}

impl YouTubeMetadata {
    pub fn new() -> Result<Self, BotError> {
        let client = RustyPipe::builder()
            .no_botguard()
            .build()
            .map_err(|e| BotError::Playback(format!("rustypipe init failed: {e}")))?;
        Ok(Self { client: Arc::new(client) })
    }

    /// Search YouTube Music for tracks matching the query.
    /// Returns up to `limit` results (sliced from the first page).
    pub async fn search_tracks(&self, query: &str, limit: u8) -> Result<Vec<YouTubeTrack>, BotError> {
        let result = self.client.query()
            .music_search_tracks(query)
            .await
            .map_err(|e| BotError::Playback(format!("YouTube search failed: {e}")))?;

        let tracks: Vec<YouTubeTrack> = result.items.items
            .into_iter()
            .take(limit as usize)
            .map(track_item_to_track)
            .collect();

        if tracks.is_empty() {
            Err(BotError::NoResults)
        } else {
            Ok(tracks)
        }
    }

    /// Resolve a YouTube video ID to a direct AAC/M4A audio stream URL.
    /// Filters to AAC codec only — that's what symphonia decodes today.
    pub async fn get_audio_url(&self, video_id: &str) -> Result<String, BotError> {
        let player = self.client.query()
            .player(video_id)
            .await
            .map_err(|e| BotError::Playback(format!("YouTube player fetch failed: {e}")))?;

        let filter = StreamFilter::default()
            .audio_codecs(vec![rustypipe::model::AudioCodec::Mp4a]);

        let stream = player.select_audio_stream(&filter)
            .ok_or_else(|| BotError::Playback("No AAC audio stream available".to_string()))?;

        Ok(stream.url.clone())
    }
}

fn track_item_to_track(item: rustypipe::model::TrackItem) -> YouTubeTrack {
    YouTubeTrack {
        id: item.id,
        name: item.name,
        artists: item.artists.into_iter().map(|a| a.name).collect(),
        album: item.album.map(|a| a.name).unwrap_or_default(),
        duration_ms: item.duration.unwrap_or(0).saturating_mul(1000),
    }
}
