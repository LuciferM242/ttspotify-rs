use std::sync::Arc;

use crossbeam_channel::Sender;
use librespot_core::session::Session;
use librespot_core::spotify_uri::SpotifyUri;
use librespot_playback::config::{Bitrate, NormalisationMethod, NormalisationType, PlayerConfig};
use librespot_playback::mixer::{NoOpVolume, VolumeGetter};
use librespot_playback::player::{Player, PlayerEventChannel};

use crate::config::BotConfig;
use crate::spotify::sink::TeamTalkSink;

pub struct SpotifyPlayer {
    player: Arc<Player>,
}

impl SpotifyPlayer {
    pub fn new(
        session: Session,
        config: &BotConfig,
        audio_tx: Sender<Vec<i16>>,
    ) -> (Self, PlayerEventChannel) {
        let player_config = PlayerConfig {
            bitrate: parse_bitrate(&config.spotify_quality),
            gapless: true,
            normalisation: config.spotify_enable_normalization,
            normalisation_type: parse_norm_type(&config.normalisation_type),
            normalisation_method: parse_norm_method(&config.normalisation_method),
            normalisation_pregain_db: config.normalisation_pregain_db,
            normalisation_threshold_dbfs: config.normalisation_threshold_dbfs,
            normalisation_knee_db: config.normalisation_knee_db,
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

        (Self { player }, event_rx)
    }

    pub fn load_track(&self, uri: &SpotifyUri) {
        self.player.load(uri.clone(), true, 0);
    }

    pub fn play(&self) {
        self.player.play();
    }

    pub fn pause(&self) {
        self.player.pause();
    }

    pub fn stop(&self) {
        self.player.stop();
    }

    pub fn seek(&self, position_ms: u32) {
        self.player.seek(position_ms);
    }

    pub fn preload(&self, uri: &SpotifyUri) {
        self.player.preload(uri.clone());
    }

    pub fn inner(&self) -> &Arc<Player> {
        &self.player
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
