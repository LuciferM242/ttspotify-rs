#![cfg_attr(windows, windows_subsystem = "windows")]
//! Entry point. On Windows this is the system-tray GUI; on every other
//! platform it is the CLI bot. Only one `main` compiles per target.

#[cfg(not(windows))]
use std::sync::Arc;

#[cfg(not(windows))]
use clap::Parser;

#[cfg(not(windows))]
use tt_spotify_bot::bot::runner::BotExit;
#[cfg(not(windows))]
use tt_spotify_bot::config::BotConfig;
#[cfg(not(windows))]
use tt_spotify_bot::error::BotError;

/// TeamTalk SDK version this build pins by default. The teamtalk crate reads
/// `TEAMTALK_SDK_VERSION` at runtime to choose which SDK to download; we set it
/// (unless already set in the environment) so builds use a known-good version
/// and never silently auto-update to a newer SDK. Bump this to move versions.
const PINNED_TEAMTALK_SDK_VERSION: &str = "v5.19a";

/// Pin the TeamTalk SDK version unless the user explicitly overrode it, and
/// pin the SDK directory to the config dir (migrating any old CWD/home copy).
/// Call once, first thing in `main`, before any TeamTalk client is created.
fn pin_teamtalk_sdk_version() {
    if std::env::var_os("TEAMTALK_SDK_VERSION").is_none() {
        std::env::set_var("TEAMTALK_SDK_VERSION", PINNED_TEAMTALK_SDK_VERSION);
    }
    tt_spotify_bot::tt::sdk::pin_sdk_dir();
}

#[cfg(not(windows))]
#[derive(Parser)]
#[command(name = "tt-spotify-bot", about = "TeamTalk Spotify Bot")]
struct Args {
    /// Path to config file
    #[arg(short, long)]
    config: Option<String>,

    /// Run the interactive config setup wizard
    #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    setup: Option<String>,

    /// Install systemd user service (Linux only)
    #[cfg(target_os = "linux")]
    #[arg(long)]
    install_service: bool,

    /// Remove systemd user service (Linux only)
    #[cfg(target_os = "linux")]
    #[arg(long)]
    uninstall_service: bool,

    /// Authenticate with Spotify and exit (no bot startup)
    #[arg(long)]
    auth: bool,

    /// Check if Spotify credentials are cached and exit
    #[arg(long)]
    auth_status: bool,

    /// Download YouTube support binaries (yt-dlp, bgutil-pot, plugin) into
    /// the bot's lib/ folder. Skips if already installed.
    #[arg(long)]
    setup_yt: bool,

    /// Update YouTube tools: runs `yt-dlp --update` for the binary's self-
    /// update, then checks GitHub for a newer bgutil-pot release.
    #[arg(long)]
    update_tools: bool,

    /// Check GitHub for a newer release; if found, show the changelog and
    /// (with confirmation) download, verify, and replace this binary.
    #[arg(long)]
    update: bool,
}

#[cfg(not(windows))]
#[tokio::main]
async fn main() -> Result<(), BotError> {
    pin_teamtalk_sdk_version();
    // One-time move of an old exe-side tools install to the XDG data dir,
    // before anything resolves tool paths.
    tt_spotify_bot::youtube::setup::migrate_legacy_tools();
    tt_spotify_bot::logging::install_panic_hook();
    let args = Args::parse();

    if let Some(ref name) = args.setup {
        let name = if name.is_empty() { None } else { Some(name.as_str()) };
        return tt_spotify_bot::wizard::run_wizard(name, true);
    }

    #[cfg(target_os = "linux")]
    if args.install_service {
        return tt_spotify_bot::service::install_service();
    }
    #[cfg(target_os = "linux")]
    if args.uninstall_service {
        return tt_spotify_bot::service::uninstall_service();
    }

    if args.auth_status {
        let auth = tt_spotify_bot::spotify::auth::SpotifyAuth::new();
        if auth.has_cached_credentials() {
            println!("Spotify: Cached credentials found.");
            println!("  (Note: credentials may be expired or revoked.)");
            std::process::exit(0);
        } else {
            println!("Spotify: No cached credentials.");
            println!("  Run with --auth to authenticate.");
            std::process::exit(1);
        }
    }

    if args.setup_yt {
        match tt_spotify_bot::wizard::run_youtube_setup() {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("YouTube setup failed: {e}");
                std::process::exit(1);
            }
        }
    }

    if args.update_tools {
        match tt_spotify_bot::wizard::run_update_tools() {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("Tool update failed: {e}");
                std::process::exit(1);
            }
        }
    }

    if args.update {
        return run_cli_update().await;
    }

    if args.auth {
        tracing_subscriber::fmt()
            .with_target(false)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
            )
            .init();

        let mut auth = tt_spotify_bot::spotify::auth::SpotifyAuth::new();
        match auth.connect().await {
            Ok(_) => {
                println!("Spotify authentication successful. Credentials cached.");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("Spotify authentication failed: {e}");
                std::process::exit(1);
            }
        }
    }

    let config_path = args.config.unwrap_or_else(|| {
        let configs = tt_spotify_bot::config::list_configs();
        if let Some((_, path)) = configs.first() {
            path.to_string_lossy().into_owned()
        } else {
            tt_spotify_bot::config::config_dir().join("config.json")
                .to_string_lossy().into_owned()
        }
    });

    let _log_guard = tt_spotify_bot::logging::init_logging(&config_path);

    // Carries the current channel across restarts (in memory); the config
    // default is used on a fresh process start.
    let last_channel = std::sync::Arc::new(parking_lot::Mutex::new(None));
    loop {
        // A missing/broken config exits with EXIT_CONFIG_ERROR so the systemd
        // unit's RestartPreventExitStatus stops the service instead of
        // crash-restarting into the same missing file every 2 seconds.
        let config = match BotConfig::load(&config_path) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(tt_spotify_bot::config::EXIT_CONFIG_ERROR);
            }
        };
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        match tt_spotify_bot::bot::runner::run_bot(config, config_path.clone(), shutdown, None, last_channel.clone()).await? {
            BotExit::Restart => {
                tracing::info!("Restarting bot...");
                continue;
            }
            _ => std::process::exit(0),
        }
    }
}

/// Interactive `--update`: check GitHub, show the changelog, confirm, then
/// download + verify + replace this binary. Refuses to run non-interactively
/// (e.g. under systemd) since it needs a y/N answer.
#[cfg(not(windows))]
async fn run_cli_update() -> Result<(), BotError> {
    use std::io::{IsTerminal, Write};
    use std::sync::atomic::AtomicBool;

    let info = match tt_spotify_bot::update::check().await {
        Ok(Some(info)) => info,
        Ok(None) => {
            println!("Already up to date (v{}).", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(e) => {
            eprintln!("Update check failed: {e}");
            std::process::exit(1);
        }
    };

    println!(
        "Update available: {} (you have v{})",
        info.tag,
        env!("CARGO_PKG_VERSION")
    );
    println!("\n{}\n", tt_spotify_bot::update::plain_changelog(&info.changelog));

    if !std::io::stdin().is_terminal() {
        eprintln!(
            "Not a terminal; refusing to update non-interactively. Run `ttspotify --update` from a shell."
        );
        std::process::exit(1);
    }

    print!("Download and install {}? [y/N] ", info.tag);
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).ok();
    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        println!("Cancelled.");
        return Ok(());
    }

    let cancel = AtomicBool::new(false);
    let progress = |done: u64, total: Option<u64>| {
        match total {
            Some(t) if t > 0 => print!("\rDownloading... {}%   ", done * 100 / t),
            _ => print!("\rDownloading... {done} bytes   "),
        }
        let _ = std::io::stdout().flush();
    };
    match tt_spotify_bot::update::download_and_apply(&info, &progress, &cancel).await {
        Ok(()) => {
            println!("\nUpdated to {}.", info.tag);
            // Offer the unit refresh BEFORE restarting bots so a restart
            // picks up the rewritten (daemon-reloaded) unit.
            #[cfg(target_os = "linux")]
            tt_spotify_bot::service::offer_unit_refresh();
            #[cfg(target_os = "linux")]
            tt_spotify_bot::service::offer_restart_running_bots();
            #[cfg(not(target_os = "linux"))]
            println!("Restart the bot to use the new version.");
            Ok(())
        }
        Err(e) => {
            eprintln!("\nUpdate failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Windows system-tray app. Manages multiple bot instances via a wxDragon
/// tray icon. `--setup` opens the GUI config dialog directly.
#[cfg(windows)]
fn main() {
    pin_teamtalk_sdk_version();
    tt_spotify_bot::logging::install_panic_hook();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--setup") {
        let name_arg = args
            .iter()
            .position(|a| a == "--setup")
            .and_then(|i| args.get(i + 1))
            .filter(|s| !s.starts_with('-'));

        let (config, path) = if let Some(name) = name_arg {
            let p = tt_spotify_bot::config::config_dir().join(format!("{name}.json"));
            if p.exists() {
                let cfg = tt_spotify_bot::config::BotConfig::load(p.to_str().unwrap_or(""))
                    .unwrap_or_default();
                (cfg, Some(p))
            } else {
                (tt_spotify_bot::config::BotConfig::default(), None)
            }
        } else {
            (tt_spotify_bot::config::BotConfig::default(), None)
        };

        let _ = wxdragon::main(|_| {
            tt_spotify_bot::gui::config_dialog::open_config_dialog(config, path, |saved_path| {
                tracing::info!("Config saved to: {}", saved_path.display());
            });
        });
        return;
    }

    tt_spotify_bot::gui::run();
}
