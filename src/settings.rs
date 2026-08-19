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

/// Default age at which an unplayed track is dropped, in days.
pub const DEFAULT_CACHE_KEEP_DAYS: u32 = 7;

fn default_cache_keep_days() -> u32 {
    DEFAULT_CACHE_KEEP_DAYS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_true", rename = "checkUpdatesOnStartup")]
    pub check_updates_on_startup: bool,
    /// How much disk the downloaded-audio caches may occupy, in MB, across
    /// every bot on this install. 0 keeps nothing.
    #[serde(default = "default_cache_limit_mb", rename = "cacheLimitMb")]
    pub cache_limit_mb: u64,
    /// Drop a cached track nothing has played for this many days, whatever the
    /// size limit says. 0 turns the age rule off.
    #[serde(default = "default_cache_keep_days", rename = "cacheKeepDays")]
    pub cache_keep_days: u32,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            check_updates_on_startup: true,
            cache_limit_mb: DEFAULT_CACHE_LIMIT_MB,
            cache_keep_days: DEFAULT_CACHE_KEEP_DAYS,
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
    /// The ceiling in bytes. Zero is a real ceiling - keep nothing - not the
    /// absence of one, so it is always a number and never an "unset" that the
    /// sweep would read as "no limit at all".
    pub fn cache_limit_bytes(&self) -> u64 {
        self.cache_limit_mb.saturating_mul(1024 * 1024)
    }

    /// True when the user asked for no saved music at all.
    pub fn caching_off(&self) -> bool {
        self.cache_limit_mb == 0
    }

    /// How long an unplayed track is kept. A very large span when the age rule
    /// is off, so callers need no special case.
    pub fn cache_keep_duration(&self) -> std::time::Duration {
        if self.cache_keep_days == 0 {
            std::time::Duration::from_secs(u32::MAX as u64 * 86_400)
        } else {
            std::time::Duration::from_secs(self.cache_keep_days as u64 * 86_400)
        }
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
        assert!(json.contains("cacheKeepDays"));
    }

    #[test]
    fn the_keep_age_is_seven_days_by_default() {
        let s = AppSettings::default();
        assert_eq!(s.cache_keep_duration(), std::time::Duration::from_secs(7 * 86_400));
    }

    #[test]
    fn zero_keep_days_turns_the_age_rule_off_rather_than_deleting_everything() {
        // Read the wrong way round this would evict the whole cache on the
        // first sweep.
        let s = AppSettings { cache_keep_days: 0, ..AppSettings::default() };
        assert!(s.cache_keep_duration() > std::time::Duration::from_secs(365 * 86_400 * 100));
    }

    #[test]
    fn a_config_without_the_age_key_gets_the_default() {
        let s: AppSettings = serde_json::from_str(r#"{"cacheLimitMb":512}"#).unwrap();
        assert_eq!(s.cache_keep_days, DEFAULT_CACHE_KEEP_DAYS);
        assert_eq!(s.cache_limit_mb, 512);
    }

    #[test]
    fn a_config_written_before_the_limit_existed_gets_the_default() {
        let s: AppSettings = serde_json::from_str(r#"{"checkUpdatesOnStartup":true}"#).unwrap();
        assert_eq!(s.cache_limit_mb, DEFAULT_CACHE_LIMIT_MB);
    }

    #[test]
    fn the_limit_is_reported_in_bytes() {
        let s = AppSettings { cache_limit_mb: 512, ..AppSettings::default() };
        assert_eq!(s.cache_limit_bytes(), 512 * 1024 * 1024);
        assert!(!s.caching_off());
    }

    #[test]
    fn zero_means_keep_nothing_rather_than_keep_everything() {
        // A zero ceiling used to arrive at the sweep as "no ceiling", which
        // switched the size rule off and let the cache grow without end -
        // the exact opposite of what the box says.
        let s = AppSettings { cache_limit_mb: 0, ..AppSettings::default() };
        assert_eq!(s.cache_limit_bytes(), 0, "zero must stay a ceiling of zero");
        assert!(s.caching_off());
    }

    #[test]
    fn an_absurd_limit_does_not_wrap_around() {
        let s = AppSettings { cache_limit_mb: u64::MAX, ..AppSettings::default() };
        assert!(s.cache_limit_bytes() > 0);
    }
}
