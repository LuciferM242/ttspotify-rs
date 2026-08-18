//! Interactive config setup wizard.
//!
//! Walks the user through creating a config file with prompted inputs.
//! Validates each field and writes valid JSON.

use std::io::{self, Write};

use crate::config::{config_dir, BotConfig};
use crate::error::BotError;
use crate::services::Service;
use crate::youtube::setup;

pub(crate) fn ask(prompt: &str, default: &str, required: bool) -> Option<String> {
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
            continue;
        }
        return Some(input);
    }
}

/// Read a numbered-menu answer, re-asking until it is one of the choices.
///
/// Anything outside the range used to fall through to the default silently, so
/// a mistyped 9 saved choice 4 with nothing said about it — while the port
/// prompts next to it validated properly. Empty keeps the default, as with
/// every other prompt.
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

/// Unwrap a wizard prompt, or cancel the whole wizard: `ask`/`ask_int` return
/// `None` on EOF or interrupt, and every question treats that as "user backed
/// out" — `Ok(None)` from `run_wizard`.
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
/// `offer_service` should be true only for the standalone `--setup` flow.
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
    // GUI's name prompt uses — path separators dropped, `.json` stripped —
    // whether it was typed here or passed as `--setup <name>`.
    let name = if let Some(n) = config_name {
        match crate::config::sanitise_config_name(n) {
            Some(n) => n,
            // A name that cannot be used is a mistake in the command, not a
            // change of mind, so it fails rather than exiting successfully
            // having done nothing.
            None => {
                return Err(BotError::Usage(format!(
                    "\"{n}\" cannot be used as a bot name. Use letters or numbers; \
                     \"all\" is reserved for commands that act on every bot."
                )))
            }
        }
    } else {
        loop {
            let typed = or_cancel!(ask("Config name (used for file name and service name)", "config", true));
            match crate::config::sanitise_config_name(&typed) {
                Some(n) => break n,
                None => println!("    Please use letters or numbers in the name."),
            }
        }
    };

    // Configs live in <root>/config/ — the directory list_configs(), the CLI
    // auto-detect and the systemd unit all read. Writing to the data root put
    // every post-first-run setup where nothing would ever find it.
    std::fs::create_dir_all(crate::paths::configs_dir())?;
    let config_path = crate::paths::config_file(&name);

    if config_path.exists() {
        let overwrite = ask(
            &format!("{} already exists. Overwrite? (y/N)", config_path.display()),
            "n",
            false,
        );
        match overwrite {
            Some(ref v) if v.eq_ignore_ascii_case("y") || v.eq_ignore_ascii_case("yes") => {}
            _ => {
                println!("Setup cancelled.");
                return Ok(None);
            }
        }
    }

    println!("TeamTalk Server Settings");
    let host = or_cancel!(ask("Server address", "", true));
    let tcp_port = or_cancel!(ask_int("TCP port", 10333));
    let udp_port = or_cancel!(ask_int("UDP port", tcp_port));

    println!();
    println!("Bot Credentials");
    let username = or_cancel!(ask("Bot username", "", true));
    let password = or_cancel!(ask("Bot password", "", false));

    println!();
    println!("Bot Settings");
    let bot_name = or_cancel!(ask("Bot nickname", "Spotify", true));
    let channel = or_cancel!(ask("Channel to join (path or leave blank for root)", "/", false));
    let channel_password = or_cancel!(ask("Channel password (if any)", "", false));

    println!();
    println!("Admin Permissions");
    println!("  Who may run the bot's admin commands:");
    println!("  1. Everyone - no restrictions, any user can run every command");
    println!("  2. TeamTalk server admins - admins from the server's user accounts");
    println!("  3. Username list - only the usernames you enter next");
    println!("  4. Both - TeamTalk server admins or the username list");
    let admin_mode = match or_cancel!(ask_menu("Which admin mode should this bot use? (1-4)", 4, "4")) {
        1 => crate::config::AdminMode::Everyone,
        2 => crate::config::AdminMode::TtRights,
        3 => crate::config::AdminMode::List,
        // Anything else lands on the restrictive choice, same as the GUI:
        // a misread must not hand out access.
        _ => crate::config::AdminMode::Both,
    };
    let admins = if matches!(
        admin_mode,
        crate::config::AdminMode::List | crate::config::AdminMode::Both
    ) {
        crate::bot::auth::parse_admin_list(&or_cancel!(ask("Admin usernames (comma separated)", "", false)))
    } else {
        Vec::new()
    };

    println!();
    println!("Language");
    let lang_codes = crate::i18n::installed_language_codes(&config_dir());
    let default_language = loop {
        let code = or_cancel!(ask(
            &format!("Default language [{}]", lang_codes.join("/")),
            "en",
            false,
        ))
        .trim()
        .to_lowercase();
        if code.is_empty() {
            break "en".to_string();
        }
        // A language nobody has installed leaves the bot answering in English
        // anyway, but silently — better to say so while the answer can still
        // be corrected.
        if lang_codes.iter().any(|c| c.eq_ignore_ascii_case(&code)) {
            break code;
        }
        println!("    No translation installed for \"{code}\". Choose one of: {}", lang_codes.join(", "));
    };

    println!();
    println!("License (optional)");
    let license_name = or_cancel!(ask("License name", "", false));
    let license_key = or_cancel!(ask("License key", "", false));

    println!();
    println!("Services");
    println!("  A bot limited to YouTube never touches the Spotify login saved on");
    println!("  this machine - useful for a bot that sits on someone else's server.");
    println!("  1. Both Spotify and YouTube");
    println!("  2. Spotify only");
    println!("  3. YouTube only");
    let enabled_services = match or_cancel!(ask_menu("Which services should this bot offer? (1-3)", 3, "1")) {
        2 => crate::config::EnabledServices { spotify: true, youtube: false },
        3 => crate::config::EnabledServices { spotify: false, youtube: true },
        _ => crate::config::EnabledServices::default(),
    };
    // With one service the default is that service — nothing to ask.
    let default_service = match enabled_services.only() {
        Some(only) => only,
        None => Service::parse_or_default(&or_cancel!(ask(
            "Which service should bare commands target? (spotify/youtube)",
            "spotify",
            true
        ))),
    };

    // Cookies only matter for YouTube; a Spotify-only bot skips the question.
    let cookies_file = if !enabled_services.youtube {
        String::new()
    } else {
        println!();
        println!("YouTube Cookies (optional)");
        println!("  Cookies help with rate-limited or age-restricted videos.");
        println!("  Playback works without them in most cases.");
        let want_cookies = ask("Configure a cookies file path? (y/N)", "n", false);
        if matches!(
            want_cookies.as_deref(),
            Some(v) if v.eq_ignore_ascii_case("y") || v.eq_ignore_ascii_case("yes")
        ) {
            let default = setup::default_cookies_path().to_string_lossy().into_owned();
            let p = or_cancel!(ask("Cookies file path", &default, false));
            if !p.is_empty() && !std::path::Path::new(&p).is_file() {
                println!("  Warning: {p} doesn't exist yet. Saving anyway — drop the file there later.");
            }
            p
        } else {
            String::new()
        }
    };

    // Build config from defaults + user input
    let mut config = BotConfig::default();
    config.host = host;
    config.tcp_port = tcp_port;
    config.udp_port = udp_port;
    config.username = username;
    config.password = password;
    config.bot_name = bot_name;
    config.channel_name = if channel.is_empty() { "/".to_string() } else { channel };
    config.channel_password = channel_password;
    config.admin_mode = admin_mode;
    config.admins = admins;
    config.default_language = default_language;
    if !license_name.is_empty() {
        config.license_name = Some(license_name);
    }
    if !license_key.is_empty() {
        config.license_key = Some(license_key);
    }
    config.default_service = default_service;
    config.enabled_services = enabled_services;
    config.youtube_cookies_file = cookies_file;

    config.save(&config_path)?;

    println!();
    println!("  Config saved to: {}", config_path.display());

    // Offer Spotify authentication — skipped when this bot cannot use it.
    if enabled_services.spotify {
        println!();
        println!("Spotify Authentication");
        let do_auth = ask("Authenticate with Spotify now? (Y/n)", "y", false);
        match do_auth {
            Some(ref v) if v.eq_ignore_ascii_case("n") || v.eq_ignore_ascii_case("no") => {
                println!("  Skipping Spotify authentication.");
                println!(
                    "  To sign in later, {}",
                    crate::hints::sign_in_spotify()
                );
            }
            _ => {
                println!("  Starting Spotify authentication...");
                // Spawn a new thread with its own tokio runtime to avoid
                // nested-runtime panic (wizard is sync, may be called from async main)
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
                }).join().ok().flatten();

                match auth_result {
                    Some(Ok(_)) => {
                        println!("  Spotify authentication successful! Credentials cached.");
                    }
                    Some(Err(e)) => {
                        println!("  Spotify authentication failed: {e}");
                        println!(
                            "  To try again, {}",
                            crate::hints::sign_in_spotify()
                        );
                    }
                    None => {
                        println!("  Could not initialize authentication.");
                        println!(
                    "  To sign in later, {}",
                    crate::hints::sign_in_spotify()
                );
                    }
                }
            }
        }
    }

    // Skip the YouTube prompt entirely if the binaries are already installed
    // (e.g. from a previous run or a release zip that ships with them).
    let yt_already_installed = setup::resolve_paths()
        .map(|p| setup::is_installed(&p))
        .unwrap_or(false);

    if enabled_services.youtube && !yt_already_installed {
        println!();
        println!("YouTube Support");
        let yt_default = if default_service == Service::YouTube { "y" } else { "n" };
        let prompt = if default_service == Service::YouTube {
            "YouTube support requires extra binaries (~50 MB: yt-dlp, bgutil-pot, plugin). Download now? (Y/n)"
        } else {
            "You can also enable YouTube support. Downloads ~50 MB of binaries (yt-dlp, bgutil-pot, plugin). Skip if you only need Spotify. Install YouTube support? (y/N)"
        };
        let do_yt = ask(prompt, yt_default, false);
        let want_yt = matches!(
            do_yt.as_deref(),
            Some(v) if v.eq_ignore_ascii_case("y") || v.eq_ignore_ascii_case("yes")
        );
        if want_yt {
            if let Err(e) = run_youtube_setup() {
                println!("  YouTube setup failed: {e}");
                println!(
                    "  To retry later, {}",
                    crate::hints::install_youtube_tools()
                );
            }
        } else {
            println!(
                "  Skipping YouTube setup. To install it later, {}",
                crate::hints::install_youtube_tools()
            );
        }
    }

    // Offer systemd wiring so adding a server doesn't end with a config on
    // disk but nothing running. Only in the standalone --setup flow (see
    // `offer_service`), and only when actually booted under systemd — OpenRC/
    // runit/s6 users just get the run-it-directly hint below.
    #[cfg(target_os = "linux")]
    if offer_service && crate::service::systemd_booted() {
        println!();
        println!("Systemd Service");
        if crate::service::service_installed() {
            crate::service::offer_enable_instance(&name);
        } else {
            let install = ask(
                "Systemd service not installed. Install it now? (y/N)",
                "n",
                false,
            );
            if matches!(
                install.as_deref(),
                Some(v) if v.eq_ignore_ascii_case("y") || v.eq_ignore_ascii_case("yes")
            ) {
                // install_service prints its own guidance and offers to
                // enable/start every config, including the one just created.
                if let Err(e) = crate::service::install_service() {
                    println!("  Service install failed: {e}");
                    println!(
                        "  To retry later, {}",
                        crate::hints::install_service()
                    );
                }
            }
        }
    }

    println!();
    println!(
        "  Run the bot with: {} --config {}",
        crate::paths::program_name(),
        config_path.display()
    );
    println!();

    Ok(Some(config_path))
}

/// Public entry point for the standalone `--setup-yt` flag.
/// Downloads the binaries (skipping if already installed). Cookies are a
/// separate, optional config-time concern — not part of this flow.
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
    println!("  Tip: cookies are optional. If you want them, edit your config and");
    println!("  set youtubeCookiesFile, or drop a cookies.txt in the config dir.");
    Ok(())
}

/// Public entry point for `--update-tools`.
///
/// 1. Self-update yt-dlp via its built-in `--update` command.
/// 2. Compare installed bgutil version (sidecar) with the latest GitHub release.
///    Re-download bgutil + plugin only if newer is available.
pub fn run_update_tools() -> Result<(), BotError> {
    let paths = setup::resolve_paths()?;

    if !setup::is_installed(&paths) {
        println!(
            "  YouTube tools aren't installed yet. To install them, {}",
            crate::hints::install_youtube_tools()
        );
        return Ok(());
    }

    // 1. yt-dlp self-update. The socket timeout bounds a dead network, same
    // as the tray's update path — a stalled connection should end in an error
    // line, not a terminal that hangs until the OS gives up.
    println!("Updating yt-dlp...");
    match std::process::Command::new(&paths.yt_dlp)
        .args(["--update", "--socket-timeout", "30"])
        .status()
    {
        Ok(status) if status.success() => {
            println!("  yt-dlp update check complete.");
        }
        Ok(status) => {
            println!("  yt-dlp --update exited with {status}");
        }
        Err(e) => {
            println!("  Could not run yt-dlp --update: {e}");
        }
    }

    // 2. bgutil version check vs GitHub releases.
    println!();
    println!("Checking bgutil-pot for updates...");
    let installed = setup::installed_bgutil_version(&paths);
    let latest = run_blocking_async(|| async { setup::latest_bgutil_version().await })?;

    if installed == latest {
        println!("  bgutil-pot already on {installed} (latest).");
    } else {
        println!("  Installed: {installed}, latest: {latest}. Updating...");
        let target = latest.clone();
        run_blocking_async(move || async move {
            let paths = setup::resolve_paths()?;
            setup::install_bgutil_version(&paths, &target, |line| println!("  {line}")).await
        })?;
    }

    // 3. JavaScript runtime: yt-dlp needs one to solve YouTube's player
    // challenges, and it changes often enough to be worth refreshing.
    println!();
    println!("Checking the JavaScript runtime (Deno)...");
    if let Err(e) = run_blocking_async(|| async {
        let paths = setup::resolve_paths()?;
        setup::update_js_runtime(&paths, |line| println!("  {line}")).await
    }) {
        println!("  Could not update Deno: {e}");
        println!("  YouTube will still work, but some formats may be unavailable.");
    }

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
        // The bug this replaced: typing 9 at a 1-4 menu saved choice 4 with no
        // message, while the port prompt beside it validated properly.
        assert_eq!(menu_choice("9", 4, "4"), None);
        assert_eq!(menu_choice("0", 4, "4"), None);
        assert_eq!(menu_choice("-1", 4, "4"), None);
        assert_eq!(menu_choice("two", 4, "4"), None);
        assert_eq!(menu_choice("2x", 4, "4"), None);
    }
}
