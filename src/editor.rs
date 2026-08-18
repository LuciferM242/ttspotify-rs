//! `--edit`: change a config that already exists.
//!
//! Windows has a tabbed editor with every field on it. Linux had `--setup`,
//! which builds a config from scratch from the defaults — so changing one
//! number meant retyping the server, the login and the channel, and the fields
//! the wizard never asks about (audio quality, normalisation, jitter buffer,
//! radio batching) could only be changed by hand-editing JSON.
//!
//! This is the same set of fields as the Windows editor, as a numbered menu.
//! Every prompt is seeded with the current value and Enter keeps it, so the
//! cost of changing one setting is one number and one answer.

use crate::config::{AdminMode, BotConfig, EnabledServices};
use crate::error::BotError;
use crate::services::Service;
use crate::wizard::ask;

const GENDERS: [&str; 3] = ["neutral", "male", "female"];
const QUALITIES: [&str; 3] = ["VERY_HIGH", "HIGH", "NORMAL"];
const NORM_TYPES: [&str; 3] = ["auto", "album", "track"];
const NORM_METHODS: [&str; 2] = ["dynamic", "basic"];

/// Read a yes/no answer, where empty means "leave it as it is".
pub fn answer_bool(input: &str, current: bool) -> Option<bool> {
    match input.trim().to_lowercase().as_str() {
        "" => Some(current),
        "y" | "yes" | "on" | "true" => Some(true),
        "n" | "no" | "off" | "false" => Some(false),
        _ => None,
    }
}

/// Read a choice from a fixed list, by number or by name. Empty keeps the
/// current value, and an answer that matches nothing is rejected rather than
/// quietly writing an invalid setting into the config.
pub fn answer_choice(input: &str, options: &[&str], current: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return Some(current.to_string());
    }
    if let Ok(number) = input.parse::<usize>() {
        return number
            .checked_sub(1)
            .and_then(|i| options.get(i))
            .map(|s| s.to_string());
    }
    options
        .iter()
        .find(|o| o.eq_ignore_ascii_case(input))
        .map(|s| s.to_string())
}

/// Read a number, where empty keeps the current one.
pub fn answer_number<T: std::str::FromStr>(input: &str, current: T) -> Option<T> {
    let input = input.trim();
    if input.is_empty() {
        return Some(current);
    }
    input.parse().ok()
}

/// Prompt until the answer parses, or the user gives up (EOF).
fn ask_bool(prompt: &str, current: bool) -> Option<bool> {
    loop {
        let raw = ask(&format!("{prompt} (y/n)"), if current { "y" } else { "n" }, false)?;
        match answer_bool(&raw, current) {
            Some(value) => return Some(value),
            None => println!("    Answer y or n."),
        }
    }
}

fn ask_choice(prompt: &str, options: &[&str], current: &str) -> Option<String> {
    println!("  {prompt}:");
    for (i, option) in options.iter().enumerate() {
        println!("    {}. {option}", i + 1);
    }
    loop {
        let raw = ask("Number or name", current, false)?;
        match answer_choice(&raw, options, current) {
            Some(value) => return Some(value),
            None => println!("    Not one of the choices."),
        }
    }
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
    match raw.trim() {
        "" => Some(current.to_string()),
        "-" => Some(String::new()),
        other => Some(other.to_string()),
    }
}

/// `--edit [name]`.
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
    println!();
    println!("Editing \"{name}\" ({})", path.display());

    loop {
        println!();
        println!("  1. Server and login");
        println!("  2. Bot name, channel and language");
        println!("  3. Who may run admin commands");
        println!("  4. Services and cookies");
        println!("  5. Audio");
        println!("  6. Radio and search");
        println!("  7. Save and quit");
        println!("  8. Quit without saving");

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
                println!("Nothing saved.");
                return Ok(());
            }
            other => println!("  \"{other}\" is not one of the choices."),
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

/// Write the file, then offer to restart the bot: an edited config does
/// nothing until the bot rereads it, which is not obvious from the outside.
fn save(config: &mut BotConfig, name: &str, path: &std::path::Path) -> Result<(), BotError> {
    for warning in config.validate() {
        println!("  Adjusted: {warning}");
    }
    config.save(path)?;
    println!("Saved to {}", path.display());

    let unit = format!("ttspotify@{}.service", crate::service::systemd_escape_instance(name));
    if crate::service::running_bot_units().contains(&unit) {
        println!();
        println!("\"{name}\" is running and still using the old settings.");
        if crate::service::prompt_yes_no("Restart it now?") {
            crate::control::control("restart", name)?;
        } else {
            println!(
                "To restart it later, {}",
                crate::hints::restart_bot(name)
            );
        }
    }
    Ok(())
}

fn edit_server(config: &mut BotConfig) {
    let Some(host) = ask("Server address", &config.host, true) else { return };
    let Some(tcp) = ask_number("TCP port", config.tcp_port) else { return };
    let Some(udp) = ask_number("UDP port", config.udp_port) else { return };
    let Some(encrypted) = ask_bool("Encrypted connection", config.encrypted) else { return };
    let Some(username) = ask("Bot username", &config.username, true) else { return };
    let Some(password) = ask_secret("Bot password", &config.password) else { return };

    config.host = host;
    config.tcp_port = tcp;
    config.udp_port = udp;
    config.encrypted = encrypted;
    config.username = username;
    config.password = password;
}

fn edit_identity(config: &mut BotConfig) {
    let Some(bot_name) = ask("Bot nickname", &config.bot_name, true) else { return };
    let Some(gender) = ask_choice("Bot gender", &GENDERS, &config.bot_gender) else { return };
    let Some(channel) = ask("Channel to join", &config.channel_name, false) else { return };
    let Some(channel_password) = ask_secret("Channel password", &config.channel_password) else {
        return;
    };
    let languages = crate::i18n::installed_language_codes(&crate::config::config_dir());
    let options: Vec<&str> = languages.iter().map(|s| s.as_str()).collect();
    let Some(language) = ask_choice("Default language", &options, &config.default_language) else {
        return;
    };

    config.bot_name = bot_name;
    config.bot_gender = gender;
    config.channel_name = if channel.is_empty() { "/".to_string() } else { channel };
    config.channel_password = channel_password;
    config.default_language = language;
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

    config.admin_mode = match mode.as_str() {
        "everyone" => AdminMode::Everyone,
        "teamtalk-admins" => AdminMode::TtRights,
        "list" => AdminMode::List,
        _ => AdminMode::Both,
    };

    if matches!(config.admin_mode, AdminMode::List | AdminMode::Both) {
        let current = config.admins.join(", ");
        let Some(list) = ask("Admin usernames (comma separated)", &current, false) else { return };
        config.admins = crate::bot::auth::parse_admin_list(&list);
    } else {
        // The list is kept rather than cleared: switching away from it and
        // back should not cost you the names you typed.
        println!("  (the username list is kept, but not used in this mode)");
    }
}

fn edit_services(config: &mut BotConfig) {
    let current = match (config.enabled_services.spotify, config.enabled_services.youtube) {
        (true, false) => "spotify",
        (false, true) => "youtube",
        _ => "both",
    };
    let Some(enabled) = ask_choice("Services this bot offers", &["both", "spotify", "youtube"], current)
    else {
        return;
    };
    config.enabled_services = match enabled.as_str() {
        "spotify" => EnabledServices { spotify: true, youtube: false },
        "youtube" => EnabledServices { spotify: false, youtube: true },
        _ => EnabledServices::default(),
    };

    // With one service enabled there is nothing to choose: validate() would
    // move the default onto it anyway.
    config.default_service = match config.enabled_services.only() {
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

    if config.enabled_services.youtube {
        let Some(cookies) = ask("YouTube cookies file (blank for none)", &config.youtube_cookies_file, false)
        else {
            return;
        };
        if !cookies.is_empty() && !std::path::Path::new(&cookies).is_file() {
            println!("  Warning: {cookies} does not exist yet.");
        }
        config.youtube_cookies_file = cookies;
    }
}

fn edit_audio(config: &mut BotConfig) {
    let Some(quality) = ask_choice("Spotify quality", &QUALITIES, &config.spotify_quality) else {
        return;
    };
    let Some(normalize) = ask_bool("Volume normalisation", config.spotify_enable_normalization) else {
        return;
    };
    config.spotify_quality = quality;
    config.spotify_enable_normalization = normalize;

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
        config.normalisation_type = kind;
        config.normalisation_method = method;
        config.normalisation_pregain_db = pregain;
        config.normalisation_threshold_dbfs = threshold;
        config.normalisation_knee_db = knee;
    }

    let Some(volume) = ask_number("Starting volume", config.volume) else { return };
    let Some(max_volume) = ask_number("Maximum volume", config.max_volume) else { return };
    let Some(jitter) = ask_number("Jitter buffer (ms)", config.jitter_buffer_ms) else { return };
    let Some(ramp) = ask_number("Volume ramp step", config.volume_ramp_step) else { return };

    config.volume = volume;
    config.max_volume = max_volume;
    config.jitter_buffer_ms = jitter;
    config.volume_ramp_step = ramp;
}

fn edit_radio(config: &mut BotConfig) {
    let Some(enabled) = ask_bool("Radio enabled", config.radio_enabled) else { return };
    config.radio_enabled = enabled;
    if enabled {
        let Some(batch) = ask_number("Tracks per radio batch", config.radio_batch_size) else { return };
        let Some(delay) = ask_number("Delay between batches (seconds)", config.radio_delay) else {
            return;
        };
        config.radio_batch_size = batch;
        config.radio_delay = delay;
    }
    let Some(limit) = ask_number("Search results to show", config.search_limit) else { return };
    config.search_limit = limit;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_answers_keep_what_is_already_there() {
        // The whole point of the editor: Enter changes nothing.
        assert_eq!(answer_bool("", true), Some(true));
        assert_eq!(answer_bool("  \n", false), Some(false));
        assert_eq!(answer_choice("", &QUALITIES, "HIGH").as_deref(), Some("HIGH"));
        assert_eq!(answer_number("", 42u8), Some(42));
    }

    #[test]
    fn yes_and_no_are_read_in_the_forms_people_type() {
        for yes in ["y", "Y", "yes", "on", "true"] {
            assert_eq!(answer_bool(yes, false), Some(true), "{yes}");
        }
        for no in ["n", "NO", "off", "false"] {
            assert_eq!(answer_bool(no, true), Some(false), "{no}");
        }
        // Anything else is a question, not an answer.
        assert_eq!(answer_bool("maybe", true), None);
    }

    #[test]
    fn a_choice_can_be_its_number_or_its_name() {
        assert_eq!(answer_choice("1", &QUALITIES, "HIGH").as_deref(), Some("VERY_HIGH"));
        assert_eq!(answer_choice("normal", &QUALITIES, "HIGH").as_deref(), Some("NORMAL"));
        assert_eq!(answer_choice("Album", &NORM_TYPES, "auto").as_deref(), Some("album"));
    }

    #[test]
    fn an_invalid_choice_is_refused_rather_than_written_to_the_config() {
        // A value outside the list reaches librespot as a setting it does not
        // understand, so it must not survive the prompt.
        assert_eq!(answer_choice("9", &QUALITIES, "HIGH"), None);
        assert_eq!(answer_choice("0", &QUALITIES, "HIGH"), None);
        assert_eq!(answer_choice("LOSSLESS", &QUALITIES, "HIGH"), None);
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
