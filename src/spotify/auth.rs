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

/// Whether an interactive OAuth flow can possibly succeed: either a browser
/// can be opened (non-headless), or stdin is a terminal so the headless
/// paste-the-URL flow has someone to answer it. Under systemd both are false
/// (no display, stdin is /dev/null) — attempting OAuth there fails after the
/// bot has already logged into TeamTalk, which `Restart=on-failure` turns
/// into a nonstop login/logout crash-restart loop.
pub fn oauth_is_feasible(headless: bool, stdin_is_terminal: bool) -> bool {
    !headless || stdin_is_terminal
}

/// Whether the DISPLAY/WAYLAND_DISPLAY values indicate a usable display
/// server. Empty strings count as absent — some service environments set
/// `DISPLAY=""`, which is not a display.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn has_display(display: Option<&str>, wayland_display: Option<&str>) -> bool {
    display.is_some_and(|v| !v.is_empty()) || wayland_display.is_some_and(|v| !v.is_empty())
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
        let display = std::env::var("DISPLAY").ok();
        let wayland = std::env::var("WAYLAND_DISPLAY").ok();
        !has_display(display.as_deref(), wayland.as_deref())
    }

    // Windows/macOS always have GUI capability
    #[cfg(not(target_os = "linux"))]
    false
}

impl Default for SpotifyAuth {
    fn default() -> Self {
        Self::new()
    }
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

    /// Whether an interactive OAuth flow could succeed in this process.
    /// See [`oauth_is_feasible`].
    pub fn oauth_feasible(&self) -> bool {
        use std::io::IsTerminal;
        oauth_is_feasible(self.headless, std::io::stdin().is_terminal())
    }

    /// Build a fresh, unconnected session. No network, no browser — just the
    /// session object. Connect it later with `connect_existing`.
    pub fn new_session(&self) -> Session {
        Session::new(self.config.clone(), self.cache.clone())
    }

    /// Connect an existing session in place: try cached credentials first, fall
    /// back to OAuth (opens a browser) if none exist or they're rejected. The
    /// session is an Arc, so any player built from it becomes usable once this
    /// succeeds.
    pub async fn connect_existing(&self, session: &Session) -> Result<(), BotError> {
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
                Ok(())
            }
            Err(e) => {
                // If cached credentials failed, try OAuth
                tracing::warn!("Cached credentials rejected: {e}. Falling back to OAuth...");
                let credentials = self.oauth_login()?;
                session.connect(credentials, true).await
                    .map_err(|e| BotError::SpotifyAuth(format!("OAuth login also failed: {e}")))?;
                tracing::info!("Spotify session established via OAuth re-authentication");
                Ok(())
            }
        }
    }

    pub async fn connect(&mut self) -> Result<Session, BotError> {
        let session = self.new_session();
        self.connect_existing(&session).await?;
        self.session = Some(session.clone());
        Ok(session)
    }

    /// Force a fresh OAuth login, ignoring any cached credentials, and store
    /// the new credentials in the cache. Opens the browser for authorization.
    pub async fn reauthenticate(&mut self) -> Result<Session, BotError> {
        let session = Session::new(self.config.clone(), self.cache.clone());
        let credentials = self.oauth_login()?;
        session.connect(credentials, true).await
            .map_err(|e| BotError::SpotifyAuth(format!("OAuth login failed: {e}")))?;
        self.session = Some(session.clone());
        Ok(session)
    }

    /// Run the OAuth PKCE flow to get credentials.
    /// Opens a browser URL for the user to authorize, then catches the callback.
    /// In headless mode, skips browser launch and prints instructions.
    fn oauth_login(&self) -> Result<Credentials, BotError> {
        // Refuse cleanly when neither a browser nor a terminal is available
        // (e.g. under systemd) instead of blocking on a stdin that is EOF.
        if !self.oauth_feasible() {
            return Err(BotError::SpotifyAuth(
                "no cached Spotify credentials and no way to log in interactively here; \
                 run `tt-spotify-bot --auth` on this machine, then restart the bot"
                    .to_string(),
            ));
        }

        // In headless mode, use a port-less redirect URI so librespot-oauth
        // falls back to stdin input instead of starting a local HTTP server.
        // The user pastes the redirect URL from their browser's address bar.
        let redirect = if self.headless { "http://127.0.0.1/login" } else { OAUTH_REDIRECT };

        println!("Spotify Authentication");
        if self.headless {
            println!("Open the URL below in a browser and authorize the app.");
            println!("The page will then show an error like 'This site can't be reached'");
            println!("or 'site not found' -- THIS IS NORMAL. Do not close it.");
            println!("Copy the full URL from the browser's address bar and paste it below.");
        } else {
            println!("A browser window will open. Log in to Spotify and authorize the app.");
            println!("If no browser opens, visit the URL printed below.");
        }

        let mut builder = OAuthClientBuilder::new(
            SPOTIFY_CLIENT_ID,
            redirect,
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

        println!("Spotify account connected successfully.");
        tracing::info!("OAuth token obtained successfully");

        Ok(Credentials::with_access_token(&token.access_token))
    }

}

#[cfg(test)]
mod tests {
    use super::oauth_is_feasible;

    #[test]
    fn oauth_feasible_with_browser_regardless_of_stdin() {
        // Non-headless (Windows tray, desktop Linux): browser flow works.
        assert!(oauth_is_feasible(false, false));
        assert!(oauth_is_feasible(false, true));
    }

    #[test]
    fn oauth_feasible_headless_with_terminal_paste_flow() {
        // SSH session on a headless box: paste-the-URL flow reads stdin.
        assert!(oauth_is_feasible(true, true));
    }

    #[test]
    fn oauth_infeasible_headless_without_terminal() {
        // systemd service: no display, stdin is /dev/null. OAuth can only fail.
        assert!(!oauth_is_feasible(true, false));
    }

    #[test]
    fn display_env_present_and_nonempty_counts_as_display() {
        assert!(super::has_display(Some(":0"), None));
        assert!(super::has_display(None, Some("wayland-0")));
        assert!(super::has_display(Some(":0"), Some("wayland-0")));
    }

    #[test]
    fn empty_or_missing_display_env_is_headless() {
        // Some service environments set DISPLAY="" — that is not a display.
        assert!(!super::has_display(None, None));
        assert!(!super::has_display(Some(""), None));
        assert!(!super::has_display(None, Some("")));
        assert!(!super::has_display(Some(""), Some("")));
    }
}
