use thiserror::Error;

#[derive(Debug, Error)]
pub enum BotError {
    #[error("Config: {0}")]
    Config(String),
    #[error("Spotify auth: {0}")]
    SpotifyAuth(String),
    #[error("Spotify playback: {0}")]
    Playback(String),
    #[error("No results found")]
    NoResults,
    #[error("TeamTalk: {0}")]
    TeamTalk(String),
    #[error("Audio pipeline: {0}")]
    Audio(String),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Channel send error: {0}")]
    ChannelSend(String),
}
