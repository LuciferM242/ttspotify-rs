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
const PINNED_TEAMTALK_SDK_VERSION: &str = "v5.22a";

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
#[command(name = "tt-spotify-bot", about = "TeamTalk Spotify Bot", version)]
struct Args {
    /// Path to config file
    #[arg(short, long)]
    config: Option<String>,

    /// Run the interactive config setup wizard
    #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    setup: Option<String>,

    /// List the bots configured on this machine
    #[cfg(target_os = "linux")]
    #[arg(long)]
    list: bool,

    /// Show which bots are running
    #[cfg(target_os = "linux")]
    #[arg(long)]
    status: bool,

    /// Start a bot by config name, or "all"
    #[cfg(target_os = "linux")]
    #[arg(long, value_name = "NAME")]
    start: Option<String>,

    /// Stop a bot by config name, or "all"
    #[cfg(target_os = "linux")]
    #[arg(long, value_name = "NAME")]
    stop: Option<String>,

    /// Restart a bot by config name, or "all"
    #[cfg(target_os = "linux")]
    #[arg(long, value_name = "NAME")]
    restart: Option<String>,

    /// Show a bot's log
    #[cfg(target_os = "linux")]
    #[arg(long, value_name = "NAME")]
    logs: Option<String>,

    /// With --logs: keep printing new lines as they arrive
    #[cfg(target_os = "linux")]
    #[arg(long, requires = "logs")]
    follow: bool,

    /// Show what is installed, what is running, and what to fix
    #[cfg(target_os = "linux")]
    #[arg(long)]
    doctor: bool,

    /// Copy this binary onto your PATH (does not touch systemd)
    #[cfg(target_os = "linux")]
    #[arg(long)]
    install: bool,

    /// Remove the binary, and optionally the service, configs and tools
    #[cfg(target_os = "linux")]
    #[arg(long)]
    uninstall: bool,

    /// With --uninstall: also ask about deleting configs, logins and logs
    #[cfg(target_os = "linux")]
    #[arg(long, requires = "uninstall")]
    purge: bool,

    /// Install the systemd user service (does not move the binary)
    #[cfg(target_os = "linux")]
    #[arg(long)]
    install_service: bool,

    /// Remove the systemd user service only
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
        return tt_spotify_bot::wizard::run_wizard(name, true).map(|_| ());
    }

    #[cfg(target_os = "linux")]
    if args.doctor {
        tt_spotify_bot::doctor::report();
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if args.list {
        tt_spotify_bot::control::list();
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if args.status {
        tt_spotify_bot::control::status();
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    for (verb, target) in [
        ("start", &args.start),
        ("stop", &args.stop),
        ("restart", &args.restart),
    ] {
        if let Some(target) = target {
            finish(tt_spotify_bot::control::control(verb, target));
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(ref name) = args.logs {
        finish(tt_spotify_bot::control::logs(name, args.follow));
    }
    #[cfg(target_os = "linux")]
    if args.install {
        finish(tt_spotify_bot::install::install());
    }
    #[cfg(target_os = "linux")]
    if args.uninstall {
        finish(tt_spotify_bot::install::uninstall(args.purge));
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

    // Sort a legacy flat data folder into config/state/cache/auth before
    // anything reads from it. Logging is not up yet, so the report is logged
    // below once it is.
    let layout_migration = tt_spotify_bot::paths::migrate_data_layout();

    let config_path = match args.config {
        Some(path) => path,
        None => {
            use std::io::IsTerminal;
            let configs = tt_spotify_bot::config::list_configs();
            let interactive = std::io::stdin().is_terminal();
            match tt_spotify_bot::config::choose_config(&configs, interactive) {
                Some(path) => path.to_string_lossy().into_owned(),
                // No configs: this path leads to the first-run wizard. Having
                // configs but choosing none means the user backed out.
                None if configs.is_empty() => tt_spotify_bot::paths::configs_dir()
                    .join("config.json")
                    .to_string_lossy()
                    .into_owned(),
                None => return Ok(()),
            }
        }
    };

    // An old systemd unit may still name the pre-move location.
    let config_path = tt_spotify_bot::config::resolve_config_path(&config_path)
        .to_string_lossy()
        .into_owned();

    let _log_guard = tt_spotify_bot::logging::init_logging(&config_path);
    tt_spotify_bot::paths::log_migration(&layout_migration);

    // Carries the current channel across restarts (in memory); the config
    // default is used on a fresh process start.
    let last_channel = std::sync::Arc::new(parking_lot::Mutex::new(None));

    // `systemctl --user stop` sends SIGTERM and Ctrl+C sends SIGINT; without a
    // handler either one kills the process outright, so the bot never leaves
    // the TeamTalk channel and the server holds a ghost user until it times
    // out. Both now set the same flag the tray's stop button sets, which the
    // event loop polls and answers with a real disconnect.
    let signalled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal_notify = Arc::new(tokio::sync::Notify::new());
    spawn_signal_watcher(signalled.clone(), signal_notify.clone());

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
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(
            // A signal that arrived between two runs (during a restart, say)
            // must not be dropped on the floor.
            signalled.load(std::sync::atomic::Ordering::Relaxed),
        ));

        // Each run gets its own flag — a restart clears it — so a task per run
        // carries the signal across to whichever flag is live at the time.
        let bridge = {
            let notify = signal_notify.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                notify.notified().await;
                shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        };

        let exit =
            tt_spotify_bot::bot::runner::run_bot(config, config_path.clone(), shutdown, None, last_channel.clone())
                .await;
        bridge.abort();

        let exit = match exit {
            Ok(exit) => exit,
            // Connecting and logging in happen inside blocking SDK calls that
            // cannot be interrupted, so a signal during startup is only seen
            // once they give up (bounded by the SDK's own 10s waits). Failing
            // to reach a server we were told to stop talking to is not an
            // error worth a non-zero exit and a systemd restart.
            Err(e) if signalled.load(std::sync::atomic::Ordering::Relaxed) => {
                tracing::info!("Stopped before the bot finished connecting ({e})");
                std::process::exit(0);
            }
            Err(e) => return Err(e),
        };

        match exit {
            BotExit::Restart => {
                tracing::info!("Restarting bot...");
                continue;
            }
            _ => std::process::exit(0),
        }
    }
}

/// End a one-shot command: its message, then an exit code.
///
/// Returning the error from `main` instead would print it through `Debug`
/// ("Usage(\"No config named ...\")"), which is not a sentence anyone wants to
/// read at the end of a mistyped command.
#[cfg(target_os = "linux")]
fn finish(result: Result<(), BotError>) -> ! {
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

/// Turn SIGTERM and SIGINT into the same clean stop the tray performs.
///
/// The first signal asks the bot to leave the server and exit; a second one
/// gives up waiting and quits on the spot, so a stop that hangs is still
/// answerable from the keyboard.
#[cfg(not(windows))]
fn spawn_signal_watcher(signalled: Arc<std::sync::atomic::AtomicBool>, notify: Arc<tokio::sync::Notify>) {
    use std::sync::atomic::Ordering;
    use tokio::signal::unix::{signal, SignalKind};

    tokio::spawn(async move {
        let (mut term, mut interrupt) =
            match (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
                (Ok(term), Ok(interrupt)) => (term, interrupt),
                _ => {
                    // Without handlers the default disposition still applies:
                    // the bot dies on signal exactly as it did before.
                    tracing::warn!("Could not install signal handlers; stop will not be graceful");
                    return;
                }
            };

        loop {
            tokio::select! {
                _ = term.recv() => {}
                _ = interrupt.recv() => {}
            }

            if signalled.swap(true, Ordering::Relaxed) {
                eprintln!("Second signal received - exiting now.");
                std::process::exit(130);
            }
            tracing::info!("Stopping: leaving the server. Signal again to quit immediately.");
            notify.notify_one();
        }
    });
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
            "Not a terminal; refusing to update non-interactively. Run `{} --update` from a shell.",
            tt_spotify_bot::paths::program_name()
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

/// Windows system-tray app. Manages multiple bot instances from the tray.
/// `--setup` opens the config editor directly.
#[cfg(windows)]
fn main() {
    // Brings winsafe's GuiWindow/GuiEventsParent methods into scope for the
    // setup path's throwaway owner window.
    use winsafe::prelude::*;

    pin_teamtalk_sdk_version();
    tt_spotify_bot::logging::install_panic_hook();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--setup") {
        let name_arg = args
            .iter()
            .position(|a| a == "--setup")
            .and_then(|i| args.get(i + 1))
            .filter(|s| !s.starts_with('-'));

        // Sort the data folder first, exactly as the tray does. Without this a
        // pre-migration install still has its configs loose in the root, the
        // lookup below misses them, and saving creates a blank config at the
        // new location that permanently shadows the real one (the migration
        // never overwrites an existing destination).
        let _ = tt_spotify_bot::paths::migrate_data_layout();

        let (config, path) = if let Some(name) = name_arg {
            let p = tt_spotify_bot::paths::config_file(name);
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

        // The editor is modal and owns its own message loop, so the setup
        // path no longer starts one of its own.
        let owner = winsafe::gui::WindowMain::new(winsafe::gui::WindowMainOpts {
            title: "TT Spotify",
            ex_style: winsafe::co::WS_EX::TOOLWINDOW,
            style: winsafe::co::WS::OVERLAPPED,
            size: (0, 0),
            ..Default::default()
        });
        let owner2 = owner.clone();
        owner.on().wm_create(move |_| {
            if let Some(saved) =
                tt_spotify_bot::gui_native::config_dialog::show(&owner2, config.clone(), path.clone())
            {
                tracing::info!("Config saved to: {}", saved.display());
            }
            let _ = owner2.hwnd().DestroyWindow();
            Ok(0)
        });
        let _ = owner.run_main(Some(winsafe::co::SW::HIDE));
        return;
    }

    tt_spotify_bot::gui_native::run();
}
