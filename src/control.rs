//! Starting, stopping and watching bots by the name you gave them.
//!
//! The tray lists every bot with its own start / stop / restart / logs. On
//! Linux the same jobs meant knowing that the config called "home" is the
//! systemd instance `ttspotify@home.service`, that its logs are under
//! `journalctl --user -u`, and that a config name with a space in it has to be
//! systemd-escaped first. None of that is the user's problem, so these
//! commands take the config name and do the translation.

use std::path::PathBuf;
use std::process::Command;

use crate::error::BotError;
use crate::service;

/// What `--start`, `--stop` and `--restart` accept in place of a name.
const ALL: &str = "all";

/// Turn the name (or `all`) into config names, or explain what was expected.
fn resolve_targets(arg: &str, configs: &[String]) -> Result<Vec<String>, String> {
    if configs.is_empty() {
        return Err("No configs exist yet. Create one with --setup <name>.".to_string());
    }
    if arg == ALL {
        return Ok(configs.to_vec());
    }
    if configs.iter().any(|c| c == arg) {
        return Ok(vec![arg.to_string()]);
    }
    Err(format!(
        "No config named \"{arg}\". Available: {} (or \"{ALL}\").",
        configs.join(", ")
    ))
}

fn config_names() -> Vec<String> {
    crate::config::list_configs().into_iter().map(|(name, _)| name).collect()
}

fn unit_for(name: &str) -> String {
    format!("ttspotify@{}.service", service::systemd_escape_instance(name))
}

/// Shared guard: every verb below drives systemctl, which is not there to be
/// driven on a system that did not boot with systemd, and cannot address our
/// instances until the template unit exists.
fn require_service() -> Result<(), BotError> {
    if !service::systemd_booted() {
        return Err(BotError::Usage(format!(
            "This needs systemd. Without it, run a bot in the foreground:\n  {} --config <path>",
            crate::paths::program_name()
        )));
    }
    if !service::service_installed() {
        return Err(BotError::Usage(format!(
            "The systemd service is not installed yet. Install it with:\n  {} --install-service",
            crate::paths::program_name()
        )));
    }
    Ok(())
}

/// `--list`: the configs on this machine, whether or not systemd is involved.
pub fn list() {
    let configs = crate::config::list_configs();
    if configs.is_empty() {
        println!("No bots configured yet.");
        println!("Create one with: {} --setup myserver", crate::paths::program_name());
        return;
    }
    for (name, path) in configs {
        println!("{name}");
        println!("  config: {}", path.display());
    }
}

/// `--status`: what each bot is doing right now.
pub fn status() {
    let configs = config_names();
    if configs.is_empty() {
        println!("No bots configured yet.");
        println!("Create one with: {} --setup myserver", crate::paths::program_name());
        return;
    }

    if !service::systemd_booted() {
        println!("systemd is not running here, so bots are not managed as services.");
        println!("Configured bots: {}", configs.join(", "));
        return;
    }

    let running = service::running_bot_units();
    let enabled = service::enabled_instance_units();
    for name in configs {
        let unit = unit_for(&name);
        let is_running = running.contains(&unit);
        println!(
            "{name}: {}{}",
            if is_running { "running" } else { "stopped" },
            if enabled.contains(&unit) { ", starts at login" } else { "" }
        );
        if is_running {
            if let Some(since) = active_since(&unit) {
                println!("  since {since}");
            }
        }
    }
}

/// When systemd last started this unit, for the status line.
fn active_since(unit: &str) -> Option<String> {
    let out = Command::new("systemctl")
        .args(["--user", "show", unit, "--property=ActiveEnterTimestamp", "--value"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() || value == "n/a" {
        None
    } else {
        Some(value)
    }
}

/// Report what happened in words rather than by bolting "ed" onto the verb,
/// which turns stop into "stoped".
fn past_tense(verb: &str) -> &str {
    match verb {
        "start" => "started",
        "stop" => "stopped",
        "restart" => "restarted",
        other => other,
    }
}

/// `--start` / `--stop` / `--restart`, over one bot or all of them.
pub fn control(verb: &str, target: &str) -> Result<(), BotError> {
    require_service()?;
    let names = resolve_targets(target, &config_names()).map_err(BotError::Usage)?;

    let mut failures = 0;
    for name in &names {
        let unit = unit_for(name);
        let ok = Command::new("systemctl")
            .args(["--user", verb, &unit])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            println!("{name}: {}.", past_tense(verb));
        } else {
            failures += 1;
            println!("{name}: {verb} failed. See: systemctl --user status {unit}");
        }
    }

    if failures > 0 {
        return Err(BotError::Usage(format!(
            "{failures} of {} bot(s) did not {verb}",
            names.len()
        )));
    }
    Ok(())
}

/// `--logs <name>`: the journal for one bot, or its log files when there is no
/// journal to read.
pub fn logs(name: &str, follow: bool) -> Result<(), BotError> {
    let configs = config_names();
    // "all" is meaningless for a single log stream, so it is not accepted here.
    if !configs.iter().any(|c| c == name) {
        return Err(BotError::Usage(format!(
            "No config named \"{name}\". Available: {}.",
            if configs.is_empty() { "none".to_string() } else { configs.join(", ") }
        )));
    }

    if service::systemd_booted() && service::service_installed() {
        let unit = unit_for(name);
        let mut cmd = Command::new("journalctl");
        cmd.args(["--user", "-u", &unit, "-n", "200"]);
        if follow {
            cmd.arg("-f");
        }
        // Inherits the terminal so `-f` streams and Ctrl+C ends it, exactly as
        // running journalctl by hand would.
        if let Ok(status) = cmd.status() {
            if status.success() {
                return Ok(());
            }
        }
        println!("Could not read the journal; falling back to the log files.");
    }

    show_log_file(name, follow)
}

/// Newest log file for a bot, if any: `<root>/logs/<name>/<date>.log`.
fn newest_log_file(dir: &PathBuf) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    // Names are dates, so sorting by name is sorting by age.
    files.sort();
    files.pop()
}

fn show_log_file(name: &str, follow: bool) -> Result<(), BotError> {
    let dir = crate::paths::root().join("logs").join(name);
    let Some(file) = newest_log_file(&dir) else {
        return Err(BotError::Usage(format!(
            "No logs for \"{name}\" yet (looked in {})",
            dir.display()
        )));
    };

    println!("{}", file.display());
    println!();
    let contents = std::fs::read_to_string(&file)?;
    for line in contents.lines().rev().take(200).collect::<Vec<_>>().into_iter().rev() {
        println!("{line}");
    }
    if follow {
        println!();
        println!("Following a file needs no help from us: tail -f {}", file.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_known_name_resolves_to_itself() {
        assert_eq!(
            resolve_targets("home", &names(&["home", "work"])).unwrap(),
            vec!["home".to_string()]
        );
    }

    #[test]
    fn all_means_every_config() {
        assert_eq!(
            resolve_targets("all", &names(&["home", "work"])).unwrap(),
            names(&["home", "work"])
        );
    }

    #[test]
    fn an_unknown_name_lists_the_ones_that_exist() {
        // Typing the wrong name is the common case; the error is the only
        // place the right names are going to come from.
        let err = resolve_targets("hom", &names(&["home", "work"])).unwrap_err();
        assert!(err.contains("home, work"), "{err}");
        assert!(err.contains("all"), "{err}");
    }

    #[test]
    fn with_no_configs_the_answer_is_to_make_one() {
        let err = resolve_targets("all", &[]).unwrap_err();
        assert!(err.contains("--setup"), "{err}");
    }

    #[test]
    fn every_verb_reports_in_words() {
        assert_eq!(past_tense("start"), "started");
        // "stoped" is what bolting "ed" onto the verb produced.
        assert_eq!(past_tense("stop"), "stopped");
        assert_eq!(past_tense("restart"), "restarted");
    }

    #[test]
    fn unit_names_escape_the_config_name() {
        // systemd cannot address an instance with a raw space in it.
        assert_eq!(unit_for("my server"), r"ttspotify@my\x20server.service");
        assert_eq!(unit_for("home"), "ttspotify@home.service");
    }
}
