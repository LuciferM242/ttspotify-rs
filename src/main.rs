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

/// The clap command, named for the file the user actually launched. The
/// derive hard-codes "tt-spotify-bot", which is the name inside the release
/// archive, but `install` puts it on PATH as `ttspotify` - so help, usage,
/// errors, completions and the man page all used to name a command the reader
/// may not have. `paths::program_name()` is the same source the hint strings
/// have always used.
#[cfg(not(windows))]
fn cli_command() -> clap::Command {
    use clap::CommandFactory;
    let name = tt_spotify_bot::paths::program_name();
    Args::command()
        .name(name.clone())
        .bin_name(name.clone())
        .display_name(name)
}

/// Text that is safe to drop into a roff document: a backslash starts an
/// escape, a hyphen renders as a soft dash unless escaped, and a line opening
/// with `.` or `'` is read as a request rather than words.
#[cfg(not(windows))]
fn roff_escape(text: &str) -> String {
    let escaped = text.replace('\\', "\\e").replace('-', "\\-");
    escaped
        .lines()
        .map(|line| {
            if line.starts_with('.') || line.starts_with('\'') {
                format!("\\&{line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How one argument is written in the man page: `-c, --config <PATH>` for an
/// option, `<NAME>` for a positional.
#[cfg(not(windows))]
fn man_arg_label(arg: &clap::Arg) -> String {
    // A switch like `--purge` still reports a value name, so asking for one
    // unconditionally documented it as `--purge <PURGE>` - something to type a
    // value after, which it is not.
    let values = arg
        .get_action()
        .takes_values()
        .then(|| arg.get_value_names())
        .flatten()
        .map(|names| names.iter().map(|n| format!("<{n}>")).collect::<Vec<_>>().join(" "));

    let mut flags = Vec::new();
    if let Some(short) = arg.get_short() {
        flags.push(format!("-{short}"));
    }
    if let Some(long) = arg.get_long() {
        flags.push(format!("--{long}"));
    }

    if flags.is_empty() {
        // Positional: the value name is the whole label.
        return values.unwrap_or_else(|| format!("<{}>", arg.get_id()));
    }
    match values {
        Some(v) => format!("{} {}", flags.join(", "), v),
        None => flags.join(", "),
    }
}

/// The man page, with every command spelled out on it.
///
/// clap_mangen's own subcommand section lists cross-references like
/// `ttspotify-run(1)`, but `man` prints this one page and nothing generates
/// those, so each reference is a dead end. The sections are rendered
/// individually here so that list can be replaced by the commands themselves.
#[cfg(not(windows))]
fn render_man_page(w: &mut dyn std::io::Write) -> std::io::Result<()> {
    // Every clap_mangen section renderer writes this two-line preamble of its
    // own accord, because each is built to produce a whole document. Composing
    // a page out of them therefore repeats it once per section, so it is kept
    // from the first section only.
    const PREAMBLE: &str = ".ie \\n(.g .ds Aq \\(aq\n.el .ds Aq '\n";

    use std::io::Write;

    let cmd = cli_command();
    let man = clap_mangen::Man::new(cmd.clone());

    let mut page = String::new();
    let mut add = |section: Vec<u8>| {
        let text = String::from_utf8_lossy(&section).into_owned();
        if page.is_empty() {
            page.push_str(&text);
        } else {
            page.push_str(text.strip_prefix(PREAMBLE).unwrap_or(&text));
        }
    };

    for render in [
        clap_mangen::Man::render_title as fn(&clap_mangen::Man, &mut dyn std::io::Write) -> std::io::Result<()>,
        clap_mangen::Man::render_name_section,
        clap_mangen::Man::render_synopsis_section,
        clap_mangen::Man::render_description_section,
        clap_mangen::Man::render_options_section,
    ] {
        let mut buf = Vec::new();
        render(&man, &mut buf)?;
        add(buf);
    }

    // Each command spelled out, rather than clap_mangen's cross-references to
    // pages like `ttspotify-run(1)` that nothing generates.
    let subs: Vec<_> = cmd.get_subcommands().filter(|c| !c.is_hide_set()).collect();
    if !subs.is_empty() {
        let mut buf = Vec::new();
        writeln!(buf, ".SH COMMANDS")?;
        for sub in subs {
            writeln!(buf, ".SS {}", roff_escape(sub.get_name()))?;
            if let Some(about) = sub.get_about() {
                writeln!(buf, "{}", roff_escape(&about.to_string()))?;
            }
            for arg in sub.get_arguments().filter(|a| !a.is_hide_set()) {
                writeln!(buf, ".TP")?;
                writeln!(buf, "\\fB{}\\fR", roff_escape(&man_arg_label(arg)))?;
                if let Some(help) = arg.get_help() {
                    writeln!(buf, "{}", roff_escape(&help.to_string()))?;
                }
            }
            // `service install`, `cache clear` and the like: without these the
            // page lists a command whose only real use is a word further on.
            for child in sub.get_subcommands().filter(|c| !c.is_hide_set()) {
                writeln!(buf, ".TP")?;
                writeln!(
                    buf,
                    "\\fB{} {}\\fR",
                    roff_escape(sub.get_name()),
                    roff_escape(child.get_name())
                )?;
                if let Some(about) = child.get_about() {
                    writeln!(buf, "{}", roff_escape(&about.to_string()))?;
                }
            }
        }
        add(buf);
    }

    let mut buf = Vec::new();
    man.render_version_section(&mut buf)?;
    add(buf);

    w.write_all(page.as_bytes())
}

#[cfg(not(windows))]
#[derive(Parser)]
#[command(name = "tt-spotify-bot", about = "TeamTalk Spotify Bot", version)]
struct Args {
    // Every generated systemd unit runs the bot as `--config <path>`, so this
    // stays a top-level option instead of becoming part of `run`: making it a
    // subcommand argument would break every service already on disk.
    /// Run the bot configured in this file
    #[arg(short, long, value_name = "PATH")]
    config: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[cfg(not(windows))]
#[derive(clap::Subcommand)]
enum Commands {
    /// Run a bot in this terminal
    Run {
        /// Which bot; asks when there is more than one and you do not say
        name: Option<String>,
    },

    /// Create a bot
    Add {
        /// Name for the new bot, used for its file and its service
        name: Option<String>,
    },

    /// Delete a bot
    #[cfg(target_os = "linux")]
    Remove {
        /// Which bot; asks when you do not say
        name: Option<String>,
    },

    /// Change a bot's settings
    #[cfg(target_os = "linux")]
    Edit {
        /// Which bot; asks when you do not say
        name: Option<String>,
    },

    /// List the bots configured on this machine
    #[cfg(target_os = "linux")]
    List,

    /// Show which bots are running
    #[cfg(target_os = "linux")]
    Status,

    /// Start a bot in the background
    #[cfg(target_os = "linux")]
    Start {
        /// A bot's name, or "all"
        name: String,
    },

    /// Stop a bot running in the background
    #[cfg(target_os = "linux")]
    Stop {
        /// A bot's name, or "all"
        name: String,
    },

    /// Restart a bot running in the background
    #[cfg(target_os = "linux")]
    Restart {
        /// A bot's name, or "all"
        name: String,
    },

    /// Show what a bot has been doing
    #[cfg(target_os = "linux")]
    Logs {
        /// Which bot's log to show
        name: String,
    },

    /// Watch a bot's log as it happens (Ctrl+C stops watching, not the bot)
    #[cfg(target_os = "linux")]
    Watch {
        /// Which bot to watch
        name: String,
    },

    /// Check this install and say what to fix
    #[cfg(target_os = "linux")]
    Doctor,

    /// Show how much music is cached, or clear it
    #[cfg(target_os = "linux")]
    Cache {
        #[command(subcommand)]
        action: Option<CacheAction>,
    },

    /// Copy this binary onto your PATH
    #[cfg(target_os = "linux")]
    Install,

    /// Remove the binary, and optionally everything else
    #[cfg(target_os = "linux")]
    Uninstall {
        /// Also ask about deleting configs, logins and logs
        #[arg(long)]
        purge: bool,
    },

    /// Install or remove the systemd service
    #[cfg(target_os = "linux")]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },

    /// Install or update the YouTube tools
    Youtube {
        #[command(subcommand)]
        action: YoutubeAction,
    },

    /// Sign in to Spotify
    Auth {
        #[command(subcommand)]
        action: Option<AuthAction>,
    },

    /// Update the bot itself
    Update,

    /// Print a completion script for your shell
    Completions {
        /// bash, zsh, fish, elvish or powershell
        shell: clap_complete::Shell,
    },

    /// Print this program's man page (roff)
    Man,
}

#[cfg(not(windows))]
#[cfg(target_os = "linux")]
#[derive(clap::Subcommand)]
enum ServiceAction {
    /// Install the systemd user service (does not move the binary)
    Install,
    /// Remove the systemd user service only
    ///
    /// `uninstall` is accepted too: the top-level pair is install/uninstall,
    /// and typing the same word here was refused with a tip that pointed at
    /// `install`.
    #[command(alias = "uninstall")]
    Remove,
}

#[cfg(not(windows))]
#[derive(clap::Subcommand)]
enum YoutubeAction {
    /// Download yt-dlp, bgutil-pot and a JavaScript runtime
    Install,
    /// Update those tools in place
    Update,
}

#[cfg(not(windows))]
#[cfg(target_os = "linux")]
#[derive(clap::Subcommand)]
enum CacheAction {
    /// Delete the cached music (it downloads again when next asked for)
    Clear,
}

#[cfg(not(windows))]
#[derive(clap::Subcommand)]
enum AuthAction {
    /// Say whether a Spotify login is already cached
    Status,
}

#[cfg(not(windows))]
#[tokio::main]
async fn main() -> Result<(), BotError> {
    pin_teamtalk_sdk_version();
    // One-time move of an old exe-side tools install to the XDG data dir,
    // before anything resolves tool paths.
    tt_spotify_bot::youtube::setup::migrate_legacy_tools();
    tt_spotify_bot::logging::install_panic_hook();
    use clap::FromArgMatches;
    let args = Args::from_arg_matches(&cli_command().get_matches())
        .unwrap_or_else(|e| e.exit());

    // Rust ignores SIGPIPE, which turns `--doctor | head` into a panic about
    // failing to print. The bot itself needs that default kept — a write to a
    // TeamTalk socket the server has closed would otherwise kill the process
    // instead of returning an error — so only the commands that print and
    // exit hand SIGPIPE back to the kernel.
    if prints_and_exits(&args.command) {
        // SAFETY: setting a signal disposition before any thread is spawned,
        // to the value every other Unix program uses.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }


    // Sort a legacy flat data folder into config/state/cache/auth before
    // anything reads from it. Logging is not up yet, so the report is logged
    // below once it is.
    let layout_migration = tt_spotify_bot::paths::migrate_data_layout();

    // `--config` says which bot to run; a subcommand says to do something else
    // entirely. Together they used to mean the subcommand won and the path was
    // silently dropped, so `--config foo.json run` quietly ignored the file it
    // was handed and asked which bot to run instead.
    if args.config.is_some() && args.command.is_some() {
        finish(Err(BotError::Usage(format!(
            "--config runs the bot in that file, so it cannot be combined with a command.\n\
             To run it: {prog} --config <path>\n\
             To run a bot by name: {prog} run <name>",
            prog = tt_spotify_bot::paths::program_name()
        ))));
    }

    // Everything except running a bot is done here and exits; `run` and a bare
    // `--config` fall through to the bot itself.
    let named = match args.command {
        Some(Commands::Run { ref name }) => {
            match tt_spotify_bot::control::resolve_run_target(name.as_deref()) {
                Ok(Some(path)) => Some(path.to_string_lossy().into_owned()),
                Ok(None) => return Ok(()), // Picker cancelled.
                Err(e) => finish(Err(e)),
            }
        }
        // Every command reports through `finish`, so a usage mistake prints a
        // sentence and exits 1 rather than reaching main's Debug formatting as
        // `Usage("...")`.
        Some(command) => finish(run_command(command).await),
        None => None,
    };

    let config_path = match named.or(args.config) {
        Some(path) => path,
        // No arguments at all: set the machine up if nothing is set up yet,
        // otherwise show what the commands are. Starting a bot nobody named
        // is what this used to do, and it hid every other command.
        None => match first_run_or_help()? {
            Some(path) => path,
            None => return Ok(()),
        },
    };

    // An old systemd unit may still name the pre-move location.
    let config_path = tt_spotify_bot::config::resolve_config_path(&config_path)
        .to_string_lossy()
        .into_owned();

    // Settle the config before logging starts. The log folder is named after
    // the config file, so a path that is a directory, or missing, or not a bot
    // config used to leave a folder named after it — `logs/config/` next to the
    // real bots — and only then report the error. This also means the
    // first-run wizard (which a missing --config triggers when someone is at
    // the keyboard) has created the file before anything logs.
    if let Err(e) = BotConfig::load(&config_path) {
        eprintln!("{e}");
        std::process::exit(tt_spotify_bot::config::EXIT_CONFIG_ERROR);
    }

    // One bot per config: a manual run while the service has the same bot
    // running would put two sessions on one TeamTalk account, each knocking the
    // other offline. Held until the process ends, whichever way it ends.
    #[cfg(target_os = "linux")]
    let _run_lock = {
        use tt_spotify_bot::runlock::LockError;
        match tt_spotify_bot::runlock::acquire(std::path::Path::new(&config_path)) {
            Ok(lock) => Some(lock),
            Err(LockError::AlreadyRunning) => {
                let name = std::path::Path::new(&config_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("that bot");
                let prog = tt_spotify_bot::paths::program_name();
                eprintln!("\"{name}\" is already running on this machine.");
                eprintln!("To see what it is doing: {prog} watch {name}");
                // Which stop applies depends on how the other copy was
                // started, and from here there is no way to tell.
                eprintln!("To stop it: {prog} stop {name}, or Ctrl+C in the terminal running it");
                // The same code a broken config exits with, and for the same
                // reason: the unit lists it in RestartPreventExitStatus, so
                // systemd stops rather than restarting every two seconds into
                // a refusal that will not change.
                std::process::exit(tt_spotify_bot::config::EXIT_CONFIG_ERROR);
            }
            // A lock we cannot create is not a reason to refuse to run: that
            // would turn a read-only data directory into a bot that never
            // starts, which is worse than the clash being prevented.
            Err(LockError::Unavailable(why)) => {
                eprintln!("Could not take the single-instance lock ({why}); carrying on.");
                None
            }
        }
    };

    let _log_guard = tt_spotify_bot::logging::init_logging(&config_path);

    // Say it where it will be seen rather than asking: this runs under systemd
    // as often as not, where there is nobody to answer a prompt. An upgrade
    // done by any means other than the built-in updater — a new tarball over
    // the old binary, a package, a copy from a build — leaves the unit as it
    // was, and nothing else mentions it outside `doctor`.
    #[cfg(target_os = "linux")]
    if let Some((installed, current)) = tt_spotify_bot::service::installed_unit_version() {
        if installed < current {
            tracing::warn!(
                "Your systemd service file is from an older release. To refresh it, {}",
                tt_spotify_bot::hints::install_service()
            );
        }
    }
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
                return Ok(());
            }
            // Through `finish` like every other path: returning the error
            // from main prints it as `TeamTalk("...")` rather than as the
            // sentence the user needs.
            Err(e) => finish(Err(e)),
        };

        match exit {
            BotExit::Restart => {
                tracing::info!("Restarting bot...");
                continue;
            }
            // `return`, not `process::exit`: exiting here would skip the log
            // guard's Drop, and the appender's buffered tail -- the disconnect
            // and the reason for stopping -- is exactly what someone reads the
            // log for afterwards.
            _ => return Ok(()),
        }
    }
}

/// Whether this command only prints and exits, and so should die quietly when
/// piped into something that stops reading (`doctor | head`).
#[cfg(not(windows))]
fn prints_and_exits(command: &Option<Commands>) -> bool {
    match command {
        Some(Commands::Completions { .. }) | Some(Commands::Man) => true,
        Some(Commands::Auth { action: Some(AuthAction::Status) }) => true,
        #[cfg(target_os = "linux")]
        Some(Commands::List) | Some(Commands::Status) | Some(Commands::Doctor) => true,
        #[cfg(target_os = "linux")]
        Some(Commands::Cache { action: None }) => true,
        // `watch` is left alone: it is meant to be piped into something that
        // reads until Ctrl+C, and dying on the first closed pipe would defeat
        // it.
        #[cfg(target_os = "linux")]
        Some(Commands::Logs { .. }) => true,
        _ => false,
    }
}

/// Everything that is not "run a bot".
#[cfg(not(windows))]
async fn run_command(command: Commands) -> Result<(), BotError> {
    use clap::CommandFactory;

    // The bot sets up file logging later; these commands exit before that, so
    // without a subscriber here every tracing::warn! they cause goes nowhere.
    // `list` skipping an unreadable config, or `run` reporting a bot as
    // missing when its file failed to parse, then look like the file is simply
    // not there.
    let _ = tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .without_time()
        .try_init();

    match command {
        // Intercepted by the caller, which needs the config path rather than
        // an exit code. Reaching here would mean `run` silently did nothing.
        Commands::Run { .. } => unreachable!("run is resolved before dispatch"),

        Commands::Add { name } => {
            tt_spotify_bot::wizard::run_wizard(name.as_deref(), true).map(|_| ())
        }

        #[cfg(target_os = "linux")]
        Commands::Remove { name } => finish(tt_spotify_bot::control::remove(name.as_deref())),
        #[cfg(target_os = "linux")]
        Commands::Edit { name } => finish(tt_spotify_bot::editor::run(name.as_deref())),
        #[cfg(target_os = "linux")]
        Commands::List => {
            tt_spotify_bot::control::list();
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Commands::Status => {
            tt_spotify_bot::control::status();
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Commands::Start { name } => finish(tt_spotify_bot::control::control("start", &name)),
        #[cfg(target_os = "linux")]
        Commands::Stop { name } => finish(tt_spotify_bot::control::control("stop", &name)),
        #[cfg(target_os = "linux")]
        Commands::Restart { name } => finish(tt_spotify_bot::control::control("restart", &name)),
        #[cfg(target_os = "linux")]
        Commands::Logs { name } => finish(tt_spotify_bot::control::logs(&name, false)),
        #[cfg(target_os = "linux")]
        Commands::Watch { name } => finish(tt_spotify_bot::control::logs(&name, true)),
        #[cfg(target_os = "linux")]
        Commands::Doctor => {
            tt_spotify_bot::doctor::report();
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Commands::Cache { action } => match action {
            None => {
                report_cache();
                Ok(())
            }
            Some(CacheAction::Clear) => finish(clear_cache()),
        },
        #[cfg(target_os = "linux")]
        Commands::Install => finish(tt_spotify_bot::install::install()),
        #[cfg(target_os = "linux")]
        Commands::Uninstall { purge } => finish(tt_spotify_bot::install::uninstall(purge)),
        #[cfg(target_os = "linux")]
        Commands::Service { action } => match action {
            ServiceAction::Install => tt_spotify_bot::service::install_service(),
            ServiceAction::Remove => tt_spotify_bot::service::uninstall_service(),
        },

        Commands::Youtube { action } => {
            let result = match action {
                YoutubeAction::Install => tt_spotify_bot::wizard::run_youtube_setup(),
                YoutubeAction::Update => tt_spotify_bot::wizard::run_update_tools(),
            };
            finish(result)
        }

        Commands::Auth { action: Some(AuthAction::Status) } => {
            let auth = tt_spotify_bot::spotify::auth::SpotifyAuth::new();
            if auth.has_cached_credentials() {
                println!("Spotify: signed in (the login may still have expired).");
                std::process::exit(0);
            }
            println!("Spotify: not signed in.");
            println!("  Sign in with: {} auth", tt_spotify_bot::paths::program_name());
            std::process::exit(1);
        }
        Commands::Auth { action: None } => {
            tracing_subscriber::fmt()
                .with_target(false)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .init();

            let mut auth = tt_spotify_bot::spotify::auth::SpotifyAuth::new();
            match auth.connect().await {
                Ok(_) => {
                    println!("Signed in to Spotify. The login is cached for every bot here.");
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Spotify sign-in failed: {e}");
                    std::process::exit(1);
                }
            }
        }

        Commands::Update => run_cli_update().await,

        Commands::Completions { shell } => {
            let mut command = cli_command();
            let name = tt_spotify_bot::paths::program_name();
            clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
            Ok(())
        }
        Commands::Man => {
            render_man_page(&mut std::io::stdout())?;
            Ok(())
        }
    }
}

/// A run with no arguments at all.
///
/// Nothing configured yet means this is someone's first go: offer to install,
/// then walk them through creating a bot, then run it — the shortest path from
/// a downloaded file to music. Once bots exist, the useful thing to show is
/// what the commands are, since starting an unnamed bot is now `run`'s job.
///
/// Returns the config to run, or `None` when the help was printed instead.
#[cfg(not(windows))]
fn first_run_or_help() -> Result<Option<String>, BotError> {
    if !tt_spotify_bot::config::list_configs().is_empty() {
        let _ = cli_command().print_help();
        println!();
        return Ok(None);
    }

    #[cfg(target_os = "linux")]
    tt_spotify_bot::install::offer_first_run_install();

    match tt_spotify_bot::wizard::run_wizard(None, true)? {
        Some(path) => Ok(Some(path.to_string_lossy().into_owned())),
        None => Ok(None),
    }
}

/// Print what the caches hold, against the ceiling that bounds them.
#[cfg(not(windows))]
#[cfg(target_os = "linux")]
fn report_cache() {
    use tt_spotify_bot::audio_cache;
    let used = audio_cache::size_bytes();
    let settings = tt_spotify_bot::settings::load();
    if settings.caching_off() {
        println!(
            "Cached music: {} (the limit is 0, so nothing is kept)",
            audio_cache::human_size(used)
        );
    } else {
        println!(
            "Cached music: {} of {}",
            audio_cache::human_size(used),
            audio_cache::human_size(settings.cache_limit_bytes())
        );
    }
    println!();
    println!("Shared by every bot on this install. Clear it with:");
    println!("  {} cache clear", tt_spotify_bot::paths::program_name());
}

/// Empty the caches and say how much that freed.
#[cfg(not(windows))]
#[cfg(target_os = "linux")]
fn clear_cache() -> Result<(), BotError> {
    use tt_spotify_bot::audio_cache;
    let used = audio_cache::size_bytes();
    if used == 0 {
        println!("Nothing cached to clear.");
        return Ok(());
    }
    let freed = audio_cache::clear()?;
    println!("Freed {}.", audio_cache::human_size(freed));
    Ok(())
}

/// End a one-shot command: its message, then an exit code.
///
/// Returning the error from `main` instead would print it through `Debug`
/// ("Usage(\"No config named ...\")"), which is not a sentence anyone wants to
/// read at the end of a mistyped command.
#[cfg(not(windows))]
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
            "Not a terminal; refusing to update non-interactively. Run `{} update` from a shell.",
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
