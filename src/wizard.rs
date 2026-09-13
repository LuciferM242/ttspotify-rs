//! Interactive setup wizard for a new bot config, and the prompts it shares
//! with the Linux config editor.

use std::io::{self, Write};

use crate::config::{config_dir, kick_delay_label, kick_delay_options, AdminMode, BotConfig, EnabledServices};
use crate::error::BotError;
use crate::services::Service;
use crate::youtube::locale::{self, LocaleOption};
use crate::youtube::setup;

pub const GENDERS: [&str; 3] = ["neutral", "male", "female"];

pub(crate) fn ask(prompt: &str, default: &str, required: bool) -> Option<String> {
    let mut refused = 0;
    loop {
        if default.is_empty() {
            print!("  {prompt}: ");
        } else {
            print!("  {prompt} [{default}]: ");
        }
        io::stdout().flush().ok();

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) | Err(_) => {
                // Shared by the setup wizard and the config editor, so the
                // wording cannot claim which one the user was in.
                println!("\nCancelled.");
                return None;
            }
            _ => {}
        }

        let input = input.trim().to_string();
        if input.is_empty() && !default.is_empty() {
            return Some(default.to_string());
        }
        if input.is_empty() && required {
            println!("    This field is required.");
            // Repeating one line forever gave no way out to anyone who did not
            // already know Ctrl+C ends it without writing anything.
            refused += 1;
            if refused == 2 {
                println!("    (press Ctrl+C to leave the setup without saving)");
            }
            continue;
        }
        return Some(input);
    }
}

/// Read a numbered-menu answer. Empty keeps the default; anything outside the
/// range is refused rather than silently becoming the default.
pub(crate) fn menu_choice(input: &str, choices: u8, default: &str) -> Option<u8> {
    let input = input.trim();
    let text = if input.is_empty() { default } else { input };
    match text.parse::<u8>() {
        Ok(n) if n >= 1 && n <= choices => Some(n),
        _ => None,
    }
}

fn ask_menu(prompt: &str, choices: u8, default: &str) -> Option<u8> {
    loop {
        let raw = ask(prompt, default, false)?;
        match menu_choice(&raw, choices, default) {
            Some(choice) => return Some(choice),
            None => println!("    Please answer with a number from 1 to {choices}."),
        }
    }
}

fn ask_int(prompt: &str, default: i32) -> Option<i32> {
    loop {
        let raw = ask(prompt, &default.to_string(), true)?;
        match raw.parse::<i32>() {
            Ok(v) => return Some(v),
            Err(_) => println!("    Invalid input. Expected a number."),
        }
    }
}

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
/// current value; an answer that matches nothing is refused rather than
/// written into the config as a setting nothing understands.
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

pub(crate) fn ask_bool(prompt: &str, current: bool) -> Option<bool> {
    loop {
        let raw = ask(&format!("{prompt}? (y/n)"), if current { "y" } else { "n" }, false)?;
        match answer_bool(&raw, current) {
            Some(value) => return Some(value),
            None => println!("    Answer y or n."),
        }
    }
}

pub(crate) fn ask_choice(prompt: &str, options: &[&str], current: &str) -> Option<String> {
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

/// Whether to rejoin after a server kick, and how long to wait first.
/// `Some(None)` means stay out.
pub(crate) fn ask_kick_delay(current: Option<u32>) -> Option<Option<u32>> {
    if !ask_bool("Rejoin the server after being kicked", current.is_some())? {
        return Some(None);
    }
    let seconds = current.unwrap_or(0);
    let delays = kick_delay_options(seconds);
    let labels: Vec<String> = delays.iter().map(|&s| kick_delay_label(s)).collect();
    let options: Vec<&str> = labels.iter().map(String::as_str).collect();
    let picked = ask_choice("Wait before rejoining", &options, &kick_delay_label(seconds))?;
    let index = options.iter().position(|o| *o == picked).unwrap_or(0);
    Some(Some(delays[index]))
}

/// What a typed YouTube location or language asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum LocaleAnswer {
    Code(String),
    ListAll,
    Several(Vec<LocaleOption>),
    Unknown,
}

/// Empty keeps the current code, `-` is YouTube's default, `?` lists
/// everything, and anything else is searched for by name or code.
pub fn answer_locale(input: &str, current: &str, search: fn(&str) -> Vec<LocaleOption>) -> LocaleAnswer {
    match input.trim() {
        "" => LocaleAnswer::Code(current.to_string()),
        "-" => LocaleAnswer::Code(String::new()),
        "?" => LocaleAnswer::ListAll,
        typed => {
            let mut found = search(typed);
            match found.len() {
                0 => LocaleAnswer::Unknown,
                1 => LocaleAnswer::Code(found.remove(0).code),
                _ => LocaleAnswer::Several(found),
            }
        }
    }
}

/// Most matches read out before asking for more of the name instead.
const MATCHES_SHOWN: usize = 10;

pub(crate) fn ask_locale(
    prompt: &str,
    current: &str,
    options: &[LocaleOption],
    search: fn(&str) -> Vec<LocaleOption>,
) -> Option<String> {
    let label = |code: &str| {
        options
            .iter()
            .find(|o| o.code.eq_ignore_ascii_case(code))
            .map_or_else(|| code.to_string(), |o| o.label.clone())
    };
    println!("  {prompt}: {}", label(current));
    println!("  Type a name or code to change it, - for YouTube's default, ? to list them all.");
    loop {
        let raw = ask("Name or code, Enter keeps it", "", false)?;
        match answer_locale(&raw, current, search) {
            LocaleAnswer::Code(code) => {
                if code != current {
                    println!("    Chosen: {}", label(&code));
                }
                return Some(code);
            }
            LocaleAnswer::ListAll => options.iter().for_each(|o| println!("    {}", o.label)),
            LocaleAnswer::Several(found) if found.len() > MATCHES_SHOWN => {
                println!("    {} match \"{}\". Type more of the name.", found.len(), raw.trim());
            }
            LocaleAnswer::Several(found) => {
                println!("    {} match. Type one of them by name or code:", found.len());
                found.iter().for_each(|o| println!("    {}", o.label));
            }
            LocaleAnswer::Unknown => {
                println!("    Nothing matches \"{}\". Type ? to list them all.", raw.trim());
            }
        }
    }
}

/// Unwrap a wizard prompt, or cancel the whole wizard: prompts return `None`
/// on EOF or interrupt, which means the user backed out.
macro_rules! or_cancel {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            None => return Ok(None),
        }
    };
}

#[allow(clippy::field_reassign_with_default)] // building config field-by-field from wizard input reads clearer
/// Run the interactive setup wizard.
///
/// `offer_service` should be true only for the standalone `add` flow.
/// The first-run wizard inside `BotConfig::load` must pass false: that path
/// continues into running the bot in the foreground, and starting a systemd
/// instance there too would run the same config twice.
pub fn run_wizard(
    config_name: Option<&str>,
    offer_service: bool,
) -> Result<Option<std::path::PathBuf>, BotError> {
    #[cfg(not(target_os = "linux"))]
    let _ = offer_service;
    println!();
    println!("TTSpotify Configuration Setup");
    println!();

    // The name becomes a file path, so it goes through the same sanitiser the
    // GUI's name prompt uses, whether typed here or passed on the command line.
    let name = if let Some(n) = config_name {
        match crate::config::sanitise_config_name(n) {
            Some(n) => n,
            // A name that cannot be used is a mistake in the command, not a
            // change of mind, so it fails rather than exiting successfully
            // having done nothing.
            None => {
                return Err(BotError::Usage(format!(
                    "\"{n}\" cannot be used as a bot name. Use letters or numbers, up to \
                     60 characters; \"all\" is reserved for commands that act on every bot."
                )))
            }
        }
    } else {
        loop {
            let typed = or_cancel!(ask("Config name (used for file name and service name)", "config", true));
            match crate::config::sanitise_config_name(&typed) {
                Some(n) => break n,
                None => println!("    Please use letters or numbers, up to 60 characters."),
            }
        }
    };

    // Configs live in <root>/config/, the directory list_configs(), the CLI
    // and the systemd unit all read.
    std::fs::create_dir_all(crate::paths::configs_dir())?;
    let config_path = crate::paths::config_file(&name);

    if config_path.exists()
        && ask_bool(&format!("{} already exists. Overwrite it", config_path.display()), false) != Some(true)
    {
        println!("Setup cancelled.");
        return Ok(None);
    }

    let mut config = BotConfig::default();

    println!("TeamTalk Server");
    config.host = or_cancel!(ask("Server address", "", true));
    config.tcp_port = or_cancel!(ask_int("TCP port", 10333));
    config.udp_port = or_cancel!(ask_int("UDP port", config.tcp_port));
    config.encrypted = or_cancel!(ask_bool("Encrypted connection", false));

    println!();
    println!("Bot Login");
    config.username = or_cancel!(ask("Bot username", "", true));
    config.password = or_cancel!(ask("Bot password", "", false));

    println!();
    println!("Bot Settings");
    config.bot_name = or_cancel!(ask("Bot nickname", &config.bot_name, true));
    config.bot_gender = or_cancel!(ask_choice("Bot gender", &GENDERS, &config.bot_gender));
    let channel = or_cancel!(ask("Channel to join (/ is the root channel)", "/", false));
    config.channel_name = if channel.is_empty() { "/".to_string() } else { channel };
    config.channel_password = or_cancel!(ask("Channel password (if any)", "", false));
    let languages = crate::i18n::installed_language_codes(&config_dir());
    let language_refs: Vec<&str> = languages.iter().map(String::as_str).collect();
    config.default_language = or_cancel!(ask_choice("Language the bot answers in", &language_refs, "en"));

    println!();
    println!("Admin Permissions");
    println!("  Who may run the bot's admin commands:");
    println!("  1. Everyone - no restrictions, any user can run every command");
    println!("  2. TeamTalk server admins - admins from the server's user accounts");
    println!("  3. Username list - only the usernames you enter next");
    println!("  4. Both - TeamTalk server admins or the username list");
    config.admin_mode = match or_cancel!(ask_menu("Which admin mode should this bot use? (1-4)", 4, "4")) {
        1 => AdminMode::Everyone,
        2 => AdminMode::TtRights,
        3 => AdminMode::List,
        // A misread must not hand out access, so anything else is the
        // restrictive choice, same as the GUI.
        _ => AdminMode::Both,
    };
    if matches!(config.admin_mode, AdminMode::List | AdminMode::Both) {
        config.admins =
            crate::bot::auth::parse_admin_list(&or_cancel!(ask("Admin usernames (comma separated)", "", false)));
    }

    println!();
    println!("Services");
    println!("  A bot limited to YouTube never touches the Spotify login saved on");
    println!("  this machine - useful for a bot that sits on someone else's server.");
    println!("  1. Both Spotify and YouTube");
    println!("  2. Spotify only");
    println!("  3. YouTube only");
    config.enabled_services = match or_cancel!(ask_menu("Which services should this bot offer? (1-3)", 3, "1")) {
        2 => EnabledServices { spotify: true, youtube: false },
        3 => EnabledServices { spotify: false, youtube: true },
        _ => EnabledServices::default(),
    };
    config.default_service = match config.enabled_services.only() {
        Some(only) => only,
        None => Service::parse_or_default(&or_cancel!(ask_choice(
            "Service that commands use unless told otherwise",
            &["spotify", "youtube"],
            "spotify"
        ))),
    };

    if config.enabled_services.youtube {
        println!();
        println!("YouTube");
        println!("  Cookies help with rate-limited or age-restricted videos.");
        println!("  Playback works without them in most cases.");
        if or_cancel!(ask_bool("Use a cookies file", false)) {
            let default = setup::default_cookies_path().to_string_lossy().into_owned();
            let path = or_cancel!(ask("Cookies file path", &default, false));
            if !path.is_empty() && !std::path::Path::new(&path).is_file() {
                println!("  Warning: {path} doesn't exist yet. Saving anyway - drop the file there later.");
            }
            config.youtube_cookies_file = path;
        }
        println!();
        println!("  YouTube Music ranks songs by where the search comes from.");
        config.youtube_country = or_cancel!(ask_locale(
            "Search location",
            "",
            &locale::country_options(),
            locale::search_countries
        ));
        config.youtube_language =
            or_cancel!(ask_locale("Language", "", &locale::language_options(), locale::search_languages));
    }

    println!();
    println!("Kicks");
    config.rejoin_after_kick_seconds = or_cancel!(ask_kick_delay(None));

    println!();
    println!("License (optional)");
    let license_name = or_cancel!(ask("License name", "", false));
    let license_key = or_cancel!(ask("License key", "", false));
    config.license_name = Some(license_name).filter(|s| !s.is_empty());
    config.license_key = Some(license_key).filter(|s| !s.is_empty());

    config.save(&config_path)?;

    println!();
    println!("  Config saved to: {}", config_path.display());

    if config.enabled_services.spotify {
        offer_spotify_sign_in();
    }

    let yt_already_installed = setup::resolve_paths()
        .map(|p| setup::is_installed(&p))
        .unwrap_or(false);
    if config.enabled_services.youtube && !yt_already_installed {
        println!();
        println!("YouTube Support");
        let prompt = "YouTube playback needs Deno, a JavaScript runtime (about 40 MB). Download it now";
        if ask_bool(prompt, true) == Some(true) {
            if let Err(e) = run_youtube_setup() {
                println!("  YouTube setup failed: {e}");
                println!("  To retry later, {}", crate::hints::install_youtube_tools());
            }
        } else {
            println!("  Skipping YouTube setup. To install it later, {}", crate::hints::install_youtube_tools());
        }
    }

    // Offer systemd wiring so adding a server doesn't end with a config on
    // disk but nothing running. Only in the standalone `add` flow (see
    // `offer_service`), and only when actually booted under systemd.
    #[cfg(target_os = "linux")]
    if offer_service && crate::service::systemd_booted() {
        println!();
        println!("Systemd Service");
        if crate::service::service_installed() {
            crate::service::offer_enable_instance(&name);
        } else if ask_bool("Systemd service not installed. Install it now", false) == Some(true) {
            // install_service prints its own guidance and offers to
            // enable/start every config, including the one just created.
            if let Err(e) = crate::service::install_service() {
                println!("  Service install failed: {e}");
                println!("  To retry later, {}", crate::hints::install_service());
            }
        }
    }

    println!();
    // By name, not by path: `--config <full path>` is what the systemd unit
    // uses, not what a person should be told to type.
    println!("  To run it in this terminal, {}", crate::hints::run_bot(&name));
    println!("  To change audio, radio and search settings, {}", crate::hints::edit_bot(&name));
    println!();

    Ok(Some(config_path))
}

/// The Spotify login is shared by every bot on the machine, so a second bot
/// only needs it once.
fn offer_spotify_sign_in() {
    println!();
    println!("Spotify Sign-in");
    let auth = crate::spotify::auth::SpotifyAuth::new();
    if auth.has_cached_credentials() {
        match auth.cached_username() {
            Some(user) => println!("  Already signed in as {user}."),
            None => println!("  Already signed in."),
        }
        return;
    }
    if ask_bool("Sign in to Spotify now", true) != Some(true) {
        println!("  Skipping Spotify sign-in. To sign in later, {}", crate::hints::sign_in_spotify());
        return;
    }

    println!("  Starting Spotify authentication...");
    // Its own thread and runtime: the wizard is sync but may be called from
    // async main, and a nested runtime panics.
    let auth_result = std::thread::spawn(|| {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("  Failed to create async runtime: {e}");
                return None;
            }
        };
        let mut auth = crate::spotify::auth::SpotifyAuth::new();
        Some(rt.block_on(auth.connect()))
    })
    .join()
    .ok()
    .flatten();

    match auth_result {
        Some(Ok(_)) => println!("  Spotify authentication successful! Credentials cached."),
        Some(Err(e)) => {
            println!("  Spotify authentication failed: {e}");
            println!("  To try again, {}", crate::hints::sign_in_spotify());
        }
        None => {
            println!("  Could not initialize authentication.");
            println!("  To sign in later, {}", crate::hints::sign_in_spotify());
        }
    }
}

/// `youtube install`: download the runtime and sidecar unless already there.
pub fn run_youtube_setup() -> Result<(), BotError> {
    let paths = setup::resolve_paths()?;

    if setup::is_installed(&paths) {
        println!("  YouTube binaries already installed at {}", paths.lib_dir.display());
        return Ok(());
    }

    println!("  Installing into {}", paths.lib_dir.display());
    run_blocking_async(|| async {
        let paths = setup::resolve_paths()?;
        setup::install(&paths, |line| println!("  {line}")).await
    })?;

    println!();
    println!("  YouTube support installed.");
    Ok(())
}

/// `youtube update`.
///
/// 1. Rewrite the sidecar script from the copy compiled into this binary.
/// 2. Refresh the JavaScript runtime it needs.
pub fn run_update_tools() -> Result<(), BotError> {
    let paths = setup::resolve_paths()?;

    if !setup::is_installed(&paths) {
        println!(
            "  YouTube tools aren't installed yet. To install them, {}",
            crate::hints::install_youtube_tools()
        );
        return Ok(());
    }

    // The sidecar and its pinned dependencies ship inside this binary, so
    // only the runtime can be out of date.
    println!("Refreshing the sidecar script...");
    match crate::youtube::sidecar::ensure_script(&paths.lib_dir) {
        Ok(_) => println!("  Sidecar up to date."),
        Err(e) => println!("  Could not write the sidecar: {e}"),
    }

    println!();
    println!("Checking the JavaScript runtime (Deno)...");
    if let Err(e) = run_blocking_async(|| async {
        let paths = setup::resolve_paths()?;
        setup::update_js_runtime(&paths, |line| println!("  {line}")).await
    }) {
        println!("  Could not update Deno: {e}");
        println!("  The Deno already installed is still used.");
    }

    println!();
    setup::warm_dependency_cache(&paths, &|line| println!("  {line}"));

    println!();
    println!("  Done.");
    Ok(())
}

/// Run an async closure on a fresh tokio runtime in a worker thread.
/// The wizard is sync but may be invoked from an async context (e.g. `main`),
/// so spinning up our own runtime avoids the nested-runtime panic.
fn run_blocking_async<T, F, Fut>(f: F) -> Result<T, BotError>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, BotError>>,
{
    std::thread::spawn(move || -> Result<T, BotError> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| BotError::Config(format!("tokio runtime: {e}")))?;
        rt.block_on(f())
    })
    .join()
    .map_err(|_| BotError::Config("async worker thread panicked".to_string()))?
}

#[cfg(test)]
mod menu_tests {
    use super::menu_choice;

    #[test]
    fn a_number_in_range_is_taken_as_given() {
        assert_eq!(menu_choice("1", 4, "4"), Some(1));
        assert_eq!(menu_choice(" 3 \n", 4, "4"), Some(3));
    }

    #[test]
    fn empty_means_the_default() {
        assert_eq!(menu_choice("", 4, "4"), Some(4));
        assert_eq!(menu_choice("  ", 3, "1"), Some(1));
    }

    #[test]
    fn out_of_range_and_junk_are_refused_rather_than_silently_defaulted() {
        assert_eq!(menu_choice("9", 4, "4"), None);
        assert_eq!(menu_choice("0", 4, "4"), None);
        assert_eq!(menu_choice("-1", 4, "4"), None);
        assert_eq!(menu_choice("two", 4, "4"), None);
        assert_eq!(menu_choice("2x", 4, "4"), None);
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    const QUALITIES: [&str; 3] = ["VERY_HIGH", "HIGH", "NORMAL"];

    #[test]
    fn empty_answers_keep_what_is_already_there() {
        assert_eq!(answer_bool("", true), Some(true));
        assert_eq!(answer_bool("  \n", false), Some(false));
        assert_eq!(answer_choice("", &QUALITIES, "HIGH").as_deref(), Some("HIGH"));
    }

    #[test]
    fn yes_and_no_are_read_in_the_forms_people_type() {
        for yes in ["y", "Y", "yes", "on", "true"] {
            assert_eq!(answer_bool(yes, false), Some(true), "{yes}");
        }
        for no in ["n", "NO", "off", "false"] {
            assert_eq!(answer_bool(no, true), Some(false), "{no}");
        }
        assert_eq!(answer_bool("maybe", true), None);
    }

    #[test]
    fn a_choice_can_be_its_number_or_its_name() {
        assert_eq!(answer_choice("1", &QUALITIES, "HIGH").as_deref(), Some("VERY_HIGH"));
        assert_eq!(answer_choice("normal", &QUALITIES, "HIGH").as_deref(), Some("NORMAL"));
        assert_eq!(answer_choice("Female", &GENDERS, "neutral").as_deref(), Some("female"));
    }

    #[test]
    fn an_invalid_choice_is_refused_rather_than_written_to_the_config() {
        assert_eq!(answer_choice("9", &QUALITIES, "HIGH"), None);
        assert_eq!(answer_choice("0", &QUALITIES, "HIGH"), None);
        assert_eq!(answer_choice("LOSSLESS", &QUALITIES, "HIGH"), None);
    }

    #[test]
    fn a_locale_is_kept_reset_listed_or_searched() {
        let countries = locale::search_countries;
        assert_eq!(answer_locale("", "IN", countries), LocaleAnswer::Code("IN".into()));
        assert_eq!(answer_locale(" - ", "IN", countries), LocaleAnswer::Code(String::new()));
        assert_eq!(answer_locale("?", "IN", countries), LocaleAnswer::ListAll);
        assert_eq!(answer_locale("germany", "", countries), LocaleAnswer::Code("DE".into()));
        assert_eq!(answer_locale("ger", "", countries), LocaleAnswer::Code("DE".into()));
        assert_eq!(answer_locale("Atlantis", "IN", countries), LocaleAnswer::Unknown);
        assert_eq!(answer_locale("en-gb", "", locale::search_languages), LocaleAnswer::Code("en-GB".into()));
    }

    #[test]
    fn part_of_a_name_shared_by_several_lists_only_those() {
        match answer_locale("english", "", locale::search_languages) {
            LocaleAnswer::Several(found) => {
                let codes: Vec<&str> = found.iter().map(|o| o.code.as_str()).collect();
                assert_eq!(codes, ["en-IN", "en-GB", "en"]);
            }
            other => panic!("{other:?}"),
        }
    }
}
