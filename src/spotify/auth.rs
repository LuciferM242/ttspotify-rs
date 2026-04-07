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
    headless: bool,
}

/// Detect if we're running in a headless environment (no display server).
fn detect_headless() -> bool {
    // Explicit override via env var
    if let Ok(val) = std::env::var("TTSPOTIFY_HEADLESS") {
        return val == "1" || val.eq_ignore_ascii_case("true");
    }

    // On Linux, check for display server
    #[cfg(target_os = "linux")]
    {
        let has_display = std::env::var("DISPLAY").is_ok()
            || std::env::var("WAYLAND_DISPLAY").is_ok();
        return !has_display;
    }

    // Windows/macOS always have GUI capability
    #[cfg(not(target_os = "linux"))]
    false
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
            headless: detect_headless(),
        }
    }

    /// Override headless detection (e.g. from CLI flag or env var).
    #[allow(dead_code)]
    pub fn set_headless(&mut self, headless: bool) {
        self.headless = headless;
    }

    /// Check if cached Spotify credentials exist (without connecting).
    pub fn has_cached_credentials(&self) -> bool {
        self.cache.as_ref().is_some_and(|c| c.credentials().is_some())
    }

    pub async fn connect(&mut self) -> Result<Session, BotError> {
        let session = Session::new(self.config.clone(), self.cache.clone());

        // Try cached credentials first
        let credentials = if let Some(cache) = &self.cache {
            if let Some(cached_creds) = cache.credentials() {
                tracing::info!("Found cached Spotify credentials, attempting connection...");
                cached_creds
            } else {
                tracing::info!("No cached Spotify credentials. Starting OAuth login...");
                self.oauth_login()?
            }
        } else {
            tracing::info!("Spotify cache not available. Starting OAuth login...");
            self.oauth_login()?
        };

        match session.connect(credentials, true).await {
            Ok(()) => {
                tracing::info!("Spotify session established");
                self.session = Some(session.clone());
                Ok(session)
            }
            Err(e) => {
                // If cached credentials failed, try OAuth
                tracing::warn!("Cached credentials rejected: {e}. Falling back to OAuth...");
                let credentials = self.oauth_login()?;
                session.connect(credentials, true).await
                    .map_err(|e| BotError::SpotifyAuth(format!("OAuth login also failed: {e}")))?;
                tracing::info!("Spotify session established via OAuth re-authentication");
                self.session = Some(session.clone());
                Ok(session)
            }
        }
    }

    /// Run the OAuth PKCE flow to get credentials.
    /// Opens a browser URL for the user to authorize, then catches the callback.
    /// In headless mode, skips browser launch and prints instructions.
    fn oauth_login(&self) -> Result<Credentials, BotError> {
        if self.headless {
            println!("Running in headless mode (no browser available).");
            println!("The OAuth server will listen on {OAUTH_REDIRECT}");
            println!("If running on a remote server, set up an SSH tunnel first:");
            println!("  ssh -L 5588:localhost:5588 your-server");
            println!("Then open the URL below in your local browser.");
        } else {
            println!("A browser window will open. Log in to Spotify and authorize the app.");
            println!("If no browser opens, visit the URL printed below.");
        }

        let mut builder = OAuthClientBuilder::new(
            SPOTIFY_CLIENT_ID,
            OAUTH_REDIRECT,
            OAUTH_SCOPES.to_vec(),
        );

        if !self.headless {
            builder = builder.open_in_browser();
        }

        let oauth_client = builder
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
