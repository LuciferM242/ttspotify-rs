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
/// Whether `path` holds a usable bot config. Used by the layout migration to
/// tell a real config from a stray JSON file it must not touch.
pub fn is_bot_config(path: &Path) -> bool {
    load_valid_config(path).is_some()
}

fn load_valid_config(path: &Path) -> Option<BotConfig> {
    read_config_file(path).ok()
}

/// Read one config, or say in one sentence why it cannot be used.
///
/// The reason matters: a file that fails to parse disappears from every
/// command, and "skipping invalid or incomplete config file" left people
/// hunting for a bot that had simply been written with the wrong spelling in
/// one field.
fn read_config_file(path: &Path) -> Result<BotConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        // io::Error's own wording ends in "(os error 13)", which is a number
        // for the program, not for whoever has to fix the permissions.
        let reason = e.to_string();
        let reason = reason.split(" (os error").next().unwrap_or(&reason).to_string();
        format!("cannot be read ({reason})")
    })?;
    // Parsed as plain JSON first, so a file that is not JSON at all is
    // reported as that rather than as whichever field serde reached first.
    if let Err(e) = serde_json::from_str::<serde_json::Value>(&text) {
        return Err(if text.trim().is_empty() {
            "is empty".to_string()
        } else {
            format!("is not valid JSON: {e}")
        });
    }
    let cfg: BotConfig = serde_json::from_str(&text).map_err(|e| format!("is not valid: {e}"))?;
    // Exactly the rule the loader uses, so the set of files listed as bots and
    // the set that will actually run are the same set. They used to differ in
    // both directions: a file relying on the default host was listed and then
    // refused at startup, and a config for an anonymous login (no username)
    // ran fine but was invisible to every command and skipped by the layout
    // migration.
    if !names_a_server(&text) {
        return Err("does not say which server to connect to (no \"host\")".to_string());
    }
    Ok(cfg)
}

/// Whether the file itself says where the server is.
///
/// Checking the parsed value cannot answer this: every field carries a serde
/// default, so a file with no `host` key at all still arrives with
/// `host = "localhost"` and looks configured.
fn names_a_server(contents: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(contents)
        .ok()
        .and_then(|v| v.get("host").and_then(|h| h.as_str()).map(str::to_string))
        .is_some_and(|host| !host.trim().is_empty())
}

/// Process exit code for "config missing or unreadable" (sysexits EX_CONFIG).
/// The systemd unit lists it in RestartPreventExitStatus: restarting can't fix
/// a missing config, and a 2s crash-restart loop hammers the TeamTalk server
/// with logins.
pub const EXIT_CONFIG_ERROR: i32 = 78;

/// List config files, skipping non-bot files.
pub fn list_configs() -> Vec<(String, PathBuf)> {
    list_configs_in(&crate::paths::configs_dir())
}

/// Read a menu answer: the number beside a bot, or its name typed out.
///
/// Returns `None` for anything else, including an out-of-range number, so the
/// caller asks again rather than starting a bot nobody chose.
pub fn parse_config_choice(input: &str, names: &[String]) -> Option<usize> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Ok(number) = input.parse::<usize>() {
        return number.checked_sub(1).filter(|i| *i < names.len());
    }
    names.iter().position(|n| n == input)
}

/// Which config a bare run (no `--config`) should use.
///
/// One config is not a choice. Several used to mean "the first one
/// alphabetically", silently — so a machine with three bots started whichever
/// one happened to sort first. With a terminal, ask; without one, carry on
/// with the first but say which, so the log records the decision.
pub fn choose_config(configs: &[(String, PathBuf)], interactive: bool) -> Option<PathBuf> {
    match configs.len() {
        0 => None,
        1 => Some(configs[0].1.clone()),
        _ if !interactive => {
            let (name, path) = &configs[0];
            println!(
                "Several bots are configured; running \"{name}\". Pass --config to pick another."
            );
            Some(path.clone())
        }
        _ => prompt_for_config(configs),
    }
}

fn prompt_for_config(configs: &[(String, PathBuf)]) -> Option<PathBuf> {
    use std::io::Write;

    let names: Vec<String> = configs.iter().map(|(name, _)| name.clone()).collect();
    println!("Which bot do you want to run?");
    for (i, name) in names.iter().enumerate() {
        println!("  {}. {name}", i + 1);
    }

    for _ in 0..3 {
        print!("Number or name: ");
        std::io::stdout().flush().ok();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).unwrap_or(0) == 0 {
            return None; // EOF: treat as "never mind".
        }
        if let Some(index) = parse_config_choice(&input, &names) {
            return Some(configs[index].1.clone());
        }
        println!("  Not one of the choices.");
    }
    None
}

/// Resolve a `--config` path that may have been written before configs moved
/// into `config/`.
///
/// An installed systemd unit bakes the config path into its ExecStart line, and
/// we cannot rewrite a unit on the user's behalf. Rather than let every existing
/// service die on the release that moves the file, accept the old path and look
/// for the same name in the new home.
pub fn resolve_config_path(given: &str) -> PathBuf {
    let path = PathBuf::from(given);
    if path.exists() {
        return path;
    }
    let Some(name) = path.file_name() else {
        return path;
    };
    let moved = crate::paths::configs_dir().join(name);
    if moved.exists() {
        // Printed, not only logged: this runs before the bot's logging exists,
        // so the tracing line went nowhere — and this is the one message that
        // tells someone with an old service file that it needs updating.
        eprintln!(
            "Config not at {}; using {} instead.\nYour service file still points at the old \
             location - re-run the service install to update it.",
            path.display(),
            moved.display()
        );
        return moved;
    }
    path
}

/// Scan `dir` for bot config files. Skips the name skip-list (auth/session
/// artifacts, app-global settings) and any file that fails content validation
/// (empty host/username, junk, or a bare `{}` placeholder). Split out from
/// `list_configs` so it can be tested against a temp directory.
fn list_configs_in(dir: &Path) -> Vec<(String, PathBuf)> {
    list_configs_and_problems_in(dir).0
}

/// Every bot in `dir`, plus a plain sentence about each JSON file that looked
/// like a bot and was not usable.
///
/// The problems used to be logged as warnings, which meant they arrived
/// coloured and mid-sentence in the middle of `list`, `status` and `doctor`
/// output. They belong to the command that asked, so it prints them where it
/// wants them.
pub fn list_configs_and_problems() -> (Vec<(String, PathBuf)>, Vec<String>) {
    list_configs_and_problems_in(&crate::paths::configs_dir())
}

fn list_configs_and_problems_in(dir: &Path) -> (Vec<(String, PathBuf)>, Vec<String>) {
    // Non-bot JSON files that share the config directory. "settings" is the
    // app-global settings.json (update-check toggle), "lang_prefs" is the i18n
    // per-user language store; the rest are auth/session artifacts. None are
    // server configs, so they must never appear as bots.
    let skip = ["credentials", "cookies", "sessions", "settings", "lang_prefs"];
    if !dir.exists() {
        return (Vec::new(), Vec::new());
    }
    let mut configs = Vec::new();
    let mut problems = Vec::new();
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
            if let Err(reason) = read_config_file(&path) {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                problems.push(format!(
                    "{} {reason}",
                    name.unwrap_or_else(|| path.display().to_string())
                ));
                continue;
            }
            configs.push((stem.to_string(), path));
        }
    }
    configs.sort_by(|a, b| a.0.cmp(&b.0));
    problems.sort();
    (configs, problems)
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
    let _ = top_up_configs_in(&crate::paths::configs_dir());
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

/// Strip a trailing `.json` and any path separators from a name typed for a new
/// config, so it can only ever name a file inside the configs folder. Used by
/// both the GUI's name prompt and the CLI wizard (including `--setup <name>`).
pub fn sanitise_config_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .trim()
        .trim_end_matches(".json")
        .chars()
        // A name is a file name, not a path: anything that could climb out of
        // the configs folder is dropped rather than escaped.
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        // Tabs and newlines survived into file names and into the systemd
        // instance string, where they are unreadable and unaddressable.
        .filter(|c| !c.is_control())
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        return None;
    }
    // systemd builds `ttspotify@<name>.service` out of this, and a unit name
    // over 255 bytes cannot be started at all — the bot listed fine and then
    // failed with a screenful of the name and a systemctl error about
    // "unavailable resources". Escaping can triple a byte, so the cap is well
    // under the limit rather than exactly at it.
    if cleaned.chars().count() > 60 {
        return None;
    }
    // Names that already mean something else. "all" is what start, stop and
    // restart accept for "every bot", so a bot called that could never be
    // started on its own. "tray" owns logs/tray/ on Windows, so a bot of that
    // name would share the tray's own log folder — and "delete this bot's
    // logs" would delete the tray's.
    if ["all", "tray"].iter().any(|r| cleaned.eq_ignore_ascii_case(r)) {
        return None;
    }
    Some(cleaned)
}

/// Which services a bot may use, stored in the config as a list:
/// `"enabledServices": ["spotify", "youtube"]`. A missing key means both, so
/// configs from before the setting exist keep working. Entries are matched
/// case-insensitively; unknown ones are ignored (a typo shows up as the
/// service being off, and `validate()` refuses to leave a bot with nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnabledServices {
    pub spotify: bool,
    pub youtube: bool,
}

impl Default for EnabledServices {
    fn default() -> Self {
        Self { spotify: true, youtube: true }
    }
}

impl EnabledServices {
    pub fn allows(self, service: Service) -> bool {
        match service {
            Service::Spotify => self.spotify,
            Service::YouTube => self.youtube,
        }
    }

    /// The single enabled service, when there is exactly one. `None` means
    /// both (or neither, which `validate()` corrects to both).
    pub fn only(self) -> Option<Service> {
        match (self.spotify, self.youtube) {
            (true, false) => Some(Service::Spotify),
            (false, true) => Some(Service::YouTube),
            _ => None,
        }
    }

    /// Human-readable list for logs and the `info` command.
    pub fn display(self) -> &'static str {
        match (self.spotify, self.youtube) {
            (true, false) => "Spotify",
            (false, true) => "YouTube",
            _ => "Spotify, YouTube",
        }
    }
}

impl Serialize for EnabledServices {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut names = Vec::new();
        if self.spotify {
            names.push("spotify");
        }
        if self.youtube {
            names.push("youtube");
        }
        names.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EnabledServices {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let names = Vec::<String>::deserialize(deserializer)?;
        let mut services = Self { spotify: false, youtube: false };
        for name in &names {
            match name.trim().to_lowercase().as_str() {
                "spotify" | "sp" => services.spotify = true,
                "youtube" | "yt" => services.youtube = true,
                _ => {}
            }
        }
        Ok(services)
    }
}

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

    // Which services this bot may use at all, e.g. ["spotify", "youtube"].
    // Both on by default. A bot on someone else's server can be limited to
    // YouTube so the owner's Spotify account is never reachable from it: with
    // Spotify off the runner skips auth entirely and /sp answers that the
    // service is not enabled here.
    #[serde(default, rename = "enabledServices")]
    pub enabled_services: EnabledServices,

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
            enabled_services: EnabledServices::default(),
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
        let config: Self = serde_json::from_str(&contents)
            .map_err(|e| BotError::Config(format!("Failed to parse {}: {e}", path.display())))?;
        // Every field carries a serde default, so JSON that merely parses —
        // `{}`, a truncated file, some other program's config — used to load as
        // "connect to localhost" (the default host) and got as far as a live
        // connection attempt before failing with a timeout that explained
        // nothing about the real problem.
        //
        // The test is whether the file actually says where the server is, not
        // whether the parsed value looks plausible: a defaulted host is
        // indistinguishable from one someone typed. Only `host` is required —
        // a blank username is a real configuration on servers that allow
        // anonymous logins, so that stays the bot's business.
        if !names_a_server(&contents) {
            return Err(BotError::Config(format!(
                "{} does not name a server, so it is not a bot config. \
                 If you meant a different file, point --config at it.",
                path.display()
            )));
        }
        Ok(config)
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

    /// Same, without logging the corrections. For commands that only want to
    /// read a config to describe it: their output is a report someone is
    /// reading, and a log line lands in the middle of it.
    pub fn inspect(path: &str) -> Result<Self, BotError> {
        let mut config = Self::parse_file(Path::new(path))?;
        let _ = config.validate();
        Ok(config)
    }

    /// Load config for startup. If the file is missing, offer the interactive
    /// setup wizard (blocks on stdin — startup only, never from a worker task).
    /// Non-interactive contexts (systemd: stdin is /dev/null) skip the prompt
    /// and fail immediately with a clear error, so a missing config becomes a
    /// clean exit instead of a hung or crash-looping service.
    pub fn load(path: &str) -> Result<Self, BotError> {
        use std::io::IsTerminal;
        let path_ref = Path::new(path);
        if !path_ref.exists() {
            // Only when there is going to be a question: without a terminal
            // this printed the same complaint the returned error carries, so
            // the missing file was reported twice, in two wordings.
            if std::io::stdin().is_terminal() {
                eprintln!("Config file not found: {}", path_ref.display());
                eprint!("Would you like to run the setup wizard? [y/N] ");
                use std::io::Write;
                std::io::stderr().flush().ok();
                let mut input = String::new();
                if std::io::stdin().read_line(&mut input).is_ok()
                    && matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
                {
                    // offer_service = false: this path continues into running the
                    // bot in the foreground; also starting a systemd instance
                    // would run the same config twice.
                    // Load exactly the file the wizard created (None = user
                    // cancelled). Guessing via list_configs().first() used to
                    // pick the alphabetically-first config instead — a
                    // different bot on a different server.
                    if let Some(created_path) = crate::wizard::run_wizard(None, false)? {
                        let mut config = Self::parse_file(&created_path)
                            .map_err(|e| BotError::Config(format!("Failed to load created config: {e}")))?;
                        for warning in config.validate() {
                            tracing::warn!("Config: {warning}");
                        }
                        return Ok(config);
                    }
                }
            }
            return Err(BotError::Usage(format!(
                "There is no config file at {}.\nTo make one, {}",
                path_ref.display(),
                crate::hints::create_bot()
            )));
        }
        Self::load_noninteractive(path)
    }

    /// Clamp out-of-range fields to sane values, returning a list of the
    /// corrections made (for logging). Keeps a hand-edited config from putting
    /// the bot into an unusable state (e.g. volume above the cap, port 0).
    pub fn validate(&mut self) -> Vec<String> {
        // Only corrections belong here — every caller prints these as things it
        // changed. Advice about a config that is merely unusual goes where it
        // can be acted on, not into a list of clamps.
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
        // A bot with no services cannot do anything; treat it as a mistake
        // (also what an enabledServices list full of typos degrades to).
        if self.enabled_services.only().is_none()
            && !(self.enabled_services.spotify && self.enabled_services.youtube)
        {
            warnings.push("enabledServices lists no known service, enabling both".to_string());
            self.enabled_services = EnabledServices::default();
        }
        // The default service must be one the bot may use — everything that
        // gates on "Spotify is disabled here" assumes the active service can
        // never start on, or switch to, a disabled one. With exactly one
        // service enabled the default IS that service, no questions asked.
        // Silent, deliberately: with one service enabled there is no other
        // answer this could have, so warning about it told every YouTube-only
        // bot off for a setting it never made.
        if let Some(only) = self.enabled_services.only() {
            self.default_service = only;
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
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| BotError::Config(format!("Failed to serialize config: {e}")))?;
        crate::paths::write_atomic(path, json.as_bytes())?;
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
        match BotConfig::parse_file(&self.path) {
            Ok(on_disk) => *guard = on_disk,
            // Rewriting the file from memory here would do the opposite of
            // what this method promises. The file is unreadable at this
            // instant for a reason — most likely someone is part-way through
            // saving an edit in a text editor, which truncates and rewrites —
            // and stamping the cached copy over it would silently discard
            // whatever they were writing.
            Err(e) => {
                tracing::warn!(
                    "Not saving {}: it could not be read back ({e}). \
                     Runtime changes will be saved once it parses again.",
                    self.path.display()
                );
                f(&mut guard);
                return;
            }
        }
        f(&mut guard);
        // A config file that is no longer there belongs to a bot someone is
        // removing. Saving would write it straight back — the bot persists its
        // volume and modes as they change and again on the way out, so a
        // deleted bot would reappear seconds later and be running again after
        // the next login. (Deleting between this check and the write is still
        // possible, but that window is microseconds rather than the lifetime
        // of a running bot.)
        if !self.path.exists() {
            tracing::info!(
                "Not saving {}: the config file has been deleted",
                self.path.display()
            );
            return;
        }
        if let Err(e) = guard.save(&self.path) {
            tracing::error!("Failed to save config {}: {e}", self.path.display());
        }
    }
}

#[cfg(test)]
mod essentials_tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ttspotify_essentials_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn json_that_is_not_a_bot_config_is_refused() {
        // Every field has a serde default, so this parses cleanly and used to
        // become "connect to localhost with no username" — getting as far as a
        // real connection attempt before failing with a timeout.
        let dir = scratch("notaconfig");
        let path = dir.join("notaconfig.json");
        std::fs::write(&path, r#"{"foo": "bar", "num": 42}"#).unwrap();

        let err = BotConfig::parse_file(&path).unwrap_err().to_string();
        assert!(err.contains("does not name a server"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_config_naming_no_server_is_refused_even_though_host_has_a_default() {
        // `{}` parses into a config whose host is "localhost" purely from the
        // serde default, which is why checking the parsed value would let it
        // through and start a connection attempt.
        let dir = scratch("empty");
        let path = dir.join("empty.json");
        std::fs::write(&path, "{}").unwrap();
        assert!(BotConfig::parse_file(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blank_username_is_still_allowed() {
        // Some servers take anonymous logins, so an empty username is a real
        // configuration rather than a broken file.
        let dir = scratch("anon");
        let path = dir.join("anon.json");
        std::fs::write(&path, r#"{"host": "tt.example.org", "username": ""}"#).unwrap();

        assert!(BotConfig::parse_file(&path).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_config_still_loads() {
        let dir = scratch("good");
        let path = dir.join("good.json");
        std::fs::write(&path, r#"{"host": "tt.example.org", "username": "bot"}"#).unwrap();

        let cfg = BotConfig::parse_file(&path).unwrap();
        assert_eq!(cfg.host, "tt.example.org");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ttspotify_store_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn update_writes_the_change_through_to_disk() {
        let dir = scratch("writes");
        let path = dir.join("home.json");
        let cfg = BotConfig {
            host: "tt.example.org".to_string(),
            ..Default::default()
        };
        cfg.save(&path).unwrap();

        let store = ConfigStore::new(path.clone(), cfg);
        store.update(|c| c.volume = 42);

        let reloaded = BotConfig::parse_file(&path).unwrap();
        assert_eq!(reloaded.volume, 42);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_does_not_overwrite_a_config_it_could_not_read() {
        // Saving from a text editor truncates and rewrites, so a runtime save
        // landing in that window reads a broken file. Writing the cached copy
        // over it would throw away the edit being typed.
        let dir = scratch("unreadable");
        let path = dir.join("mid-edit.json");
        let cfg = BotConfig {
            host: "tt.example.org".to_string(),
            ..Default::default()
        };
        cfg.save(&path).unwrap();

        let store = ConfigStore::new(path.clone(), cfg);
        std::fs::write(&path, "{ \"host\": \"tt.exa").unwrap(); // half-written
        store.update(|c| c.volume = 99);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ \"host\": \"tt.exa",
            "a file that could not be parsed must be left as it is"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_does_not_recreate_a_config_that_was_deleted() {
        // Removing a bot deletes its config while the bot may still be
        // running. The bot saves its volume and modes as they change and again
        // on the way out; if that recreated the file, the bot would come back
        // from the dead and start again at the next login.
        let dir = scratch("deleted");
        let path = dir.join("gone.json");
        let cfg = BotConfig {
            host: "tt.example.org".to_string(),
            ..Default::default()
        };
        cfg.save(&path).unwrap();

        let store = ConfigStore::new(path.clone(), cfg);
        std::fs::remove_file(&path).unwrap();
        store.update(|c| c.volume = 99);

        assert!(!path.exists(), "a deleted config must stay deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod choice_tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["home".to_string(), "work".to_string()]
    }

    #[test]
    fn a_number_picks_the_bot_beside_it() {
        assert_eq!(parse_config_choice("1", &names()), Some(0));
        assert_eq!(parse_config_choice(" 2 \n", &names()), Some(1));
    }

    #[test]
    fn a_name_works_as_well_as_its_number() {
        assert_eq!(parse_config_choice("work", &names()), Some(1));
        assert_eq!(parse_config_choice("elsewhere", &names()), None);
    }

    #[test]
    fn out_of_range_numbers_are_refused_rather_than_clamped() {
        // Starting bot 1 because someone typed 3 would be worse than asking
        // again, and 0 must not wrap round to the last entry.
        assert_eq!(parse_config_choice("3", &names()), None);
        assert_eq!(parse_config_choice("0", &names()), None);
        assert_eq!(parse_config_choice("", &names()), None);
        assert_eq!(parse_config_choice("-1", &names()), None);
    }

    #[test]
    fn one_config_is_not_a_choice_and_none_is_not_an_answer() {
        let one = vec![("home".to_string(), PathBuf::from("/x/home.json"))];
        assert_eq!(
            choose_config(&one, false).as_deref(),
            Some(Path::new("/x/home.json"))
        );
        // Interactivity is irrelevant when there is nothing to choose between.
        assert_eq!(
            choose_config(&one, true).as_deref(),
            Some(Path::new("/x/home.json"))
        );
        assert_eq!(choose_config(&[], true), None);
    }

    #[test]
    fn without_a_terminal_several_configs_still_start_the_first() {
        // Under systemd the unit always passes --config, so this path is a
        // hand-run with no terminal: keep the old behaviour rather than
        // refusing to start, but say which one it took.
        let many = vec![
            ("home".to_string(), PathBuf::from("/x/home.json")),
            ("work".to_string(), PathBuf::from("/x/work.json")),
        ];
        assert_eq!(
            choose_config(&many, false).as_deref(),
            Some(Path::new("/x/home.json"))
        );
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)] // tweaking a couple of fields off Default reads fine in tests
mod tests {

    #[test]
    fn a_service_reads_back_however_it_was_capitalised() {
        // `"youtube"` used to fail the whole parse, and a config that fails to
        // parse does not report a bad field — the bot simply vanishes from
        // every command.
        for spelling in ["YouTube", "youtube", "YOUTUBE", " yt "] {
            let json = format!("{{\"host\":\"h\",\"defaultService\":\"{spelling}\"}}");
            let cfg: BotConfig = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("{spelling} should parse: {e}"));
            assert_eq!(cfg.default_service, Service::YouTube);
        }
    }

    #[test]
    fn a_service_that_names_nothing_is_still_refused() {
        let json = "{\"host\":\"h\",\"defaultService\":\"soundcloud\"}";
        let err = serde_json::from_str::<BotConfig>(json).unwrap_err().to_string();
        assert!(err.contains("soundcloud"), "the message should quote the value: {err}");
    }

    #[test]
    fn a_broken_config_says_what_is_wrong_with_it() {
        let dir = std::env::temp_dir()
            .join(format!("ttspotify_problems_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("empty.json"), "").unwrap();
        std::fs::write(dir.join("junk.json"), "not json").unwrap();
        std::fs::write(dir.join("nohost.json"), "{\"tcpPort\":1}").unwrap();
        std::fs::write(dir.join("fine.json"), "{\"host\":\"h\"}").unwrap();

        let (configs, problems) = list_configs_and_problems_in(&dir);
        assert_eq!(configs.len(), 1);
        assert_eq!(problems.len(), 3);
        // Every problem names its file and says something about the cause;
        // "skipping invalid or incomplete config file" said neither.
        assert!(problems.iter().any(|p| p.starts_with("empty.json") && p.contains("empty")));
        assert!(problems.iter().any(|p| p.starts_with("junk.json") && p.contains("not valid")));
        assert!(problems.iter().any(|p| p.starts_with("nohost.json") && p.contains("host")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_too_long_for_a_service_is_refused_up_front() {
        // Saved fine, listed fine, and then could never start: systemd cannot
        // address a unit name that long, and said so in a screenful of the
        // name itself.
        assert_eq!(sanitise_config_name(&"a".repeat(61)), None);
        assert_eq!(sanitise_config_name(&"a".repeat(60)), Some("a".repeat(60)));
    }

    #[test]
    fn control_characters_never_reach_a_file_name() {
        assert_eq!(sanitise_config_name("one
two"), Some("onetwo".to_string()));
        assert_eq!(sanitise_config_name("tab	here"), Some("tabhere".to_string()));
        assert_eq!(sanitise_config_name("
	"), None);
    }
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

    #[test]
    fn all_is_reserved_because_the_commands_use_it_for_every_bot() {
        // A bot called "all" could never be started on its own: start, stop and
        // restart read that word as "the whole fleet", so every command aimed
        // at it would hit the others too.
        assert_eq!(sanitise_config_name("all"), None);
        assert_eq!(sanitise_config_name("ALL"), None);
        assert_eq!(sanitise_config_name(" All "), None);
        // The tray keeps its own log in logs/tray/, and "delete this bot's
        // logs" would take the tray's with it.
        assert_eq!(sanitise_config_name("tray"), None);
        // Names that merely contain it are fine.
        assert_eq!(sanitise_config_name("hall"), Some("hall".to_string()));
        assert_eq!(sanitise_config_name("all-servers"), Some("all-servers".to_string()));
    }

    #[test]
    fn services_default_on_and_survive_old_configs() {
        // An old config file has no enabledServices key; both must come up
        // enabled or every existing install would lose a service on upgrade.
        let cfg: BotConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.enabled_services.spotify);
        assert!(cfg.enabled_services.youtube);
    }

    #[test]
    fn the_enabled_services_list_reads_and_writes_names() {
        let yt_only: BotConfig =
            serde_json::from_str(r#"{"enabledServices": ["YouTube"]}"#).unwrap();
        assert!(!yt_only.enabled_services.spotify, "matching is case-insensitive");
        assert!(yt_only.enabled_services.youtube);
        assert_eq!(yt_only.enabled_services.only(), Some(Service::YouTube));

        let json = serde_json::to_value(&yt_only).unwrap();
        assert_eq!(json["enabledServices"], serde_json::json!(["youtube"]));

        let both = BotConfig::default();
        let json = serde_json::to_value(&both).unwrap();
        assert_eq!(json["enabledServices"], serde_json::json!(["spotify", "youtube"]));
    }

    #[test]
    fn validate_reenables_services_when_none_are_left() {
        // An empty list, or a list full of typos, degrades to nothing
        // enabled — a bot that can play nothing helps nobody.
        let mut cfg: BotConfig =
            serde_json::from_str(r#"{"enabledServices": ["spotfy"]}"#).unwrap();
        let warnings = cfg.validate();
        assert!(cfg.enabled_services.spotify && cfg.enabled_services.youtube);
        assert!(!warnings.is_empty());
    }

    #[test]
    fn validate_moves_the_default_service_off_a_disabled_one() {
        // Everything downstream assumes the active service can never start on
        // a disabled one; validate is what upholds that.
        let mut cfg = BotConfig::default();
        cfg.enabled_services = EnabledServices { spotify: false, youtube: true };
        cfg.default_service = Service::Spotify;
        let warnings = cfg.validate();
        assert_eq!(cfg.default_service, Service::YouTube);
        // Silently: with one service enabled there is no other answer, so
        // there is nothing to tell anyone about.
        assert!(warnings.is_empty());

        let mut cfg = BotConfig::default();
        cfg.enabled_services = EnabledServices { spotify: true, youtube: false };
        cfg.default_service = Service::YouTube;
        cfg.validate();
        assert_eq!(cfg.default_service, Service::Spotify);
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
        cfg.host = "tt.example.org".to_string();
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
        let cfg = BotConfig {
            host: "tt.example.org".to_string(),
            ..Default::default()
        };
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
        std::fs::write(p.join("blank.json"), "{}").unwrap(); // parses to defaults, names no server
        std::fs::write(p.join("settings.json"), r#"{"host":"h","username":"u"}"#).unwrap(); // name skip-list
        // Names a server but has no username. This is listed: it is either an
        // anonymous login or a half-finished bot, and either way hiding it
        // means it cannot be started, edited or removed by name while still
        // running perfectly well via --config.
        std::fs::write(p.join("nouser.json"), r#"{"host":"h"}"#).unwrap();

        let listed: Vec<String> = list_configs_in(p).into_iter().map(|(name, _)| name).collect();
        assert_eq!(listed, vec!["good".to_string(), "nouser".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_configs_skips_lang_prefs_by_name() {
        let dir = std::env::temp_dir().join(format!("ttspotify_cfglangprefs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.as_path();
        let mut good = BotConfig::default();
        good.host = "srv.example.com".to_string();
        good.username = "botacct".to_string();
        good.save(&p.join("good.json")).unwrap();
        // lang_prefs.json is the i18n per-user language store, not a bot config.
        // Even content that would pass config validation must be skipped by name.
        std::fs::write(p.join("lang_prefs.json"), r#"{"host":"h","username":"u"}"#).unwrap();

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
