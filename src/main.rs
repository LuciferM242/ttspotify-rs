mod audio;
mod bot;
mod config;
mod error;
mod logging;
mod service;
mod spotify;
mod tt;
mod wizard;

use std::sync::Arc;

use clap::Parser;

use crate::bot::runner::BotExit;
use crate::config::BotConfig;
use crate::error::BotError;

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
    #[arg(long)]
    install_service: bool,

    /// Remove systemd user service (Linux only)
    #[arg(long)]
    uninstall_service: bool,

    /// Authenticate with Spotify and exit (no bot startup)
    #[arg(long)]
    auth: bool,

    /// Check if Spotify credentials are cached and exit
    #[arg(long)]
    auth_status: bool,
}

#[tokio::main]
async fn main() -> Result<(), BotError> {
    // Parse CLI args
    let args = Args::parse();

    // Handle setup wizard
    if let Some(ref name) = args.setup {
        let name = if name.is_empty() { None } else { Some(name.as_str()) };
        return wizard::run_wizard(name);
    }

    // Handle systemd service management (Linux only)
    if args.install_service {
        return service::install_service();
    }
    if args.uninstall_service {
        return service::uninstall_service();
    }

    // Handle --auth-status (read-only check, no connection)
    if args.auth_status {
        let auth = spotify::auth::SpotifyAuth::new();
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

    // Handle --auth (standalone OAuth flow)
    if args.auth {
        tracing_subscriber::fmt()
            .with_target(false)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
            )
            .init();

        let mut auth = spotify::auth::SpotifyAuth::new();
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

    // Resolve config path: explicit --config, or first config in platform config dir
    let config_path = args.config.unwrap_or_else(|| {
        let configs = config::list_configs();
        if let Some((_, path)) = configs.first() {
            path.to_str().unwrap_or("data/config.json").to_string()
        } else {
            config::config_dir().join("config.json").to_str()
                .unwrap_or("data/config.json").to_string()
        }
    });

    // Init logging (stdout + file)
    let _log_guard = logging::init_logging(&config_path);

    // Load config and run the bot
    let config = BotConfig::load(&config_path)?;
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    match bot::runner::run_bot(config, config_path, shutdown, None).await? {
        BotExit::Quit => std::process::exit(0),
        BotExit::Restart => std::process::exit(42),
        BotExit::Shutdown => std::process::exit(0),
    }
}
