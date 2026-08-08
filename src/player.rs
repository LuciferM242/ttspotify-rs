//! Service-agnostic media player trait.
//!
//! Both Spotify (librespot-backed) and YouTube (rustypipe-backed in Phase 3)
//! implement this so the runner can drive either through one interface.
//! The audio sink is shared — both implementations push 44.1k stereo PCM
//! into the same `crossbeam_channel<Vec<i16>>` consumed by the audio
//! pipeline.

use crate::services::Service;

#[cfg_attr(test, mockall::automock)]
pub trait MediaPlayer: Send + Sync {
    /// Load and start playing the given service-specific URI.
    fn load(&self, uri: &str);

    /// Resume playback.
    fn play(&self);

    /// Pause playback (preserve position).
    fn pause(&self);

    /// Stop playback and release any held resources.
    fn stop(&self);

    /// Seek to absolute position in milliseconds.
    fn seek(&self, position_ms: u32);

    /// Hint the player to begin fetching the given URI in the background.
    /// Implementations may treat this as a no-op.
    fn preload(&self, uri: &str);
}

/// Pick the player that owns a track, by the service the track came from.
///
/// The runner holds one player per service and every per-track command has to
/// route to the right one; doing that inline meant the routing itself was
/// never tested. Sending a command to the wrong player is silent — the track
/// keeps playing and the command does nothing.
pub fn player_for<'a>(
    service: Service,
    spotify: &'a dyn MediaPlayer,
    youtube: &'a dyn MediaPlayer,
) -> &'a dyn MediaPlayer {
    match service {
        Service::Spotify => spotify,
        Service::YouTube => youtube,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_on_a_spotify_track_reaches_only_the_spotify_player() {
        let mut spotify = MockMediaPlayer::new();
        let mut youtube = MockMediaPlayer::new();
        spotify.expect_seek().times(1).withf(|ms| *ms == 30_000).return_const(());
        youtube.expect_seek().never();

        player_for(Service::Spotify, &spotify, &youtube).seek(30_000);
    }

    #[test]
    fn seek_on_a_youtube_track_reaches_only_the_youtube_player() {
        let mut spotify = MockMediaPlayer::new();
        let mut youtube = MockMediaPlayer::new();
        youtube.expect_seek().times(1).withf(|ms| *ms == 5_000).return_const(());
        spotify.expect_seek().never();

        player_for(Service::YouTube, &spotify, &youtube).seek(5_000);
    }

    #[test]
    fn preload_routes_by_service_too() {
        let mut spotify = MockMediaPlayer::new();
        let mut youtube = MockMediaPlayer::new();
        spotify.expect_preload().times(1)
            .withf(|uri| uri == "spotify:track:abc").return_const(());
        youtube.expect_preload().never();

        player_for(Service::Spotify, &spotify, &youtube).preload("spotify:track:abc");
    }
}
