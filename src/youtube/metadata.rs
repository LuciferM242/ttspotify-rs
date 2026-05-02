use std::sync::Arc;

use rustypipe::client::RustyPipe;

use std::path::PathBuf;

use crate::config::BotConfig;
use crate::error::BotError;
use crate::youtube::setup::{default_cookies_path, resolve_paths, which, YoutubeSetupPaths};
use crate::youtube::types::YouTubeTrack;

/// YouTube Music metadata service.
///
/// Search and track metadata go through rustypipe (fast, native).
/// Stream URL resolution goes through `yt-dlp` because rustypipe's
/// signature deobfuscator can't keep up with YouTube's player JS
/// changes.
pub struct YouTubeMetadata {
    client: Arc<RustyPipe>,
    /// Path passed to `yt-dlp --cookies <file>`. Empty = don't pass.
    /// Resolved at init: explicit config override → falls back to the
    /// default `<config_dir>/cookies.txt` if it exists → empty.
    cookies_file: String,
    /// Resolved paths for the bundled binaries + plugin dir.
    /// `Some` if the bot can find them; `None` falls back to PATH.
    bundle: Option<YoutubeSetupPaths>,
    /// Resolved yt-dlp executable path. PATH lookup happens once at
    /// construction; falls back to the bundled binary or the bare name.
    yt_dlp_exe: PathBuf,
}

impl YouTubeMetadata {
    pub fn new(config: &BotConfig) -> Result<Self, BotError> {
        let client = RustyPipe::builder()
            .no_botguard()
            .build()
            .map_err(|e| BotError::Playback(format!("rustypipe init failed: {e}")))?;
        // Resolve bundled paths but don't require them — falling back to PATH
        // keeps the manual-install path working.
        let bundle = resolve_paths().ok().filter(|p| p.yt_dlp.is_file());

        // Cookies: explicit override wins; otherwise look for the default path.
        let cookies_file = if !config.youtube_cookies_file.is_empty() {
            config.youtube_cookies_file.clone()
        } else {
            let default = default_cookies_path();
            if default.is_file() {
                tracing::info!("YouTube: auto-loaded cookies from {}", default.display());
                default.to_string_lossy().into_owned()
            } else {
                String::new()
            }
        };

        // Resolve yt-dlp once: PATH first, then the bundled copy under
        // <exe-dir>/lib, then a bare `yt-dlp` (NotFound at spawn time).
        let yt_dlp_exe = which("yt-dlp")
            .or_else(|| bundle.as_ref().map(|b| b.yt_dlp.clone()))
            .unwrap_or_else(|| PathBuf::from("yt-dlp"));

        Ok(Self {
            client: Arc::new(client),
            cookies_file,
            bundle,
            yt_dlp_exe,
        })
    }

    /// Search YouTube Music for tracks matching the query.
    /// Returns up to `limit` results (sliced from the first page).
    pub async fn search_tracks(&self, query: &str, limit: u8) -> Result<Vec<YouTubeTrack>, BotError> {
        let result = self.client.query()
            .music_search_tracks(query)
            .await
            .map_err(|e| BotError::Playback(format!("YouTube search failed: {e}")))?;

        let tracks: Vec<YouTubeTrack> = result.items.items
            .into_iter()
            .take(limit as usize)
            .map(track_item_to_track)
            .collect();

        if tracks.is_empty() {
            Err(BotError::NoResults)
        } else {
            Ok(tracks)
        }
    }

    /// Spawn yt-dlp as a child process that streams M4A audio bytes to its
    /// stdout. The caller owns the `Child` — drop or kill it to stop the
    /// download (and free the pipe). yt-dlp handles all of YouTube's
    /// header/cookie/fragment requirements.
    pub fn spawn_ytdlp(&self, video_id: &str) -> Result<std::process::Child, BotError> {
        use std::process::{Command, Stdio};
        let url = format!("https://www.youtube.com/watch?v={video_id}");

        let mut cmd = Command::new(&self.yt_dlp_exe);
        cmd.args([
            "--no-warnings",
            "--no-playlist",
            "-f", "bestaudio[ext=m4a]/bestaudio",
            "-o", "-",
        ]);

        // Wire the bgutil-pot plugin and binary if bundled.
        if let Some(b) = &self.bundle {
            if b.plugin_dir.is_dir() {
                cmd.arg("--plugin-dirs");
                cmd.arg(&b.plugin_dir);
            }
            if b.bgutil_pot.is_file() {
                cmd.arg("--extractor-args");
                cmd.arg(format!(
                    "youtubepot-bgutilscript:script_path={}",
                    b.bgutil_pot.display()
                ));
            }
        }

        // Cookies (optional, helps with rate-limited / age-restricted videos).
        if !self.cookies_file.is_empty() {
            cmd.arg("--cookies");
            cmd.arg(&self.cookies_file);
        }

        cmd.arg("--").arg(&url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => BotError::Playback(
                    "yt-dlp not found. Run: tt-spotify-bot --setup-youtube".to_string()
                ),
                _ => BotError::Playback(format!("yt-dlp spawn: {e}")),
            })
    }
}

fn track_item_to_track(item: rustypipe::model::TrackItem) -> YouTubeTrack {
    YouTubeTrack {
        id: item.id,
        name: item.name,
        artists: item.artists.into_iter().map(|a| a.name).collect(),
        album: item.album.map(|a| a.name).unwrap_or_default(),
        duration_ms: item.duration.unwrap_or(0).saturating_mul(1000),
    }
}
