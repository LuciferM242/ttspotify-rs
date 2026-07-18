use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::error::BotError;
use crate::services::Service;

/// Check whether a string is a recognised gender alias.
pub fn is_valid_gender(s: &str) -> bool {
    matches!(
        s.to_lowercase().as_str(),
        "male" | "m" | "man" | "female" | "f" | "woman" | "neutral" | "n" | "nb"
    )
}

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
/// Windows: `data/` next to the executable (not the current working directory),
/// so launching from a shortcut/autostart with a different working dir still
/// finds the right config. Falls back to `<cwd>/data` only if that's where an
/// existing install already lives, keeping older setups working.
pub fn config_dir() -> PathBuf {
    if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ttspotify")
    } else {
        let exe_data = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.join("data")));
        match exe_data {
            Some(exe_data) => {
                if exe_data.exists() {
                    exe_data
                } else {
                    let cwd_data = PathBuf::from("data");
                    if cwd_data.exists() {
                        tracing::warn!(
                            "Using config dir {} (cwd) — consider moving it next to the executable",
                            cwd_data.display()
                        );
                        cwd_data
                    } else {
                        exe_data
                    }
                }
            }
            None => PathBuf::from("data"),
        }
    }
}

/// List config files in the config directory, skipping non-bot files.
pub fn list_configs() -> Vec<(String, PathBuf)> {
    // Non-bot JSON files that share the config directory. "settings" is the
    // app-global settings.json (update-check toggle); the rest are auth/session
    // artifacts. None are server configs, so they must never appear as bots.
    let skip = ["credentials", "cookies", "sessions", "settings"];
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    // TeamTalk license (optional, overridden by compile-time TT_LICENSE_NAME/TT_LICENSE_KEY)
    #[serde(default, rename = "licenseName", skip_serializing_if = "Option::is_none")]
    pub license_name: Option<String>,
    #[serde(default, rename = "licenseKey", skip_serializing_if = "Option::is_none")]
    pub license_key: Option<String>,

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

    // Playback modes (persisted across restarts)
    #[serde(default, rename = "repeatTrack")]
    pub repeat_track: bool,
    #[serde(default, rename = "repeatQueue")]
    pub repeat_queue: bool,
    #[serde(default)]
    pub shuffle: bool,

    // Service that the bot starts on and that bare commands (p, search) target.
    #[serde(default, rename = "defaultService")]
    pub default_service: Service,

    // YouTube: path to a Netscape-format cookies file (optional).
    // Empty = check for `<config_dir>/cookies.txt`; if neither set nor
    // present, yt-dlp runs cookie-less and relies on bgutil-pot only.
    // Helps avoid 403s on rate-limited or age-restricted videos.
    #[serde(default, rename = "youtubeCookiesFile")]
    pub youtube_cookies_file: String,
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
            license_name: None,
            license_key: None,

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

            repeat_track: false,
            repeat_queue: false,
            shuffle: false,
            default_service: Service::default(),
            youtube_cookies_file: String::new(),
        }
    }
}

impl BotConfig {
    /// Read and parse a config file. No wizard prompt, no validation — pure I/O
    /// plus deserialization. Safe to call from async/background contexts (never
    /// blocks on stdin).
    pub(crate) fn parse_file(path: &Path) -> Result<Self, BotError> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| BotError::Config(format!("Failed to read {}: {e}", path.display())))?;
        serde_json::from_str(&contents)
            .map_err(|e| BotError::Config(format!("Failed to parse {}: {e}", path.display())))
    }

    /// Load and validate a config without any interactive prompt. Fails if the
    /// file is missing. Use this from any runtime/background path.
    pub fn load_noninteractive(path: &str) -> Result<Self, BotError> {
        let mut config = Self::parse_file(Path::new(path))?;
        for warning in config.validate() {
            tracing::warn!("Config {path}: {warning}");
        }
        Ok(config)
    }

    /// Load config for startup. If the file is missing, offer the interactive
    /// setup wizard (blocks on stdin — startup only, never from a worker task).
    pub fn load(path: &str) -> Result<Self, BotError> {
        let path_ref = Path::new(path);
        if !path_ref.exists() {
            eprintln!("Config file not found: {}", path_ref.display());
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
                    let mut config = Self::parse_file(created_path)
                        .map_err(|e| BotError::Config(format!("Failed to load created config: {e}")))?;
                    for warning in config.validate() {
                        tracing::warn!("Config: {warning}");
                    }
                    return Ok(config);
                }
            }
            return Err(BotError::Config(format!(
                "Config not found: {}\nRun: tt-spotify-bot --setup",
                path_ref.display()
            )));
        }
        Self::load_noninteractive(path)
    }

    /// Clamp out-of-range fields to sane values, returning a list of the
    /// corrections made (for logging). Keeps a hand-edited config from putting
    /// the bot into an unusable state (e.g. volume above the cap, port 0).
    pub fn validate(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.max_volume > 100 {
            warnings.push(format!("max_volume {} > 100, clamped to 100", self.max_volume));
            self.max_volume = 100;
        }
        if self.volume > self.max_volume {
            warnings.push(format!(
                "volume {} > max_volume {}, clamped",
                self.volume, self.max_volume
            ));
            self.volume = self.max_volume;
        }
        if self.radio_batch_size < 1 {
            warnings.push("radio_batch_size < 1, set to 1".to_string());
            self.radio_batch_size = 1;
        }
        if self.search_limit < 1 || self.search_limit > 20 {
            let clamped = self.search_limit.clamp(1, 20);
            warnings.push(format!("search_limit {} out of 1..=20, set to {clamped}", self.search_limit));
            self.search_limit = clamped;
        }
        if self.volume_ramp_step <= 0.0 || !self.volume_ramp_step.is_finite() {
            warnings.push(format!("volume_ramp_step {} invalid, reset to 0.03", self.volume_ramp_step));
            self.volume_ramp_step = 0.03;
        }
        if !(1..=65535).contains(&self.tcp_port) {
            warnings.push(format!("tcp_port {} out of range, reset to 10333", self.tcp_port));
            self.tcp_port = 10333;
        }
        if !(1..=65535).contains(&self.udp_port) {
            warnings.push(format!("udp_port {} out of range, reset to 10333", self.udp_port));
            self.udp_port = 10333;
        }
        if self.host.trim().is_empty() {
            warnings.push("host is empty, reset to localhost".to_string());
            self.host = "localhost".to_string();
        }
        if self.bot_name.trim().is_empty() {
            warnings.push("bot_name is empty, reset to Spotify".to_string());
            self.bot_name = "Spotify".to_string();
        }
        warnings
    }

    /// Write the config atomically: serialize to a temp file, then rename over
    /// the target. A crash mid-write can never leave a truncated config, and
    /// concurrent writers see whole files (last writer wins) rather than torn
    /// ones. `std::fs::rename` replaces the destination on both Unix and Windows.
    pub fn save(&self, path: &Path) -> Result<(), BotError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| BotError::Config(format!("Failed to serialize config: {e}")))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Single owner of a bot's on-disk config during runtime. All runtime config
/// mutations (volume debounce, mode/radio/gender saves, exit persistence) go
/// through `update()` under one lock, eliminating the read-modify-write races
/// the old `BotConfig::update(path, ..)` free function had (each call reloaded,
/// mutated, and rewrote the whole file, clobbering concurrent writers).
pub struct ConfigStore {
    path: PathBuf,
    cfg: parking_lot::Mutex<BotConfig>,
}

impl ConfigStore {
    pub fn new(path: impl Into<PathBuf>, cfg: BotConfig) -> Self {
        Self {
            path: path.into(),
            cfg: parking_lot::Mutex::new(cfg),
        }
    }

    /// Apply a mutation to the config and persist it atomically, all under one
    /// lock. Before mutating, re-sync from disk so edits made externally (e.g.
    /// the tray GUI's config editor writing the same file in another thread)
    /// are preserved rather than clobbered by a stale in-memory copy. Falls
    /// back to the cached copy if the file is momentarily unreadable.
    pub fn update(&self, f: impl FnOnce(&mut BotConfig)) {
        let mut guard = self.cfg.lock();
        if let Ok(on_disk) = BotConfig::parse_file(&self.path) {
            *guard = on_disk;
        }
        f(&mut guard);
        if let Err(e) = guard.save(&self.path) {
            tracing::error!("Failed to save config {}: {e}", self.path.display());
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)] // tweaking a couple of fields off Default reads fine in tests
mod tests {
    use super::*;
    use ::teamtalk::types::UserGender;

    // -- BotConfig equality (unchanged-edit detection in the GUI dialog) --

    #[test]
    fn botconfig_eq_clone_equal_and_field_change_detected() {
        let a = BotConfig::default();
        let mut b = a.clone();
        assert_eq!(a, b);
        b.volume = a.volume + 1;
        assert_ne!(a, b);
    }

    // -- is_valid_gender --

    #[test]
    fn is_valid_gender_male_aliases() {
        for s in ["male", "m", "man", "MALE", "Man"] {
            assert!(is_valid_gender(s), "{s} should be valid");
        }
    }

    #[test]
    fn is_valid_gender_female_aliases() {
        for s in ["female", "f", "woman", "FEMALE", "Woman"] {
            assert!(is_valid_gender(s), "{s} should be valid");
        }
    }

    #[test]
    fn is_valid_gender_neutral_aliases() {
        for s in ["neutral", "n", "nb", "NEUTRAL", "NB"] {
            assert!(is_valid_gender(s), "{s} should be valid");
        }
    }

    #[test]
    fn is_valid_gender_rejects_unknown() {
        for s in ["", "other", "xyz", "ma", "fem", "neutral!"] {
            assert!(!is_valid_gender(s), "{s} should be invalid");
        }
    }

    // -- parse_gender --

    #[test]
    fn parse_gender_male_aliases() {
        for s in ["male", "m", "man", "MALE", "Man"] {
            assert_eq!(parse_gender(s), UserGender::Male, "{s}");
        }
    }

    #[test]
    fn parse_gender_female_aliases() {
        for s in ["female", "f", "woman", "FEMALE", "Woman"] {
            assert_eq!(parse_gender(s), UserGender::Female, "{s}");
        }
    }

    #[test]
    fn parse_gender_neutral_aliases() {
        for s in ["neutral", "n", "nb", "NEUTRAL"] {
            assert_eq!(parse_gender(s), UserGender::Neutral, "{s}");
        }
    }

    #[test]
    fn parse_gender_unknown_defaults_to_neutral() {
        // parse_gender is "anything else defaults to Neutral" by design.
        for s in ["", "xyz", "other"] {
            assert_eq!(parse_gender(s), UserGender::Neutral, "{s}");
        }
    }

    // -- validate --

    #[test]
    fn validate_default_config_is_clean() {
        let mut cfg = BotConfig::default();
        assert!(cfg.validate().is_empty(), "default config should need no corrections");
    }

    #[test]
    fn validate_clamps_volume_to_max() {
        let mut cfg = BotConfig::default();
        cfg.max_volume = 60;
        cfg.volume = 90;
        let warnings = cfg.validate();
        assert_eq!(cfg.volume, 60);
        assert!(!warnings.is_empty());
    }

    #[test]
    fn validate_clamps_max_volume_over_100() {
        let mut cfg = BotConfig::default();
        cfg.max_volume = 200;
        cfg.volume = 150;
        cfg.validate();
        assert_eq!(cfg.max_volume, 100);
        assert_eq!(cfg.volume, 100);
    }

    #[test]
    fn validate_fixes_zero_ports() {
        let mut cfg = BotConfig::default();
        cfg.tcp_port = 0;
        cfg.udp_port = 99999;
        cfg.validate();
        assert_eq!(cfg.tcp_port, 10333);
        assert_eq!(cfg.udp_port, 10333);
    }

    #[test]
    fn validate_fixes_empty_host_and_name() {
        let mut cfg = BotConfig::default();
        cfg.host = "  ".to_string();
        cfg.bot_name = String::new();
        cfg.validate();
        assert_eq!(cfg.host, "localhost");
        assert_eq!(cfg.bot_name, "Spotify");
    }

    #[test]
    fn validate_fixes_bad_ramp_and_batch() {
        let mut cfg = BotConfig::default();
        cfg.volume_ramp_step = 0.0;
        cfg.radio_batch_size = 0;
        cfg.search_limit = 50;
        cfg.validate();
        assert_eq!(cfg.volume_ramp_step, 0.03);
        assert_eq!(cfg.radio_batch_size, 1);
        assert_eq!(cfg.search_limit, 20);
    }

    #[test]
    fn config_store_update_persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("ttspotify_cfgtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store_test.json");
        let mut cfg = BotConfig::default();
        cfg.volume = 30;
        cfg.save(&path).unwrap();

        let store = ConfigStore::new(path.clone(), cfg);
        store.update(|c| c.volume = 55);
        store.update(|c| c.radio_enabled = true);

        let reloaded = BotConfig::parse_file(&path).unwrap();
        assert_eq!(reloaded.volume, 55);
        assert!(reloaded.radio_enabled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_store_update_preserves_external_edits() {
        let dir = std::env::temp_dir().join(format!("ttspotify_cfgext_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ext_test.json");
        let cfg = BotConfig::default();
        cfg.save(&path).unwrap();
        let store = ConfigStore::new(path.clone(), cfg);

        // Simulate an external writer (e.g. GUI) changing a non-runtime field.
        let mut external = BotConfig::parse_file(&path).unwrap();
        external.host = "edited.example.com".to_string();
        external.save(&path).unwrap();

        // A runtime update must not clobber the external edit.
        store.update(|c| c.volume = 42);

        let reloaded = BotConfig::parse_file(&path).unwrap();
        assert_eq!(reloaded.host, "edited.example.com");
        assert_eq!(reloaded.volume, 42);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_round_trip_preserves_fields() {
        let mut cfg = BotConfig::default();
        cfg.host = "tt.example.com".to_string();
        cfg.tcp_port = 12345;
        cfg.volume = 42;
        cfg.max_volume = 88;
        cfg.radio_enabled = true;
        cfg.default_service = Service::YouTube;
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let parsed: BotConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.host, "tt.example.com");
        assert_eq!(parsed.tcp_port, 12345);
        assert_eq!(parsed.volume, 42);
        assert_eq!(parsed.max_volume, 88);
        assert!(parsed.radio_enabled);
        assert_eq!(parsed.default_service, Service::YouTube);
    }
}
