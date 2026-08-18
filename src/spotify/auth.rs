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

/// Whether a failed `Session::connect` means the stored login was refused, as
/// opposed to Spotify being unreachable.
///
/// `connect` reports both through one `Err`, and treating every failure as a
/// refusal sent the user to a browser whenever the network was briefly down —
/// on every retry, so a boot before the network was up produced a run of
/// browser windows asking for a login that was never the problem.
///
/// librespot maps a rejected login (and an HTTP 401/403) to `PermissionDenied`
/// and everything else to other kinds: a lost connection is `Unavailable`, a
/// timeout is `DeadlineExceeded`. `PermissionDenied` is used in exactly those
/// two credential cases, so it is the whole rule.
pub fn credentials_were_rejected(error: &librespot_core::Error) -> bool {
    error.kind == librespot_core::error::ErrorKind::PermissionDenied
}

/// A username worth showing, or `None`.
///
/// Spotify does not always return one with the stored credentials, and has been
/// seen to return an empty or padded string. Showing "signed in as " with
/// nothing after it reads as a bug, so an unusable name is treated as absent.
fn usable_username(raw: Option<String>) -> Option<String> {
    let name = raw?;
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
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
        // Credentials are secret and kept apart from the caches, which are
        // safe to delete at any time.
        let credentials_dir = crate::paths::auth_dir();
        let cache_dir = crate::paths::cache_dir().join("spotify_cache");
        let audio_cache_dir = cache_dir.join("audio");

        let cache = Cache::new(
            Some(credentials_dir),
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

    /// Check if cached Spotify credentials exist (without connecting).
    pub fn has_cached_credentials(&self) -> bool {
        self.cache.as_ref().is_some_and(|c| c.credentials().is_some())
    }

    /// The account the cached credentials belong to, if there are any and they
    /// name one.
    ///
    /// Read from the cache rather than a session, because the tray has no
    /// session: it only ever asks whether somebody is signed in. Spotify
    /// returns a canonical username with the reusable credentials, but not
    /// always, so this can be `None` even when a sign-in exists.
    pub fn cached_username(&self) -> Option<String> {
        usable_username(
            self.cache
                .as_ref()
                .and_then(|c| c.credentials())
                .and_then(|c| c.username),
        )
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
                self.oauth_login().await?
            }
        } else {
            tracing::info!("Spotify cache not available. Starting OAuth login...");
            self.oauth_login().await?
        };

        match session.connect(credentials, true).await {
            Ok(()) => {
                tracing::info!("Spotify session established");
                log_account_type(session);
                Ok(())
            }
            Err(e) if credentials_were_rejected(&e) => {
                tracing::warn!("Spotify rejected the stored credentials: {e}. Signing in again...");
                let credentials = self.oauth_login().await?;
                session.connect(credentials, true).await
                    .map_err(|e| BotError::SpotifyAuth(format!("OAuth login also failed: {e}")))?;
                tracing::info!("Spotify session established via OAuth re-authentication");
                log_account_type(session);
                Ok(())
            }
            Err(e) => {
                // Not a credential problem, so signing in again cannot help.
                // Opening a browser here is actively wrong: it happens on every
                // boot before the network is up, and the caller retries, so a
                // transient outage produced a browser window per attempt.
                Err(BotError::SpotifyAuth(format!(
                    "could not reach Spotify: {e}. The stored login was not \
                     rejected, so this is a connection problem rather than a \
                     sign-in one"
                )))
            }
        }
    }

    /// Connect a session using cached credentials only — never opens a browser
    /// and never prompts. For background contexts (session recovery): there is
    /// nobody at the keyboard, so a rejected login is an error to report, not
    /// a reason to start an interactive flow. `connect_existing`'s rejected-
    /// credentials arm falls back to OAuth, which from a background task would
    /// pop a login tab out of nowhere and then block on the OAuth listener
    /// forever if the user never completes it.
    pub async fn connect_cached_only(&self, session: &Session) -> Result<(), BotError> {
        let credentials = self
            .cache
            .as_ref()
            .and_then(|c| c.credentials())
            .ok_or_else(|| {
                BotError::SpotifyAuth("no cached Spotify credentials to reconnect with".into())
            })?;
        session.connect(credentials, true).await.map_err(|e| {
            BotError::SpotifyAuth(format!("could not reconnect to Spotify: {e}"))
        })?;
        tracing::info!("Spotify session re-established from cached credentials");
        log_account_type(session);
        Ok(())
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
        let credentials = self.oauth_login().await?;
        session.connect(credentials, true).await
            .map_err(|e| BotError::SpotifyAuth(format!("OAuth login failed: {e}")))?;
        log_account_type(&session);
        self.session = Some(session.clone());
        Ok(session)
    }

    /// Run the OAuth PKCE flow to get credentials.
    /// Opens a browser URL for the user to authorize, then catches the callback.
    /// In headless mode, skips browser launch and prints instructions.
    /// Run the sign-in without holding an async worker hostage.
    ///
    /// The flow blocks on a TcpListener waiting for the browser to come back
    /// (or on stdin in headless mode) for as long as the user takes, which can
    /// be minutes. librespot's `_async` variants do not help: they call the
    /// same blocking listener internally and only await the token exchange
    /// afterwards. The blocking pool is where this belongs.
    async fn oauth_login(&self) -> Result<Credentials, BotError> {
        let headless = self.headless;
        let feasible = self.oauth_feasible();
        tokio::task::spawn_blocking(move || Self::oauth_login_blocking(headless, feasible))
            .await
            .map_err(|e| BotError::SpotifyAuth(format!("sign-in worker failed: {e}")))?
    }

    fn oauth_login_blocking(headless: bool, oauth_feasible: bool) -> Result<Credentials, BotError> {
        // Refuse cleanly when neither a browser nor a terminal is available
        // (e.g. under systemd) instead of blocking on a stdin that is EOF.
        if !oauth_feasible {
            return Err(BotError::SpotifyAuth(format!(
                "no cached Spotify credentials and no way to log in interactively here; \
                 on this machine, {}, then restart the bot",
                crate::hints::sign_in_spotify()
            )));
        }

        // In headless mode, use a port-less redirect URI so librespot-oauth
        // falls back to stdin input instead of starting a local HTTP server.
        // The user pastes the redirect URL from their browser's address bar.
        let redirect = if headless { "http://127.0.0.1/login" } else { OAUTH_REDIRECT };

        println!("Spotify Authentication");
        if headless {
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

        if !headless {
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

/// Log what kind of account just signed in, so a log answers "is this Premium?"
/// outright. Playback needs Premium; without it Spotify refuses the audio key
/// and librespot streams undecryptable bytes, which only shows up much later as
/// a decode failure.
fn log_account_type(session: &Session) {
    use crate::spotify::account::{classify, describe, AccountType};

    let data = session.user_data();
    let account = classify(session.get_user_attribute("type").as_deref());
    let line = describe(&account, &data.country);
    match account {
        AccountType::Other(_) => tracing::warn!("{line}"),
        _ => tracing::info!("{line}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{credentials_were_rejected, oauth_is_feasible};

    /// Errors built through librespot's own constructors, so these track what
    /// it really produces rather than a guess at its shape.
    #[test]
    fn a_blank_username_is_treated_as_no_username() {
        // Spotify has been seen to return empty or padded names. "signed in as"
        // followed by nothing reads as a bug rather than as a missing name.
        use super::usable_username;
        assert_eq!(usable_username(None), None);
        assert_eq!(usable_username(Some(String::new())), None);
        assert_eq!(usable_username(Some("   ".to_string())), None);
        assert_eq!(
            usable_username(Some("  aloys  ".to_string())),
            Some("aloys".to_string()),
            "a real name should be kept, and tidied"
        );
    }

    #[test]
    fn refusing_an_impossible_sign_in_survives_the_blocking_hop() {
        // The sign-in runs on the blocking pool now, so its refusal comes back
        // through a join. Losing it there would turn "cannot sign in on this
        // machine" into a crash under systemd, which is what the refusal exists
        // to prevent.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let err = rt.block_on(async {
            tokio::task::spawn_blocking(|| {
                super::SpotifyAuth::oauth_login_blocking(true, false)
            })
            .await
            .expect("the blocking task must not panic")
        });
        let err = err.expect_err("an infeasible sign-in must refuse, not proceed");
        assert!(
            err.to_string().contains(" auth"),
            "the refusal should say what to do instead: {err}"
        );
    }

    mod connect_failures {
        use super::credentials_were_rejected;
        use librespot_core::Error;

        #[derive(Debug, thiserror::Error)]
        #[error("boom")]
        struct Boom;

        #[test]
        fn a_refused_login_asks_the_user_to_sign_in_again() {
            // What librespot returns for AuthenticationError::LoginFailed, and
            // for an HTTP 401/403 - the only two things it calls
            // permission_denied.
            assert!(credentials_were_rejected(&Error::permission_denied(Boom)));
        }

        #[test]
        fn an_unreachable_spotify_does_not_open_a_browser() {
            // The bug this exists for: a bot starting before the network is up
            // used to be sent to a browser, once per retry.
            assert!(!credentials_were_rejected(&Error::unavailable(Boom)));
        }

        #[test]
        fn a_timeout_does_not_open_a_browser() {
            assert!(!credentials_were_rejected(&Error::deadline_exceeded(Boom)));
        }

        #[test]
        fn no_other_failure_is_mistaken_for_a_refusal() {
            // Everything else librespot can produce on this path. If a future
            // version starts reporting refusals differently this stays safe:
            // the worst case is not offering a sign-in, not a surprise browser.
            for e in [
                Error::unavailable(Boom),
                Error::deadline_exceeded(Boom),
                Error::internal(Boom),
                Error::unknown(Boom),
                Error::invalid_argument(Boom),
                Error::not_found(Boom),
                Error::aborted(Boom),
                Error::cancelled(Boom),
                Error::failed_precondition(Boom),
                Error::resource_exhausted(Boom),
                Error::unauthenticated(Boom),
                Error::unimplemented(Boom),
            ] {
                assert!(
                    !credentials_were_rejected(&e),
                    "{:?} should not be treated as a refused login",
                    e.kind
                );
            }
        }
    }

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
