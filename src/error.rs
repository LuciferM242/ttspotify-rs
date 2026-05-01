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
    #[error("Not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
}
