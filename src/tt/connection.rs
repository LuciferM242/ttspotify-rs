use std::time::Duration;
use teamtalk::{Client, Event};
use teamtalk::types::{ChannelId, UserStatus};

use crate::config::BotConfig;
use crate::error::BotError;

/// Virtual sound device ID (TT_SOUNDDEVICE_ID_TEAMTALK_VIRTUAL = 1978)
const VIRTUAL_DEVICE_ID: i32 = 1978;

/// Set up the TeamTalk client: connect, login, init virtual devices, join channel.
pub fn setup_teamtalk(config: &BotConfig) -> Result<Client, BotError> {
    let client = Client::new()
        .map_err(|e| BotError::TeamTalk(format!("Failed to create client: {e}")))?;

    // Connect
    tracing::info!("Connecting to TeamTalk server {}:{}...", config.host, config.tcp_port);
    client.connect(&config.host, config.tcp_port, config.udp_port, config.encrypted)
        .map_err(|e| BotError::TeamTalk(format!("Connection failed: {e}")))?;

    // Wait for connection
    wait_for_event(&client, Event::ConnectSuccess, 10_000, "Connection timeout")?;
    tracing::info!("Connected to TeamTalk server");

    // Login
    tracing::info!("Logging in as '{}'...", config.bot_name);
    client.login(&config.bot_name, &config.username, &config.password, "TTSpotifyBot");
    wait_for_event(&client, Event::MySelfLoggedIn, 10_000, "Login timeout")?;
    tracing::info!("Logged in successfully");

    // Init virtual sound devices for audio block injection
    if !client.init_sound_input_device(VIRTUAL_DEVICE_ID) {
        return Err(BotError::TeamTalk("Failed to init virtual input device".into()));
    }
    if !client.init_sound_output_device(VIRTUAL_DEVICE_ID) {
        return Err(BotError::TeamTalk("Failed to init virtual output device".into()));
    }
    tracing::info!("Virtual sound devices initialized");

    // Disable voice transmission (we inject audio blocks manually)
    client.enable_voice_transmission(false);

    // Set bot gender
    let gender = crate::config::parse_gender(&config.bot_gender);
    let status = UserStatus {
        gender,
        ..Default::default()
    };
    client.set_status(status, "");
    tracing::info!("Bot gender set to {:?}", gender);

    // Join channel
    join_channel(&client, config)?;

    Ok(client)
}

fn wait_for_event(client: &Client, target: Event, timeout_ms: i32, err_msg: &str) -> Result<(), BotError> {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(BotError::TeamTalk(err_msg.to_string()));
        }
        if let Some((event, _msg)) = client.poll(remaining.as_millis().min(100) as i32) {
            if event == target {
                return Ok(());
            }
            if event == Event::ConnectFailed || event == Event::ConnectionLost {
                return Err(BotError::TeamTalk(format!("Connection lost/failed while waiting for {target:?}")));
            }
        }
    }
}

fn join_channel(client: &Client, config: &BotConfig) -> Result<(), BotError> {
    let channel_path = &config.channel_name;
    tracing::info!("Joining channel '{channel_path}'...");

    let channel_id = client.get_channel_id_from_path(channel_path);
    if channel_id == ChannelId(0) {
        // Try root channel
        tracing::warn!("Channel '{channel_path}' not found, joining root channel");
        client.join_channel(ChannelId(1), &config.channel_password);
    } else {
        client.join_channel(channel_id, &config.channel_password);
    }

    // Wait briefly for join to process
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let Some((event, _)) = client.poll(100) {
            // UserJoined for ourselves indicates we joined
            if event == Event::UserJoined {
                tracing::info!("Joined channel successfully");
                return Ok(());
            }
        }
    }

    tracing::warn!("Channel join confirmation timeout, continuing anyway");
    Ok(())
}
