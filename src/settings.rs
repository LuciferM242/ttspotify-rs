//! App-global preferences shared across all bot instances (they run one shared
//! binary, so these are not per-bot config fields). Stored as settings.json in
//! the platform config dir. "Launch on startup" is NOT here — on Windows that
//! lives in the registry (see gui::autostart).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::config_dir;
use crate::error::BotError;

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_true", rename = "checkUpdatesOnStartup")]
    pub check_updates_on_startup: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            check_updates_on_startup: true,
        }
    }
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
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
    /// Persist atomically (tmp + rename), matching config.rs's write pattern.
    pub fn save(&self) -> Result<(), BotError> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| BotError::Config(format!("Failed to serialize settings: {e}")))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
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
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: AppSettings = serde_json::from_str(&json).unwrap();
        assert!(!back.check_updates_on_startup);
    }

    #[test]
    fn serializes_with_camelcase_key() {
        let json = serde_json::to_string(&AppSettings::default()).unwrap();
        assert!(json.contains("checkUpdatesOnStartup"));
    }
}
