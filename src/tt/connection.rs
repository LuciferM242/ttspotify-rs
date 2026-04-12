use std::time::Duration;
use teamtalk::Client;
use teamtalk::client::connection::{ConnectParamsOwned, ReconnectConfig, ReconnectWorkflowConfig};
use teamtalk::client::users::LoginParams;
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
    client.wait_for(teamtalk::Event::ConnectSuccess, 10_000)
        .ok_or_else(|| BotError::TeamTalk("Connection timeout".into()))?;
    tracing::info!("Connected to TeamTalk server");

    // Login
    tracing::info!("Logging in as '{}'...", config.bot_name);
    client.login_and_wait(&config.bot_name, &config.username, &config.password, "TTSpotifyBot", 10_000)
        .map_err(|e| BotError::TeamTalk(format!("Login failed: {e}")))?;
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
    let channel_id = join_channel(&client, config)?;

    // Enable SDK auto-reconnect: handles reconnect -> re-login -> re-join automatically
    let reconnect_config = ReconnectConfig {
        max_attempts: 10,
        min_delay: Duration::from_secs(2),
        max_delay: Duration::from_secs(30),
        ..Default::default()
    };
    client.enable_full_auto_reconnect(
        reconnect_config,
        ReconnectWorkflowConfig::default(),
        ConnectParamsOwned::new(&config.host, config.tcp_port, config.udp_port, config.encrypted),
        LoginParams::new(&config.bot_name, &config.username, &config.password, "TTSpotifyBot"),
    );
    if channel_id != ChannelId(0) {
        client.set_last_channel(channel_id, Some(&config.channel_password));
    }
    tracing::info!("Auto-reconnect enabled");

    Ok(client)
}

fn join_channel(client: &Client, config: &BotConfig) -> Result<ChannelId, BotError> {
    let channel_path = &config.channel_name;
    tracing::info!("Joining channel '{channel_path}'...");

    // Wait for the channel tree to be populated after login.
    // The server sends channels after MySelfLoggedIn but the client
    // may not have processed them yet on fast restarts.
    let mut channel_id = client.get_channel_id_from_path(channel_path);
    if channel_id == ChannelId(0) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            client.poll(100);
            channel_id = client.get_channel_id_from_path(channel_path);
            if channel_id != ChannelId(0) {
                break;
            }
        }
    }

    let joined_id = if channel_id == ChannelId(0) {
        tracing::warn!("Channel '{channel_path}' not found, joining root channel");
        ChannelId(1)
    } else {
        channel_id
    };

    match client.join_channel_and_wait(joined_id, &config.channel_password, 5_000) {
        Ok(_) => tracing::info!("Joined channel successfully"),
        Err(e) => tracing::warn!("Channel join did not confirm in time: {e}, continuing anyway"),
    }

    Ok(joined_id)
}
