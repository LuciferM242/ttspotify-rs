use std::sync::Arc;

use crossbeam_channel::Sender;
use librespot_core::session::Session;
use librespot_core::spotify_uri::SpotifyUri;
use librespot_playback::config::{Bitrate, NormalisationMethod, NormalisationType, PlayerConfig};
use librespot_playback::mixer::{NoOpVolume, VolumeGetter};
use librespot_playback::player::{Player, PlayerEventChannel};
use parking_lot::Mutex;

use crate::config::BotConfig;
use crate::player::MediaPlayer;
use crate::spotify::sink::TeamTalkSink;

/// Build a fresh librespot `Player` bound to `session`, returning it alongside
/// its event channel. Shared by `new()` and `rebuild()`.
fn build_player(
    session: Session,
    config: &BotConfig,
    audio_tx: Sender<Vec<i16>>,
) -> (Arc<Player>, PlayerEventChannel) {
    let player_config = PlayerConfig {
        bitrate: parse_bitrate(&config.spotify_quality),
        gapless: true,
        normalisation: config.spotify_enable_normalization,
        normalisation_type: parse_norm_type(&config.normalisation_type),
        normalisation_method: parse_norm_method(&config.normalisation_method),
        normalisation_pregain_db: config.normalisation_pregain_db,
        normalisation_threshold_dbfs: config.normalisation_threshold_dbfs,
        normalisation_knee_db: config.normalisation_knee_db,
        position_update_interval: Some(std::time::Duration::from_secs(1)),
        ..Default::default()
    };

    let player = Player::new(
        player_config,
        session,
        Box::new(NoOpVolume) as Box<dyn VolumeGetter + Send>,
        move || -> Box<dyn librespot_playback::audio_backend::Sink> {
            Box::new(TeamTalkSink::new(audio_tx.clone()))
        },
    );

    let event_rx = player.get_player_event_channel();
    (player, event_rx)
}

/// Wrapper around a librespot `Player` whose inner player can be hot-swapped.
///
/// librespot's `Session` is single-use — once its connection dies it cannot be
/// reconnected — so recovering a dead session means building a NEW session and a
/// NEW `Player` from it. The inner `Player` sits behind a mutex so `rebuild()`
/// can swap it in place, leaving every call site (and clones of this wrapper)
/// pointing at the live player. Cheap to clone (shared `Arc`).
#[derive(Clone)]
pub struct SpotifyPlayer {
    player: Arc<Mutex<Arc<Player>>>,
}

impl SpotifyPlayer {
    pub fn new(
        session: Session,
        config: &BotConfig,
        audio_tx: Sender<Vec<i16>>,
    ) -> (Self, PlayerEventChannel) {
        let (player, event_rx) = build_player(session, config, audio_tx);
        (
            Self {
                player: Arc::new(Mutex::new(player)),
            },
            event_rx,
        )
    }

    /// Replace the inner player with one bound to a freshly-rebuilt `session`,
    /// returning the new player's event channel (the caller must restart the
    /// player event loop with it). The old player is dropped, which closes its
    /// event channel and ends the old event loop.
    pub fn rebuild(
        &self,
        session: Session,
        config: &BotConfig,
        audio_tx: Sender<Vec<i16>>,
    ) -> PlayerEventChannel {
        let (player, event_rx) = build_player(session, config, audio_tx);
        *self.player.lock() = player;
        event_rx
    }

    /// Snapshot the current inner player (cheap `Arc` clone) for delegation.
    fn inner(&self) -> Arc<Player> {
        self.player.lock().clone()
    }

    pub fn load_track(&self, uri: &SpotifyUri) {
        self.inner().load(uri.clone(), true, 0);
    }

    /// Load and start a track at a specific position (ms). Used to resume the
    /// interrupted track after a session recovery.
    pub fn load_track_at(&self, uri: &SpotifyUri, position_ms: u32) {
        self.inner().load(uri.clone(), true, position_ms);
    }

    pub fn play(&self) {
        self.inner().play();
    }

    pub fn pause(&self) {
        self.inner().pause();
    }

    pub fn stop(&self) {
        self.inner().stop();
    }

    pub fn seek(&self, position_ms: u32) {
        self.inner().seek(position_ms);
    }

    pub fn preload(&self, uri: &SpotifyUri) {
        self.inner().preload(uri.clone());
    }
}

impl MediaPlayer for SpotifyPlayer {
    fn load(&self, uri: &str) {
        match SpotifyUri::from_uri(uri) {
            Ok(parsed) => self.inner().load(parsed, true, 0),
            Err(e) => tracing::error!("SpotifyPlayer::load: invalid URI {uri}: {e}"),
        }
    }

    fn play(&self) {
        self.inner().play();
    }

    fn pause(&self) {
        self.inner().pause();
    }

    fn stop(&self) {
        self.inner().stop();
    }

    fn seek(&self, position_ms: u32) {
        self.inner().seek(position_ms);
    }

    fn preload(&self, uri: &str) {
        match SpotifyUri::from_uri(uri) {
            Ok(parsed) => self.inner().preload(parsed),
            Err(e) => tracing::warn!("SpotifyPlayer::preload: invalid URI {uri}: {e}"),
        }
    }
}

fn parse_bitrate(quality: &str) -> Bitrate {
    match quality.to_uppercase().as_str() {
        "VERY_HIGH" | "320" => Bitrate::Bitrate320,
        "HIGH" | "160" => Bitrate::Bitrate160,
        "NORMAL" | "LOW" | "96" => Bitrate::Bitrate96,
        _ => Bitrate::Bitrate320,
    }
}

fn parse_norm_type(t: &str) -> NormalisationType {
    match t.to_lowercase().as_str() {
        "album" => NormalisationType::Album,
        "track" => NormalisationType::Track,
        _ => NormalisationType::Auto,
    }
}

fn parse_norm_method(m: &str) -> NormalisationMethod {
    match m.to_lowercase().as_str() {
        "basic" => NormalisationMethod::Basic,
        _ => NormalisationMethod::Dynamic,
    }
}
