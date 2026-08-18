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
        return Err(format!("No bots exist yet. To create one, {}", crate::hints::create_bot()));
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
            "The systemd service is not installed yet. To install it, {}",
            crate::hints::install_service()
        )));
    }
    if !service::systemd_reachable() {
        return Err(BotError::Usage(service::no_session_hint()));
    }
    Ok(())
}

/// `--list`: the configs on this machine, whether or not systemd is involved.
pub fn list() {
    let (configs, problems) = crate::config::list_configs_and_problems();
    if configs.is_empty() {
        println!("No bots configured yet.");
        println!("To create one, {}", crate::hints::create_bot());
    }
    for (name, path) in configs {
        println!("{name}");
        println!("  config: {}", path.display());
    }
    print_problems(&problems);
}

/// Say which files in the config folder were passed over, and why.
///
/// Printed after the bots rather than logged: a bot missing from the list is
/// exactly when someone needs this, and a warning line in the middle of the
/// output was easy to miss and easy to mistake for a crash.
fn print_problems(problems: &[String]) {
    if problems.is_empty() {
        return;
    }
    println!();
    println!("These files in the config folder are not usable as bots:");
    for problem in problems {
        println!("  {problem}");
    }
}

/// `--status`: what each bot is doing right now.
pub fn status() {
    let (found, problems) = crate::config::list_configs_and_problems();
    let configs: Vec<String> = found.into_iter().map(|(name, _)| name).collect();
    if configs.is_empty() {
        println!("No bots configured yet.");
        println!("To create one, {}", crate::hints::create_bot());
        print_problems(&problems);
        return;
    }

    if !service::systemd_booted() {
        println!("systemd is not running here, so bots are not managed as services.");
        println!("Configured bots: {}", configs.join(", "));
        print_problems(&problems);
        return;
    }
    // Without a session bus every unit reads as not running, so listing them
    // all as "stopped" would be a made-up answer rather than a report.
    if !service::systemd_reachable() {
        println!("{}", service::no_session_hint());
        println!("Configured bots: {}", configs.join(", "));
        print_problems(&problems);
        return;
    }

    let enabled = service::enabled_instance_units();
    for name in configs {
        let unit = unit_for(&name);
        let health = service::unit_health(&unit);
        let word = match health {
            service::UnitHealth::Running => "running",
            service::UnitHealth::Restarting => "failing and being restarted",
            service::UnitHealth::Failed => "failed",
            service::UnitHealth::Stopped => "stopped",
        };
        println!(
            "{name}: {word}{}",
            if enabled.contains(&unit) { ", starts at login" } else { "" }
        );
        match health {
            service::UnitHealth::Running => {
                if let Some(since) = active_since(&unit) {
                    println!("  since {since}");
                }
            }
            // Saying "failed" and stopping there sends people to systemctl to
            // find out why, which is the thing these commands exist to avoid.
            service::UnitHealth::Restarting | service::UnitHealth::Failed => {
                println!("  it starts and then stops, usually a wrong server address, password");
                println!("  or channel. To see the reason: {}", log_hint(&name));
            }
            service::UnitHealth::Stopped => {}
        }
    }
    print_problems(&problems);
}

/// How to read one bot's log, for the messages that send people there.
fn log_hint(name: &str) -> String {
    format!("{} logs {name}", crate::paths::program_name())
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
        let before = service::unit_health(&unit);
        // "start" on something already up used to answer "started.", which
        // reads as "it was not running and now it is".
        if verb == "start" && before == service::UnitHealth::Running {
            println!("{name}: already running.");
            continue;
        }
        // A unit that hit its start limit refuses to start again until the
        // failure is cleared, and the refusal says nothing a user could act
        // on. Asking to start it is asking for that clean slate.
        if matches!(verb, "start" | "restart") && before == service::UnitHealth::Failed {
            service::reset_failed(&unit);
        }
        let ok = Command::new("systemctl")
            .args(["--user", verb, &unit])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            println!("{name}: {}.", past_tense(verb));
        } else {
            failures += 1;
            println!("{name}: {verb} failed. To see why: {}", log_hint(name));
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

/// `logs <name>` and `watch <name>`: the journal for one bot, or its log files
/// when there is no journal to read. `following` is what separates the two.
pub fn logs(name: &str, following: bool) -> Result<(), BotError> {
    let configs = config_names();
    // "all" is meaningless for a single log stream, so it is not accepted here.
    if !configs.iter().any(|c| c == name) {
        return Err(BotError::Usage(format!(
            "No config named \"{name}\". Available: {}.",
            if configs.is_empty() { "none".to_string() } else { configs.join(", ") }
        )));
    }

    // The journal only has anything to show if this bot was run as a service.
    // Started from a terminal with `run`, its unit has no entries at all —
    // and journalctl reports that with "-- No entries --" and exit 0, so
    // trusting the exit code showed the user an empty screen while the real
    // log sat in a file.
    if service::systemd_booted() && service::service_installed() && journal_has_entries(name) {
        let unit = unit_for(name);
        let mut cmd = Command::new("journalctl");
        // 200 lines was almost entirely the TeamTalk SDK's ALSA complaints on
        // startup, repeated per restart, which buried the one line that says
        // why the bot stopped. 60 is roughly one start plus what followed.
        cmd.args(["--user", "-u", &unit, "-n", "60"]);
        if following {
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

    show_log_file(name, following)
}

/// `remove [name]`: delete a bot.
///
/// A bot owns exactly two things: its config and its log folder. The rest of
/// the data root is shared — `state/` holds per-TeamTalk-user language choices
/// for every bot, and `auth/` holds the one Spotify login this install uses, so
/// deleting either because one bot went away would take the others with it.
/// (When a bot can have its own Spotify account, that credential joins the list
/// below.)
pub fn remove(name: Option<&str>) -> Result<(), BotError> {
    let configs = crate::config::list_configs();
    if configs.is_empty() {
        return Err(BotError::Usage("There are no bots to remove.".to_string()));
    }

    let (name, path) = match name {
        Some(wanted) => configs
            .iter()
            .find(|(n, _)| n == wanted)
            .cloned()
            .ok_or_else(|| {
                BotError::Usage(format!(
                    "No bot named \"{wanted}\". Available: {}.",
                    configs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
                ))
            })?,
        None => match pick_bot(&configs) {
            Some(chosen) => chosen,
            None => return Ok(()),
        },
    };

    println!();
    println!("This deletes {}.", path.display());
    if !service::prompt_yes_no(&format!("Remove the bot \"{name}\"?")) {
        println!("Nothing deleted.");
        return Ok(());
    }

    // Only after the answer. Stopping first would knock a running bot off the
    // server and un-enable it before the user had agreed to anything, and
    // "Nothing deleted." would then be a lie about a bot that had just been
    // killed and would not come back at the next login.
    if service::systemd_booted() && service::service_installed() {
        let unit = unit_for(&name);
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", &unit])
            .output();
    }

    // Raw `?` here answered a read-only config folder with
    // "IO: Permission denied (os error 13)" — true, and no help at all.
    std::fs::remove_file(&path).map_err(|e| {
        BotError::Usage(format!(
            "Could not delete {}: {e}.\nThe bot is still there.",
            path.display()
        ))
    })?;
    // The lock is named after the config; nothing will ever take it again.
    let _ = std::fs::remove_file(crate::runlock::lock_path(&path));
    println!("Removed \"{name}\".");

    // Logs are often both the reason a bot is being deleted and the record of
    // why, so they are a separate question.
    let logs = crate::paths::root().join("logs").join(&name);
    if logs.is_dir() {
        let count = std::fs::read_dir(&logs).map(|d| d.count()).unwrap_or(0);
        let files = if count == 1 { "file" } else { "files" };
        println!();
        if service::prompt_yes_no(&format!(
            "Also delete its logs ({count} {files} in {})?",
            logs.display()
        )) {
            match std::fs::remove_dir_all(&logs) {
                Ok(()) => println!("Logs deleted."),
                Err(e) => println!("Could not delete the logs: {e}"),
            }
        } else {
            println!("Logs kept in {}", logs.display());
        }
    }
    Ok(())
}

/// The numbered list shown when a command that needs a bot was not told which.
fn pick_bot(configs: &[(String, std::path::PathBuf)]) -> Option<(String, std::path::PathBuf)> {
    let names: Vec<String> = configs.iter().map(|(n, _)| n.clone()).collect();
    println!("Which bot?");
    for (i, name) in names.iter().enumerate() {
        println!("  {}. {name}", i + 1);
    }
    for _ in 0..3 {
        let raw = crate::wizard::ask("Number or name", "", false)?;
        if let Some(index) = crate::config::parse_config_choice(&raw, &names) {
            return Some(configs[index].clone());
        }
        println!("  Not one of the choices.");
    }
    None
}

/// The config `run` should start: the named one, or one picked from the list.
pub fn resolve_run_target(name: Option<&str>) -> Result<Option<std::path::PathBuf>, BotError> {
    let configs = crate::config::list_configs();
    match name {
        Some(wanted) => configs
            .iter()
            .find(|(n, _)| n == wanted)
            .map(|(_, path)| Some(path.clone()))
            .ok_or_else(|| {
                BotError::Usage(format!(
                    "No bot named \"{wanted}\". Available: {}.",
                    if configs.is_empty() {
                        "none".to_string()
                    } else {
                        configs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
                    }
                ))
            }),
        None if configs.len() == 1 => Ok(Some(configs[0].1.clone())),
        None if configs.is_empty() => Err(BotError::Usage(format!(
            "No bots yet. To create one, {}",
            crate::hints::create_bot()
        ))),
        None => Ok(pick_bot(&configs).map(|(_, path)| path)),
    }
}

/// Whether the journal actually holds anything for this bot's unit.
fn journal_has_entries(name: &str) -> bool {
    let unit = unit_for(name);
    Command::new("journalctl")
        .args(["--user", "-u", &unit, "-n", "1", "--no-pager", "--quiet"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
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

fn show_log_file(name: &str, following: bool) -> Result<(), BotError> {
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
    if following {
        println!();
        println!("Watching needs systemd's journal, which is not available here.");
        println!("Use: tail -f {}", file.display());
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
        assert!(err.contains("add"), "{err}");
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
