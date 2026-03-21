use librespot_core::cache::Cache;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_core::authentication::Credentials;
use librespot_oauth::OAuthClientBuilder;
use crate::error::BotError;

/// Spotify client ID (same one librespot uses internally)
const SPOTIFY_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
const OAUTH_REDIRECT: &str = "http://127.0.0.1:5588/login";
const OAUTH_SCOPES: &[&str] = &[
    "streaming",
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
];

pub struct SpotifyAuth {
    session: Option<Session>,
    cache: Option<Cache>,
    config: SessionConfig,
}

impl SpotifyAuth {
    pub fn new() -> Self {
        let base = crate::config::config_dir();
        let cache_dir = base.join("spotify_cache");
        let audio_cache_dir = cache_dir.join("audio");

        let cache = Cache::new(
            Some(base),
            Some(cache_dir),
            Some(audio_cache_dir),
            None,
        ).ok();

        let config = SessionConfig::default();

        Self {
            session: None,
            cache,
            config,
        }
    }

    pub async fn connect(&mut self) -> Result<Session, BotError> {
        let session = Session::new(self.config.clone(), self.cache.clone());

        // Try cached credentials first
        let credentials = if let Some(cache) = &self.cache {
            if let Some(cached_creds) = cache.credentials() {
                tracing::info!("Using cached Spotify credentials");
                cached_creds
            } else {
                tracing::info!("No cached credentials found, starting OAuth flow...");
                self.oauth_login()?
            }
        } else {
            tracing::info!("No cache available, starting OAuth flow...");
            self.oauth_login()?
        };

        match session.connect(credentials, true).await {
            Ok(()) => {
                tracing::info!("Spotify session connected");
                self.session = Some(session.clone());
                Ok(session)
            }
            Err(e) => {
                // If cached credentials failed, try OAuth
                tracing::warn!("Cached credentials failed: {e}. Trying OAuth...");
                let credentials = self.oauth_login()?;
                session.connect(credentials, true).await
                    .map_err(|e| BotError::SpotifyAuth(format!("OAuth login also failed: {e}")))?;
                tracing::info!("Spotify session connected via OAuth");
                self.session = Some(session.clone());
                Ok(session)
            }
        }
    }

    /// Run the OAuth PKCE flow to get credentials.
    /// Opens a browser URL for the user to authorize, then catches the callback.
    fn oauth_login(&self) -> Result<Credentials, BotError> {
        tracing::info!("Starting Spotify OAuth login...");
        tracing::info!("A browser window will open. Log in to Spotify and authorize the app.");
        tracing::info!("If no browser opens, visit the URL printed below.");

        let oauth_client = OAuthClientBuilder::new(
            SPOTIFY_CLIENT_ID,
            OAUTH_REDIRECT,
            OAUTH_SCOPES.to_vec(),
        )
        .open_in_browser()
        .with_custom_message(
            "<html><body><h1>Success!</h1><p>You can close this window and return to the bot.</p></body></html>"
        )
        .build()
        .map_err(|e| BotError::SpotifyAuth(format!("Failed to build OAuth client: {e}")))?;

        let token = oauth_client.get_access_token()
            .map_err(|e| BotError::SpotifyAuth(format!("OAuth flow failed: {e}")))?;

        tracing::info!("OAuth token obtained successfully");

        // Convert OAuth token to librespot Credentials
        let credentials = Credentials::with_access_token(&token.access_token);

        // The session will cache reusable credentials on successful connect
        Ok(credentials)
    }

}
