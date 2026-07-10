use std::time::Duration;
use teamtalk::Client;
use teamtalk::client::connection::{ConnectParamsOwned, ReconnectConfig, ReconnectWorkflowConfig};
use teamtalk::client::users::LoginParams;
use teamtalk::types::{ChannelId, SoundDeviceId, UserStatus};

use crate::config::BotConfig;
use crate::error::BotError;

/// Virtual sound device ID (TT_SOUNDDEVICE_ID_TEAMTALK_VIRTUAL = 1978)
const VIRTUAL_DEVICE_ID: SoundDeviceId = SoundDeviceId(1978);

/// Set up the TeamTalk client: connect, login, init virtual devices, join channel.
pub fn setup_teamtalk(config: &BotConfig) -> Result<Client, BotError> {
    // Set license before creating client (compile-time env vars take priority over config)
    let license_name = option_env!("TT_LICENSE_NAME")
        .map(String::from)
        .or(config.license_name.clone());
    let license_key = option_env!("TT_LICENSE_KEY")
        .map(String::from)
        .or(config.license_key.clone());

    if let (Some(name), Some(key)) = (&license_name, &license_key) {
        teamtalk::set_license(name, key)
            .map_err(|e| BotError::TeamTalk(format!("Failed to set license: {e}")))?;
        tracing::info!("TeamTalk license set for '{name}'");
    }

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
    let _ = client.enable_voice_transmission(false);

    // Set bot gender
    let gender = crate::config::parse_gender(&config.bot_gender);
    let mut status = UserStatus::default();
    status.gender = gender;
    let _ = client.set_status(status, "");
    tracing::info!("Bot gender set to {:?}", gender);

    // Join channel
    let _channel_id = join_channel(&client, config)?;

    // Enable SDK auto-reconnect for connection + login only.
    // Channel rejoin is handled by the event loop so admin moves are respected.
    let mut reconnect_config = ReconnectConfig::default();
    reconnect_config.max_attempts = 10;
    reconnect_config.min_delay = Duration::from_secs(2);
    reconnect_config.max_delay = Duration::from_secs(30);

    let mut join_cfg = ReconnectConfig::default();
    join_cfg.max_attempts = 0;
    let mut workflow = ReconnectWorkflowConfig::default();
    workflow.join = join_cfg;
    client.enable_full_auto_reconnect(
        reconnect_config,
        workflow,
        ConnectParamsOwned::new(&config.host, config.tcp_port, config.udp_port, config.encrypted),
        // password is an `impl Into<SecretString>`; pass an owned String.
        LoginParams::new(&config.bot_name, &config.username, config.password.clone(), "TTSpotifyBot"),
    );
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
