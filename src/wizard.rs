//! Interactive config setup wizard.
//!
//! Walks the user through creating a config file with prompted inputs.
//! Validates each field and writes valid JSON.

use std::io::{self, Write};

use crate::config::{config_dir, BotConfig};
use crate::error::BotError;

fn ask(prompt: &str, default: &str, required: bool) -> Option<String> {
    loop {
        if default.is_empty() {
            print!("  {prompt}: ");
        } else {
            print!("  {prompt} [{default}]: ");
        }
        io::stdout().flush().ok();

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) | Err(_) => {
                println!("\nSetup cancelled.");
                return None;
            }
            _ => {}
        }

        let input = input.trim().to_string();
        if input.is_empty() && !default.is_empty() {
            return Some(default.to_string());
        }
        if input.is_empty() && required {
            println!("    This field is required.");
            continue;
        }
        return Some(input);
    }
}

fn ask_int(prompt: &str, default: i32) -> Option<i32> {
    loop {
        let raw = ask(prompt, &default.to_string(), true)?;
        match raw.parse::<i32>() {
            Ok(v) => return Some(v),
            Err(_) => println!("    Invalid input. Expected a number."),
        }
    }
}

pub fn run_wizard(config_name: Option<&str>) -> Result<(), BotError> {
    println!();
    println!("TTSpotify Configuration Setup");
    println!();

    // Config file name
    let name = if let Some(n) = config_name {
        n.to_string()
    } else {
        match ask("Config name (used for file name and service name)", "config", true) {
            Some(n) => n.replace(".json", ""),
            None => return Ok(()),
        }
    };

    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let config_path = dir.join(format!("{name}.json"));

    if config_path.exists() {
        let overwrite = ask(
            &format!("{} already exists. Overwrite? (y/N)", config_path.display()),
            "n",
            false,
        );
        match overwrite {
            Some(ref v) if v.eq_ignore_ascii_case("y") || v.eq_ignore_ascii_case("yes") => {}
            _ => {
                println!("Setup cancelled.");
                return Ok(());
            }
        }
    }

    println!("TeamTalk Server Settings");
    let host = match ask("Server address", "", true) {
        Some(v) => v,
        None => return Ok(()),
    };
    let tcp_port = match ask_int("TCP port", 10333) {
        Some(v) => v,
        None => return Ok(()),
    };
    let udp_port = match ask_int("UDP port", tcp_port) {
        Some(v) => v,
        None => return Ok(()),
    };

    println!();
    println!("Bot Credentials");
    let username = match ask("Bot username", "", true) {
        Some(v) => v,
        None => return Ok(()),
    };
    let password = match ask("Bot password", "", false) {
        Some(v) => v,
        None => return Ok(()),
    };

    println!();
    println!("Bot Settings");
    let bot_name = match ask("Bot nickname", "MusicBot", true) {
        Some(v) => v,
        None => return Ok(()),
    };
    let channel = match ask("Channel to join (path or leave blank for root)", "/", false) {
        Some(v) => v,
        None => return Ok(()),
    };
    let channel_password = match ask("Channel password (if any)", "", false) {
        Some(v) => v,
        None => return Ok(()),
    };

    // Build config from defaults + user input
    let mut config = BotConfig::default();
    config.host = host;
    config.tcp_port = tcp_port;
    config.udp_port = udp_port;
    config.username = username;
    config.password = password;
    config.bot_name = bot_name;
    config.channel_name = if channel.is_empty() { "/".to_string() } else { channel };
    config.channel_password = channel_password;

    config.save(&config_path)?;

    println!();
    println!("  Config saved to: {}", config_path.display());

    // Offer Spotify authentication
    println!();
    println!("Spotify Authentication");
    let do_auth = ask("Authenticate with Spotify now? (Y/n)", "y", false);
    match do_auth {
        Some(ref v) if v.eq_ignore_ascii_case("n") || v.eq_ignore_ascii_case("no") => {
            println!("  Skipping Spotify authentication.");
            println!("  You can authenticate later with: tt-spotify-bot --auth");
        }
        _ => {
            println!("  Starting Spotify authentication...");
            // Spawn a new thread with its own tokio runtime to avoid
            // nested-runtime panic (wizard is sync, may be called from async main)
            let auth_result = std::thread::spawn(|| {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("  Failed to create async runtime: {e}");
                        return None;
                    }
                };
                let mut auth = crate::spotify::auth::SpotifyAuth::new();
                Some(rt.block_on(auth.connect()))
            }).join().ok().flatten();

            match auth_result {
                Some(Ok(_)) => {
                    println!("  Spotify authentication successful! Credentials cached.");
                }
                Some(Err(e)) => {
                    println!("  Spotify authentication failed: {e}");
                    println!("  You can try again with: tt-spotify-bot --auth");
                }
                None => {
                    println!("  Could not initialize authentication.");
                    println!("  You can authenticate later with: tt-spotify-bot --auth");
                }
            }
        }
    }

    println!();
    println!("  Run the bot with: tt-spotify-bot --config {}", config_path.display());
    println!();

    Ok(())
}
