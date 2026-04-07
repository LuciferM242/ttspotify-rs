#![cfg_attr(windows, windows_subsystem = "windows")]
//! Windows system tray app for TTSpotify.
//!
//! Manages multiple bot instances via a wxDragon-based tray icon.
//! Each server gets a submenu for start/stop/restart/logs/config.
//! Status shown in menu items and tray tooltip.

#[cfg(not(windows))]
fn main() {
    eprintln!("The tray app is only available on Windows.");
    eprintln!("On Linux, use systemd services instead:");
    eprintln!("  tt-spotify-bot --install-service");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    // If launched with --setup, open the GUI config dialog directly
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--setup") {
        let name_arg = args
            .iter()
            .position(|a| a == "--setup")
            .and_then(|i| args.get(i + 1))
            .filter(|s| !s.starts_with('-'));

        // If a name was given and a config exists, open it for editing.
        // Otherwise open a blank config dialog.
        let (config, path) = if let Some(name) = name_arg {
            let p = tt_spotify_bot::config::config_dir().join(format!("{name}.json"));
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

        let _ = wxdragon::main(|_| {
            tt_spotify_bot::gui::config_dialog::open_config_dialog(config, path, |saved_path| {
                tracing::info!("Config saved to: {}", saved_path.display());
            });
        });
        return;
    }

    tt_spotify_bot::gui::run();
}
