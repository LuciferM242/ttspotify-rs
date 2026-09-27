//! `edit`: change an existing config through a numbered menu. Every prompt
//! starts at the current value, and Enter keeps it.

use crate::config::{AdminMode, BotConfig, EnabledServices};
use crate::error::BotError;
use crate::services::Service;
use crate::wizard::{ask, ask_bool, ask_choice, ask_kick_delay, ask_locale, GENDERS};
use crate::youtube::locale;

const QUALITIES: [&str; 3] = ["VERY_HIGH", "HIGH", "NORMAL"];
const NORM_TYPES: [&str; 3] = ["auto", "album", "track"];
const NORM_METHODS: [&str; 2] = ["dynamic", "basic"];

/// Read a number, where empty keeps the current one.
pub fn answer_number<T: std::str::FromStr>(input: &str, current: T) -> Option<T> {
    let input = input.trim();
    if input.is_empty() {
        return Some(current);
    }
    input.parse().ok()
}

fn ask_number<T>(prompt: &str, current: T) -> Option<T>
where
    T: std::str::FromStr + std::fmt::Display + Copy,
{
    loop {
        let raw = ask(prompt, &current.to_string(), false)?;
        match answer_number(&raw, current) {
            Some(value) => return Some(value),
            None => println!("    Expected a number."),
        }
    }
}

/// Passwords are not echoed back into the prompt as a default the way every
/// other field is; "unchanged" is shown instead, and a single `-` clears it.
fn ask_secret(prompt: &str, current: &str) -> Option<String> {
    let shown = if current.is_empty() { "none set" } else { "unchanged" };
    let raw = ask(&format!("{prompt} [Enter keeps {shown}, - clears it]"), "", false)?;
    Some(answer_clearable(&raw, current))
}

/// An optional field: Enter keeps it and `-` empties it.
fn ask_clearable(prompt: &str, current: &str) -> Option<String> {
    let prompt = if current.is_empty() { format!("{prompt} (optional)") } else { format!("{prompt} (- clears it)") };
    let raw = ask(&prompt, current, false)?;
    Some(answer_clearable(&raw, current))
}

pub fn answer_clearable(input: &str, current: &str) -> String {
    match input.trim() {
        "" => current.to_string(),
        "-" => String::new(),
        other => other.to_string(),
    }
}

/// `edit [name]`.
pub fn run(name: Option<&str>) -> Result<(), BotError> {
    let configs = crate::config::list_configs();
    if configs.is_empty() {
        return Err(BotError::Usage(format!(
            "No configs to edit. To create one, {}",
            crate::hints::create_bot()
        )));
    }

    let (name, path) = match name {
        Some(wanted) => configs
            .iter()
            .find(|(n, _)| n == wanted)
            .cloned()
            .ok_or_else(|| {
                BotError::Usage(format!(
                    "No config named \"{wanted}\". Available: {}.",
                    configs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
                ))
            })?,
        None => match pick_config(&configs) {
            Some(chosen) => chosen,
            None => return Ok(()),
        },
    };

    let mut config = BotConfig::load_noninteractive(&path.to_string_lossy())?;
    let original = config.clone();
    println!();
    println!("Editing \"{name}\" ({})", path.display());

    // The menu is shown when it changes what you can do; a bad answer only
    // re-asks, so one stray Enter is not eight lines to listen through again.
    let mut show_menu = true;
    loop {
        if show_menu {
            println!();
            println!("  1. Server, login and license");
            println!("  2. Bot name, channel, language and kicks");
            println!("  3. Who may run admin commands");
            println!("  4. Services and YouTube");
            println!("  5. Audio");
            println!("  6. Radio and search");
            println!("  7. Save and quit");
            println!("  8. Quit without saving");
        }
        show_menu = true;

        let Some(choice) = ask("Choose", "", false) else {
            println!("Nothing saved.");
            return Ok(());
        };

        match choice.trim() {
            "1" => edit_server(&mut config),
            "2" => edit_identity(&mut config),
            "3" => edit_admins(&mut config),
            "4" => edit_services(&mut config),
            "5" => edit_audio(&mut config),
            "6" => edit_radio(&mut config),
            "7" => return save(&mut config, &name, &path),
            "8" => {
                if config == original || ask_bool("Leave without saving your changes", false) != Some(false) {
                    println!("Nothing saved.");
                    return Ok(());
                }
            }
            "" => {
                println!("  Type a number from 1 to 8, or 8 to leave without saving.");
                show_menu = false;
            }
            other => {
                println!("  \"{other}\" is not one of the choices. Type a number from 1 to 8.");
                show_menu = false;
            }
        }
    }
}

fn pick_config(configs: &[(String, std::path::PathBuf)]) -> Option<(String, std::path::PathBuf)> {
    let names: Vec<String> = configs.iter().map(|(n, _)| n.clone()).collect();
    println!("Which bot do you want to edit?");
    for (i, name) in names.iter().enumerate() {
        println!("  {}. {name}", i + 1);
    }
    for _ in 0..3 {
        let raw = ask("Number or name", "", false)?;
        if let Some(index) = crate::config::parse_config_choice(&raw, &names) {
            return Some(configs[index].clone());
        }
        println!("  Not one of the choices.");
    }
    None
}

/// Write the file, then offer what the bot still needs: tools or a sign-in
/// for a service it now offers, and a restart, since a running bot keeps the
/// settings it started with.
fn save(config: &mut BotConfig, name: &str, path: &std::path::Path) -> Result<(), BotError> {
    for warning in config.validate() {
        println!("  Adjusted: {warning}");
    }
    config.save(path)?;
    println!("Saved to {}", path.display());

    offer_missing_setup(config);

    let unit = format!("ttspotify@{}.service", crate::service::systemd_escape_instance(name));
    if crate::service::running_bot_units().contains(&unit) {
        println!();
        println!("\"{name}\" is running and still using the old settings.");
        if crate::service::prompt_yes_no("Restart it now?") {
            crate::control::control("restart", name)?;
        } else {
            println!("To restart it later, {}", crate::hints::restart_bot(name));
        }
    }
    Ok(())
}

fn offer_missing_setup(config: &BotConfig) {
    let youtube_installed = crate::youtube::setup::resolve_paths()
        .map(|p| crate::youtube::setup::is_installed(&p))
        .unwrap_or(false);
    if config.enabled_services.youtube && !youtube_installed {
        println!();
        if ask_bool("YouTube tools are not installed, so YouTube cannot play. Install them now", true) == Some(true) {
            if let Err(e) = crate::wizard::run_youtube_setup() {
                println!("  YouTube setup failed: {e}");
                println!("  To retry later, {}", crate::hints::install_youtube_tools());
            }
        } else {
            println!("  To install them later, {}", crate::hints::install_youtube_tools());
        }
    }
    if config.enabled_services.spotify && !crate::spotify::auth::SpotifyAuth::new().has_cached_credentials() {
        println!();
        println!("Spotify is not signed in on this machine. To sign in, {}", crate::hints::sign_in_spotify());
    }
}

// Each section edits a copy and writes it back only once every question is
// answered, so backing out part way leaves the config as it was.

fn edit_server(config: &mut BotConfig) {
    let mut edited = config.clone();
    let Some(host) = ask("Server address", &config.host, true) else { return };
    let Some(tcp) = ask_number("TCP port", config.tcp_port) else { return };
    let Some(udp) = ask_number("UDP port", config.udp_port) else { return };
    let Some(encrypted) = ask_bool("Encrypted connection", config.encrypted) else { return };
    let Some(username) = ask("Bot username", &config.username, true) else { return };
    let Some(password) = ask_secret("Bot password", &config.password) else { return };
    let Some(license_name) = ask_clearable("License name", config.license_name.as_deref().unwrap_or("")) else {
        return;
    };
    let Some(license_key) = ask_clearable("License key", config.license_key.as_deref().unwrap_or("")) else {
        return;
    };

    edited.host = host;
    edited.tcp_port = tcp;
    edited.udp_port = udp;
    edited.encrypted = encrypted;
    edited.username = username;
    edited.password = password;
    edited.license_name = Some(license_name).filter(|s| !s.is_empty());
    edited.license_key = Some(license_key).filter(|s| !s.is_empty());
    *config = edited;
}

fn edit_identity(config: &mut BotConfig) {
    let mut edited = config.clone();
    let Some(bot_name) = ask("Bot nickname", &config.bot_name, true) else { return };
    let Some(gender) = ask_choice("Bot gender", &GENDERS, &config.bot_gender) else { return };
    let Some(channel) = ask("Channel to join", &config.channel_name, false) else { return };
    let Some(channel_password) = ask_secret("Channel password", &config.channel_password) else {
        return;
    };
    let languages = crate::i18n::installed_language_codes(&crate::config::config_dir());
    let options: Vec<&str> = languages.iter().map(|s| s.as_str()).collect();
    let Some(language) = ask_choice("Language the bot answers in", &options, &config.default_language) else {
        return;
    };
    let Some(rejoin) = ask_kick_delay(config.rejoin_after_kick_seconds) else { return };

    edited.bot_name = bot_name;
    edited.bot_gender = gender;
    edited.channel_name = if channel.is_empty() { "/".to_string() } else { channel };
    edited.channel_password = channel_password;
    edited.default_language = language;
    edited.rejoin_after_kick_seconds = rejoin;
    *config = edited;
}

fn edit_admins(config: &mut BotConfig) {
    let current = match config.admin_mode {
        AdminMode::Everyone => "everyone",
        AdminMode::TtRights => "teamtalk-admins",
        AdminMode::List => "list",
        AdminMode::Both => "both",
    };
    let options = ["everyone", "teamtalk-admins", "list", "both"];
    let Some(mode) = ask_choice("Who may run admin commands", &options, current) else { return };

    let mode = match mode.as_str() {
        "everyone" => AdminMode::Everyone,
        "teamtalk-admins" => AdminMode::TtRights,
        "list" => AdminMode::List,
        _ => AdminMode::Both,
    };

    if matches!(mode, AdminMode::List | AdminMode::Both) {
        let current = config.admins.join(", ");
        let Some(list) = ask("Admin usernames (comma separated)", &current, false) else { return };
        config.admins = crate::bot::auth::parse_admin_list(&list);
    } else {
        // The list is kept rather than cleared: switching away from it and
        // back should not cost you the names you typed.
        println!("  (the username list is kept, but not used in this mode)");
    }
    config.admin_mode = mode;
}

fn edit_services(config: &mut BotConfig) {
    let mut edited = config.clone();
    let current = match (config.enabled_services.spotify, config.enabled_services.youtube) {
        (true, false) => "spotify",
        (false, true) => "youtube",
        _ => "both",
    };
    let Some(enabled) = ask_choice("Services this bot offers", &["both", "spotify", "youtube"], current)
    else {
        return;
    };
    edited.enabled_services = match enabled.as_str() {
        "spotify" => EnabledServices { spotify: true, youtube: false },
        "youtube" => EnabledServices { spotify: false, youtube: true },
        _ => EnabledServices::default(),
    };

    // With one service enabled there is nothing to choose: validate() would
    // move the default onto it anyway.
    edited.default_service = match edited.enabled_services.only() {
        Some(only) => only,
        None => {
            let current = match config.default_service {
                Service::Spotify => "spotify",
                Service::YouTube => "youtube",
            };
            let Some(service) = ask_choice("Which service bare commands use", &["spotify", "youtube"], current)
            else {
                return;
            };
            Service::parse_or_default(&service)
        }
    };

    if edited.enabled_services.youtube {
        let Some(cookies) = ask_clearable("YouTube cookies file", &config.youtube_cookies_file) else {
            return;
        };
        if !cookies.is_empty() && !std::path::Path::new(&cookies).is_file() {
            println!("  Warning: {cookies} does not exist yet.");
        }
        let Some(country) = ask_locale(
            "YouTube search location",
            crate::wizard::SEARCH_LOCATION_INTRO,
            &config.youtube_country,
            &locale::country_options(),
            locale::search_countries,
        ) else {
            return;
        };
        let Some(language) = ask_locale(
            "YouTube language",
            "",
            &config.youtube_language,
            &locale::language_options(),
            locale::search_languages,
        ) else {
            return;
        };
        edited.youtube_cookies_file = cookies;
        edited.youtube_country = country;
        edited.youtube_language = language;
    }
    *config = edited;
}

fn edit_audio(config: &mut BotConfig) {
    let mut edited = config.clone();
    // Quality and normalisation only reach librespot.
    if config.enabled_services.spotify {
        let Some(quality) = ask_choice("Spotify quality", &QUALITIES, &config.spotify_quality) else {
            return;
        };
        let Some(normalize) = ask_bool("Volume normalisation", config.spotify_enable_normalization) else {
            return;
        };
        edited.spotify_quality = quality;
        edited.spotify_enable_normalization = normalize;

        if normalize {
            let Some(kind) = ask_choice("Normalisation type", &NORM_TYPES, &config.normalisation_type) else {
                return;
            };
            let Some(method) = ask_choice("Normalisation method", &NORM_METHODS, &config.normalisation_method)
            else {
                return;
            };
            let Some(pregain) = ask_number("Pregain (dB)", config.normalisation_pregain_db) else { return };
            let Some(threshold) = ask_number("Threshold (dBFS)", config.normalisation_threshold_dbfs) else {
                return;
            };
            let Some(knee) = ask_number("Knee (dB)", config.normalisation_knee_db) else { return };
            edited.normalisation_type = kind;
            edited.normalisation_method = method;
            edited.normalisation_pregain_db = pregain;
            edited.normalisation_threshold_dbfs = threshold;
            edited.normalisation_knee_db = knee;
        }
    }

    let Some(volume) = ask_number("Starting volume", config.volume) else { return };
    let Some(max_volume) = ask_number("Maximum volume", config.max_volume) else { return };
    let Some(jitter) = ask_number("Jitter buffer (ms)", config.jitter_buffer_ms) else { return };
    let Some(ramp) = ask_number("Volume ramp step", config.volume_ramp_step) else { return };

    edited.volume = volume;
    edited.max_volume = max_volume;
    edited.jitter_buffer_ms = jitter;
    edited.volume_ramp_step = ramp;
    *config = edited;
}

fn edit_radio(config: &mut BotConfig) {
    let mut edited = config.clone();
    let Some(enabled) = ask_bool("Radio enabled", config.radio_enabled) else { return };
    edited.radio_enabled = enabled;
    if enabled {
        let Some(batch) = ask_number("Tracks per radio batch", config.radio_batch_size) else { return };
        let Some(delay) = ask_number("Delay between batches (seconds)", config.radio_delay) else {
            return;
        };
        edited.radio_batch_size = batch;
        edited.radio_delay = delay;
    }
    let Some(limit) = ask_number("Search results to show", config.search_limit) else { return };
    edited.search_limit = limit;
    *config = edited;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_answers_keep_what_is_already_there() {
        assert_eq!(answer_number("", 42u8), Some(42));
        assert_eq!(answer_clearable("", "cookies.txt"), "cookies.txt");
    }

    #[test]
    fn a_dash_clears_an_optional_field() {
        assert_eq!(answer_clearable(" - ", "cookies.txt"), "");
        assert_eq!(answer_clearable(" other.txt ", "cookies.txt"), "other.txt");
    }

    #[test]
    fn numbers_are_parsed_in_the_type_the_field_uses() {
        assert_eq!(answer_number("200", 100u8), Some(200));
        // A u8 field cannot hold 300, and the prompt asks again rather than
        // wrapping it round to 44.
        assert_eq!(answer_number::<u8>("300", 100), None);
        assert_eq!(answer_number("1.5", 0.25f32), Some(1.5));
        assert_eq!(answer_number::<u32>("abc", 5), None);
    }
}
