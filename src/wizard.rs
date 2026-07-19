//! Interactive config setup wizard.
//!
//! Walks the user through creating a config file with prompted inputs.
//! Validates each field and writes valid JSON.

use std::io::{self, Write};

use crate::config::{config_dir, BotConfig};
use crate::error::BotError;
use crate::services::Service;
use crate::youtube::setup;

fn ask(prompt: &str, default: &str, required: bool) -> Option<String> {
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
                println!("\nSetup cancelled.");
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

fn ask_int(prompt: &str, default: i32) -> Option<i32> {
    loop {
        let raw = ask(prompt, &default.to_string(), true)?;
        match raw.parse::<i32>() {
            Ok(v) => return Some(v),
            Err(_) => println!("    Invalid input. Expected a number."),
        }
    }
}

#[allow(clippy::field_reassign_with_default)] // building config field-by-field from wizard input reads clearer
/// Run the interactive setup wizard.
///
/// `offer_service` should be true only for the standalone `--setup` flow.
/// The first-run wizard inside `BotConfig::load` must pass false: that path
/// continues into running the bot in the foreground, and starting a systemd
/// instance there too would run the same config twice.
pub fn run_wizard(config_name: Option<&str>, offer_service: bool) -> Result<(), BotError> {
    #[cfg(not(target_os = "linux"))]
    let _ = offer_service;
    println!();
    println!("TTSpotify Configuration Setup");
    println!();

    // Config file name
    let name = if let Some(n) = config_name {
        n.to_string()
    } else {
        match ask("Config name (used for file name and service name)", "config", true) {
            Some(n) => n.replace(".json", ""),
            None => return Ok(()),
        }
    };

    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let config_path = dir.join(format!("{name}.json"));

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
                return Ok(());
            }
        }
    }

    println!("TeamTalk Server Settings");
    let host = match ask("Server address", "", true) {
        Some(v) => v,
        None => return Ok(()),
    };
    let tcp_port = match ask_int("TCP port", 10333) {
        Some(v) => v,
        None => return Ok(()),
    };
    let udp_port = match ask_int("UDP port", tcp_port) {
        Some(v) => v,
        None => return Ok(()),
    };

    println!();
    println!("Bot Credentials");
    let username = match ask("Bot username", "", true) {
        Some(v) => v,
        None => return Ok(()),
    };
    let password = match ask("Bot password", "", false) {
        Some(v) => v,
        None => return Ok(()),
    };

    println!();
    println!("Bot Settings");
    let bot_name = match ask("Bot nickname", "Spotify", true) {
        Some(v) => v,
        None => return Ok(()),
    };
    let channel = match ask("Channel to join (path or leave blank for root)", "/", false) {
        Some(v) => v,
        None => return Ok(()),
    };
    let channel_password = match ask("Channel password (if any)", "", false) {
        Some(v) => v,
        None => return Ok(()),
    };

    println!();
    println!("Admin Permissions");
    let admin_mode = match ask("Admin mode [everyone/ttrights/list/both]", "both", false) {
        Some(s) => match s.trim().to_lowercase().as_str() {
            "everyone" => crate::config::AdminMode::Everyone,
            "ttrights" => crate::config::AdminMode::TtRights,
            "list" => crate::config::AdminMode::List,
            _ => crate::config::AdminMode::Both,
        },
        None => return Ok(()),
    };
    let admins = if matches!(
        admin_mode,
        crate::config::AdminMode::List | crate::config::AdminMode::Both
    ) {
        match ask("Admin usernames (comma separated)", "", false) {
            Some(s) => crate::bot::auth::parse_admin_list(&s),
            None => return Ok(()),
        }
    } else {
        Vec::new()
    };

    println!();
    println!("Language");
    let lang_codes = crate::i18n::installed_language_codes(&config_dir());
    let default_language = match ask(
        &format!("Default language [{}]", lang_codes.join("/")),
        "en",
        false,
    ) {
        Some(s) => {
            let code = s.trim().to_lowercase();
            if code.is_empty() { "en".to_string() } else { code }
        }
        None => return Ok(()),
    };

    println!();
    println!("License (optional)");
    let license_name = match ask("License name", "", false) {
        Some(v) => v,
        None => return Ok(()),
    };
    let license_key = match ask("License key", "", false) {
        Some(v) => v,
        None => return Ok(()),
    };

    println!();
    println!("Default Service");
    let default_service = match ask("Which service should bare commands target? (spotify/youtube)", "spotify", true) {
        Some(v) => Service::parse_or_default(&v),
        None => return Ok(()),
    };

    println!();
    println!("YouTube Cookies (optional)");
    println!("  Cookies help with rate-limited or age-restricted videos.");
    println!("  Playback works without them in most cases.");
    let want_cookies = ask("Configure a cookies file path? (y/N)", "n", false);
    let cookies_file = if matches!(
        want_cookies.as_deref(),
        Some(v) if v.eq_ignore_ascii_case("y") || v.eq_ignore_ascii_case("yes")
    ) {
        let default = setup::default_cookies_path().to_string_lossy().into_owned();
        match ask("Cookies file path", &default, false) {
            Some(p) => {
                if !p.is_empty() && !std::path::Path::new(&p).is_file() {
                    println!("  Warning: {p} doesn't exist yet. Saving anyway — drop the file there later.");
                }
                p
            }
            None => return Ok(()),
        }
    } else {
        String::new()
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
    config.youtube_cookies_file = cookies_file;

    config.save(&config_path)?;

    println!();
    println!("  Config saved to: {}", config_path.display());

    // Offer Spotify authentication
    println!();
    println!("Spotify Authentication");
    let do_auth = ask("Authenticate with Spotify now? (Y/n)", "y", false);
    match do_auth {
        Some(ref v) if v.eq_ignore_ascii_case("n") || v.eq_ignore_ascii_case("no") => {
            println!("  Skipping Spotify authentication.");
            println!("  You can authenticate later with: tt-spotify-bot --auth");
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
                    println!("  You can try again with: tt-spotify-bot --auth");
                }
                None => {
                    println!("  Could not initialize authentication.");
                    println!("  You can authenticate later with: tt-spotify-bot --auth");
                }
            }
        }
    }

    // Skip the YouTube prompt entirely if the binaries are already installed
    // (e.g. from a previous run or a release zip that ships with them).
    let yt_already_installed = setup::resolve_paths()
        .map(|p| setup::is_installed(&p))
        .unwrap_or(false);

    if !yt_already_installed {
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
                println!("  You can retry later with: tt-spotify-bot --setup-yt");
            }
        } else {
            println!("  Skipping YouTube setup. You can run it later with: tt-spotify-bot --setup-yt");
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
                    println!("  You can retry later with: tt-spotify-bot --install-service");
                }
            }
        }
    }

    println!();
    println!("  Run the bot with: tt-spotify-bot --config {}", config_path.display());
    println!();

    Ok(())
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
        println!("  YouTube tools aren't installed yet. Run --setup-yt first.");
        return Ok(());
    }

    // 1. yt-dlp self-update.
    println!("Updating yt-dlp...");
    match std::process::Command::new(&paths.yt_dlp)
        .arg("--update")
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
