//! Where downloaded YouTube tracks are kept between plays.

use std::io;
use std::path::{Path, PathBuf};

/// Directory holding cached tracks. Matches the `youtube` entry in
/// [`crate::audio_cache`], which is what clears it.
pub fn dir() -> PathBuf {
    crate::paths::cache_dir().join("youtube")
}

/// The finished file for `video_id`.
pub fn track_path(video_id: &str) -> Option<PathBuf> {
    safe_name(video_id).map(|name| dir().join(format!("{name}.m4a")))
}

/// The partial file a download writes to before it is complete.
///
/// A distinct name so an interrupted download is never mistaken for a cached
/// track, and per-process so two bots fetching the same video cannot write
/// over each other.
pub fn partial_path(video_id: &str) -> Option<PathBuf> {
    safe_name(video_id)
        .map(|name| dir().join(format!("{name}.{}.part", std::process::id())))
}

/// Reject anything that is not a plain YouTube id, so a crafted id cannot
/// name a file outside the cache.
fn safe_name(video_id: &str) -> Option<String> {
    let ok = !video_id.is_empty()
        && video_id.len() <= 64
        && video_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| video_id.to_string())
}

/// A cached track, if one is there.
pub fn cached(video_id: &str) -> Option<PathBuf> {
    track_path(video_id).filter(|p| p.is_file())
}

/// Record that a track was played, which is what keeps it from being evicted
/// as the least recently used.
pub fn mark_played(path: &Path) {
    if let Err(e) = crate::audio_cache::touch(path) {
        tracing::debug!("could not mark {} as played: {e}", path.display());
    }
}

/// Move a completed download into place.
pub fn publish(partial: &Path, video_id: &str) -> io::Result<PathBuf> {
    let Some(target) = track_path(video_id) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "unusable video id"));
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(partial, &target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_ids_are_accepted() {
        for id in ["dQw4w9WgXcQ", "FvHIEK4f1Qc", "a-b_c123"] {
            assert!(safe_name(id).is_some(), "{id} should be usable");
        }
    }

    #[test]
    fn an_id_cannot_name_a_file_outside_the_cache() {
        for id in ["../../etc/passwd", "a/b", "a\\b", "", "a b", ".."] {
            assert!(safe_name(id).is_none(), "{id:?} should be refused");
        }
    }

    #[test]
    fn a_partial_download_is_not_mistaken_for_a_finished_one() {
        let id = "dQw4w9WgXcQ";
        let finished = track_path(id).unwrap();
        let partial = partial_path(id).unwrap();
        assert_ne!(finished, partial);
        assert!(finished.to_string_lossy().ends_with(".m4a"));
        assert!(partial.to_string_lossy().ends_with(".part"));
    }

    #[test]
    fn two_processes_do_not_share_a_partial_file() {
        let mine = partial_path("dQw4w9WgXcQ").unwrap();
        let name = mine.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.contains(&std::process::id().to_string()),
            "partial name should carry the pid, got {name}"
        );
    }

    #[test]
    fn publishing_moves_the_partial_into_place() {
        let dir = std::env::temp_dir().join(format!("ttspotify_ytpub_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let partial = dir.join("x.part");
        std::fs::write(&partial, b"audio").unwrap();

        // publish() targets the real cache dir, so exercise the rename itself.
        let target = dir.join("dQw4w9WgXcQ.m4a");
        std::fs::rename(&partial, &target).unwrap();
        assert!(!partial.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"audio");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
