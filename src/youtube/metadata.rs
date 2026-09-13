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
    /// Later pages of an account playlist are read signed in too.
    signed_in: bool,
}

/// YouTube Music's playlist of the signed-in account's liked songs.
const LIKED_MUSIC: &str = "LM";

/// A live YouTube Music radio station: the paginator YouTube handed back, plus
/// enough memory to know whether we are still inside it.
///
/// Held across top-ups so autoplay pages one station the way the YouTube Music
/// app does, instead of starting a fresh station from whatever happens to be
/// playing. Reseeding per batch drifts: measured from one seed, a station
/// reseeded from its own fifth track shared only 11 of 50 tracks with the
/// original.
pub struct YtRadioStation {
    paginator: rustypipe::model::paginator::Paginator<rustypipe::model::TrackItem>,
    /// Fetched but not yet handed out.
    buffer: std::collections::VecDeque<YouTubeTrack>,
    /// The seed, plus every id this station has produced. Used to tell "still
    /// playing our own station" from "the user has put on something else".
    known: std::collections::HashSet<String>,
    /// YouTube has no more pages. Stop asking.
    exhausted: bool,
}

impl YtRadioStation {
    /// Whether `video_id` is this station's seed or something it handed out.
    /// A seed from anywhere else means the user moved on and the station
    /// should be replaced rather than continued.
    pub fn covers(&self, video_id: &str) -> bool {
        self.known.contains(video_id)
    }
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
    /// Signed in with the cookies file, for the account's own library. Built on
    /// first use and never stored: rustypipe would write the cookie to its cache file.
    account: tokio::sync::OnceCell<Arc<RustyPipe>>,
    country: Option<rustypipe::param::Country>,
    language: Option<rustypipe::param::Language>,
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
        // The cache goes in the data root: the working directory may be unwritable under systemd.
        // YouTube Music ranks results by locale, so the configured one is applied.
        let mut builder = RustyPipe::builder()
            .no_botguard()
            .storage_dir(crate::paths::cache_dir());
        let country = parse_country(&config.youtube_country);
        let language = parse_language(&config.youtube_language);
        if let Some(c) = country {
            builder = builder.country(c);
        }
        if let Some(l) = language {
            builder = builder.lang(l);
        }
        let client = builder
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
                // Configs from before the layout migration still name <root>/cookies.txt.
                let default = default_cookies_path();
                tracing::warn!(
                    "YouTube: configured cookies file {} does not exist; using {} instead. Update youtubeCookiesFile in the config.",
                    configured.display(),
                    default.display()
                );
                default.to_string_lossy().into_owned()
            } else {
                tracing::warn!(
                    "YouTube: configured cookies file {} does not exist; tracks that need a sign-in will fail until it is restored or the setting is cleared",
                    configured.display()
                );
                String::new()
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
        if !cookies_file.is_empty() {
            match std::fs::read_to_string(&cookies_file) {
                Ok(text) if !crate::youtube::sidecar::cookies_have_sign_in(&text) => tracing::warn!(
                    "YouTube: {cookies_file} holds no YouTube sign-in, so tracks that need one will still fail. Export it from a signed-in browser."
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!("YouTube: cannot read cookies file {cookies_file}: {e}"),
            }
        }

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
            account: tokio::sync::OnceCell::new(),
            country,
            language,
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

    /// The account's YouTube Music liked songs, read signed in with the cookies file.
    pub async fn liked(&self) -> Result<YtResolved, BotError> {
        self.fetch_playlist_first_page(LIKED_MUSIC).await
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
                    self.search_top_track(query).await
                }
            },
            Some(YouTubeRef::Playlist(id)) => self.fetch_playlist(&id).await,
            Some(YouTubeRef::Album(id)) => self.fetch_album(&id).await,
            // A free-form search returns just the top hit so play_and_queue
            // doesn't accidentally enqueue 5 tracks for a single song name.
            None => self.search_top_track(query).await,
        }
    }

    async fn fetch_video(&self, video_id: &str) -> Result<YouTubeTrack, BotError> {
        let q = self.client.query();
        let details = retry_once(|| q.music_details(video_id))
            .await
            .map_err(|e| BotError::Playback(format!("YouTube video fetch failed: {e}")))?;
        Ok(track_item_to_track(details.track))
    }

    /// The client signed in with the cookies file, built on first use.
    async fn account(&self) -> Result<Arc<RustyPipe>, BotError> {
        self.account
            .get_or_try_init(|| async {
                let text = std::fs::read_to_string(&self.cookies_file).unwrap_or_default();
                if self.cookies_file.is_empty() || !crate::youtube::sidecar::cookies_have_sign_in(&text) {
                    return Err(BotError::YouTubeSignInMissing);
                }
                let mut builder = RustyPipe::builder().no_botguard().no_storage();
                if let Some(c) = self.country {
                    builder = builder.country(c);
                }
                if let Some(l) = self.language {
                    builder = builder.lang(l);
                }
                let client = builder
                    .build()
                    .map_err(|e| BotError::Playback(format!("rustypipe init failed: {e}")))?;
                client
                    .user_auth_set_cookie_txt(&text)
                    .await
                    .map_err(|e| BotError::YouTubeSignInRejected(e.to_string()))?;
                Ok(Arc::new(client))
            })
            .await
            .map(Arc::clone)
    }

    /// Liked Music belongs to the account, so it is read signed in.
    async fn playlist_query(&self, playlist_id: &str) -> Result<rustypipe::client::RustyPipeQuery, BotError> {
        if playlist_id == LIKED_MUSIC {
            Ok(self.account().await?.query().authenticated())
        } else {
            Ok(self.client.query())
        }
    }

    async fn fetch_playlist(&self, playlist_id: &str) -> Result<Vec<YouTubeTrack>, BotError> {
        let q = self.playlist_query(playlist_id).await?;
        let mut playlist = retry_once(|| q.music_playlist(playlist_id))
            .await
            .map_err(|e| BotError::Playback(format!("YouTube playlist fetch failed: {e}")))?;
        // Pull all pages, not just the first. A paging failure truncates the
        // list; say so instead of silently returning a partial playlist.
        if let Err(e) = playlist.tracks.extend_all(&q).await {
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
        let q = self.playlist_query(playlist_id).await?;
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
        let rest = paginator.ctoken.is_some().then_some(YtPlaylistRest {
            paginator,
            signed_in: playlist_id == LIKED_MUSIC,
        });
        Ok(YtResolved::PlaylistFirstPage { tracks, rest })
    }

    /// Fetch the next page of a partially-loaded playlist. `Ok(None)` when the
    /// playlist is exhausted.
    pub async fn fetch_more_playlist(
        &self,
        rest: &mut YtPlaylistRest,
    ) -> Result<Option<Vec<YouTubeTrack>>, BotError> {
        let q = if rest.signed_in {
            self.account().await?.query().authenticated()
        } else {
            self.client.query()
        };
        let more = rest.paginator.extend(q)
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
    ///
    /// Deliberately the songs shelf: this backs the numbered `search` list,
    /// where a tidy list of actual songs beats a better single top hit. The
    /// all-categories search ranks its first result better but its tail worse,
    /// mixing in remixes and live clips.
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

    /// The single best song for a query, for a bare play: the first YouTube
    /// Music song in the all-categories ranking, else the songs shelf.
    ///
    /// Uploaded videos in the ranking are passed over. YouTube returns
    /// different top results on repeated runs, so tests must not pin a song.
    pub async fn search_top_track(&self, query: &str) -> Result<Vec<YouTubeTrack>, BotError> {
        let q = self.client.query();
        match retry_once(|| q.music_search_main(query)).await {
            Ok(result) => {
                if let Some(corrected) = &result.corrected_query {
                    tracing::info!("YouTube: searched {corrected:?} instead of {query:?}");
                }
                match top_song(result.items.items) {
                    Some(t) => Ok(vec![track_item_to_track(t)]),
                    None => {
                        tracing::debug!("YouTube: no song among the top results for {query:?}; using the songs shelf");
                        self.search_tracks(query, 1).await
                    }
                }
            }
            Err(e) => {
                // Never fail the play over the better-ranked path being
                // unavailable; the songs shelf is still a usable answer.
                tracing::warn!("YouTube: top-result search failed ({e}); using the songs shelf");
                self.search_tracks(query, 1).await
            }
        }
    }

    /// Open a YouTube Music radio station for a track.
    ///
    /// `music_radio_track` is YouTube Music's autoplay, not a separate radio
    /// feature: it asks the same endpoint the app does, with automix on. The
    /// first page excludes the seed and starts at what plays next.
    ///
    /// `music_related` is deliberately not used - that is a browse shelf, it
    /// includes the seed track itself, and feeding it to a queue would replay
    /// the song that is currently playing.
    pub async fn start_radio(&self, video_id: &str) -> Result<YtRadioStation, BotError> {
        let q = self.client.query();
        let mut paginator = retry_once(|| q.music_radio_track(video_id))
            .await
            .map_err(|e| BotError::Playback(format!("YouTube radio fetch failed: {e}")))?;

        // Drain the first page out of the paginator: extend() appends, so
        // leaving it in place would hand the same tracks back on every later
        // page. The playlist loader takes the same precaution.
        let mut known = std::collections::HashSet::new();
        known.insert(video_id.to_string());
        let buffer: std::collections::VecDeque<YouTubeTrack> = std::mem::take(&mut paginator.items)
            .into_iter()
            .map(track_item_to_track)
            .collect();

        Ok(YtRadioStation { paginator, buffer, known, exhausted: false })
    }

    /// Take up to `limit` more tracks from a station, paging YouTube as needed.
    ///
    /// Returns fewer than `limit` - possibly none - once the station runs out.
    /// An empty result is the caller's cue to seed a new station rather than a
    /// failure.
    pub async fn next_radio_tracks(
        &self,
        station: &mut YtRadioStation,
        limit: usize,
        exclude: &[String],
    ) -> Result<Vec<YouTubeTrack>, BotError> {
        let mut out = Vec::with_capacity(limit);
        while out.len() < limit {
            while let Some(track) = station.buffer.pop_front() {
                let seen = station.known.contains(&track.id) || exclude.contains(&track.id);
                station.known.insert(track.id.clone());
                if !seen {
                    out.push(track);
                    if out.len() == limit {
                        return Ok(out);
                    }
                }
            }
            if station.exhausted {
                break;
            }
            // Buffer empty: ask for the next page. extend() appends to
            // `items`, which is empty here precisely because the previous page
            // was drained, so what lands is only the new tracks.
            let more = station
                .paginator
                .extend(self.client.query())
                .await
                .map_err(|e| BotError::Playback(format!("YouTube radio page fetch failed: {e}")))?;
            if !more {
                station.exhausted = true;
                break;
            }
            station.buffer = std::mem::take(&mut station.paginator.items)
                .into_iter()
                .map(track_item_to_track)
                .collect();
            if station.buffer.is_empty() {
                station.exhausted = true;
                break;
            }
        }
        Ok(out)
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

/// The configured location. Unset or unrecognised leaves the library default,
/// so a typo never stops the bot starting.
fn parse_country(code: &str) -> Option<rustypipe::param::Country> {
    let parsed = crate::youtube::locale::parse_country(code);
    if parsed.is_none() && !code.trim().is_empty() {
        tracing::warn!("YouTube: ignoring unrecognised youtubeCountry {code:?}");
    }
    parsed
}

/// As `parse_country`, for the language.
fn parse_language(code: &str) -> Option<rustypipe::param::Language> {
    let parsed = crate::youtube::locale::parse_language(code);
    if parsed.is_none() && !code.trim().is_empty() {
        tracing::warn!("YouTube: ignoring unrecognised youtubeLanguage {code:?}");
    }
    parsed
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

/// The first YouTube Music song in search results; artists, albums and
/// uploaded videos are skipped.
fn top_song(items: Vec<rustypipe::model::MusicItem>) -> Option<rustypipe::model::TrackItem> {
    items.into_iter().find_map(|item| match item {
        rustypipe::model::MusicItem::Track(t) if t.track_type == rustypipe::model::TrackType::Track => Some(t),
        _ => None,
    })
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
    #[ignore = "hits the network"]
    async fn radio_returns_a_queue_that_excludes_the_seed() {
        let meta = super::YouTubeMetadata::new(&crate::config::BotConfig::default()).unwrap();
        // A YouTube Music "- Topic" upload: the normal case for this bot.
        let seed = "5oWyMakvQew";
        let mut station = meta.start_radio(seed).await.expect("station");
        let tracks = meta.next_radio_tracks(&mut station, 5, &[]).await.expect("tracks");
        assert_eq!(tracks.len(), 5, "should honour the limit");
        assert!(
            !tracks.iter().any(|t| t.id == seed),
            "autoplay must not replay the track it was seeded from"
        );
    }

    #[tokio::test]
    #[ignore = "hits the network"]
    async fn a_station_keeps_handing_out_fresh_tracks() {
        // The failure this guards: extend() APPENDS to the paginator, so a
        // page that is not drained first comes back again and autoplay
        // re-queues the same songs. Pull more than one page's worth.
        let meta = super::YouTubeMetadata::new(&crate::config::BotConfig::default()).unwrap();
        let mut station = meta.start_radio("5oWyMakvQew").await.expect("station");

        let mut seen: Vec<String> = Vec::new();
        for round in 0..12 {
            let batch = meta.next_radio_tracks(&mut station, 5, &[]).await.expect("batch");
            if batch.is_empty() {
                break;
            }
            for t in batch {
                assert!(
                    !seen.contains(&t.id),
                    "round {round}: {} came back a second time",
                    t.name
                );
                seen.push(t.id);
            }
        }
        assert!(
            seen.len() > 50,
            "a paged station should outlast one page; got {}",
            seen.len()
        );
    }

    #[tokio::test]
    #[ignore = "hits the network"]
    async fn a_station_knows_which_tracks_are_its_own() {
        // This is what stops autoplay reseeding every batch: the station is
        // continued while the bot is still playing tracks it produced.
        let meta = super::YouTubeMetadata::new(&crate::config::BotConfig::default()).unwrap();
        let seed = "5oWyMakvQew";
        let mut station = meta.start_radio(seed).await.expect("station");
        let tracks = meta.next_radio_tracks(&mut station, 3, &[]).await.expect("tracks");

        assert!(station.covers(seed), "the seed itself belongs to the station");
        for t in &tracks {
            assert!(station.covers(&t.id), "{} should be recognised as ours", t.name);
        }
        assert!(
            !station.covers("dQw4w9WgXcQ"),
            "an unrelated track must not look like part of this station"
        );
    }

    #[tokio::test]
    #[ignore = "hits the network"]
    async fn radio_skips_tracks_already_queued() {
        let meta = super::YouTubeMetadata::new(&crate::config::BotConfig::default()).unwrap();
        let mut a = meta.start_radio("5oWyMakvQew").await.unwrap();
        let first = meta.next_radio_tracks(&mut a, 3, &[]).await.unwrap();
        let exclude: Vec<String> = first.iter().map(|t| t.id.clone()).collect();

        let mut b = meta.start_radio("5oWyMakvQew").await.unwrap();
        let second = meta.next_radio_tracks(&mut b, 3, &exclude).await.unwrap();
        assert!(
            !second.iter().any(|t| exclude.contains(&t.id)),
            "excluded ids came back anyway"
        );
    }

    #[tokio::test]
    #[ignore = "hits the network"]
    async fn a_bare_play_always_gets_exactly_one_playable_track() {
        // Deliberately asserts the shape, not which song. YouTube returns
        // different top results for the same query on repeated runs - measured
        // across countries and rounds, "perfect by edge" alternates between two
        // different songs regardless of locale - so pinning a title here would
        // be a coin flip in CI.
        let meta = super::YouTubeMetadata::new(&crate::config::BotConfig::default()).unwrap();
        // "relaxing piano music" leads with an uploaded video; a song must be played instead.
        for query in ["perfect by edge", "shape of yu", "konkani", "relaxing piano music"] {
            let top = meta.search_top_track(query).await.expect(query);
            assert_eq!(top.len(), 1, "{query}: a bare play must get one track");
            assert!(!top[0].id.is_empty(), "{query}: track has no id");
            assert!(!top[0].name.is_empty(), "{query}: track has no name");
            let details = meta.client.query().music_details(&top[0].id).await.expect("details");
            assert_eq!(
                details.track.track_type,
                rustypipe::model::TrackType::Track,
                "{query}: {} is an uploaded video, not a YouTube Music song",
                top[0].name
            );
        }
    }

    #[tokio::test]
    #[ignore = "hits the network"]
    async fn a_query_whose_top_hit_is_an_artist_still_yields_a_track() {
        // "konkani" returns an Artist first from the all-categories search,
        // which is not playable; the songs shelf has to catch it.
        let meta = super::YouTubeMetadata::new(&crate::config::BotConfig::default()).unwrap();
        let top = meta.search_top_track("konkani").await.expect("top");
        assert_eq!(top.len(), 1, "a bare play must always get exactly one track");
        assert!(!top[0].id.is_empty());
    }

    #[tokio::test]
    #[ignore = "hits the network"]
    async fn an_age_restricted_video_is_found_without_a_sign_in() {
        // Only playback falls back to the cookies file, so the track itself must resolve.
        let meta = super::YouTubeMetadata::new(&crate::config::BotConfig::default()).unwrap();
        for id in ["HtVdAasjOgU", "Tq92D6wQ1mg"] {
            let track = meta.fetch_video(id).await.unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(track.id, id);
        }
    }

    #[tokio::test]
    async fn liked_music_without_a_signed_in_cookies_file_asks_for_one() {
        use crate::error::BotError;
        let mut meta = super::YouTubeMetadata::new(&crate::config::BotConfig::default()).unwrap();
        meta.cookies_file = String::new();
        let result = meta.liked().await;
        assert!(matches!(result, Err(BotError::YouTubeSignInMissing)));

        let signed_out = std::env::temp_dir().join(format!("liked_signed_out_{}.txt", std::process::id()));
        std::fs::write(&signed_out, ".youtube.com\tTRUE\t/\tTRUE\t0\tYSC\tx\n").unwrap();
        let mut meta = super::YouTubeMetadata::new(&crate::config::BotConfig::default()).unwrap();
        meta.cookies_file = signed_out.to_string_lossy().into_owned();
        let result = meta.liked().await;
        let _ = std::fs::remove_file(&signed_out);
        assert!(matches!(result, Err(BotError::YouTubeSignInMissing)));
    }

    #[tokio::test]
    #[ignore = "hits the network and needs TTSPOTIFY_TEST_COOKIES"]
    async fn liked_music_loads_signed_in_and_search_stays_anonymous() {
        let Ok(path) = std::env::var("TTSPOTIFY_TEST_COOKIES") else {
            println!("skipped: no TTSPOTIFY_TEST_COOKIES");
            return;
        };
        let mut meta = super::YouTubeMetadata::new(&crate::config::BotConfig::default()).unwrap();
        meta.cookies_file = path;
        match meta.liked().await.expect("liked music") {
            super::YtResolved::PlaylistFirstPage { tracks, rest } => {
                assert!(!tracks.is_empty());
                assert!(tracks.iter().all(|t| !t.id.is_empty()));
                if let Some(mut rest) = rest {
                    assert!(rest.signed_in);
                    meta.fetch_more_playlist(&mut rest).await.expect("next page");
                }
            }
            super::YtResolved::Tracks(_) => panic!("liked music is a playlist"),
        }
        let top = meta.search_top_track("imagine dragons believer").await.expect("search");
        assert_eq!(top.len(), 1);
    }

    #[tokio::test]
    #[ignore = "hits the network"]
    async fn a_typo_still_finds_the_song() {
        let meta = super::YouTubeMetadata::new(&crate::config::BotConfig::default()).unwrap();
        let top = meta.search_top_track("believr imagine dragon").await.expect("top");
        assert!(
            top[0].name.to_lowercase().contains("believer"),
            "got {} - {}",
            top[0].artists.join(", "),
            top[0].name
        );
    }

    #[test]
    fn locale_codes_are_parsed_case_insensitively() {
        use rustypipe::param::{Country, Language};
        assert_eq!(super::parse_country("in"), Some(Country::In));
        assert_eq!(super::parse_country("IN"), Some(Country::In));
        assert_eq!(super::parse_language("EN"), Some(Language::En));
    }

    #[test]
    fn an_unset_locale_leaves_the_library_default_alone() {
        assert_eq!(super::parse_country(""), None);
        assert_eq!(super::parse_country("   "), None);
        assert_eq!(super::parse_language(""), None);
    }

    #[test]
    fn a_typo_is_ignored_rather_than_fatal() {
        // A bad code in a config file must not stop the bot starting.
        assert_eq!(super::parse_country("XX"), None);
        assert_eq!(super::parse_country("not a country"), None);
        assert_eq!(super::parse_language("zzz"), None);
    }

    /// A YouTube Music search item as rustypipe deserializes it.
    fn item(id: &str, track_type: &str) -> rustypipe::model::MusicItem {
        serde_json::from_value(serde_json::json!({
            "Track": {
                "id": id,
                "name": format!("name of {id}"),
                "duration": 200,
                "cover": [],
                "artists": [{ "id": null, "name": "someone" }],
                "artist_id": null,
                "album": null,
                "view_count": null,
                "track_type": track_type,
                "track_nr": null,
                "by_va": false,
                "unavailable": false
            }
        }))
        .expect("a valid search item")
    }

    #[test]
    fn a_bare_play_takes_the_first_song_not_an_uploaded_video() {
        let items = vec![item("video1", "video"), item("song1", "track"), item("song2", "track")];
        assert_eq!(super::top_song(items).map(|t| t.id), Some("song1".to_string()));
    }

    #[test]
    fn results_holding_only_videos_give_no_song() {
        let items = vec![item("video1", "video"), item("episode1", "episode")];
        assert!(super::top_song(items).is_none(), "the caller falls back to the songs shelf");
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
