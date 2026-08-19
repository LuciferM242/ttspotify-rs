//! Reporting and clearing the downloaded-audio caches.

use std::path::{Path, PathBuf};

use crate::error::BotError;

/// Directories under `cache/` holding nothing but re-downloadable audio.
const CLEARABLE: [&str; 2] = ["spotify_cache/audio", "youtube"];

/// Never cleared, wherever it turns up.
const PROTECTED: &str = "TEAMTALK_DLL";

/// True if `path` names the SDK at any level.
fn is_protected(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_string_lossy() == PROTECTED)
}

/// Absolute paths of the caches that may be cleared, whether or not they exist.
pub fn clearable_dirs() -> Vec<PathBuf> {
    clearable_dirs_under(&crate::paths::cache_dir())
}

fn clearable_dirs_under(cache_dir: &Path) -> Vec<PathBuf> {
    CLEARABLE
        .iter()
        .map(|rel| rel.split('/').fold(cache_dir.to_path_buf(), |p, part| p.join(part)))
        .filter(|p| !is_protected(p))
        .collect()
}

/// Bytes currently held by the clearable caches.
pub fn size_bytes() -> u64 {
    clearable_dirs().iter().map(|d| dir_size(d)).sum()
}

/// Total size of everything under `path`, or 0 if it cannot be read.
fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Delete the cached audio, returning how many bytes were freed.
pub fn clear() -> Result<u64, BotError> {
    let mut freed = 0;
    let mut first_error = None;
    for dir in clearable_dirs() {
        freed += dir_size(&dir);
        if let Err(e) = clear_contents(&dir) {
            first_error.get_or_insert(format!("{}: {e}", dir.display()));
        }
    }
    match first_error {
        Some(e) => Err(BotError::Config(format!("Could not clear the cache ({e})"))),
        None => Ok(freed),
    }
}

/// Empty a directory without removing the directory itself.
fn clear_contents(dir: &Path) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Record that a cached file was just used, so eviction sees it as recent.
///
/// The timestamp goes on the file rather than into memory, so it survives a
/// restart and is visible to every bot sharing the cache.
pub fn touch(path: &Path) -> std::io::Result<()> {
    let now = std::time::SystemTime::now();
    let times = std::fs::FileTimes::new().set_accessed(now).set_modified(now);
    std::fs::OpenOptions::new().write(true).open(path)?.set_times(times)
}

/// When a cached file was last used, for eviction order.
pub fn last_used(path: &Path) -> Option<std::time::SystemTime> {
    let meta = std::fs::metadata(path).ok()?;
    // Modified time is what `touch` writes. Access time is preferred where the
    // filesystem keeps it, but Windows disables it by default and Linux
    // usually mounts relatime, so it cannot be relied on alone.
    meta.modified().or_else(|_| meta.accessed()).ok()
}

/// One cached file, as eviction sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub size: u64,
    pub last_used: std::time::SystemTime,
}

/// What a sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub removed: usize,
    pub freed: u64,
}

/// Choose what to delete: anything unused for `keep`, then least recently used
/// until the total fits `limit`.
///
/// Pure so the policy can be tested without touching a disk. `now` is passed in
/// for the same reason.
fn plan_eviction(
    mut entries: Vec<Entry>,
    limit: Option<u64>,
    keep: std::time::Duration,
    now: std::time::SystemTime,
) -> Vec<Entry> {
    // Least recently used first, so both rules read from the same end.
    entries.sort_by(|a, b| a.last_used.cmp(&b.last_used).then_with(|| a.path.cmp(&b.path)));

    let mut doomed = Vec::new();
    let mut kept = Vec::new();
    for e in entries {
        let stale = now
            .duration_since(e.last_used)
            .map(|age| age > keep)
            .unwrap_or(false);
        if stale {
            doomed.push(e);
        } else {
            kept.push(e);
        }
    }

    let Some(limit) = limit else {
        // No ceiling still means nothing is kept when the limit is "none at
        // all"; that case is handled by the caller passing Some(0).
        return doomed;
    };
    let mut total: u64 = kept.iter().map(|e| e.size).sum();
    let mut i = 0;
    while total > limit && i < kept.len() {
        total -= kept[i].size;
        doomed.push(kept[i].clone());
        i += 1;
    }
    doomed
}

/// True for the temporary file a download writes before it is complete.
fn is_partial(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "part")
}

/// Every cached file, with its size and when it was last played.
fn entries() -> Vec<Entry> {
    let mut out = Vec::new();
    for dir in clearable_dirs() {
        collect(&dir, &mut out);
    }
    out
}

fn collect(dir: &Path, out: &mut Vec<Entry>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect(&path, out),
            Ok(t) if t.is_file() => {
                // A download in progress is not a cache entry: evicting one
                // would truncate the track being played right now.
                if is_partial(&path) {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                let last_used = last_used(&path).unwrap_or(std::time::UNIX_EPOCH);
                out.push(Entry { path, size: meta.len(), last_used });
            }
            _ => {}
        }
    }
}

/// Apply the cache policy: drop what is stale, then what is least recently
/// played, until the total fits.
pub fn sweep(limit: Option<u64>, keep: std::time::Duration) -> Swept {
    let doomed = plan_eviction(entries(), limit, keep, std::time::SystemTime::now());
    let mut swept = Swept::default();
    for e in doomed {
        if std::fs::remove_file(&e.path).is_ok() {
            swept.removed += 1;
            swept.freed += e.size;
        }
    }
    if swept.removed > 0 {
        tracing::info!(
            "Cache: removed {} file(s), freed {}",
            swept.removed,
            human_size(swept.freed)
        );
    }
    swept
}

/// Apply the policy from the user's settings.
pub fn sweep_to_settings() -> Swept {
    let settings = crate::settings::load();
    sweep(settings.cache_limit_bytes(), settings.cache_keep_duration())
}

/// Render a byte count the way a person reads it.
pub fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", b / GB)
    } else if bytes >= 1024 * 1024 {
        format!("{:.0} MB", b / MB)
    } else if bytes >= 1024 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("ttspotify_audio_cache_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, len: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![0u8; len]).unwrap();
    }

    #[test]
    fn a_clearable_entry_that_reached_the_sdk_would_be_dropped() {
        assert!(is_protected(Path::new("/root/cache/TEAMTALK_DLL")));
        assert!(is_protected(Path::new("/root/cache/TEAMTALK_DLL/sub")));
        assert!(!is_protected(Path::new("/root/cache/spotify_cache/audio")));
    }

    #[test]
    fn the_teamtalk_sdk_is_not_one_of_the_clearable_directories() {
        let dirs = clearable_dirs_under(Path::new("/root/cache"));
        assert!(
            !dirs.iter().any(|d| d.to_string_lossy().contains(PROTECTED)),
            "the SDK must never be clearable, got {dirs:?}"
        );
    }

    #[test]
    fn clearing_removes_the_audio_but_leaves_the_sdk_alone() {
        let cache = scratch("clear");
        write(&cache.join("spotify_cache/audio/a9/track"), 4096);
        write(&cache.join("youtube/vid.m4a"), 2048);
        write(&cache.join(PROTECTED).join("TeamTalk.dll"), 512);
        write(&cache.join("spotify_cache/credentials.json"), 64);

        let dirs = clearable_dirs_under(&cache);
        let freed: u64 = dirs.iter().map(|d| dir_size(d)).sum();
        for dir in &dirs {
            clear_contents(dir).unwrap();
        }

        assert_eq!(freed, 4096 + 2048, "should free exactly the audio");
        assert!(!cache.join("spotify_cache/audio/a9/track").exists());
        assert!(!cache.join("youtube/vid.m4a").exists());
        assert!(cache.join(PROTECTED).join("TeamTalk.dll").exists(), "SDK deleted");
        assert!(cache.join("spotify_cache/credentials.json").exists(), "login deleted");

        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn the_cache_directories_survive_being_emptied() {
        let cache = scratch("keepdirs");
        write(&cache.join("spotify_cache/audio/a9/track"), 16);
        let dir = cache.join("spotify_cache").join("audio");
        clear_contents(&dir).unwrap();
        assert!(dir.is_dir(), "the directory itself must remain");
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn clearing_a_cache_that_was_never_created_is_not_an_error() {
        let cache = scratch("missing");
        for dir in clearable_dirs_under(&cache) {
            clear_contents(&dir).expect("absent cache should be fine");
        }
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn size_counts_every_level_not_just_the_top() {
        let cache = scratch("size");
        write(&cache.join("spotify_cache/audio/a9/one"), 1000);
        write(&cache.join("spotify_cache/audio/b4/two"), 2000);
        let total: u64 = clearable_dirs_under(&cache).iter().map(|d| dir_size(d)).sum();
        assert_eq!(total, 3000);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn size_does_not_count_the_sdk() {
        let cache = scratch("size_sdk");
        write(&cache.join(PROTECTED).join("big.dll"), 9000);
        let total: u64 = clearable_dirs_under(&cache).iter().map(|d| dir_size(d)).sum();
        assert_eq!(total, 0, "the SDK is not part of the reported cache size");
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn touching_a_file_moves_its_timestamp_forward() {
        let cache = scratch("touch");
        let f = cache.join("track.m4a");
        write(&f, 16);
        // Backdate it, then touch and check it caught up. Comparing against
        // "now" alone would pass even if touch did nothing.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60 * 60 * 24 * 30);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&f)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        let before = last_used(&f).expect("timestamp");

        touch(&f).expect("touch should work");
        let after = last_used(&f).expect("timestamp");
        assert!(after > before, "touch must move the timestamp forward");
        assert!(
            after.elapsed().unwrap() < std::time::Duration::from_secs(60),
            "touched file should read as used just now"
        );
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn touching_a_file_that_is_not_there_is_an_error_not_a_panic() {
        let cache = scratch("touch_missing");
        assert!(touch(&cache.join("gone.m4a")).is_err());
        let _ = std::fs::remove_dir_all(&cache);
    }

    use std::time::{Duration, SystemTime};

    const DAY: Duration = Duration::from_secs(86_400);

    fn entry(name: &str, size: u64, days_ago: u64) -> Entry {
        Entry {
            path: PathBuf::from(name),
            size,
            last_used: SystemTime::UNIX_EPOCH + Duration::from_secs(365 * 86_400)
                - Duration::from_secs(days_ago * 86_400),
        }
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(365 * 86_400)
    }

    fn names(v: &[Entry]) -> Vec<String> {
        let mut n: Vec<String> = v.iter().map(|e| e.path.display().to_string()).collect();
        n.sort();
        n
    }

    #[test]
    fn a_track_nobody_played_for_longer_than_the_age_goes() {
        let doomed = plan_eviction(
            vec![entry("old", 10, 8), entry("fresh", 10, 1)],
            Some(1_000_000),
            7 * DAY,
            now(),
        );
        assert_eq!(names(&doomed), vec!["old"]);
    }

    #[test]
    fn a_track_played_yesterday_survives_being_ancient() {
        // The whole point of the bump: age counts from the last play, not from
        // when it was downloaded.
        let doomed = plan_eviction(vec![entry("classic", 10, 0)], Some(1_000_000), 7 * DAY, now());
        assert!(doomed.is_empty(), "a track played today must not be evicted");
    }

    #[test]
    fn over_the_limit_the_least_recently_played_goes_first() {
        let doomed = plan_eviction(
            vec![entry("a", 100, 1), entry("b", 100, 3), entry("c", 100, 2)],
            Some(250),
            30 * DAY,
            now(),
        );
        assert_eq!(names(&doomed), vec!["b"], "oldest play should go, and only it");
    }

    #[test]
    fn it_stops_deleting_as_soon_as_it_fits() {
        let doomed = plan_eviction(
            vec![entry("a", 100, 1), entry("b", 100, 4), entry("c", 100, 3)],
            Some(150),
            30 * DAY,
            now(),
        );
        assert_eq!(names(&doomed), vec!["b", "c"], "should free just enough");
    }

    #[test]
    fn a_cache_that_fits_and_is_fresh_loses_nothing() {
        let doomed = plan_eviction(
            vec![entry("a", 10, 1), entry("b", 10, 2)],
            Some(1_000),
            7 * DAY,
            now(),
        );
        assert!(doomed.is_empty());
    }

    #[test]
    fn the_two_rules_do_not_evict_the_same_file_twice() {
        // A stale file also over the limit must be counted once, or the freed
        // total is reported as more than was really there.
        let doomed = plan_eviction(
            vec![entry("stale", 100, 40), entry("fresh", 100, 0)],
            Some(50),
            7 * DAY,
            now(),
        );
        assert_eq!(names(&doomed), vec!["fresh", "stale"]);
        assert_eq!(doomed.len(), 2, "no duplicates");
    }

    #[test]
    fn spotify_and_youtube_are_one_pool_not_two() {
        // The design decision, pinned: a YouTube track can be evicted to make
        // room for Spotify and the other way round. Nothing splits the budget.
        let doomed = plan_eviction(
            vec![
                Entry { path: PathBuf::from("spotify_cache/audio/a9/x"), size: 100, last_used: entry("", 0, 1).last_used },
                Entry { path: PathBuf::from("youtube/v.m4a"), size: 100, last_used: entry("", 0, 5).last_used },
            ],
            Some(150),
            30 * DAY,
            now(),
        );
        assert_eq!(
            names(&doomed),
            vec!["youtube/v.m4a"],
            "the older play should go regardless of which service it came from"
        );
    }

    #[test]
    fn a_zero_limit_clears_the_lot() {
        let doomed = plan_eviction(
            vec![entry("a", 10, 0), entry("b", 10, 0)],
            Some(0),
            30 * DAY,
            now(),
        );
        assert_eq!(doomed.len(), 2);
    }

    #[test]
    fn a_file_with_a_timestamp_in_the_future_is_not_treated_as_ancient() {
        // Clock skew, or a file copied in from elsewhere. duration_since fails
        // here, and reading that as "very old" would delete it immediately.
        let future = Entry {
            path: PathBuf::from("future"),
            size: 10,
            last_used: now() + Duration::from_secs(60 * 60),
        };
        let doomed = plan_eviction(vec![future], Some(1_000), 7 * DAY, now());
        assert!(doomed.is_empty());
    }

    #[test]
    fn a_download_in_progress_is_never_evicted() {
        assert!(is_partial(Path::new("/c/youtube/abc.1234.part")));
        assert!(!is_partial(Path::new("/c/youtube/abc.m4a")));
        assert!(!is_partial(Path::new("/c/spotify_cache/audio/a9/x")));
    }

    #[test]
    fn a_partial_file_is_left_out_of_the_listing() {
        let cache = scratch("partials");
        write(&cache.join("youtube/done.m4a"), 100);
        write(&cache.join("youtube/busy.999.part"), 100);
        let mut found = Vec::new();
        for dir in clearable_dirs_under(&cache) {
            collect(&dir, &mut found);
        }
        let names: Vec<String> = found
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["done.m4a"], "a .part download must be invisible");
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// The policy against real files, not just the planner.
    #[test]
    fn a_sweep_deletes_from_disk_and_reports_what_it_freed() {
        let cache = scratch("sweep");
        let stale = cache.join("youtube/stale.m4a");
        let fresh = cache.join("spotify_cache/audio/a9/fresh");
        let busy = cache.join("youtube/busy.1.part");
        write(&stale, 1000);
        write(&fresh, 1000);
        write(&busy, 1000);

        let old = std::time::SystemTime::now() - Duration::from_secs(30 * 86_400);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();

        let mut found = Vec::new();
        for dir in clearable_dirs_under(&cache) {
            collect(&dir, &mut found);
        }
        let doomed = plan_eviction(found, Some(10_000), 7 * DAY, SystemTime::now());
        let mut swept = Swept::default();
        for e in doomed {
            if std::fs::remove_file(&e.path).is_ok() {
                swept.removed += 1;
                swept.freed += e.size;
            }
        }

        assert_eq!(swept, Swept { removed: 1, freed: 1000 });
        assert!(!stale.exists(), "the stale track should be gone");
        assert!(fresh.exists(), "a track played today must stay");
        assert!(busy.exists(), "a download in progress must stay");
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn touching_a_stale_track_saves_it_from_the_next_sweep() {
        // End to end on the guarantee that matters: playing a track keeps it.
        let cache = scratch("rescue");
        let track = cache.join("youtube/old.m4a");
        write(&track, 100);
        let old = std::time::SystemTime::now() - Duration::from_secs(30 * 86_400);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&track)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();

        let plan = |cache: &Path| {
            let mut found = Vec::new();
            for dir in clearable_dirs_under(cache) {
                collect(&dir, &mut found);
            }
            plan_eviction(found, Some(10_000), 7 * DAY, SystemTime::now())
        };
        assert_eq!(plan(&cache).len(), 1, "stale before it is played");

        touch(&track).unwrap();
        assert!(plan(&cache).is_empty(), "playing it must save it");
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// `sweep_to_settings` against the install's real directories.
    ///
    /// Ignored: it writes into and deletes from the actual cache, so it is for
    /// running by hand on a throwaway install, not in CI. Covers the wiring
    /// from settings.json through to files leaving the disk, which the
    /// in-process tests above stop short of.
    #[test]
    #[ignore = "reads and deletes the real cache directory"]
    fn sweep_to_settings_evicts_from_the_real_cache() {
        let dirs = clearable_dirs();
        let yt = dirs
            .iter()
            .find(|d| d.ends_with("youtube"))
            .expect("a youtube cache dir");
        std::fs::create_dir_all(yt).unwrap();

        let stale = yt.join("zz_sweep_test_stale.m4a");
        let fresh = yt.join("zz_sweep_test_fresh.m4a");
        std::fs::write(&stale, vec![0u8; 4096]).unwrap();
        std::fs::write(&fresh, vec![0u8; 4096]).unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(400 * 86_400);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();

        let swept = sweep_to_settings();
        println!("swept {} file(s), freed {}", swept.removed, human_size(swept.freed));
        assert!(!stale.exists(), "a track unplayed for over a year should go");
        assert!(fresh.exists(), "a track written just now should stay");

        let _ = std::fs::remove_file(&fresh);
    }

    #[test]
    fn sizes_read_the_way_a_person_would_say_them() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(999), "999 bytes");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(56 * 1024 * 1024), "56 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
    }
}
