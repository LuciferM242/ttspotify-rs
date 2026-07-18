//! Systemd user service generator (Linux only).
//!
//! Generates and installs a systemd user service template for running
//! multiple bot instances via `systemctl --user start ttspotify@myserver`.

use std::io::{self, Write};
use std::path::PathBuf;
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

fn prompt_yes_no(message: &str) -> bool {
    print!("{message} [y/N] ");
    io::stdout().flush().ok();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

pub fn install_service() -> Result<(), BotError> {
    let exe_path = std::env::current_exe()
        .map_err(|e| BotError::Config(format!("Cannot determine executable path: {e}")))?;
    let config_base = config_dir();

    let systemd = systemd_dir();
    std::fs::create_dir_all(&systemd)?;
    std::fs::create_dir_all(&config_base)?;

    let service_path = systemd.join(SERVICE_NAME);
    // Quote the binary and config paths so spaces in either don't break the unit.
    let exec_start = format!(
        "\"{}\" --config \"{}/{}.json\"",
        exe_path.display(),
        config_base.display(),
        "%i"
    );

    let unit = format!(
        r#"[Unit]
Description=TTSpotify Bot (%i)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exec_start}
Restart=on-failure
RestartForceExitStatus=42
RestartSec=2

[Install]
WantedBy=default.target
"#
    );

    std::fs::write(&service_path, unit)?;

    // Reload systemd
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();

    println!();
    println!("TTSpotify service installed.");
    println!("Config files go in: {}", config_base.display());
    println!();
    println!("Quick start:");
    println!("  tt-spotify-bot --setup myserver");
    println!("  systemctl --user start ttspotify@myserver");
    println!("  systemctl --user enable ttspotify@myserver");
    println!("  journalctl --user -u ttspotify@myserver -f");

    // Offer to enable/start existing configs
    let configs = list_configs();
    for (name, _) in configs {
        if prompt_yes_no(&format!("Enable and start ttspotify@{name} now?")) {
            let _ = Command::new("systemctl")
                .args(["--user", "enable", &format!("ttspotify@{name}")])
                .status();
            let _ = Command::new("systemctl")
                .args(["--user", "start", &format!("ttspotify@{name}")])
                .status();
            println!("  ttspotify@{name} enabled and started.");
        }
    }

    Ok(())
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

pub fn uninstall_service() -> Result<(), BotError> {
    let service_path = systemd_dir().join(SERVICE_NAME);
    if service_path.exists() {
        std::fs::remove_file(&service_path)?;
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        println!("TTSpotify service removed.");
        println!("Running instances are not affected until stopped.");
    } else {
        println!("No service file found at {}", service_path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_running_units;

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
}
