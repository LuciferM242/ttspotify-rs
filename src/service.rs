//! Systemd user service generator (Linux only).
//!
//! Generates and installs a systemd user service template for running
//! multiple bot instances via `systemctl --user start ttspotify@myserver`.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{config_dir, list_configs};
use crate::error::BotError;

fn systemd_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("systemd")
        .join("user")
}

const SERVICE_NAME: &str = "ttspotify@.service";

/// Version of the generated unit file's CONTENT. Bump whenever
/// `unit_file_contents` changes in a way installed units should pick up;
/// `--update` then offers to rewrite older installed units. Files without a
/// stamp (pre-versioning installs) read as 0.
const UNIT_FILE_VERSION: u32 = 4;

/// Read the version stamp out of a unit file's contents (0 when absent or
/// unparsable — always older than any current version).
fn unit_version_from_contents(contents: &str) -> u32 {
    contents
        .lines()
        .find_map(|l| l.strip_prefix("# ttspotify-unit-version: "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// True when the system was booted under systemd (the same check
/// `sd_booted()` performs). Without it `systemctl` is absent.
pub fn systemd_booted() -> bool {
    std::path::Path::new("/run/systemd/system").exists()
}

/// True if the ttspotify@ systemd user unit file is installed.
pub fn service_installed() -> bool {
    systemd_dir().join(SERVICE_NAME).exists()
}

/// Contents of the installed unit file, when there is one.
pub fn installed_unit() -> Option<String> {
    std::fs::read_to_string(systemd_dir().join(SERVICE_NAME)).ok()
}

/// Version stamp of the installed unit file, and the version this build
/// writes, so a check can say whether a refresh is waiting.
pub fn installed_unit_version() -> Option<(u32, u32)> {
    installed_unit().map(|u| (unit_version_from_contents(&u), UNIT_FILE_VERSION))
}

/// The `ttspotify@` instances systemd has enabled.
pub fn enabled_instance_units() -> Vec<String> {
    known_instances(&list_unit_files_output(), &[], &[])
}

/// Whether user services survive logout. `None` when the answer cannot be
/// determined (no loginctl, or no idea who we are).
pub fn linger_state() -> Option<bool> {
    let user = current_user();
    if user.is_empty() {
        return None;
    }
    Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "Linger=yes")
}

/// The login name lingering would be enabled for.
pub fn linger_user() -> String {
    current_user()
}

/// Escape a config name for use as a systemd template instance, matching
/// `systemd-escape`: `/` becomes `-`, a leading `.` and every byte outside
/// `[A-Za-z0-9:_.]` become `\xNN`. Without this, a config like
/// `my server.json` yields an instance string systemctl can't address.
pub(crate) fn systemd_escape_instance(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, b) in name.bytes().enumerate() {
        let allowed = b.is_ascii_alphanumeric()
            || b == b':'
            || b == b'_'
            || (b == b'.' && i != 0);
        if b == b'/' {
            out.push('-');
        } else if allowed {
            out.push(b as char);
        } else {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

/// Offer (y/N prompt) to enable and start `ttspotify@<name>` now. Used by the
/// setup wizard right after a config is created, and by `--install-service`
/// for each existing config.
pub fn offer_enable_instance(name: &str) {
    let instance = systemd_escape_instance(name);
    if prompt_yes_no(&format!("Enable and start ttspotify@{instance} now?")) {
        let _ = Command::new("systemctl")
            .args(["--user", "enable", &format!("ttspotify@{instance}")])
            .status();
        let _ = Command::new("systemctl")
            .args(["--user", "start", &format!("ttspotify@{instance}")])
            .status();
        println!("  ttspotify@{instance} enabled and started.");
    }
}

/// Current login name, for loginctl calls. Prefers $USER, falls back to `id -un`.
fn current_user() -> String {
    if let Ok(u) = std::env::var("USER") {
        if !u.is_empty() {
            return u;
        }
    }
    Command::new("id")
        .arg("-un")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Whether systemd lingering is enabled for `user`. Lingering keeps the user's
/// systemd instance (and thus `--user` services) running after logout; without
/// it a headless bot dies when the operator disconnects.
fn linger_enabled(user: &str) -> bool {
    Command::new("loginctl")
        .args(["show-user", user, "--property=Linger"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "Linger=yes")
        .unwrap_or(false)
}

/// The binary an installed unit runs, as written in its `ExecStart`.
///
/// `write_unit_file` quotes the path so spaces survive, so the quoted form is
/// the one that matters; the bare form is read too in case someone edited the
/// unit by hand.
pub fn exec_start_binary(unit: &str) -> Option<String> {
    let line = unit
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("ExecStart="))?
        .trim();
    if let Some(rest) = line.strip_prefix('"') {
        return rest.split('"').next().filter(|s| !s.is_empty()).map(str::to_string);
    }
    line.split_whitespace().next().filter(|s| !s.is_empty()).map(str::to_string)
}

pub(crate) fn prompt_yes_no(message: &str) -> bool {
    print!("{message} [y/N] ");
    io::stdout().flush().ok();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

pub fn install_service() -> Result<(), BotError> {
    // Without systemd, `systemctl` is absent and installing a unit file would
    // print a false success, so bail with real alternatives.
    if !systemd_booted() {
        println!("systemd not detected. This installer needs systemd.");
        println!("Run the binary directly, or supervise it with your");
        println!("init system (OpenRC, runit, s6).");
        return Ok(());
    }

    let config_base = write_unit_file()?;

    println!();
    println!("TTSpotify service installed.");
    println!("Config files go in: {}", config_base.display());
    println!();
    // Our own commands, not the systemd ones they replace: this is printed at
    // the moment someone is most likely to copy a line out of it.
    let prog = crate::paths::program_name();
    println!("Quick start:");
    println!("  {prog} add myserver        create a bot");
    println!("  {prog} start myserver      run it in the background");
    println!("  {prog} status              see what is running");
    println!("  {prog} watch myserver      follow its log");

    // Ensure the user's systemd instance survives logout before we start
    // anything: `--user` services stop when the last session ends unless
    // lingering is on, which would silently kill a headless bot after the
    // operator disconnects. Only prompt when it isn't already enabled.
    let user = current_user();
    if !user.is_empty() && !linger_enabled(&user) {
        println!();
        println!("Lingering is off, so the bot would stop when you log out.");
        if prompt_yes_no("Enable linger so it keeps running after logout?") {
            let ok = Command::new("loginctl")
                .args(["enable-linger", &user])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                println!("Linger enabled.");
            } else {
                println!("Could not enable linger. Run manually: loginctl enable-linger {user}");
            }
        } else {
            println!("Skipped. Enable later with: loginctl enable-linger {user}");
        }
    }

    // Offer to enable/start existing configs
    let configs = list_configs();
    for (name, _) in configs {
        offer_enable_instance(&name);
    }

    Ok(())
}

/// Generate the current unit file, write it to the systemd user dir and
/// daemon-reload. Shared by `--install-service` and the post-update refresh.
/// Returns the config base dir the unit points at.
fn write_unit_file() -> Result<PathBuf, BotError> {
    let exe_path = std::env::current_exe()
        .map_err(|e| BotError::Config(format!("Cannot determine executable path: {e}")))?;
    write_unit_file_for(&exe_path)
}

/// Same, for a binary other than the running one — `--install` points the unit
/// at the copy it just placed on PATH, which is not the copy being executed.
pub(crate) fn write_unit_file_for(exe_path: &Path) -> Result<PathBuf, BotError> {
    let config_base = crate::paths::configs_dir();
    let data_root = config_dir();

    let systemd = systemd_dir();
    std::fs::create_dir_all(&systemd)?;
    std::fs::create_dir_all(&config_base)?;

    let service_path = systemd.join(SERVICE_NAME);
    // Quote the binary and config paths so spaces in either don't break the
    // unit. %I (unescaped) rather than %i: instance names are systemd-escaped
    // when starting (see systemd_escape_instance), and the config file on disk
    // uses the original name.
    let exec_start = format!(
        "\"{}\" --config \"{}/{}.json\"",
        exe_path.display(),
        config_base.display(),
        "%I"
    );

    let tools_dir = crate::youtube::setup::resolve_paths().ok().map(|p| p.lib_dir);
    let unit = unit_file_contents(&exec_start, &data_root, tools_dir.as_deref());

    std::fs::write(&service_path, unit)?;

    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();

    Ok(config_base)
}

/// After a successful `--update`: if the installed unit file predates the
/// current template, offer (y/N) to rewrite it. Never touches anything
/// without the user saying yes — a rewrite replaces manual edits.
pub fn offer_unit_refresh() {
    let service_path = systemd_dir().join(SERVICE_NAME);
    let Ok(contents) = std::fs::read_to_string(&service_path) else {
        return; // Not installed as a service: nothing to refresh.
    };
    if unit_version_from_contents(&contents) >= UNIT_FILE_VERSION {
        return;
    }
    println!();
    println!("Your systemd service file was generated by an older version;");
    println!("this release improves it (sandboxing, restart behavior).");
    println!("Rewriting replaces any manual edits you made to it.");
    if prompt_yes_no("Rewrite the service file now?") {
        match write_unit_file() {
            Ok(_) => println!("Service file updated (takes effect on the next bot restart)."),
            Err(e) => println!("Could not update the service file: {e}"),
        }
    } else {
        println!(
            "Keeping the current file. To update it later, {}",
            crate::hints::install_service()
        );
    }
}

/// Render the `ttspotify@.service` user unit.
///
/// A missing/broken config exits with EXIT_CONFIG_ERROR;
/// RestartPreventExitStatus keeps systemd from crash-restarting into the same
/// missing file every 2 seconds (which logs the bot in and out of the
/// TeamTalk server nonstop).
///
/// The sandbox block makes the filesystem read-only to the bot except its own
/// dirs: the config dir (configs, logs, caches, and — via WorkingDirectory —
/// the downloaded TeamTalk SDK), the YouTube tools dir, and ~/.cache (yt-dlp's
/// own cache). The `-` prefix keeps a not-yet-created path from failing the
/// unit.
fn unit_file_contents(exec_start: &str, config_dir: &Path, tools_dir: Option<&Path>) -> String {
    let tools_rw = tools_dir
        .map(|d| format!("ReadWritePaths=-{}\n", d.display()))
        .unwrap_or_default();
    format!(
        r#"# ttspotify-unit-version: {unit_version}
[Unit]
Description=TTSpotify Bot (%i)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory={config_dir}
ExecStart={exec_start}
Restart=on-failure
RestartPreventExitStatus={config_exit}
RestartSec=2
# The bot answers SIGTERM by leaving the TeamTalk channel before it exits, so
# systemd is asked to wait for that instead of killing it mid-logout.
TimeoutStopSec=20

# Sandbox: everything is read-only to the bot except the paths below.
# Using a custom --config path in ExecStart? Add its folder as another
# ReadWritePaths line. If the service fails to start on a kernel without
# unprivileged user namespaces, delete this block.
ProtectSystem=strict
PrivateTmp=true
NoNewPrivileges=true
ReadWritePaths=-{config_dir}
{tools_rw}ReadWritePaths=-%h/.local/share/ttspotify
ReadWritePaths=-%h/.cache

[Install]
WantedBy=default.target
"#,
        unit_version = UNIT_FILE_VERSION,
        config_dir = config_dir.display(),
        config_exit = crate::config::EXIT_CONFIG_ERROR,
    )
}

/// Parse `systemctl --user list-units 'ttspotify@*' --state=running --plain
/// --no-legend` output into unit names. First column of each line, filtered to
/// our template's instances.
fn parse_running_units(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| unit.starts_with("ttspotify@") && unit.ends_with(".service"))
        .map(str::to_string)
        .collect()
}

/// Names of the `ttspotify@` user units currently running. Empty when systemd
/// is unavailable or nothing is running.
pub fn running_bot_units() -> Vec<String> {
    let out = Command::new("systemctl")
        .args([
            "--user",
            "list-units",
            "ttspotify@*",
            "--state=running",
            "--plain",
            "--no-legend",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => parse_running_units(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    }
}

/// After a successful self-update, offer to restart the running bot units so
/// they pick up the new binary. Prints a manual hint when nothing is running
/// or the user declines.
pub fn offer_restart_running_bots() {
    let units = running_bot_units();
    if units.is_empty() {
        println!("If running as a service, restart it: systemctl --user restart ttspotify@<name>");
        return;
    }
    if !prompt_yes_no(&format!("Restart {} running bot(s) now?", units.len())) {
        println!("Restart later with: systemctl --user restart ttspotify@<name>");
        return;
    }
    for unit in &units {
        let ok = Command::new("systemctl")
            .args(["--user", "restart", unit])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            println!("  {unit} restarted.");
        } else {
            println!("  {unit} failed to restart - check: systemctl --user status {unit}");
        }
    }
}

/// Raw `systemctl --user list-unit-files 'ttspotify@*'` output, or empty when
/// systemctl is unavailable.
fn list_unit_files_output() -> String {
    Command::new("systemctl")
        .args([
            "--user",
            "list-unit-files",
            "ttspotify@*",
            "--plain",
            "--no-legend",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Every `ttspotify@` instance worth disabling, from three sources: unit files
/// systemd lists (an enabled instance leaves a symlink in `default.target.wants`
/// that outlives the template file), units currently running, and one per config
/// on disk. Deduped, order stable.
///
/// The bare template `ttspotify@.service` is not an instance and is dropped —
/// `disable` on it would be a no-op at best.
fn known_instances(unit_files: &str, running: &[String], config_names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |unit: String| {
        if unit != SERVICE_NAME && !out.contains(&unit) {
            out.push(unit);
        }
    };

    for unit in unit_files
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|u| u.starts_with("ttspotify@") && u.ends_with(".service"))
    {
        push(unit.to_string());
    }
    for unit in running {
        push(unit.clone());
    }
    for name in config_names {
        push(format!("ttspotify@{}.service", systemd_escape_instance(name)));
    }
    out
}

/// Stop and un-enable every instance before the template file goes away.
/// Deleting the template alone leaves the enable symlinks behind (systemd then
/// complains about them at every login) and strands running bots as processes
/// systemd can no longer stop or restart.
///
/// `disable --now` does both in one call, and is a silent no-op on an instance
/// that was never enabled and isn't running.
fn stop_and_disable_instances() {
    let config_names: Vec<String> = list_configs().into_iter().map(|(name, _)| name).collect();
    let running = running_bot_units();
    let units = known_instances(&list_unit_files_output(), &running, &config_names);

    for unit in units {
        let was_running = running.contains(&unit);
        let link = wants_symlink(&unit);
        let was_enabled = link_present(&link);

        // `output()` rather than `status()`: systemctl writes a line to stderr
        // for an instance that was never enabled, which is the common case here
        // and not something to show the user.
        let disabled = Command::new("systemctl")
            .args(["--user", "disable", "--now", &unit])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        // With the template file already deleted — the state an older version
        // of this command left behind — `disable` clears the dangling symlink
        // but still exits non-zero ("Unit file ... does not exist"). So the
        // symlink, not the exit status, says whether anything was cleaned up.
        // Remove it directly if systemd left it in place.
        let mut cleaned = was_enabled && !link_present(&link);
        if link_present(&link) && std::fs::remove_file(&link).is_ok() {
            cleaned = true;
        }

        // `disable --now` succeeds on an instance that was neither enabled nor
        // running, so the report follows the state we saw beforehand rather
        // than the exit status — otherwise every config on disk gets announced
        // as stopped when nothing was.
        match (disabled, was_enabled, was_running, cleaned) {
            (true, true, true, _) => println!("  {unit} stopped and disabled."),
            (true, true, false, _) => println!("  {unit} disabled."),
            (true, false, true, _) => println!("  {unit} stopped."),
            (_, _, _, true) => println!("  {unit} disabled (left enabled by an earlier uninstall)."),
            _ => {}
        }
    }
}

/// Where systemd puts the symlink that marks an instance as enabled. Our unit
/// is `WantedBy=default.target`, so that is the only target to look in.
fn wants_symlink(unit: &str) -> PathBuf {
    systemd_dir().join("default.target.wants").join(unit)
}

/// Whether the enable symlink is there at all — `Path::exists` follows the
/// link and answers "no" for the exact case that matters here: a symlink left
/// pointing at a template file an older uninstall already deleted.
fn link_present(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

pub fn uninstall_service() -> Result<(), BotError> {
    remove_service(true)
}

/// `note_untouched` is false when `--uninstall` calls this on its way to
/// removing the binary too: saying "the binary is untouched" one line before
/// offering to delete it reads as a contradiction.
pub(crate) fn remove_service(note_untouched: bool) -> Result<(), BotError> {
    // Runs even when the template file is already gone: a unit removed by an
    // older version of this command left its enable symlinks behind, and this
    // is what clears them.
    if systemd_booted() {
        stop_and_disable_instances();
    }

    let service_path = systemd_dir().join(SERVICE_NAME);
    if service_path.exists() {
        std::fs::remove_file(&service_path)?;
        println!("TTSpotify service removed.");
    } else {
        println!("No service file found at {}", service_path.display());
    }

    // Runs whether or not the template file was there: the sweep above may
    // have removed enable symlinks by hand, and systemd keeps serving the old
    // state until it is told to re-read it.
    if systemd_booted() {
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
    }

    if note_untouched {
        println!("Configs, logs and the binary itself are untouched.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_running_units, unit_file_contents, unit_version_from_contents, UNIT_FILE_VERSION};

    #[test]
    fn unit_file_does_not_restart_on_config_error() {
        let unit = unit_file_contents(
            "\"/opt/bot\" --config \"/home/u/.config/ttspotify/%i.json\"",
            std::path::Path::new("/home/u/.config/ttspotify"),
            Some(std::path::Path::new("/home/u/.local/share/ttspotify/lib")),
        );
        // Exit code 78 (EX_CONFIG) means "config missing/broken": restarting
        // can't help and would hammer the TeamTalk server with logins.
        assert!(unit.contains(&format!(
            "RestartPreventExitStatus={}",
            crate::config::EXIT_CONFIG_ERROR
        )));
        // The old directive referenced an exit code nothing ever emits.
        assert!(!unit.contains("RestartForceExitStatus"));
        assert!(unit.contains("Restart=on-failure"));
        // SIGTERM starts a clean logout; systemd must wait for it rather than
        // SIGKILL the bot mid-disconnect and leave a ghost user behind.
        assert!(unit.contains("TimeoutStopSec="));
        assert!(unit.contains("ExecStart=\"/opt/bot\""));
    }

    #[test]
    fn unit_file_sandboxes_with_writable_bot_dirs() {
        let unit = unit_file_contents(
            "\"/opt/bot\" --config \"/home/u/.config/ttspotify/%i.json\"",
            std::path::Path::new("/home/u/.config/ttspotify"),
            Some(std::path::Path::new("/home/u/.local/share/ttspotify/lib")),
        );
        assert!(unit.contains("ProtectSystem=strict"));
        assert!(unit.contains("PrivateTmp=true"));
        assert!(unit.contains("NoNewPrivileges=true"));
        // `-` prefix: a listed path that doesn't exist yet must not fail the unit.
        assert!(unit.contains("ReadWritePaths=-/home/u/.config/ttspotify"));
        assert!(unit.contains("ReadWritePaths=-/home/u/.local/share/ttspotify/lib"));
        assert!(unit.contains("ReadWritePaths=-%h/.cache"));
        // SDK downloads land relative to the CWD; pin it to the config dir so
        // they fall inside the writable set.
        assert!(unit.contains("WorkingDirectory=/home/u/.config/ttspotify"));
    }

    #[test]
    fn unit_file_carries_current_version_stamp() {
        let unit = unit_file_contents(
            "\"/opt/bot\" --config \"/x/%i.json\"",
            std::path::Path::new("/x"),
            None,
        );
        assert_eq!(unit_version_from_contents(&unit), UNIT_FILE_VERSION);
    }

    #[test]
    fn unit_version_parses_stamp_and_defaults_to_zero() {
        assert_eq!(
            unit_version_from_contents("[Unit]\n# ttspotify-unit-version: 7\n[Service]\n"),
            7
        );
        // Pre-stamp installs and hand-mangled stamps read as version 0
        // (always older than any current version, so a refresh is offered).
        assert_eq!(unit_version_from_contents("[Unit]\nExecStart=x\n"), 0);
        assert_eq!(unit_version_from_contents("# ttspotify-unit-version: banana\n"), 0);
    }

    #[test]
    fn unit_file_without_tools_dir_omits_its_rw_line() {
        let unit = unit_file_contents(
            "\"/opt/bot\" --config \"/x/%i.json\"",
            std::path::Path::new("/x"),
            None,
        );
        assert!(unit.contains("ReadWritePaths=-/x"));
        // The XDG tools home stays whitelisted even when no tools dir was
        // detected at install time — the startup migration may create it later.
        assert!(unit.contains("ReadWritePaths=-%h/.local/share/ttspotify"));
        assert!(!unit.contains("ReadWritePaths=-/home"));
    }

    #[test]
    fn escape_instance_passes_plain_names_through() {
        assert_eq!(super::systemd_escape_instance("myserver"), "myserver");
        assert_eq!(super::systemd_escape_instance("srv_2.home:x"), "srv_2.home:x");
    }

    #[test]
    fn escape_instance_encodes_specials_like_systemd_escape() {
        // Same output `systemd-escape` produces for these inputs.
        assert_eq!(super::systemd_escape_instance("my server"), r"my\x20server");
        assert_eq!(super::systemd_escape_instance("a/b"), "a-b");
        assert_eq!(super::systemd_escape_instance(".hidden"), r"\x2ehidden");
    }

    #[test]
    fn parses_unit_names_from_first_column() {
        let out = "ttspotify@home.service loaded active running TTSpotify bot (home)\n\
                   ttspotify@work.service loaded active running TTSpotify bot (work)\n";
        assert_eq!(
            parse_running_units(out),
            vec!["ttspotify@home.service", "ttspotify@work.service"]
        );
    }

    #[test]
    fn ignores_foreign_units_and_blank_lines() {
        let out = "\nother@x.service loaded active running Something else\n\
                   ttspotify@home.service loaded active running TTSpotify bot\n\n";
        assert_eq!(parse_running_units(out), vec!["ttspotify@home.service"]);
    }

    #[test]
    fn empty_output_is_empty() {
        assert!(parse_running_units("").is_empty());
    }

    #[test]
    fn known_instances_takes_enabled_units_and_drops_the_template() {
        // `list-unit-files 'ttspotify@*'` lists the template itself alongside
        // any enabled instance. The template is not something to disable.
        let unit_files = "ttspotify@.service     enabled enabled\n\
                          ttspotify@home.service enabled enabled\n";
        assert_eq!(
            super::known_instances(unit_files, &[], &[]),
            vec!["ttspotify@home.service"]
        );
    }

    #[test]
    fn known_instances_merges_sources_without_duplicates() {
        // The same bot can show up as an enabled unit file, as a running unit,
        // and as a config on disk; it must be disabled once.
        let unit_files = "ttspotify@home.service enabled enabled\n";
        let running = vec!["ttspotify@home.service".to_string(), "ttspotify@work.service".to_string()];
        let configs = vec!["home".to_string(), "spare".to_string()];
        assert_eq!(
            super::known_instances(unit_files, &running, &configs),
            vec![
                "ttspotify@home.service",
                "ttspotify@work.service",
                "ttspotify@spare.service",
            ]
        );
    }

    #[test]
    fn known_instances_escapes_config_names() {
        // A config named "my server" is addressed as the escaped instance, the
        // same string `--install-service` enabled it under.
        assert_eq!(
            super::known_instances("", &[], &["my server".to_string()]),
            vec![r"ttspotify@my\x20server.service"]
        );
    }

    #[test]
    fn exec_start_binary_reads_the_quoted_path_we_write() {
        let unit = unit_file_contents(
            "\"/usr/local/bin/tt spotify\" --config \"/x/%i.json\"",
            std::path::Path::new("/x"),
            None,
        );
        // Quoting is what lets a path with a space survive, so the quotes are
        // the boundary rather than the first blank.
        assert_eq!(
            super::exec_start_binary(&unit).as_deref(),
            Some("/usr/local/bin/tt spotify")
        );
    }

    #[test]
    fn exec_start_binary_reads_a_hand_edited_unquoted_path() {
        assert_eq!(
            super::exec_start_binary("[Service]\nExecStart=/usr/bin/ttspotify --config /x.json\n")
                .as_deref(),
            Some("/usr/bin/ttspotify")
        );
        assert_eq!(super::exec_start_binary("[Service]\nType=simple\n"), None);
        assert_eq!(super::exec_start_binary("ExecStart=\n"), None);
    }

    #[test]
    fn link_present_sees_a_symlink_whose_target_is_gone() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!(
            "ttspotify_service_{}_{}",
            std::process::id(),
            "dangling"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("ttspotify@gone.service");
        let _ = std::fs::remove_file(&link);
        symlink(dir.join("ttspotify@.service"), &link).unwrap();

        // The reason this helper exists: an enable symlink left behind by an
        // older uninstall points at a template file that is no longer there,
        // and `exists()` follows the link and answers "no".
        assert!(!link.exists());
        assert!(super::link_present(&link));

        std::fs::remove_file(&link).unwrap();
        assert!(!super::link_present(&link));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn known_instances_ignores_foreign_units_and_empty_input() {
        assert!(super::known_instances("", &[], &[]).is_empty());
        assert!(super::known_instances("other@x.service enabled enabled\n", &[], &[]).is_empty());
    }
}
