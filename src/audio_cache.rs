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

    #[test]
    fn sizes_read_the_way_a_person_would_say_them() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(999), "999 bytes");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(56 * 1024 * 1024), "56 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
    }
}
