use std::sync::Arc;

use rustypipe::client::RustyPipe;

use std::path::PathBuf;

use crate::config::BotConfig;
use crate::error::BotError;
use crate::youtube::setup::{default_cookies_path, resolve_paths, which, YoutubeSetupPaths};
use crate::youtube::types::{parse_youtube_ref, YouTubeRef, YouTubeTrack};

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
        // Keep rustypipe's cache (rustypipe_cache.json) in the config dir.
        // The default is the process working directory, which under systemd
        // may be unwritable (silently losing the cache) and during development
        // litters the repo root.
        let client = RustyPipe::builder()
            .no_botguard()
            .storage_dir(crate::config::config_dir())
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

        // Resolve yt-dlp once: prefer the bundled copy under <exe-dir>/lib since
        // its version is paired with the bundled bgutil plugin and kept current
        // by --update-tools. Fall back to a PATH install, then a bare `yt-dlp`
        // (NotFound at spawn time). A stale PATH yt-dlp otherwise wins and 403s
        // on YouTube's current PO-token requirements.
        let yt_dlp_exe = bundle.as_ref().map(|b| b.yt_dlp.clone())
            .or_else(|| which("yt-dlp"))
            .unwrap_or_else(|| PathBuf::from("yt-dlp"));

        Ok(Self {
            client: Arc::new(client),
            cookies_file,
            bundle,
            yt_dlp_exe,
        })
    }

    /// Resolve a YouTube URL/ID/playlist/album/search query into a list of
    /// tracks. URLs and bare IDs become single-track or playlist/album
    /// fetches; anything else falls back to the top match for the search.
    pub async fn resolve(&self, query: &str, _search_limit: u8) -> Result<Vec<YouTubeTrack>, BotError> {
        match parse_youtube_ref(query) {
            Some(YouTubeRef::Video(id)) => self.fetch_video(&id).await.map(|t| vec![t]),
            // A bare 11-char token is probably an ID but might be an
            // 11-letter search word; if the ID lookup fails, search instead
            // of surfacing "video fetch failed" for a legitimate query.
            Some(YouTubeRef::BareVideo(id)) => match self.fetch_video(&id).await {
                Ok(t) => Ok(vec![t]),
                Err(e) => {
                    tracing::debug!("Bare token '{id}' is not a video id ({e}); searching instead");
                    self.search_tracks(query, 1).await
                }
            },
            Some(YouTubeRef::Playlist(id)) => self.fetch_playlist(&id).await,
            Some(YouTubeRef::Album(id)) => self.fetch_album(&id).await,
            // A free-form search returns just the top hit so play_and_queue
            // doesn't accidentally enqueue 5 tracks for a single song name.
            None => self.search_tracks(query, 1).await,
        }
    }

    async fn fetch_video(&self, video_id: &str) -> Result<YouTubeTrack, BotError> {
        let details = self.client.query()
            .music_details(video_id)
            .await
            .map_err(|e| BotError::Playback(format!("YouTube video fetch failed: {e}")))?;
        Ok(track_item_to_track(details.track))
    }

    async fn fetch_playlist(&self, playlist_id: &str) -> Result<Vec<YouTubeTrack>, BotError> {
        let mut playlist = self.client.query()
            .music_playlist(playlist_id)
            .await
            .map_err(|e| BotError::Playback(format!("YouTube playlist fetch failed: {e}")))?;
        // Pull all pages, not just the first.
        let _ = playlist.tracks.extend_all(&self.client.query()).await;
        let tracks: Vec<YouTubeTrack> = playlist.tracks.items.into_iter().map(track_item_to_track).collect();
        if tracks.is_empty() {
            Err(BotError::NoResults)
        } else {
            Ok(tracks)
        }
    }

    async fn fetch_album(&self, album_id: &str) -> Result<Vec<YouTubeTrack>, BotError> {
        let album = self.client.query()
            .music_album(album_id)
            .await
            .map_err(|e| BotError::Playback(format!("YouTube album fetch failed: {e}")))?;
        let tracks: Vec<YouTubeTrack> = album.tracks.into_iter().map(track_item_to_track).collect();
        if tracks.is_empty() {
            Err(BotError::NoResults)
        } else {
            Ok(tracks)
        }
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
                // yt-dlp searches <plugin-dir>/*/yt_dlp_plugins, one level down,
                // so point it at lib_dir (which contains the yt-dlp-plugins
                // package), not at the package dir itself.
                cmd.arg("--plugin-dirs");
                cmd.arg(&b.lib_dir);
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

        // The tray is a GUI process with no console, so a child console app
        // flashes a command window on each spawn. CREATE_NO_WINDOW suppresses it
        // for yt-dlp and the bgutil-pot child it launches.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        cmd.arg("--").arg(&url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => BotError::Playback(
                    "yt-dlp not found. Run: tt-spotify-bot --setup-yt".to_string()
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
