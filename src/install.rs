//! Putting the binary on PATH, and taking it (and everything it created) back
//! off again.
//!
//! Windows ships a tray app you unzip and double-click; Linux shipped a tarball
//! and a README paragraph telling you to run `sudo install -m755 ...` and to
//! remember, on the way out, six paths nobody could be expected to guess. This
//! module is that paragraph, as a command.
//!
//! The one thing it must never do is leave two copies at different versions:
//! the shell would run one and the systemd unit the other, and "I updated but
//! it still behaves like the old one" is close to undebuggable over a chat
//! window. So an install looks for copies already on disk and replaces the one
//! in use rather than adding another, and it reconciles the unit's ExecStart
//! with wherever the binary ended up.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::BotError;
use crate::service::prompt_yes_no;

/// The name to install under when nothing is installed yet. The README, the
/// systemd instances and every hint string use it.
const DEFAULT_NAME: &str = "ttspotify";

/// Names an existing install could be sitting under: the documented one, the
/// one Cargo builds, and whatever this copy is currently called.
fn known_names(program: &str) -> Vec<String> {
    let mut names = vec![DEFAULT_NAME.to_string(), "tt-spotify-bot".to_string()];
    if !names.iter().any(|n| n == program) {
        names.push(program.to_string());
    }
    names
}

/// Directories worth searching for an earlier install: everything on PATH plus
/// the usual install locations. A copy in /usr/local/bin matters even when
/// PATH does not mention it — it is exactly the copy an old unit file points
/// at.
fn candidate_dirs(path_env: &str, home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |dir: PathBuf| {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    for entry in path_env.split(':') {
        push(PathBuf::from(entry));
    }
    push(home.join(".local/bin"));
    push(PathBuf::from("/usr/local/bin"));
    push(PathBuf::from("/usr/bin"));
    dirs
}

/// Whether PATH already lists this directory, so the install can say "and
/// you'll need this on your PATH" only when it is true.
fn path_lists_dir(path_env: &str, dir: &Path) -> bool {
    path_env.split(':').any(|entry| !entry.is_empty() && Path::new(entry) == dir)
}

/// Where to install: over an existing copy when there is one, otherwise
/// `~/.local/bin`, which needs no sudo.
fn destination(existing: &[PathBuf], home: &Path) -> PathBuf {
    match existing.first() {
        Some(path) => path.clone(),
        None => home.join(".local/bin").join(DEFAULT_NAME),
    }
}

/// Copies of the bot already installed, in `dirs` order.
fn find_existing(dirs: &[PathBuf], names: &[String], skip: Option<&Path>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in dirs {
        for name in names {
            let candidate = dir.join(name);
            if !candidate.is_file() {
                continue;
            }
            // The copy we are running from is the source, not a rival install.
            let same_as_source = skip
                .and_then(|s| s.canonicalize().ok())
                .zip(candidate.canonicalize().ok())
                .map(|(a, b)| a == b)
                .unwrap_or(false);
            if same_as_source || found.contains(&candidate) {
                continue;
            }
            found.push(candidate);
        }
    }
    found
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn path_env() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// Copy `src` over `dst` without ever writing into the destination in place:
/// a half-written file at a path something may be executing is worth avoiding,
/// and a rename is atomic. Falls back to sudo when the directory is not ours.
fn place_binary(src: &Path, dst: &Path) -> Result<(), BotError> {
    use std::os::unix::fs::PermissionsExt;

    let dir = dst.parent().unwrap_or(Path::new("/"));
    std::fs::create_dir_all(dir).ok();

    let tmp = dir.join(format!(
        ".{}.new",
        dst.file_name().and_then(|n| n.to_str()).unwrap_or(DEFAULT_NAME)
    ));

    match std::fs::copy(src, &tmp) {
        Ok(_) => {
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
            std::fs::rename(&tmp, dst)?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let _ = std::fs::remove_file(&tmp);
            println!("{} is not writable by you.", dir.display());
            if !prompt_yes_no("Install there with sudo?") {
                return Err(BotError::Config(format!(
                    "Not installed. Pick a writable directory, or run: sudo install -m755 {} {}",
                    src.display(),
                    dst.display()
                )));
            }
            let ok = Command::new("sudo")
                .args(["install", "-m755"])
                .arg(src)
                .arg(dst)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                Ok(())
            } else {
                Err(BotError::Config("sudo install failed".to_string()))
            }
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

/// Whether this copy looks like an installed one: either it is running from a
/// directory installs go to, or a copy of it already sits in one.
fn looks_installed(src: &Path, dirs: &[PathBuf], names: &[String]) -> bool {
    if src.parent().is_some_and(|parent| dirs.iter().any(|dir| dir == parent)) {
        return true;
    }
    !find_existing(dirs, names, Some(src)).is_empty()
}

/// Whether this binary is one `cargo build` just produced.
///
/// Without this check, `cargo run` would offer to install itself on every
/// first run during development, which is nobody's idea of helpful.
fn is_cargo_build(src: &Path) -> bool {
    let parent_is_profile = src
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "debug" || n == "release");
    let under_target = src
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        == Some("target");
    parent_is_profile && under_target
}

/// Offer to install before the first-run wizard, so a fresh download becomes a
/// working command without a second step. Only ever asks — putting a file on
/// someone's PATH unbidden is a side effect they did not request.
pub fn offer_first_run_install() {
    use std::io::IsTerminal;

    let Ok(src) = std::env::current_exe() else { return };
    if !std::io::stdin().is_terminal() || is_cargo_build(&src) {
        return;
    }
    let home = home_dir();
    let names = known_names(&crate::paths::program_name());
    let dirs = candidate_dirs(&path_env(), &home);
    if looks_installed(&src, &dirs, &names) {
        return;
    }

    println!();
    println!("This copy is not installed yet, so \"{DEFAULT_NAME}\" will not be a command");
    println!("you can type from anywhere.");
    if !prompt_yes_no("Install it now?") {
        println!(
            "Skipped. To install later, run: {} install",
            src.display()
        );
        return;
    }
    if let Err(e) = install() {
        println!("Install failed: {e}");
        println!("Carrying on with setup; the bot still runs from here.");
    }
}

/// `--install`: put this binary on PATH.
pub fn install() -> Result<(), BotError> {
    let src = std::env::current_exe()
        .map_err(|e| BotError::Config(format!("Cannot determine executable path: {e}")))?;
    let home = home_dir();
    let path_env = path_env();

    let names = known_names(&crate::paths::program_name());
    let dirs = candidate_dirs(&path_env, &home);
    let existing = find_existing(&dirs, &names, Some(&src));

    if !existing.is_empty() {
        println!("Already installed:");
        for path in &existing {
            println!("  {}", path.display());
        }
        println!("Replacing the first of those rather than adding a second copy;");
        println!("two copies at different versions is a confusing thing to debug.");
        println!();
    }

    let dst = destination(&existing, &home);

    // Running `--install` from the installed copy is a no-op, and treating it
    // as a copy would truncate the file we are executing.
    let same = src
        .canonicalize()
        .ok()
        .zip(dst.canonicalize().ok())
        .map(|(a, b)| a == b)
        .unwrap_or(false);
    if same {
        println!("Already installed at {} — nothing to do.", dst.display());
    } else {
        place_binary(&src, &dst)?;
        println!("Installed {} -> {}", src.display(), dst.display());
    }

    let dir = dst.parent().unwrap_or(Path::new("/")).to_path_buf();
    if !path_lists_dir(&path_env, &dir) {
        println!();
        println!("{} is not on your PATH. Add it:", dir.display());
        println!("  echo 'export PATH=\"{}:$PATH\"' >> ~/.profile", dir.display());
        println!("Then open a new shell, or run: export PATH=\"{}:$PATH\"", dir.display());
    }

    if existing.len() > 1 {
        println!();
        println!("Other copies are still on disk:");
        for path in existing.iter().skip(1) {
            println!("  {}", path.display());
        }
        println!("Remove them yourself if they are not wanted; whichever comes");
        println!("first on PATH is the one your shell will run.");
    }

    reconcile_unit(&dst);

    println!();
    println!(
        "Run `{} add <name>` to create a bot.",
        dst.file_name().and_then(|n| n.to_str()).unwrap_or(DEFAULT_NAME)
    );
    Ok(())
}

/// If an installed unit runs a different file than the one we just installed,
/// the service and the shell disagree about which version is "the" bot. Offer
/// to point the unit at the new location.
fn reconcile_unit(dst: &Path) {
    let Some(unit) = crate::service::installed_unit() else {
        return;
    };
    let Some(current) = crate::service::exec_start_binary(&unit) else {
        return;
    };
    if Path::new(&current) == dst {
        return;
    }

    println!();
    println!("Your systemd service still runs {current}.");
    println!("Until it is updated, the service and your shell run different copies.");
    if !prompt_yes_no("Point the service at the newly installed binary?") {
        println!("Left as it is. To update it later, {}", crate::hints::install_service());
        return;
    }
    match crate::service::write_unit_file_for(dst) {
        Ok(_) => {
            println!("Service file updated.");
            crate::service::offer_restart_running_bots();
        }
        Err(e) => println!("Could not update the service file: {e}"),
    }
}

/// `--uninstall`: undo an install, asking before each destructive step.
///
/// `purge` opts into deleting the data directories; without it they are only
/// named, because they hold configs, cached credentials and logs that no
/// uninstall should assume are disposable.
pub fn uninstall(purge: bool) -> Result<(), BotError> {
    let src = std::env::current_exe().ok();
    let home = home_dir();

    // Stops and disables every instance, removes the unit file.
    crate::service::remove_service(false)?;

    let names = known_names(&crate::paths::program_name());
    let dirs = candidate_dirs(&path_env(), &home);
    let mut installed = find_existing(&dirs, &names, None);
    // The running copy counts here: this is the command that removes it.
    if let Some(src) = src.as_ref() {
        if src.is_file() && !installed.contains(src) {
            installed.push(src.clone());
        }
    }

    if !installed.is_empty() {
        println!();
        println!("Installed binaries:");
        for path in &installed {
            println!("  {}", path.display());
        }
        for path in &installed {
            if prompt_yes_no(&format!("Delete {}?", path.display())) {
                match std::fs::remove_file(path) {
                    Ok(()) => println!("  removed."),
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        println!("  needs root: sudo rm {}", path.display());
                    }
                    Err(e) => println!("  could not remove: {e}"),
                }
            }
        }
    }

    let data_dirs = data_dirs();
    println!();
    if purge {
        println!("These hold your configs, cached logins and logs:");
        for dir in &data_dirs {
            println!("  {}", dir.display());
        }
        for dir in &data_dirs {
            if !dir.exists() {
                continue;
            }
            if prompt_yes_no(&format!("Delete {} and everything in it?", dir.display())) {
                match std::fs::remove_dir_all(dir) {
                    Ok(()) => println!("  removed."),
                    Err(e) => println!("  could not remove: {e}"),
                }
            }
        }
    } else {
        println!("Left alone (configs, logins, logs, downloaded tools):");
        for dir in &data_dirs {
            if dir.exists() {
                println!("  {}", dir.display());
            }
        }
        println!("Add --purge to be asked about deleting those too.");
    }

    offer_linger_off();
    Ok(())
}

/// Everything the bot writes to, outside the binary itself.
fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![crate::config::config_dir()];
    if let Ok(paths) = crate::youtube::setup::resolve_paths() {
        // The tools live under a data home of their own; take its parent so
        // the sidecar files next to lib/ go with it.
        let tools_root = paths.lib_dir.parent().unwrap_or(&paths.lib_dir).to_path_buf();
        if !dirs.contains(&tools_root) {
            dirs.push(tools_root);
        }
    }
    dirs
}

/// Lingering was offered at install time, but it is a per-user setting other
/// services may also rely on, so it is never turned off without asking.
fn offer_linger_off() {
    let user = std::env::var("USER").unwrap_or_default();
    if user.is_empty() {
        return;
    }
    let enabled = Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "Linger=yes")
        .unwrap_or(false);
    if !enabled {
        return;
    }
    println!();
    println!("Lingering is on for {user}, which keeps user services running after logout.");
    println!("Other services may rely on it too.");
    if prompt_yes_no("Turn lingering off?") {
        let ok = Command::new("loginctl")
            .args(["disable-linger", &user])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        println!(
            "{}",
            if ok {
                "Lingering disabled."
            } else {
                "Could not disable lingering."
            }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_names_cover_both_shipped_names_without_repeating_them() {
        assert_eq!(
            known_names("ttspotify"),
            vec!["ttspotify".to_string(), "tt-spotify-bot".to_string()]
        );
        // Someone who renamed the file still gets their own name searched.
        assert_eq!(
            known_names("ttbot"),
            vec![
                "ttspotify".to_string(),
                "tt-spotify-bot".to_string(),
                "ttbot".to_string()
            ]
        );
    }

    #[test]
    fn candidate_dirs_keep_path_order_and_add_the_usual_places() {
        let dirs = candidate_dirs("/usr/bin:/opt/x", Path::new("/home/u"));
        assert_eq!(dirs[0], PathBuf::from("/usr/bin"));
        assert_eq!(dirs[1], PathBuf::from("/opt/x"));
        // /usr/local/bin is searched even when PATH omits it: an old unit file
        // may still point there.
        assert!(dirs.contains(&PathBuf::from("/usr/local/bin")));
        assert!(dirs.contains(&PathBuf::from("/home/u/.local/bin")));
        // /usr/bin came from PATH and must not appear twice.
        assert_eq!(dirs.iter().filter(|d| *d == Path::new("/usr/bin")).count(), 1);
    }

    #[test]
    fn candidate_dirs_ignores_empty_path_entries() {
        // A trailing colon in PATH means "the current directory" to some
        // shells; installing there would be a surprise.
        let dirs = candidate_dirs("/usr/bin::", Path::new("/home/u"));
        assert!(!dirs.iter().any(|d| d.as_os_str().is_empty()));
    }

    #[test]
    fn path_membership_is_exact_not_substring() {
        assert!(path_lists_dir("/usr/bin:/home/u/.local/bin", Path::new("/home/u/.local/bin")));
        assert!(!path_lists_dir("/usr/bin", Path::new("/usr/bin/extra")));
        // A directory whose name merely starts the same must not count.
        assert!(!path_lists_dir("/usr/binary", Path::new("/usr/bin")));
        assert!(!path_lists_dir("", Path::new("/usr/bin")));
    }

    #[test]
    fn a_cargo_build_is_never_offered_an_install() {
        assert!(is_cargo_build(Path::new("/home/u/src/bot/target/debug/tt-spotify-bot")));
        assert!(is_cargo_build(Path::new("/home/u/src/bot/target/release/tt-spotify-bot")));
        // A real install, and a build laid out some other way, are not.
        assert!(!is_cargo_build(Path::new("/usr/local/bin/ttspotify")));
        assert!(!is_cargo_build(Path::new("/home/u/.local/bin/ttspotify")));
        assert!(!is_cargo_build(Path::new("/opt/debug/ttspotify")));
    }

    #[test]
    fn running_from_an_install_directory_counts_as_installed() {
        let dirs = vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/home/u/.local/bin")];
        let names = known_names("ttspotify");
        // No filesystem lookup needed: the running copy is already in place.
        assert!(looks_installed(Path::new("/usr/local/bin/ttspotify"), &dirs, &names));
        // An unpacked tarball in a download folder is not installed, and
        // nothing else exists to find in this test environment.
        assert!(!looks_installed(Path::new("/home/u/Downloads/tt-spotify-bot"), &dirs, &names));
    }

    #[test]
    fn destination_replaces_an_existing_install_before_inventing_a_new_one() {
        let home = Path::new("/home/u");
        assert_eq!(
            destination(&[PathBuf::from("/usr/local/bin/ttspotify")], home),
            PathBuf::from("/usr/local/bin/ttspotify")
        );
        // Nothing installed: the no-sudo location, under the documented name.
        assert_eq!(
            destination(&[], home),
            PathBuf::from("/home/u/.local/bin/ttspotify")
        );
    }
}
