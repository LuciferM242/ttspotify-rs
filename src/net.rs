//! The one HTTP client policy downloads share.

/// A client bounded by stall, not by total time: connecting must succeed
/// within 15 seconds and silence between bytes may last at most 30, but a
/// slow, healthy transfer may take as long as it needs.
///
/// The stall bounds matter more than they look: these requests run under the
/// tray's modal progress dialogs, which cannot be closed until the worker
/// finishes — with no read timeout a half-open socket kept the whole tray
/// hostage until the OS TCP keepalive gave up, hours later.
///
/// The deliberate exception is the update *check* (`update::github::check`),
/// which fetches one small JSON document and uses a plain total timeout —
/// a check that cannot finish quickly should fail quickly.
pub fn stall_bounded_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("tt-spotify-bot/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(30))
        .build()
}
