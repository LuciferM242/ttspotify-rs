use std::sync::Arc;

use rustypipe::client::RustyPipe;

use std::path::PathBuf;

use crate::config::BotConfig;
use crate::error::BotError;
use crate::youtube::setup::{default_cookies_path, resolve_paths, which};
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
/// Audio fetching goes through the Deno sidecar (see `sidecar.rs`), because
/// rustypipe's signature deobfuscator can't keep up with YouTube's player JS
/// changes.
pub struct YouTubeMetadata {
    client: Arc<RustyPipe>,
    /// Cookies file handed to the sidecar. Empty = don't pass one.
    /// Resolved at init: explicit config override → falls back to the
    /// default `<config_dir>/cookies.txt` if it exists → empty.
    cookies_file: String,
    /// Resolved Deno executable. The bundled copy wins so `youtube update`
    /// stays in control; a new enough Deno already on PATH is used as-is
    /// rather than downloading a second one.
    deno_exe: PathBuf,
    /// Where the sidecar script, its import map and its lockfile are written.
    lib_dir: PathBuf,
}

/// `resolve_paths` looks beside the running executable; a test binary runs
/// from `target/<profile>/deps`, one level below the real `lib/`.
#[cfg(test)]
pub fn find_bundled_tools() -> Option<crate::youtube::setup::YoutubeSetupPaths> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?;
    for _ in 0..3 {
        let lib_dir = dir.join("lib");
        let deno = lib_dir.join(if cfg!(windows) { "deno.exe" } else { "deno" });
        if deno.is_file() {
            return Some(crate::youtube::setup::YoutubeSetupPaths { deno, lib_dir });
        }
        dir = dir.parent()?;
    }
    None
}

/// A metadata client for tests that really fetch: the bundled tools when they
/// are there, otherwise a Deno on PATH. `None` when there is no Deno at all.
#[cfg(test)]
pub fn for_tests() -> Option<YouTubeMetadata> {
    let mut meta = YouTubeMetadata::new(&crate::config::BotConfig::default()).ok()?;
    if let Some(bundle) = find_bundled_tools() {
        meta.deno_exe = bundle.deno.clone();
        meta.lib_dir = bundle.lib_dir.clone();
    } else if which(if cfg!(windows) { "deno.exe" } else { "deno" }).is_none() {
        return None;
    }
    Some(meta)
}

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
        // Not filtered on any tool being present: this only answers "where do
        // the tools live", and the answer must not change because one of them
        // has not been installed yet.
        let bundle = resolve_paths().ok();

        // Cookies: explicit override wins; otherwise look for the default path.
        let cookies_file = if !config.youtube_cookies_file.is_empty() {
            let configured = PathBuf::from(&config.youtube_cookies_file);
            if configured.is_file() {
                config.youtube_cookies_file.clone()
            } else if default_cookies_path().is_file() {
                // The layout migration moved <root>/cookies.txt into config/
                // but configs holding the old absolute path were not rewritten;
                // without this rescue every spawn died on "unable to open
                // cookie file" with nothing tying it to the move.
                let default = default_cookies_path();
                tracing::warn!(
                    "YouTube: configured cookies file {} does not exist; using {} instead. Update youtubeCookiesFile in the config.",
                    configured.display(),
                    default.display()
                );
                default.to_string_lossy().into_owned()
            } else {
                // No fallback available: keep the configured path so the
                // failure still names a genuine typo, but say up front why
                // playback is about to fail.
                tracing::warn!(
                    "YouTube: configured cookies file {} does not exist; playback will fail until the file is restored or the setting is cleared",
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

        // Resolve the runtime once. The bundled copy wins so `youtube update`
        // stays in control of it; otherwise a Deno the user already installed
        // is used rather than downloading a second one, and a bare name is the
        // last resort so a missing runtime is a NotFound at spawn time with a
        // message that names it.
        let lib_dir = bundle
            .as_ref()
            .map(|b| b.lib_dir.clone())
            .unwrap_or_else(|| PathBuf::from("lib"));
        let deno_exe = bundle
            .as_ref()
            .map(|b| b.deno.clone())
            .filter(|p| p.is_file())
            .or_else(|| which(if cfg!(windows) { "deno.exe" } else { "deno" }))
            .unwrap_or_else(|| PathBuf::from("deno"));

        Ok(Self {
            client: Arc::new(client),
            cookies_file,
            deno_exe,
            lib_dir,
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

    /// Spawn the sidecar, which prints where the track's audio is: one JSON
    /// line read with `sidecar::parse_stream_info`.
    ///
    /// The caller owns the `Child` - kill it to give up and free the pipes. The
    /// sidecar walks its own list of InnerTube clients against a session it has
    /// already built, so a client that cannot serve the track costs
    /// milliseconds rather than another process spawn.
    pub fn spawn_sidecar(&self, video_id: &str) -> Result<std::process::Child, BotError> {
        let script = crate::youtube::sidecar::ensure_script(&self.lib_dir)?;
        crate::youtube::sidecar::spawn(&script, &self.deno_exe, video_id, &self.cookies_file)
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

    /// The sidecar finds a gated "- Topic" track. Run by hand when playback
    /// breaks: these uploads are what YouTube Music search returns, and they
    /// are the first thing to fail when a client is retired.
    #[test]
    #[ignore = "hits the network and needs Deno installed"]
    fn sidecar_finds_a_topic_track() {
        let Some(meta) = super::for_tests() else {
            println!("skipped: no Deno found; run the YouTube install first");
            return;
        };
        let child = match meta.spawn_sidecar("5oWyMakvQew") {
            Ok(c) => c,
            Err(e) => panic!("could not spawn the sidecar: {e}"),
        };
        let out = child.wait_with_output().expect("wait for the sidecar");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let info = crate::youtube::sidecar::parse_stream_info(&String::from_utf8_lossy(&out.stdout))
            .unwrap_or_else(|e| {
                panic!(
                    "no stream info ({e}); YouTube has moved again. It said: {}",
                    crate::youtube::sidecar::complaint(&stderr)
                )
            });
        println!("{} serves {} bytes", info.client, info.content_length);
        assert!(info.content_length > 1_000_000, "a whole song is more than a megabyte");
    }

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
