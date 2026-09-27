//! `--doctor`: everything about this install, in one paste-able screen.
//!
//! The tray shows its state through menu items and a tooltip; a Linux install
//! shows nothing at all, so "it doesn't work" arrives with no way to tell
//! whether the tools are missing, the login expired, the service is pointing
//! at a deleted binary, or libpulse was never installed. Each of those has a
//! different one-line fix, and this prints which one applies.
//!
//! Everything here reads local state only — no network — so it stays fast and
//! works on a machine that cannot reach GitHub.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::BotConfig;

/// Shared libraries the TeamTalk SDK links against. Without either one the SDK
/// refuses to initialise, and the error it gives says nothing about audio.
const PULSE_SONAME: &str = "libpulse.so.0";
const ALSA_SONAME: &str = "libasound.so.2";

/// Pull the resolved path for a library out of `ldconfig -p` output.
///
/// Lines look like `libpulse.so.0 (libc6,x86-64) => /lib/x86_64-linux-gnu/libpulse.so.0`.
fn library_from_ldconfig(output: &str, soname: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| line.split_whitespace().next() == Some(soname))
        .find_map(|line| line.split("=>").nth(1))
        .map(|path| path.trim().to_string())
}

/// Places a distribution may keep a library when `ldconfig` is unavailable.
fn fallback_library_paths(soname: &str) -> Vec<PathBuf> {
    [
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib64",
        "/usr/lib",
        "/lib",
    ]
    .iter()
    .map(|dir| Path::new(dir).join(soname))
    .collect()
}

/// Where a library is, if it is anywhere.
fn library_path(soname: &str) -> Option<String> {
    let from_ldconfig = Command::new("ldconfig")
        .arg("-p")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| library_from_ldconfig(&String::from_utf8_lossy(&o.stdout), soname));
    if from_ldconfig.is_some() {
        return from_ldconfig;
    }
    fallback_library_paths(soname)
        .into_iter()
        .find(|p| p.exists())
        .map(|p| p.display().to_string())
}

/// The package that provides ALSA's library. Ubuntu 24.04 renamed it
/// libasound2t64, and there `libasound2` is a virtual name apt refuses to pick.
fn alsa_package() -> &'static str {
    let renamed = Command::new("apt-cache")
        .args(["show", "libasound2t64"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if renamed {
        "libasound2t64"
    } else {
        "libasound2"
    }
}

/// The SDK's libraries that are missing, each with the package providing it.
fn missing_libraries() -> Vec<(&'static str, &'static str)> {
    let mut missing = Vec::new();
    if library_path(PULSE_SONAME).is_none() {
        missing.push((PULSE_SONAME, "libpulse0"));
    }
    if library_path(ALSA_SONAME).is_none() {
        missing.push((ALSA_SONAME, alsa_package()));
    }
    missing
}

fn install_command(missing: &[(&str, &str)]) -> String {
    let packages: Vec<&str> = missing.iter().map(|(_, package)| *package).collect();
    format!("sudo apt install {}", packages.join(" "))
}

fn library_hint(missing: &[(&str, &str)]) -> Option<String> {
    let names: Vec<&str> = missing.iter().map(|(soname, _)| *soname).collect();
    let verb = match names.len() {
        0 => return None,
        1 => "is",
        _ => "are",
    };
    Some(format!(
        "{} {verb} not installed - the TeamTalk SDK needs it. \
         On Debian, Ubuntu and Raspberry Pi OS: {}",
        names.join(" and "),
        install_command(missing)
    ))
}

/// The sentence to add to an SDK startup failure when a library it needs is
/// missing. `None` when both are present and the failure is something else.
pub fn missing_library_hint() -> Option<String> {
    library_hint(&missing_libraries())
}

/// The SDK's shared library, the file whose presence means it was downloaded.
const SDK_LIBRARY: &str = "libTeamTalk5.so";

/// Describe the SDK download. A directory holding the library but no version
/// marker is a real state — an older download, or one moved in by hand — and
/// saying "not downloaded yet" about a bot that plainly runs is worse than
/// saying the version is unknown.
fn sdk_status(marker: Option<String>, library_present: bool) -> String {
    match marker.map(|m| m.trim().to_string()).filter(|m| !m.is_empty()) {
        Some(version) => version,
        None if library_present => "present, version unknown".to_string(),
        None => "not downloaded yet (fetched on first run)".to_string(),
    }
}

/// How a config's bot is doing, as one word.
///
/// `health` comes from systemd rather than from the running list: a bot that
/// crashes and is restarted is absent from that list for most of every cycle,
/// so "not running" was what a crash loop looked like here.
fn instance_state(
    unit: &str,
    running: &[String],
    enabled: &[String],
    health: crate::service::UnitHealth,
) -> &'static str {
    use crate::service::UnitHealth;
    match health {
        UnitHealth::Failed => "failed - it starts and then stops",
        UnitHealth::Restarting => "failing and being restarted",
        _ => match (running.iter().any(|u| u == unit), enabled.iter().any(|u| u == unit)) {
            (true, true) => "running, starts at login",
            (true, false) => "running, but not enabled (it will not come back after a reboot)",
            (false, true) => "enabled but not running",
            (false, false) => "not running",
        },
    }
}

/// The fix for a binary not on PATH. An installed copy only needs its folder
/// added; telling it to install again names a command the shell cannot find.
fn path_fix(installed_dir: Option<&Path>) -> String {
    match installed_dir {
        Some(dir) => format!("Add {} to your PATH - {}", dir.display(), crate::hints::add_to_path(dir)),
        None => format!("Put the binary on your PATH - {}", crate::hints::install_binary()),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

/// Print the whole report, and finish with the fixes for whatever looks wrong.
pub fn report() {
    let program = crate::paths::program_name();
    let exe = std::env::current_exe().ok();
    let path_env = std::env::var("PATH").unwrap_or_default();
    let on_path = exe
        .as_ref()
        .and_then(|e| e.parent())
        .map(|dir| path_env.split(':').any(|entry| !entry.is_empty() && Path::new(entry) == dir))
        .unwrap_or(false);

    let mut fixes: Vec<String> = Vec::new();

    println!("TTSpotify check");
    println!();
    println!("Program");
    println!("  version: {}", env!("CARGO_PKG_VERSION"));
    println!(
        "  binary: {}",
        exe.as_ref().map(|e| e.display().to_string()).unwrap_or_else(|| "unknown".into())
    );
    println!("  on PATH: {}", yes_no(on_path));
    if !on_path {
        let installed = crate::install::installed_binary();
        let installed_dir = exe.as_deref().filter(|e| installed.as_deref() == Some(*e)).and_then(Path::parent);
        fixes.push(path_fix(installed_dir));
    }

    println!();
    println!("Files");
    println!("  data root: {}", crate::config::config_dir().display());
    println!("  configs:   {}", crate::paths::configs_dir().display());
    println!("  logs:      {}", crate::paths::root().join("logs").display());

    let (configs, problems) = crate::config::list_configs_and_problems();
    println!();
    println!("Bots");
    if configs.is_empty() {
        println!("  none configured");
        fixes.push(format!("Create one - {}", crate::hints::create_bot()));
    }
    let running = crate::service::running_bot_units();
    let enabled = crate::service::enabled_instance_units();
    let mut wants_spotify = false;
    let mut wants_youtube = false;
    for (name, path) in &configs {
        let unit = crate::control::unit_for(name);
        let health = crate::service::unit_health(&unit);
        println!("  {name}: {}", instance_state(&unit, &running, &enabled, health));
        if matches!(
            health,
            crate::service::UnitHealth::Failed | crate::service::UnitHealth::Restarting
        ) {
            fixes.push(format!(
                "See why {name} will not stay up - run: {program} logs {name}"
            ));
        }
        match BotConfig::inspect(&path.to_string_lossy()) {
            Ok(config) => {
                wants_spotify |= config.enabled_services.spotify;
                wants_youtube |= config.enabled_services.youtube;
                println!(
                    "    server {}:{}  services: {}",
                    config.host,
                    config.tcp_port,
                    describe_services(config.enabled_services.spotify, config.enabled_services.youtube)
                );
            }
            Err(e) => {
                println!("    config will not load: {e}");
                // Not `edit`: it loads the config too, and fails the same way. The
                // file has to be replaced, or repaired by hand.
                fixes.push(format!(
                    "Repair {} by hand, or replace it: {program} remove {name} then {program} add {name}",
                    path.display()
                ));
            }
        }
    }

    // After the bots, not before them: these are files that failed to become
    // bots, and reading them first made the section look like the bot list.
    if !problems.is_empty() {
        println!("  files that are not usable as bots:");
        for problem in &problems {
            println!("    {problem}");
        }
        fixes.push(format!(
            "Repair or delete the unusable file(s) in {}",
            crate::paths::configs_dir().display()
        ));
    }

    println!();
    println!("Services");
    let spotify_cached = crate::spotify::auth::SpotifyAuth::new().has_cached_credentials();
    println!("  Spotify login: {}", if spotify_cached { "cached" } else { "not cached" });
    if wants_spotify && !spotify_cached {
        fixes.push(format!("Sign in to Spotify once - {}", crate::hints::sign_in_spotify()));
    }

    let tools = crate::youtube::setup::installed_tool_versions();
    let paths = crate::youtube::setup::resolve_paths().ok();
    let script_present = paths
        .as_ref()
        .map(|p| p.lib_dir.join(crate::youtube::sidecar::SCRIPT_NAME).is_file())
        .unwrap_or(false);
    println!(
        "  JavaScript runtime: {}",
        tools.js_runtime.as_deref().unwrap_or("none - YouTube playback cannot start")
    );
    println!(
        "  YouTube sidecar: {}",
        if script_present { "installed" } else { "not installed" }
    );
    if wants_youtube {
        // Both halves are needed: the sidecar is a Deno program, so either
        // one missing stops playback entirely.
        if let Some(what) = youtube_tools_status(tools.js_runtime.is_some(), script_present) {
            fixes.push(what);
        }
    }

    let settings = crate::settings::load();
    let used = crate::audio_cache::size_bytes();
    if settings.caching_off() {
        println!(
            "  Cached audio: {} (the limit is 0, so nothing is kept)",
            crate::audio_cache::human_size(used)
        );
    } else {
        println!(
            "  Cached audio: {} of {}",
            crate::audio_cache::human_size(used),
            crate::audio_cache::human_size(settings.cache_limit_bytes())
        );
    }
    if settings.cache_keep_days > 0 {
        println!("  Cached audio is dropped after {} days unplayed", settings.cache_keep_days);
    }

    println!();
    println!("System");
    for soname in [PULSE_SONAME, ALSA_SONAME] {
        println!(
            "  {soname}: {}",
            library_path(soname).as_deref().unwrap_or("MISSING - the bot cannot start without it")
        );
    }
    let missing = missing_libraries();
    if !missing.is_empty() {
        fixes.push(format!("Install the audio libraries: {}", install_command(&missing)));
    }

    // `main` pins TEAMTALK_SDK_DIR before anything loads the SDK, so that is
    // the directory the loader will actually use — reporting the default when
    // the environment says otherwise would describe a different install.
    let sdk_dir = std::env::var_os("TEAMTALK_SDK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(crate::tt::sdk::pinned_sdk_dir);
    let marker = std::fs::read_to_string(sdk_dir.join("TEAMTALK_SDK_VERSION.txt")).ok();
    let lib_present = sdk_dir.join(SDK_LIBRARY).exists();
    println!("  TeamTalk SDK: {} in {}", sdk_status(marker, lib_present), sdk_dir.display());

    let systemd = crate::service::systemd_booted();
    let reachable = systemd && crate::service::systemd_reachable();
    if systemd && !reachable {
        // "systemd: yes" here was a lie by omission: it is running, and this
        // shell cannot reach it, which is why every start had just failed.
        println!("  systemd: running, but this shell cannot reach it");
        fixes.push(
            "Log in properly (not with plain `su`) to manage bots as services".to_string(),
        );
    } else {
        println!("  systemd: {}", yes_no(systemd));
    }
    if systemd {
        match crate::service::installed_unit_version() {
            Some((installed, current)) if installed < current => {
                println!("  service file: installed, older than this release");
                fixes.push(format!("Refresh the service file - {}", crate::hints::install_service()));
            }
            Some(_) => println!("  service file: installed and current"),
            None => {
                println!("  service file: not installed");
                if !configs.is_empty() {
                    fixes.push(format!(
                        "Run bots in the background - {}", crate::hints::install_service()
                    ));
                }
            }
        }

        match crate::service::linger_state() {
            Some(true) => println!("  lingering: on (bots keep running after logout)"),
            Some(false) => {
                println!("  lingering: off (bots stop when you log out)");
                if !enabled.is_empty() {
                    fixes.push(format!(
                        "Keep bots running after logout: loginctl enable-linger {}",
                        crate::service::linger_user()
                    ));
                }
            }
            None => println!("  lingering: unknown"),
        }

        // A unit that runs a binary which no longer exists fails with a
        // message about the unit, not about the missing file.
        if let Some(exec) = crate::service::installed_unit().as_deref().and_then(crate::service::exec_start_binary) {
            let exists = Path::new(&exec).exists();
            println!("  service runs: {exec}{}", if exists { "" } else { "  (MISSING)" });
            if !exists {
                fixes.push(format!("Point the service at this binary - {}", crate::hints::install_binary()));
            }
        }
    }

    println!();
    if fixes.is_empty() {
        println!("Nothing looks wrong.");
    } else {
        println!("Suggested next steps:");
        for (i, fix) in fixes.iter().enumerate() {
            println!("  {}. {fix}", i + 1);
        }
    }
}

/// The fix to list when YouTube playback cannot start. Both the runtime and the
/// sidecar script are needed; either one missing stops every track.
fn youtube_tools_status(runtime_present: bool, script_present: bool) -> Option<String> {
    let install = crate::hints::install_youtube_tools();
    match (runtime_present, script_present) {
        (true, true) => None,
        (false, _) => Some(format!("Install Deno, which YouTube playback runs on - {install}")),
        (true, false) => Some(format!("Install the YouTube sidecar - {install}")),
    }
}

fn describe_services(spotify: bool, youtube: bool) -> &'static str {
    match (spotify, youtube) {
        (true, true) => "Spotify and YouTube",
        (true, false) => "Spotify only",
        (false, true) => "YouTube only",
        (false, false) => "none enabled",
    }
}

#[cfg(test)]
mod tests {
    use super::youtube_tools_status;

    #[test]
    fn an_installed_copy_off_path_is_told_to_add_its_folder_not_to_install() {
        let fix = super::path_fix(Some(std::path::Path::new("/home/u/.local/bin")));
        assert!(fix.contains("/home/u/.local/bin:$PATH"), "{fix}");
        assert!(!fix.contains(" install"), "{fix}");
        assert!(super::path_fix(None).contains(" install"));
    }

    #[test]
    fn nothing_to_say_when_both_halves_are_present() {
        assert_eq!(youtube_tools_status(true, true), None);
    }

    #[test]
    fn a_missing_runtime_names_deno_not_the_old_tools() {
        let msg = youtube_tools_status(false, true).expect("should report");
        assert!(msg.contains("Deno"), "{msg}");
        let lower = msg.to_lowercase();
        assert!(!lower.contains("yt-dlp") && !lower.contains("bgutil"), "{msg}");
    }

    #[test]
    fn a_missing_script_is_reported_separately() {
        let msg = youtube_tools_status(true, false).expect("should report");
        assert!(msg.contains("sidecar"), "{msg}");
    }

    use super::*;

    #[test]
    fn a_library_is_read_from_the_ldconfig_line_for_it() {
        let out = "\tlibpulse-simple.so.0 (libc6,x86-64) => /lib/x86_64-linux-gnu/libpulse-simple.so.0\n\
                   \tlibpulse.so.0 (libc6,x86-64) => /lib/x86_64-linux-gnu/libpulse.so.0\n\
                   \tlibasound.so.2 (libc6,x86-64) => /lib/x86_64-linux-gnu/libasound.so.2\n";
        assert_eq!(
            library_from_ldconfig(out, PULSE_SONAME).as_deref(),
            Some("/lib/x86_64-linux-gnu/libpulse.so.0")
        );
        assert_eq!(
            library_from_ldconfig(out, ALSA_SONAME).as_deref(),
            Some("/lib/x86_64-linux-gnu/libasound.so.2")
        );
    }

    #[test]
    fn a_similarly_named_library_is_not_mistaken_for_libpulse() {
        // libpulse-simple and libpulsecommon are usually present as
        // dependencies of something else; neither means libpulse.so.0 is.
        let out = "\tlibpulse-simple.so.0 (libc6,x86-64) => /lib/libpulse-simple.so.0\n\
                   \tlibpulsecommon-15.99.so (libc6,x86-64) => /lib/libpulsecommon-15.99.so\n";
        assert_eq!(library_from_ldconfig(out, PULSE_SONAME), None);
        assert_eq!(library_from_ldconfig("", PULSE_SONAME), None);
    }

    #[test]
    fn the_hint_names_every_missing_library_in_one_install_command() {
        assert_eq!(library_hint(&[]), None);
        let alsa = library_hint(&[(ALSA_SONAME, "libasound2t64")]).unwrap();
        assert!(alsa.starts_with("libasound.so.2 is not installed"), "{alsa}");
        assert!(alsa.ends_with("sudo apt install libasound2t64"), "{alsa}");
        let both = library_hint(&[(PULSE_SONAME, "libpulse0"), (ALSA_SONAME, "libasound2")]).unwrap();
        assert!(both.starts_with("libpulse.so.0 and libasound.so.2 are not installed"), "{both}");
        assert!(both.ends_with("sudo apt install libpulse0 libasound2"), "{both}");
    }

    #[test]
    fn sdk_status_prefers_the_marker_but_still_notices_the_library() {
        assert_eq!(sdk_status(Some("v5.22a\n".to_string()), true), "v5.22a");
        // A download whose marker is missing still works; the bot is running
        // off it, so the report must not call it absent.
        assert_eq!(sdk_status(None, true), "present, version unknown");
        assert_eq!(sdk_status(Some("  ".to_string()), true), "present, version unknown");
        assert_eq!(sdk_status(None, false), "not downloaded yet (fetched on first run)");
    }

    #[test]
    fn instance_state_separates_running_from_enabled() {
        let unit = "ttspotify@home.service".to_string();
        let this = std::slice::from_ref(&unit);
        let none: Vec<String> = Vec::new();
        let up = crate::service::UnitHealth::Running;
        let down = crate::service::UnitHealth::Stopped;
        assert_eq!(instance_state(&unit, this, this, up), "running, starts at login");
        assert_eq!(instance_state(&unit, &none, this, down), "enabled but not running");
        // Running without being enabled is the state that surprises people
        // after a reboot, so it says so rather than just "running".
        assert_eq!(
            instance_state(&unit, this, &none, up),
            "running, but not enabled (it will not come back after a reboot)"
        );
        assert_eq!(instance_state(&unit, &none, &none, down), "not running");
    }

    #[test]
    fn a_bot_in_a_crash_loop_is_not_reported_as_merely_stopped() {
        // The lists say "not running" for most of every restart cycle, which
        // is exactly when someone runs doctor to find out what is wrong.
        let unit = "ttspotify@home.service".to_string();
        let none: Vec<String> = Vec::new();
        assert_eq!(
            instance_state(&unit, &none, &none, crate::service::UnitHealth::Restarting),
            "failing and being restarted"
        );
        assert_eq!(
            instance_state(&unit, &none, &none, crate::service::UnitHealth::Failed),
            "failed - it starts and then stops"
        );
    }

    #[test]
    fn instance_state_does_not_confuse_one_bot_with_another() {
        let home = "ttspotify@home.service".to_string();
        let work = "ttspotify@work.service".to_string();
        let other = std::slice::from_ref(&work);
        assert_eq!(
            instance_state(&home, other, other, crate::service::UnitHealth::Stopped),
            "not running"
        );
    }
}
