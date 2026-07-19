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

/// Read and validate a candidate config file. Returns the parsed config only if
/// it deserializes AND has the essential fields (`host`, `username`) filled — so
/// empty files, junk, and bare `{}` placeholders are rejected.
fn load_valid_config(path: &Path) -> Option<BotConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    let cfg: BotConfig = serde_json::from_str(&text).ok()?;
    if cfg.host.trim().is_empty() || cfg.username.trim().is_empty() {
        return None;
    }
    Some(cfg)
}

/// List config files in the config directory, skipping non-bot files.
pub fn list_configs() -> Vec<(String, PathBuf)> {
    list_configs_in(&config_dir())
}

/// Scan `dir` for bot config files. Skips the name skip-list (auth/session
/// artifacts, app-global settings) and any file that fails content validation
/// (empty host/username, junk, or a bare `{}` placeholder). Split out from
/// `list_configs` so it can be tested against a temp directory.
fn list_configs_in(dir: &Path) -> Vec<(String, PathBuf)> {
    // Non-bot JSON files that share the config directory. "settings" is the
    // app-global settings.json (update-check toggle); the rest are auth/session
    // artifacts. None are server configs, so they must never appear as bots.
    let skip = ["credentials", "cookies", "sessions", "settings"];
    if !dir.exists() {
        return Vec::new();
    }
    let mut configs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if skip.contains(&stem) {
                continue;
            }
            if load_valid_config(&path).is_none() {
                tracing::warn!("Skipping invalid or incomplete config file: {}", path.display());
                continue;
            }
            configs.push((stem.to_string(), path));
        }
    }
    configs.sort_by(|a, b| a.0.cmp(&b.0));
    configs
}

/// After an update, add any newly-added keys (with defaults) to every real config
/// in `dir`, preserving existing values. Idempotent: only rewrites a file whose
/// serialized form differs from what is on disk. Broken/incomplete files (rejected
/// by `load_valid_config`) are left untouched. Returns the number of files rewritten.
fn top_up_configs_in(dir: &Path) -> usize {
    let mut updated = 0;
    // Only ever touch files that `list_configs_in` deems real bot configs: this
    // applies the name skip-list (credentials/settings/cookies/sessions) AND the
    // host+username validation, so an auth/session artifact that happens to carry
    // a "host"/"username" key is never misparsed as a config and overwritten.
    for (_name, path) in list_configs_in(dir) {
        let Some(cfg) = load_valid_config(&path) else { continue };
        let Ok(current) = std::fs::read_to_string(&path) else { continue };
        let Ok(canonical) = serde_json::to_string_pretty(&cfg) else { continue };
        if current.trim() == canonical.trim() {
            continue;
        }
        match cfg.save(&path) {
            Ok(()) => {
                updated += 1;
                tracing::info!("Topped up config with new keys: {}", path.display());
            }
            Err(e) => tracing::warn!("Could not top up config {}: {e}", path.display()),
        }
    }
    updated
}

/// Top up every config in the default config dir with any newly-added keys.
/// Best-effort; per-file errors are logged and skipped, never propagated.
pub fn top_up_configs() {
    let _ = top_up_configs_in(&config_dir());
}

/// How the bot decides who may run admin-gated commands.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AdminMode {
    /// No gating; every user may run every command (opt-out / legacy behavior).
    Everyone,
    /// Only TeamTalk server admins (account marked admin on the server).
    TtRights,
    /// Only usernames in the `admins` list.
    List,
    /// A TeamTalk server admin OR a username in the `admins` list.
    #[default]
    Both,
}

fn default_radio_delay() -> f32 { 10.0 }
fn default_norm_type() -> String { "auto".to_string() }
fn default_norm_method() -> String { "dynamic".to_string() }
fn default_norm_pregain() -> f64 { 0.0 }
fn default_norm_threshold() -> f64 { -2.0 }
fn default_norm_knee() -> f64 { 5.0 }
fn default_language_en() -> String { "en".to_string() }

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
    #[serde(default, rename = "adminMode")]
    pub admin_mode: AdminMode,
    #[serde(default)]
    pub admins: Vec<String>,
    #[serde(default = "default_language_en", rename = "defaultLanguage")]
    pub default_language: String,

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
            admin_mode: AdminMode::default(),
            admins: Vec::new(),
            default_language: default_language_en(),
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
        if self.jitter_buffer_ms > 2000 {
            warnings.push(format!("jitter_buffer_ms {} > 2000, clamped to 2000", self.jitter_buffer_ms));
            self.jitter_buffer_ms = 2000;
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
    fn validate_clamps_jitter_buffer_over_2000() {
        let mut cfg = BotConfig::default();
        cfg.jitter_buffer_ms = 10_000;
        let warnings = cfg.validate();
        assert_eq!(cfg.jitter_buffer_ms, 2000);
        assert!(!warnings.is_empty());
    }

    #[test]
    fn validate_accepts_jitter_buffer_zero_and_default() {
        let mut cfg = BotConfig::default();
        cfg.jitter_buffer_ms = 0;
        assert!(cfg.validate().is_empty());
        cfg.jitter_buffer_ms = 400;
        assert!(cfg.validate().is_empty());
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
    fn admin_mode_defaults_to_both_when_absent() {
        // A config JSON missing the admin fields must load with safe defaults.
        let json = r#"{
            "host": "localhost", "tcpPort": 10333, "udpPort": 10333,
            "botName": "Spotify", "username": "", "password": "",
            "ChannelName": "/", "ChannelPassword": "", "botGender": "neutral",
            "spotifyQuality": "VERY_HIGH", "spotifyEnableNormalization": true
        }"#;
        let cfg: BotConfig = serde_json::from_str(json).expect("config should deserialize");
        assert_eq!(cfg.admin_mode, AdminMode::Both);
        assert!(cfg.admins.is_empty());
    }

    #[test]
    fn default_language_defaults_to_en_and_round_trips() {
        // Absent field -> "en" (existing configs keep working).
        let json = r#"{
            "host": "localhost", "tcpPort": 10333, "udpPort": 10333,
            "botName": "Spotify", "username": "", "password": "",
            "ChannelName": "/", "ChannelPassword": "", "botGender": "neutral",
            "spotifyQuality": "VERY_HIGH", "spotifyEnableNormalization": true
        }"#;
        let cfg: BotConfig = serde_json::from_str(json).expect("config should deserialize");
        assert_eq!(cfg.default_language, "en");

        // Round-trip preserves a non-default value under the serde name.
        let mut cfg = BotConfig::default();
        cfg.default_language = "pt".to_string();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"defaultLanguage\":\"pt\""));
        let back: BotConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.default_language, "pt");
    }

    #[test]
    fn admin_mode_round_trips() {
        let mut cfg = BotConfig::default();
        cfg.admin_mode = AdminMode::List;
        cfg.admins = vec!["alice".to_string(), "bob".to_string()];
        let json = serde_json::to_string(&cfg).unwrap();
        let back: BotConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.admin_mode, AdminMode::List);
        assert_eq!(back.admins, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn list_configs_skips_invalid_files() {
        // Reuse the temp-dir approach the other config tests use (no tempfile crate).
        let dir = std::env::temp_dir().join(format!("ttspotify_listcfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.as_path();
        // A real config: host + username set.
        let mut good = BotConfig::default();
        good.host = "srv.example.com".to_string();
        good.username = "botacct".to_string();
        good.save(&p.join("good.json")).unwrap();
        // Junk / empty / placeholder files that must NOT be listed.
        std::fs::write(p.join("empty.json"), "").unwrap();
        std::fs::write(p.join("junk.json"), "not json at all").unwrap();
        std::fs::write(p.join("blank.json"), "{}").unwrap(); // parses to defaults, empty host/username
        std::fs::write(p.join("nouser.json"), r#"{"host":"h"}"#).unwrap(); // host but no username
        std::fs::write(p.join("settings.json"), r#"{"host":"h","username":"u"}"#).unwrap(); // name skip-list

        let listed: Vec<String> = list_configs_in(p).into_iter().map(|(name, _)| name).collect();
        assert_eq!(listed, vec!["good".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn top_up_adds_missing_keys_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("ttspotify_topup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.as_path();
        // A valid config written WITHOUT the admin keys (simulating a pre-update file).
        let path = p.join("srv.json");
        std::fs::write(&path, r#"{"host":"h","username":"u"}"#).unwrap();
        // Junk that must be left untouched.
        let junk = p.join("junk.json");
        std::fs::write(&junk, "garbage").unwrap();
        let junk_before = std::fs::read_to_string(&junk).unwrap();

        // First pass tops up the valid config.
        assert_eq!(top_up_configs_in(p), 1);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("adminMode"));
        assert!(text.contains("admins"));
        // Existing values preserved.
        let cfg: BotConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(cfg.host, "h");
        assert_eq!(cfg.username, "u");
        assert_eq!(cfg.admin_mode, AdminMode::Both);

        // Second pass is a no-op (idempotent).
        assert_eq!(top_up_configs_in(p), 0);
        // Junk untouched.
        assert_eq!(std::fs::read_to_string(&junk).unwrap(), junk_before);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn top_up_never_touches_skip_listed_files() {
        let dir = std::env::temp_dir().join(format!("ttspotify_topupskip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.as_path();
        // A skip-listed auth artifact that happens to carry host+username (plus a
        // real credential key). It must NEVER be parsed as a config and rewritten,
        // or the credential key would be silently dropped.
        let cred = p.join("credentials.json");
        std::fs::write(&cred, r#"{"host":"h","username":"u","refresh_token":"secret"}"#).unwrap();
        let cred_before = std::fs::read_to_string(&cred).unwrap();
        // settings.json likewise must be left alone.
        let settings = p.join("settings.json");
        std::fs::write(&settings, r#"{"host":"h","username":"u","check_updates_on_startup":true}"#).unwrap();
        let settings_before = std::fs::read_to_string(&settings).unwrap();

        let updated = top_up_configs_in(p);

        assert_eq!(updated, 0, "no bot configs present, nothing should be rewritten");
        assert_eq!(std::fs::read_to_string(&cred).unwrap(), cred_before, "credentials.json must be untouched");
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), settings_before, "settings.json must be untouched");

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
