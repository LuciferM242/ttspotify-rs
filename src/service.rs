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
    let exec_start = format!(
        "{} --config {}/{}.json",
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
RestartSec=5

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
