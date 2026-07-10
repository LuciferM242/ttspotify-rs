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

/// Pin the TeamTalk SDK version unless the user explicitly overrode it. Call
/// once, first thing in `main`, before any TeamTalk client is created.
fn pin_teamtalk_sdk_version() {
    if std::env::var_os("TEAMTALK_SDK_VERSION").is_none() {
        std::env::set_var("TEAMTALK_SDK_VERSION", PINNED_TEAMTALK_SDK_VERSION);
    }
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
}

#[cfg(not(windows))]
#[tokio::main]
async fn main() -> Result<(), BotError> {
    pin_teamtalk_sdk_version();
    tt_spotify_bot::logging::install_panic_hook();
    let args = Args::parse();

    if let Some(ref name) = args.setup {
        let name = if name.is_empty() { None } else { Some(name.as_str()) };
        return tt_spotify_bot::wizard::run_wizard(name);
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
        let config = BotConfig::load(&config_path)?;
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
