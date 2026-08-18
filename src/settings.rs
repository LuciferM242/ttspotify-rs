//! App-global preferences shared across all bot instances (they run one shared
//! binary, so these are not per-bot config fields). Stored as settings.json in
//! the platform config dir. "Launch on startup" is NOT here — on Windows that
//! lives in the registry (see gui::autostart).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::error::BotError;

fn default_true() -> bool {
    true
}

/// Default cache ceiling, in MB.
pub const DEFAULT_CACHE_LIMIT_MB: u64 = 1024;

fn default_cache_limit_mb() -> u64 {
    DEFAULT_CACHE_LIMIT_MB
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_true", rename = "checkUpdatesOnStartup")]
    pub check_updates_on_startup: bool,
    /// How much disk the downloaded-audio caches may occupy, in MB, across
    /// every bot on this install. 0 keeps nothing.
    #[serde(default = "default_cache_limit_mb", rename = "cacheLimitMb")]
    pub cache_limit_mb: u64,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            check_updates_on_startup: true,
            cache_limit_mb: DEFAULT_CACHE_LIMIT_MB,
        }
    }
}

pub fn settings_path() -> PathBuf {
    crate::paths::state_dir().join("settings.json")
}

/// Load settings, falling back to defaults if the file is missing or unreadable.
pub fn load() -> AppSettings {
    let path = settings_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => AppSettings::default(),
    }
}

impl AppSettings {
    /// The ceiling in bytes, or `None` to keep nothing.
    pub fn cache_limit_bytes(&self) -> Option<u64> {
        (self.cache_limit_mb > 0).then(|| self.cache_limit_mb.saturating_mul(1024 * 1024))
    }

    /// Persist atomically (tmp + rename), matching config.rs's write pattern.
    pub fn save(&self) -> Result<(), BotError> {
        let path = settings_path();
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| BotError::Config(format!("Failed to serialize settings: {e}")))?;
        crate::paths::write_atomic(&path, json.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_update_check_on() {
        assert!(AppSettings::default().check_updates_on_startup);
    }

    #[test]
    fn deserialize_missing_field_defaults_on() {
        let s: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(s.check_updates_on_startup);
    }

    #[test]
    fn round_trips_false() {
        let s = AppSettings {
            check_updates_on_startup: false,
            ..AppSettings::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: AppSettings = serde_json::from_str(&json).unwrap();
        assert!(!back.check_updates_on_startup);
    }

    #[test]
    fn serializes_with_camelcase_key() {
        let json = serde_json::to_string(&AppSettings::default()).unwrap();
        assert!(json.contains("checkUpdatesOnStartup"));
        assert!(json.contains("cacheLimitMb"));
    }

    #[test]
    fn a_config_written_before_the_limit_existed_gets_the_default() {
        let s: AppSettings = serde_json::from_str(r#"{"checkUpdatesOnStartup":true}"#).unwrap();
        assert_eq!(s.cache_limit_mb, DEFAULT_CACHE_LIMIT_MB);
    }

    #[test]
    fn the_limit_reaches_librespot_as_bytes() {
        let s = AppSettings { cache_limit_mb: 512, ..AppSettings::default() };
        assert_eq!(s.cache_limit_bytes(), Some(512 * 1024 * 1024));
    }

    #[test]
    fn zero_means_keep_nothing_rather_than_keep_everything() {
        let s = AppSettings { cache_limit_mb: 0, ..AppSettings::default() };
        assert_eq!(s.cache_limit_bytes(), None);
    }

    #[test]
    fn an_absurd_limit_does_not_wrap_around() {
        let s = AppSettings { cache_limit_mb: u64::MAX, ..AppSettings::default() };
        assert!(s.cache_limit_bytes().unwrap() > 0);
    }
}
