use std::sync::Arc;

use rustypipe::client::RustyPipe;

use std::path::PathBuf;

use crate::config::BotConfig;
use crate::error::BotError;
use crate::youtube::setup::{default_cookies_path, resolve_paths, which, YoutubeSetupPaths};
use crate::youtube::types::{parse_youtube_ref, YouTubeRef, YouTubeTrack};

/// Opaque continuation for a playlist whose first page has been returned but
/// whose remaining pages are still on YouTube's side. Fed back into
/// `fetch_more_playlist` by the background loader.
pub struct YtPlaylistRest {
    paginator: rustypipe::model::paginator::Paginator<rustypipe::model::TrackItem>,
}

/// Result of `resolve_paged`.
pub enum YtResolved {
    /// Fully resolved (single track, album, search hit).
    Tracks(Vec<YouTubeTrack>),
    /// First playlist page; `rest` is Some when more pages exist.
    PlaylistFirstPage {
        tracks: Vec<YouTubeTrack>,
        rest: Option<YtPlaylistRest>,
    },
}

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

/// The client tried first: fastest, because it needs no PO token and so skips
/// that round trip. About 1.9s to the first audio byte against tv_simply's 5s.
///
/// Needing no token is also its weakness. yt-dlp's PO Token Guide warns that
/// requests without one "may return HTTP Error 403, or result in your account
/// or IP address being blocked", and this is the client YouTube refuses first
/// when it distrusts an address - which a datacenter IP is more likely to be.
pub const PRIMARY_CLIENT: &str = "android_vr";

/// The client tried when the primary is refused: slower, and backed by a PO
/// token, which is what makes it hold up when the fast one is being turned
/// away. Measured 6/6 where android_vr managed 2/6 on the same addresses.
///
/// It only became usable once the PO token provider was wired up correctly;
/// before that it failed with "Requested format is not available", having been
/// given no token to present.
pub const FALLBACK_CLIENT: &str = "tv_simply";

impl YouTubeMetadata {
    pub fn new(config: &BotConfig) -> Result<Self, BotError> {
        // Keep rustypipe's cache (rustypipe_cache.json) in the config dir.
        // The default is the process working directory, which under systemd
        // may be unwritable (silently losing the cache) and during development
        // litters the repo root.
        let client = RustyPipe::builder()
            .no_botguard()
            .storage_dir(crate::paths::cache_dir())
            .build()
            .map_err(|e| BotError::Playback(format!("rustypipe init failed: {e}")))?;
        // Resolve bundled paths but don't require them — falling back to PATH
        // keeps the manual-install path working.
        let bundle = resolve_paths().ok().filter(|p| p.yt_dlp.is_file());

        // Cookies: explicit override wins; otherwise look for the default path.
        let cookies_file = if !config.youtube_cookies_file.is_empty() {
            let configured = PathBuf::from(&config.youtube_cookies_file);
            if configured.is_file() {
                config.youtube_cookies_file.clone()
            } else if default_cookies_path().is_file() {
                // The layout migration moved <root>/cookies.txt into config/
                // but configs holding the old absolute path were not rewritten;
                // without this rescue every yt-dlp spawn died on "unable to
                // open cookie file" with nothing tying it to the move.
                let default = default_cookies_path();
                tracing::warn!(
                    "YouTube: configured cookies file {} does not exist; using {} instead. Update youtubeCookiesFile in the config.",
                    configured.display(),
                    default.display()
                );
                default.to_string_lossy().into_owned()
            } else {
                // No fallback available: keep the configured path so yt-dlp's
                // own loud "unable to open cookie file" error still surfaces a
                // genuine typo, but say up front why playback is about to fail.
                tracing::warn!(
                    "YouTube: configured cookies file {} does not exist; yt-dlp will refuse to start until the file is restored or the setting is cleared",
                    configured.display()
                );
                config.youtube_cookies_file.clone()
            }
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

    /// Like `resolve`, but playlists return only their first page plus a
    /// continuation so the caller can start playback immediately and pull the
    /// remaining pages in the background (mirrors Spotify bulk loading).
    pub async fn resolve_paged(&self, query: &str, search_limit: u8) -> Result<YtResolved, BotError> {
        if let Some(YouTubeRef::Playlist(id)) = parse_youtube_ref(query) {
            return self.fetch_playlist_first_page(&id).await;
        }
        self.resolve(query, search_limit).await.map(YtResolved::Tracks)
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
        let q = self.client.query();
        let details = retry_once(|| q.music_details(video_id))
            .await
            .map_err(|e| BotError::Playback(format!("YouTube video fetch failed: {e}")))?;
        Ok(track_item_to_track(details.track))
    }

    async fn fetch_playlist(&self, playlist_id: &str) -> Result<Vec<YouTubeTrack>, BotError> {
        let q = self.client.query();
        let mut playlist = retry_once(|| q.music_playlist(playlist_id))
            .await
            .map_err(|e| BotError::Playback(format!("YouTube playlist fetch failed: {e}")))?;
        // Pull all pages, not just the first. A paging failure truncates the
        // list; say so instead of silently returning a partial playlist.
        if let Err(e) = playlist.tracks.extend_all(&self.client.query()).await {
            tracing::warn!("YouTube playlist only partially loaded: {e}");
        }
        let tracks: Vec<YouTubeTrack> = playlist.tracks.items.into_iter().map(track_item_to_track).collect();
        if tracks.is_empty() {
            Err(BotError::NoResults)
        } else {
            Ok(tracks)
        }
    }

    /// First page of a playlist plus a continuation for background loading.
    async fn fetch_playlist_first_page(&self, playlist_id: &str) -> Result<YtResolved, BotError> {
        let q = self.client.query();
        let playlist = retry_once(|| q.music_playlist(playlist_id))
            .await
            .map_err(|e| BotError::Playback(format!("YouTube playlist fetch failed: {e}")))?;
        let mut paginator = playlist.tracks;
        // Drain the page out of the paginator: each later extend() appends
        // only the next page, so fetch_more_playlist can drain again and get
        // exactly the new tracks.
        let tracks: Vec<YouTubeTrack> = std::mem::take(&mut paginator.items)
            .into_iter()
            .map(track_item_to_track)
            .collect();
        if tracks.is_empty() {
            return Err(BotError::NoResults);
        }
        let rest = paginator.ctoken.is_some().then_some(YtPlaylistRest { paginator });
        Ok(YtResolved::PlaylistFirstPage { tracks, rest })
    }

    /// Fetch the next page of a partially-loaded playlist. `Ok(None)` when the
    /// playlist is exhausted.
    pub async fn fetch_more_playlist(
        &self,
        rest: &mut YtPlaylistRest,
    ) -> Result<Option<Vec<YouTubeTrack>>, BotError> {
        let more = rest.paginator.extend(self.client.query())
            .await
            .map_err(|e| BotError::Playback(format!("YouTube playlist page fetch failed: {e}")))?;
        if !more {
            return Ok(None);
        }
        let tracks: Vec<YouTubeTrack> = std::mem::take(&mut rest.paginator.items)
            .into_iter()
            .map(track_item_to_track)
            .collect();
        Ok(Some(tracks))
    }

    async fn fetch_album(&self, album_id: &str) -> Result<Vec<YouTubeTrack>, BotError> {
        let q = self.client.query();
        let album = retry_once(|| q.music_album(album_id))
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
        let q = self.client.query();
        let result = retry_once(|| q.music_search_tracks(query))
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
        self.spawn_ytdlp_with_client(video_id, PRIMARY_CLIENT)
    }

    /// As `spawn_ytdlp`, asking YouTube through a named player client.
    ///
    /// See [`PRIMARY_CLIENT`] and [`FALLBACK_CLIENT`] for why there are two.
    pub fn spawn_ytdlp_with_client(
        &self,
        video_id: &str,
        client: &str,
    ) -> Result<std::process::Child, BotError> {
        use std::process::{Command, Stdio};
        let url = format!("https://www.youtube.com/watch?v={video_id}");

        let mut cmd = Command::new(&self.yt_dlp_exe);
        cmd.args([
            "--no-warnings",
            "--no-playlist",
            "-f", "bestaudio[ext=m4a]/bestaudio",
            "-o", "-",
        ]);

        // Ask one client rather than letting yt-dlp poll several. Left to
        // itself it queries the tv and android players and merges the formats,
        // which is most of the wait before a track starts.
        //
        // Deliberately no `player_skip`. It is the usual advice for speed and
        // it does not work here: skipping the webpage made YouTube answer
        // "Sign in to confirm you're not a bot" whether or not a PO token was
        // present.
        cmd.arg("--extractor-args");
        cmd.arg(format!("youtube:player_client={client}"));

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
                // `bgutilcli:cli_path`, not `bgutilscript:script_path`. Those
                // are two different providers: the script one runs a .js file
                // under Node, the CLI one runs the bgutil-pot binary we ship.
                // Naming the wrong one left the CLI provider with no path, so
                // it looked for a bare `bgutil-pot` on PATH, found nothing, and
                // silently carried on without a PO token - which is what YouTube
                // answers with 403.
                //
                // Measured over eight videos: 5 played with the wrong name,
                // 7 with the right one.
                cmd.arg("--extractor-args");
                cmd.arg(format!(
                    "youtubepot-bgutilcli:cli_path={}",
                    b.bgutil_pot.display()
                ));
            }
            // Since yt-dlp 2025.11.12, YouTube's player challenges are solved
            // by running its JavaScript, and without a runtime some formats
            // are unavailable. A Deno on PATH is found by yt-dlp itself; ours
            // has to be pointed at.
            if let crate::youtube::setup::JsRuntime::Bundled(deno) =
                crate::youtube::setup::find_js_runtime(b)
            {
                cmd.arg("--js-runtimes");
                cmd.arg(format!("deno:{}", deno.display()));
            }
        }

        // Cookies (optional, helps with rate-limited / age-restricted videos).
        if !self.cookies_file.is_empty() {
            cmd.arg("--cookies");
            cmd.arg(&self.cookies_file);
        }

        // Covers yt-dlp itself and no further: the flag denies yt-dlp a
        // console, so anything yt-dlp starts is given a fresh one by Windows —
        // which is why deno opens a black window titled with its own path
        // while a track is playing.
        //
        // Not fixable from here. yt-dlp offers no say over how it spawns a
        // runtime, and deno has no flag for it; the runtimes yt-dlp supports
        // (deno, node, quickjs, bun) are all console programs. Telling yt-dlp
        // to skip the runtime does stop the window, and costs far too much:
        // alternating both settings over three rounds, android_vr played 8/9
        // tracks with a runtime and 3/9 without, failing with 403. YouTube's
        // "n" parameter is solved by that JavaScript, and an unsolved one is
        // refused.
        //
        // The only remaining lever is giving this process a console of its own
        // and hiding it, so the whole tree inherits it.
        crate::proc::hide_console_window(&mut cmd);

        cmd.arg("--").arg(&url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => BotError::Playback(format!(
                    "yt-dlp not found. Run: {} --setup-yt",
                    crate::paths::program_name()
                )),
                _ => BotError::Playback(format!("yt-dlp spawn: {e}")),
            })
    }
}

/// Run a rustypipe query, retrying once on error.
///
/// Every rustypipe request is backed by a visitor-data fetch that can fail
/// transiently: a cold cache scrapes `music.youtube.com` for a token, and that
/// scrape sometimes comes back empty (consent wall, a changed page, a
/// momentarily bot-flagged IP). A second attempt fetches fresh visitor data and
/// usually succeeds, so one cheap retry turns most of those one-off failures
/// into a normal result instead of an error surfaced to the user. `op` is
/// re-invoked from scratch on retry so it issues a brand-new request.
async fn retry_once<T, E, F, Fut>(mut op: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    match op().await {
        Ok(v) => Ok(v),
        Err(first) => {
            tracing::debug!("YouTube query failed ({first}); retrying once");
            op().await
        }
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

#[cfg(test)]
mod tests {
    use super::retry_once;
    use std::cell::Cell;

    #[tokio::test]
    async fn retry_once_returns_first_success_without_retrying() {
        let calls = Cell::new(0u32);
        let res: Result<u32, &str> = retry_once(|| {
            calls.set(calls.get() + 1);
            async { Ok(42) }
        })
        .await;
        assert_eq!(res, Ok(42));
        assert_eq!(calls.get(), 1, "should not retry after a first-try success");
    }

    #[tokio::test]
    async fn retry_once_recovers_on_the_second_attempt() {
        let calls = Cell::new(0u32);
        let res: Result<u32, &str> = retry_once(|| {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move {
                if n < 2 {
                    Err("transient")
                } else {
                    Ok(7)
                }
            }
        })
        .await;
        assert_eq!(res, Ok(7));
        assert_eq!(calls.get(), 2, "should have retried exactly once");
    }

    #[tokio::test]
    async fn retry_once_gives_up_after_two_failures() {
        let calls = Cell::new(0u32);
        let res: Result<u32, &str> = retry_once(|| {
            calls.set(calls.get() + 1);
            async { Err("still broken") }
        })
        .await;
        assert_eq!(res, Err("still broken"));
        assert_eq!(calls.get(), 2, "should attempt exactly twice, no more");
    }
}
