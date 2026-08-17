//! The config editor's logic, with no window attached.
//!
//! Reading controls and writing a `BotConfig` is where the editor can actually
//! be wrong: a field written to the wrong place, a number that silently becomes
//! zero, a validation rule that lets an unusable config through. Keeping that
//! here means it is testable without opening a dialog, and the dialog itself is
//! reduced to moving strings between controls and this module.
//!
//! Advanced normalisation settings are deliberately absent from the form. They
//! are preserved by starting from the loaded config rather than a default one,
//! which is also why `apply` takes the original.

use crate::config::{AdminMode, BotConfig};

/// The admin modes, in the order the dropdown lists them.
pub const ADMIN_MODES: [AdminMode; 4] = [
    AdminMode::Everyone,
    AdminMode::TtRights,
    AdminMode::List,
    AdminMode::Both,
];

/// Descriptions shown in the dropdown, in the same order.
pub const ADMIN_MODE_LABELS: [&str; 4] = [
    "Everyone - no restrictions, any user can run every command",
    "TeamTalk server admins - admins defined in your TeamTalk server's user accounts",
    "Username list - only the usernames you enter below",
    "Both - TeamTalk server admins or the username list below",
];

/// Dropdown position for an admin mode.
pub fn admin_mode_to_index(mode: AdminMode) -> u32 {
    ADMIN_MODES.iter().position(|m| *m == mode).unwrap_or(3) as u32
}

/// Admin mode for a dropdown position. Anything unexpected becomes `Both`,
/// which is the restrictive choice: a misread must not hand out access.
pub fn index_to_admin_mode(index: u32) -> AdminMode {
    ADMIN_MODES.get(index as usize).copied().unwrap_or(AdminMode::Both)
}

/// Whether the admin username list is used by this mode, and so whether the
/// field should be editable.
pub fn mode_uses_username_list(index: u32) -> bool {
    matches!(
        index_to_admin_mode(index),
        AdminMode::List | AdminMode::Both
    )
}

/// Everything the form holds, as text and numbers straight from the controls.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigForm {
    // Server
    pub host: String,
    pub tcp_port: i32,
    pub udp_port: i32,
    pub encrypted: bool,
    pub username: String,
    pub password: String,
    pub bot_name: String,
    pub channel_name: String,
    pub channel_password: String,
    pub bot_gender: String,
    pub spotify_enabled: bool,
    pub youtube_enabled: bool,
    pub default_service: String,
    pub license_name: String,
    pub license_key: String,
    pub youtube_cookies_file: String,
    pub default_language: String,
    pub admin_mode_index: u32,
    pub admin_users: String,
    // Audio
    pub spotify_quality: String,
    pub spotify_enable_normalization: bool,
    pub volume: i32,
    pub max_volume: i32,
    pub jitter_buffer_ms: i32,
    pub volume_ramp_step: String,
    // Radio and search
    pub radio_enabled: bool,
    pub radio_batch_size: i32,
    pub radio_delay: String,
    pub search_limit: i32,
}

impl ConfigForm {
    /// Fill the form from a config, ready to show.
    pub fn from_config(cfg: &BotConfig) -> Self {
        Self {
            host: cfg.host.clone(),
            tcp_port: cfg.tcp_port,
            udp_port: cfg.udp_port,
            encrypted: cfg.encrypted,
            username: cfg.username.clone(),
            password: cfg.password.clone(),
            bot_name: cfg.bot_name.clone(),
            channel_name: cfg.channel_name.clone(),
            channel_password: cfg.channel_password.clone(),
            bot_gender: cfg.bot_gender.clone(),
            spotify_enabled: cfg.enabled_services.spotify,
            youtube_enabled: cfg.enabled_services.youtube,
            default_service: cfg.default_service.name().to_string(),
            license_name: cfg.license_name.clone().unwrap_or_default(),
            license_key: cfg.license_key.clone().unwrap_or_default(),
            youtube_cookies_file: cfg.youtube_cookies_file.clone(),
            default_language: cfg.default_language.clone(),
            admin_mode_index: admin_mode_to_index(cfg.admin_mode),
            admin_users: cfg.admins.join(", "),
            spotify_quality: cfg.spotify_quality.clone(),
            spotify_enable_normalization: cfg.spotify_enable_normalization,
            volume: cfg.volume as i32,
            max_volume: cfg.max_volume as i32,
            jitter_buffer_ms: cfg.jitter_buffer_ms as i32,
            volume_ramp_step: cfg.volume_ramp_step.to_string(),
            radio_enabled: cfg.radio_enabled,
            radio_batch_size: cfg.radio_batch_size as i32,
            radio_delay: cfg.radio_delay.to_string(),
            search_limit: cfg.search_limit as i32,
        }
    }

    /// Write the form onto a copy of `original`.
    ///
    /// Starting from the original rather than a default preserves the advanced
    /// normalisation settings, which the form never shows and so cannot carry.
    pub fn apply(&self, original: &BotConfig) -> BotConfig {
        let mut cfg = original.clone();

        cfg.host = self.host.trim().to_string();
        cfg.tcp_port = self.tcp_port;
        cfg.udp_port = self.udp_port;
        cfg.encrypted = self.encrypted;
        cfg.username = self.username.clone();
        cfg.password = self.password.clone();
        cfg.bot_name = self.bot_name.clone();
        // An empty channel means the root channel, not "no channel".
        cfg.channel_name = if self.channel_name.trim().is_empty() {
            "/".to_string()
        } else {
            self.channel_name.clone()
        };
        cfg.channel_password = self.channel_password.clone();
        cfg.bot_gender = self.bot_gender.clone();
        cfg.enabled_services = crate::config::EnabledServices {
            spotify: self.spotify_enabled,
            youtube: self.youtube_enabled,
        };
        // With exactly one service enabled the default IS that service; the
        // dropdown is hidden in that state, so its stale value must not win.
        cfg.default_service = cfg
            .enabled_services
            .only()
            .unwrap_or_else(|| crate::services::Service::parse_or_default(&self.default_service));
        cfg.youtube_cookies_file = self.youtube_cookies_file.clone();
        // Absent, not empty: the config treats None and Some("") differently.
        cfg.license_name = non_empty(&self.license_name);
        cfg.license_key = non_empty(&self.license_key);
        cfg.admin_mode = index_to_admin_mode(self.admin_mode_index);
        cfg.admins = crate::bot::auth::parse_admin_list(&self.admin_users);
        let lang = self.default_language.trim().to_lowercase();
        cfg.default_language = if lang.is_empty() { "en".to_string() } else { lang };

        cfg.spotify_quality = self.spotify_quality.clone();
        cfg.spotify_enable_normalization = self.spotify_enable_normalization;
        cfg.volume = self.volume.clamp(0, 255) as u8;
        cfg.max_volume = self.max_volume.clamp(0, 255) as u8;
        cfg.jitter_buffer_ms = self.jitter_buffer_ms.max(0) as u32;
        // A number that will not parse keeps the previous value rather than
        // becoming zero, which would silently disable the volume ramp.
        cfg.volume_ramp_step = self
            .volume_ramp_step
            .trim()
            .parse()
            .unwrap_or(original.volume_ramp_step);

        cfg.radio_enabled = self.radio_enabled;
        cfg.radio_batch_size = self.radio_batch_size.clamp(0, 255) as u8;
        cfg.radio_delay = self.radio_delay.trim().parse().unwrap_or(original.radio_delay);
        cfg.search_limit = self.search_limit.clamp(0, 255) as u8;

        cfg
    }

    /// Problems that must be fixed before saving. Empty means the form is good.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.host.trim().is_empty() {
            errors.push("Host is required.".to_string());
        }
        if self.username.trim().is_empty() {
            errors.push("Username is required.".to_string());
        }
        if self.volume > self.max_volume {
            errors.push("Default volume cannot exceed max volume.".to_string());
        }
        if !self.spotify_enabled && !self.youtube_enabled {
            errors.push("At least one service must be enabled.".to_string());
        }
        errors
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Strip a trailing `.json` and any path separators from a name typed for a new
/// config, so it can only ever name a file inside the configs folder.
pub fn sanitise_config_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .trim()
        .trim_end_matches(".json")
        .chars()
        // A name is a file name, not a path: anything that could climb out of
        // the configs folder is dropped rather than escaped.
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> BotConfig {
        BotConfig {
            host: "example.com".to_string(),
            username: "bot".to_string(),
            volume: 50,
            max_volume: 100,
            ..Default::default()
        }
    }

    #[test]
    fn a_config_survives_a_round_trip_through_the_form() {
        // The core promise of the editor: opening it and saving without
        // touching anything must not change the config.
        let cfg = base();
        let form = ConfigForm::from_config(&cfg);
        assert_eq!(form.apply(&cfg), cfg, "opening and saving altered the config");
    }

    #[test]
    fn settings_the_form_never_shows_are_preserved() {
        // The reason apply() starts from the original. These have no controls,
        // so a form built from defaults would silently reset them.
        let mut cfg = base();
        cfg.normalisation_type = "album".to_string();
        cfg.normalisation_pregain_db = -3.5;
        cfg.normalisation_knee_db = 7.0;

        let mut form = ConfigForm::from_config(&cfg);
        form.host = "changed.example.com".to_string();
        let saved = form.apply(&cfg);

        assert_eq!(saved.host, "changed.example.com");
        assert_eq!(saved.normalisation_type, "album");
        assert_eq!(saved.normalisation_pregain_db, -3.5);
        assert_eq!(saved.normalisation_knee_db, 7.0);
    }

    #[test]
    fn service_checkboxes_round_trip_and_gate_the_save() {
        // Unchecking Spotify must land in the config, and unchecking both is
        // an error the dialog shows instead of saving a bot that can play
        // nothing.
        let cfg = base();
        let mut form = ConfigForm::from_config(&cfg);
        assert!(form.spotify_enabled && form.youtube_enabled, "defaults are on");
        form.spotify_enabled = false;
        let saved = form.apply(&cfg);
        assert!(!saved.enabled_services.spotify);
        assert!(saved.enabled_services.youtube);
        // One service left: the (hidden) default dropdown must not win.
        assert_eq!(saved.default_service, crate::services::Service::YouTube);
        assert!(form.validate().is_empty());

        form.youtube_enabled = false;
        assert!(
            form.validate().iter().any(|e| e.contains("service")),
            "no-services form must fail validation"
        );
    }

    #[test]
    fn an_empty_channel_becomes_the_root_channel() {
        let cfg = base();
        let mut form = ConfigForm::from_config(&cfg);
        form.channel_name = "   ".to_string();
        assert_eq!(form.apply(&cfg).channel_name, "/");
    }

    #[test]
    fn blank_licence_fields_are_absent_rather_than_empty() {
        // The config distinguishes None from Some(""); writing an empty string
        // makes the bot send a blank licence instead of none at all.
        let cfg = base();
        let mut form = ConfigForm::from_config(&cfg);
        form.license_name = "  ".to_string();
        form.license_key = String::new();
        let saved = form.apply(&cfg);
        assert_eq!(saved.license_name, None);
        assert_eq!(saved.license_key, None);
    }

    #[test]
    fn a_licence_is_trimmed_but_kept() {
        let cfg = base();
        let mut form = ConfigForm::from_config(&cfg);
        form.license_name = "  BearWare.dk  ".to_string();
        assert_eq!(
            form.apply(&cfg).license_name,
            Some("BearWare.dk".to_string())
        );
    }

    #[test]
    fn an_unparseable_number_keeps_the_previous_value() {
        // Typing rubbish into the ramp step must not silently disable the
        // volume ramp by turning it into zero.
        let mut cfg = base();
        cfg.volume_ramp_step = 0.05;
        cfg.radio_delay = 2.5;

        let mut form = ConfigForm::from_config(&cfg);
        form.volume_ramp_step = "not a number".to_string();
        form.radio_delay = String::new();

        let saved = form.apply(&cfg);
        assert_eq!(saved.volume_ramp_step, 0.05);
        assert_eq!(saved.radio_delay, 2.5);
    }

    #[test]
    fn a_missing_language_falls_back_to_english() {
        let cfg = base();
        let mut form = ConfigForm::from_config(&cfg);
        form.default_language = "   ".to_string();
        assert_eq!(form.apply(&cfg).default_language, "en");
    }

    #[test]
    fn a_language_is_lowercased() {
        let cfg = base();
        let mut form = ConfigForm::from_config(&cfg);
        form.default_language = "  ES ".to_string();
        assert_eq!(form.apply(&cfg).default_language, "es");
    }

    #[test]
    fn a_good_form_has_nothing_to_report() {
        assert!(ConfigForm::from_config(&base()).validate().is_empty());
    }

    #[test]
    fn the_required_fields_are_reported_when_blank() {
        let cfg = base();
        let mut form = ConfigForm::from_config(&cfg);
        form.host = "  ".to_string();
        form.username = String::new();
        let errors = form.validate();
        assert_eq!(errors.len(), 2, "got: {errors:?}");
        assert!(errors.iter().any(|e| e.contains("Host")));
        assert!(errors.iter().any(|e| e.contains("Username")));
    }

    #[test]
    fn a_default_volume_above_the_maximum_is_rejected() {
        // Otherwise the bot starts louder than its own ceiling.
        let cfg = base();
        let mut form = ConfigForm::from_config(&cfg);
        form.volume = 90;
        form.max_volume = 50;
        assert!(form.validate().iter().any(|e| e.contains("max volume")));
    }

    #[test]
    fn every_admin_mode_survives_the_dropdown() {
        for mode in ADMIN_MODES {
            assert_eq!(index_to_admin_mode(admin_mode_to_index(mode)), mode);
        }
    }

    #[test]
    fn an_unexpected_dropdown_index_gives_the_restrictive_mode() {
        // A misread must never widen access.
        assert_eq!(index_to_admin_mode(99), AdminMode::Both);
    }

    #[test]
    fn the_username_list_is_only_used_by_the_modes_that_read_it() {
        assert!(!mode_uses_username_list(admin_mode_to_index(AdminMode::Everyone)));
        assert!(!mode_uses_username_list(admin_mode_to_index(AdminMode::TtRights)));
        assert!(mode_uses_username_list(admin_mode_to_index(AdminMode::List)));
        assert!(mode_uses_username_list(admin_mode_to_index(AdminMode::Both)));
    }

    #[test]
    fn there_is_a_label_for_every_admin_mode() {
        assert_eq!(ADMIN_MODES.len(), ADMIN_MODE_LABELS.len());
    }

    #[test]
    fn a_config_name_cannot_escape_the_configs_folder() {
        // The name is typed by hand and turned straight into a file path.
        assert_eq!(sanitise_config_name("../../evil"), Some("evil".to_string()));
        assert_eq!(sanitise_config_name(r"C:\windows\system32"), Some("Cwindowssystem32".to_string()));
        assert_eq!(sanitise_config_name("my/server"), Some("myserver".to_string()));
    }

    #[test]
    fn a_config_name_loses_a_typed_json_suffix() {
        assert_eq!(sanitise_config_name("myserver.json"), Some("myserver".to_string()));
        assert_eq!(sanitise_config_name(" myserver "), Some("myserver".to_string()));
    }

    #[test]
    fn a_name_that_is_only_punctuation_is_rejected() {
        assert_eq!(sanitise_config_name("   "), None);
        assert_eq!(sanitise_config_name("..."), None);
        assert_eq!(sanitise_config_name("/"), None);
        assert_eq!(sanitise_config_name(".json"), None);
    }
}
