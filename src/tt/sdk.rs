//! TeamTalk SDK directory pinning + one-time migration.
//!
//! The teamtalk crate historically downloaded the SDK into a CWD-relative
//! `TEAMTALK_DLL/`, scattering copies wherever the bot was started from
//! (under systemd: $HOME). The fork now honors `TEAMTALK_SDK_DIR`; we pin it
//! to `<config_dir>/TEAMTALK_DLL` and move an existing copy there so nothing
//! is re-downloaded and no duplicate lingers.

use std::path::{Path, PathBuf};

/// Marker file the teamtalk crate writes into its SDK directory. Its presence
/// is proof the directory is an SDK download of ours and safe to move.
const SDK_MARKER: &str = "TEAMTALK_SDK_VERSION.txt";

/// The pinned SDK location: `<config_dir>/TEAMTALK_DLL`.
pub fn pinned_sdk_dir() -> PathBuf {
    crate::config::config_dir().join("TEAMTALK_DLL")
}

/// Pin the SDK directory for the teamtalk crate (unless the user already set
/// `TEAMTALK_SDK_DIR` themselves) and migrate a legacy CWD/home copy into it.
/// Call once, first thing in `main`, before any TeamTalk client is created.
pub fn pin_sdk_dir() {
    if std::env::var_os("TEAMTALK_SDK_DIR").is_some() {
        return;
    }
    let target = pinned_sdk_dir();
    for legacy in legacy_sdk_candidates() {
        migrate_sdk_dir(&legacy, &target);
    }
    std::env::set_var("TEAMTALK_SDK_DIR", &target);
}

/// Places an old CWD-relative SDK download may sit: the current working
/// directory (manual runs, the old Windows tray behavior) and the home
/// directory (the old systemd behavior, where the service started in $HOME).
fn legacy_sdk_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("TEAMTALK_DLL"));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join("TEAMTALK_DLL"));
    }
    candidates
}

/// Move `legacy` to `target` when it is provably our SDK download (contains
/// the version marker), the target doesn't exist yet, and they aren't the
/// same directory. Best-effort: on any failure the bot just re-downloads.
/// Returns whether a migration happened.
pub fn migrate_sdk_dir(legacy: &Path, target: &Path) -> bool {
    if target.exists() || !legacy.join(SDK_MARKER).is_file() {
        return false;
    }
    // Guard against CWD == config dir (target and candidate are the same).
    if let (Ok(l), Ok(t_parent)) = (legacy.canonicalize(), target.parent().map(|p| p.canonicalize()).unwrap_or(Ok(PathBuf::new()))) {
        if t_parent.join("TEAMTALK_DLL") == l {
            return false;
        }
    }
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::rename(legacy, target) {
        Ok(()) => {
            tracing::info!(
                "Moved TeamTalk SDK from {} to {}",
                legacy.display(),
                target.display()
            );
            true
        }
        Err(e) => {
            // Cross-device or locked: leave it; the loader downloads fresh.
            tracing::debug!("SDK migration from {} skipped: {e}", legacy.display());
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ttspotify_sdkmig_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn moves_marked_sdk_dir_to_target() {
        let base = tmp("move");
        let legacy = base.join("TEAMTALK_DLL");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join(SDK_MARKER), "v5.19a").unwrap();
        std::fs::write(legacy.join("libTeamTalk5.so"), "x").unwrap();
        let target = base.join("cfg").join("TEAMTALK_DLL");

        assert!(migrate_sdk_dir(&legacy, &target));
        assert!(!legacy.exists());
        assert!(target.join(SDK_MARKER).is_file());
        assert!(target.join("libTeamTalk5.so").is_file());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn refuses_unmarked_dir() {
        // No marker file: could be anything the user put there. Don't touch.
        let base = tmp("unmarked");
        let legacy = base.join("TEAMTALK_DLL");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("something.txt"), "x").unwrap();
        let target = base.join("cfg").join("TEAMTALK_DLL");

        assert!(!migrate_sdk_dir(&legacy, &target));
        assert!(legacy.exists());
        assert!(!target.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn keeps_existing_target() {
        // Target already has an SDK: never overwrite it.
        let base = tmp("existing");
        let legacy = base.join("TEAMTALK_DLL");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join(SDK_MARKER), "v5.19a").unwrap();
        let target = base.join("cfg").join("TEAMTALK_DLL");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join(SDK_MARKER), "v5.20").unwrap();

        assert!(!migrate_sdk_dir(&legacy, &target));
        assert!(legacy.exists());
        assert_eq!(std::fs::read_to_string(target.join(SDK_MARKER)).unwrap(), "v5.20");

        let _ = std::fs::remove_dir_all(&base);
    }
}
