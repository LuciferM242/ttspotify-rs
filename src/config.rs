use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::error::BotError;

/// Parse a gender string into a TeamTalk UserGender.
/// Accepts: male/m/man, female/f/woman, neutral/n/nb (and anything else defaults to Neutral).
pub fn parse_gender(s: &str) -> ::teamtalk::types::UserGender {
    match s.to_lowercase().as_str() {
        "male" | "m" | "man" => ::teamtalk::types::UserGender::Male,
        "female" | "f" | "woman" => ::teamtalk::types::UserGender::Female,
        _ => ::teamtalk::types::UserGender::Neutral,
    }
}

/// Platform-aware config directory.
/// Linux/macOS: ~/.config/ttspotify/
/// Windows: data/ (next to the executable)
pub fn config_dir() -> PathBuf {
    if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ttspotify")
    } else {
        PathBuf::from("data")
    }
}

/// List config files in the config directory, skipping non-bot files.
pub fn list_configs() -> Vec<(String, PathBuf)> {
    let skip = ["credentials", "cookies", "sessions"];
    let dir = config_dir();
    if !dir.exists() {
        return Vec::new();
    }
    let mut configs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if !skip.contains(&stem) {
                        configs.push((stem.to_string(), path));
                    }
                }
            }
        }
    }
    configs.sort_by(|a, b| a.0.cmp(&b.0));
    configs
}

fn default_radio_delay() -> f32 { 10.0 }
fn default_norm_type() -> String { "auto".to_string() }
fn default_norm_method() -> String { "dynamic".to_string() }
fn default_norm_pregain() -> f64 { 0.0 }
fn default_norm_threshold() -> f64 { -2.0 }
fn default_norm_knee() -> f64 { 5.0 }

/// Config format matches the Python ttspotify bot's data/config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BotConfig {
    // TeamTalk connection
    pub host: String,
    #[serde(rename = "tcpPort")]
    pub tcp_port: i32,
    #[serde(rename = "udpPort")]
    pub udp_port: i32,
    #[serde(default)]
    pub encrypted: bool,
    #[serde(rename = "botName")]
    pub bot_name: String,
    pub username: String,
    pub password: String,
    #[serde(rename = "ChannelName")]
    pub channel_name: String,
    #[serde(rename = "ChannelPassword")]
    pub channel_password: String,
    #[serde(rename = "botGender")]
    pub bot_gender: String,

    // Spotify
    #[serde(rename = "spotifyQuality")]
    pub spotify_quality: String,
    #[serde(rename = "spotifyEnableNormalization")]
    pub spotify_enable_normalization: bool,
    #[serde(rename = "spotifyNormalisationType", default = "default_norm_type")]
    pub normalisation_type: String,
    #[serde(rename = "spotifyNormalisationMethod", default = "default_norm_method")]
    pub normalisation_method: String,
    #[serde(rename = "spotifyNormalisationPregainDb", default = "default_norm_pregain")]
    pub normalisation_pregain_db: f64,
    #[serde(rename = "spotifyNormalisationThresholdDbfs", default = "default_norm_threshold")]
    pub normalisation_threshold_dbfs: f64,
    #[serde(rename = "spotifyNormalisationKneeDb", default = "default_norm_knee")]
    pub normalisation_knee_db: f64,

    // Audio
    pub volume: u8,
    #[serde(rename = "spotifyMaxVolume")]
    pub max_volume: u8,
    #[serde(rename = "spotifyJitterBufferSizeMs")]
    pub jitter_buffer_ms: u32,
    #[serde(rename = "spotifyVolumeRampStep")]
    pub volume_ramp_step: f32,

    // Radio/recommendations
    #[serde(rename = "spotifyRadio")]
    pub radio_enabled: bool,
    #[serde(rename = "spotifyRadioBatch")]
    pub radio_batch_size: u8,
    #[serde(rename = "spotifyRadioDelay", default = "default_radio_delay")]
    pub radio_delay: f32,

    // Search
    #[serde(rename = "spotifySearchLimit")]
    pub search_limit: u8,
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            tcp_port: 10333,
            udp_port: 10333,
            encrypted: false,
            bot_name: "Spotify".to_string(),
            username: String::new(),
            password: String::new(),
            channel_name: "/".to_string(),
            channel_password: String::new(),
            bot_gender: "neutral".to_string(),

            spotify_quality: "VERY_HIGH".to_string(),
            spotify_enable_normalization: true,
            normalisation_type: "auto".to_string(),
            normalisation_method: "dynamic".to_string(),
            normalisation_pregain_db: 0.0,
            normalisation_threshold_dbfs: -2.0,
            normalisation_knee_db: 5.0,

            volume: 50,
            max_volume: 70,
            jitter_buffer_ms: 400,
            volume_ramp_step: 0.03,

            radio_enabled: false,
            radio_batch_size: 3,
            radio_delay: 10.0,

            search_limit: 5,
        }
    }
}

impl BotConfig {
    pub fn load(path: &str) -> Result<Self, BotError> {
        let path = Path::new(path);
        if !path.exists() {
            eprintln!("Config file not found: {}", path.display());
            eprint!("Would you like to run the setup wizard? [y/N] ");
            use std::io::Write;
            std::io::stderr().flush().ok();
            let mut input = String::new();
            if std::io::stdin().read_line(&mut input).is_ok()
                && matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
            {
                crate::wizard::run_wizard(None)?;
                // Re-check if a config was created in the default config dir
                let configs = list_configs();
                if let Some((_, created_path)) = configs.first() {
                    let contents = std::fs::read_to_string(created_path)
                        .map_err(|e| BotError::Config(format!("Failed to read config: {e}")))?;
                    let config: Self = serde_json::from_str(&contents)
                        .map_err(|e| BotError::Config(format!("Failed to parse config: {e}")))?;
                    return Ok(config);
                }
            }
            return Err(BotError::Config(format!(
                "Config not found: {}\nRun: tt-spotify-bot --setup",
                path.display()
            )));
        }
        let contents = std::fs::read_to_string(path)
            .map_err(|e| BotError::Config(format!("Failed to read config: {e}")))?;
        let config: Self = serde_json::from_str(&contents)
            .map_err(|e| BotError::Config(format!("Failed to parse config: {e}")))?;
        Ok(config)
    }

    /// Load config, apply a mutation, and save it back.
    pub fn update(path: &str, f: impl FnOnce(&mut BotConfig)) {
        if let Ok(mut cfg) = Self::load(path) {
            f(&mut cfg);
            if let Err(e) = cfg.save(Path::new(path)) {
                tracing::error!("Failed to save config: {e}");
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), BotError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| BotError::Config(format!("Failed to serialize config: {e}")))?;
        std::fs::write(path, json)?;
        Ok(())
    }
}
