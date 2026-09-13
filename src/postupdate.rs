//! Work that has to happen once after the binary changes version, run by the
//! binary that arrived rather than the one it replaced.
//!
//! Every step here compares something on disk against a constant compiled into
//! this build: the config schema, the systemd unit template, the YouTube tools
//! this build plays with. That comparison only means anything in the new binary. It used to
//! run in the old one — `self_replace` swaps the file on disk, but the process
//! already in memory keeps its own code and its own constants, so a v0.7.0 bot
//! updating to v1.0.0 compared v0.7.0's unit stamp against v0.7.0's version
//! number, found them equal, and said nothing. Nobody's service file was ever
//! rewritten by an update.
//!
//! So there are two triggers and one function. [`reconcile`] runs at every bot
//! startup, which catches a binary replaced by any means — the built-in
//! updater, a tarball unpacked over the old one, a distro package, a copy from
//! a build — and it runs again immediately after an update, through the hidden
//! `post-update` subcommand that the updater re-execs. Both are the new binary.
//! It is idempotent and silent when there is nothing to do.

/// Where a reconcile is running, which decides whether it may ask questions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A bot starting up. Often a systemd service with nobody watching, so
    /// this path never prompts and never downloads.
    Startup,
    /// The `post-update` run, straight after an update in the user's terminal.
    Interactive,
}

/// Bring everything on disk into line with what this build expects.
///
/// Best-effort throughout: a bot must start even if every step here fails.
pub fn reconcile(mode: Mode) {
    // New config keys, stamped into every bot's file so they can be seen and
    // edited without waiting for each bot to run.
    crate::config::top_up_configs();

    #[cfg(target_os = "linux")]
    reconcile_unit(mode);

    check_youtube_tools(mode);
}

/// Rewrite the systemd unit when it predates this build's template.
#[cfg(target_os = "linux")]
fn reconcile_unit(mode: Mode) {
    use crate::service::UnitRefresh;
    match crate::service::refresh_stale_unit() {
        UnitRefresh::NotInstalled | UnitRefresh::Current => {}
        UnitRefresh::Refreshed(was) => {
            let msg = format!(
                "Service file updated (was version {was}). It takes effect the next time each \
                 bot restarts; the previous file is kept as ttspotify@.service.bak."
            );
            match mode {
                Mode::Interactive => println!("{msg}"),
                Mode::Startup => tracing::info!("{msg}"),
            }
        }
        // The unit lives outside every path the sandbox grants a bot, so a
        // refresh from inside a running service can be refused depending on
        // the kernel and systemd version. Say what to run, which is exactly
        // what the release before this one did for everybody.
        UnitRefresh::Failed(why) => {
            let msg = format!(
                "Your systemd service file is from an older release and could not be updated \
                 automatically ({why}). To refresh it, {}",
                crate::hints::install_service()
            );
            match mode {
                Mode::Interactive => println!("{msg}"),
                Mode::Startup => tracing::warn!("{msg}"),
            }
        }
    }
}

/// YouTube tools installed by a version that played through yt-dlp do not
/// include the Deno sidecar, so YouTube is silent until they are installed.
fn check_youtube_tools(mode: Mode) {
    use crate::youtube::setup;
    let Ok(paths) = setup::resolve_paths() else {
        return;
    };
    if !setup::has_legacy_tools(&paths) || setup::is_installed(&paths) {
        return;
    }
    match mode {
        Mode::Startup => tracing::warn!(
            "YouTube playback now runs on Deno, which the YouTube tools installed by an older \
             version do not include; YouTube tracks will not play until they are installed. \
             To install them, {}",
            crate::hints::install_youtube_tools()
        ),
        Mode::Interactive => {
            println!();
            println!("YouTube playback now runs on Deno, which your YouTube tools do not include yet,");
            println!("so YouTube tracks would not play.");
            if prompt_yes_no("Install the YouTube tools now?") {
                if let Err(e) = crate::wizard::run_youtube_setup() {
                    println!("  Could not install the YouTube tools: {e}");
                    println!("  To try again later, {}", crate::hints::install_youtube_tools());
                }
            } else {
                println!("  Skipped. To do it later, {}", crate::hints::install_youtube_tools());
            }
        }
    }
}

/// Its own copy rather than the one in `service.rs`, which is Linux-only while
/// this module compiles everywhere.
fn prompt_yes_no(message: &str) -> bool {
    use std::io::Write;
    print!("{message} [y/N] ");
    std::io::stdout().flush().ok();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}
