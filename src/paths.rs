//! Where everything the bot owns lives, and the one-time move into that layout.
//!
//! Everything sits under a single root (`data/` beside the exe on Windows,
//! `~/.config/ttspotify/` elsewhere) so the whole installation can be copied to
//! another machine. Inside it, files are grouped by who owns them and what
//! happens if they are deleted, using the names the XDG spec uses:
//!
//! - `config/` — bot configs and `cookies.txt`. Yours to edit.
//! - `lang/`   — the English template plus any translations you add.
//! - `state/`  — small things the bot must remember (language choices, settings).
//! - `auth/`   — cached service credentials. Secret: never share this folder.
//! - `cache/`  — anything the bot can fetch again. Always safe to delete.
//! - `logs/`   — one file per instance per day.
//!
//! Before this, all of it sat loose in the root, which meant the config scanner
//! had to carry a hand-maintained list of "JSON files that are not configs" and
//! warned about every one it did not know.

use std::path::{Path, PathBuf};

/// Layout the current version expects. Bumped when files move again.
pub const LAYOUT_VERSION: u32 = 2;

/// Records the layout a root was last migrated to.
const VERSION_MARKER: &str = ".layout-version";

pub fn root() -> PathBuf {
    crate::config::config_dir()
}

/// Bot configs and `cookies.txt`.
pub fn configs_dir() -> PathBuf {
    root().join("config")
}

/// Language template and user translations.
pub fn lang_dir() -> PathBuf {
    root().join("lang")
}

/// Small persistent things the bot owns.
pub fn state_dir() -> PathBuf {
    state_dir_in(&root())
}

/// `state/` under an explicit root, for callers that are handed one (and for
/// tests, which must not touch the real data folder).
pub fn state_dir_in(root: &Path) -> PathBuf {
    root.join("state")
}

/// `lang/` under an explicit root.
pub fn lang_dir_in(root: &Path) -> PathBuf {
    root.join("lang")
}

/// Cached service credentials.
pub fn auth_dir() -> PathBuf {
    root().join("auth")
}

/// Anything that can be fetched again.
pub fn cache_dir() -> PathBuf {
    root().join("cache")
}

pub fn logs_dir() -> PathBuf {
    root().join("logs")
}

/// Create the standard folders if they are missing, so a fresh install has the
/// same shape as a migrated one and every writer can assume its folder exists.
pub fn ensure_dirs() {
    for dir in [configs_dir(), lang_dir(), state_dir(), auth_dir(), cache_dir(), logs_dir()] {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("Could not create {}: {e}", dir.display());
        }
    }
}

/// Short plain-text guide dropped in the data folder, so someone opening it
/// can tell what is safe to touch without reading the manual.
const FOLDER_README: &str = "This folder holds everything TTSpotify keeps.

config/   Your bot configs and cookies.txt. Edit these.
lang/     Translations. en.lang is the template to copy for a new language.
state/    Small things the bot remembers (language choices, settings).
auth/     Saved service logins. Secret - never share this folder.
cache/    Downloads and caches. Safe to delete at any time; they come back.
logs/     One file per bot per day.

If something misbehaves, deleting cache/ is safe and often enough.
When sending logs for a bug report, send logs/ only - not the whole folder,
because auth/ contains your saved logins.
";

/// Write the folder guide if it is not already there.
fn write_folder_readme() {
    let path = root().join("README.txt");
    if path.exists() {
        return;
    }
    let _ = std::fs::write(path, FOLDER_README);
}

/// Sort the real data folder into the current layout.
///
/// Runs before logging is up (it decides where the logs go), so the report is
/// returned for the caller to log once tracing is initialised rather than
/// written straight out.
pub fn migrate_data_layout() -> Migration {
    let migration = migrate_layout(&root(), &crate::config::is_bot_config);
    ensure_dirs();
    write_folder_readme();
    migration
}

/// Write a migration report to the log. Successful moves are worth one line so
/// a user can see where their files went; failures are warnings, because the
/// bot carries on reading the old location for whatever did not move.
pub fn log_migration(migration: &Migration) {
    if migration.skipped || migration.moves.is_empty() {
        return;
    }
    for moved in &migration.moves {
        match moved {
            Moved::Ok { from, to } => tracing::info!(
                "Moved {} into {}",
                from.file_name().unwrap_or_default().to_string_lossy(),
                to.parent().unwrap_or(to).display()
            ),
            Moved::Failed { from, reason } => tracing::warn!(
                "Could not move {} into the new layout: {reason}",
                from.display()
            ),
        }
    }
    tracing::info!(
        "Data folder reorganised: {} item(s) moved into config/, state/, auth/ and cache/",
        migration.succeeded()
    );
}

/// One relocation the migration performed or could not perform.
#[derive(Debug, PartialEq, Eq)]
pub enum Moved {
    Ok { from: PathBuf, to: PathBuf },
    Failed { from: PathBuf, reason: String },
}

/// What a migration did, for logging and tests.
#[derive(Debug, Default)]
pub struct Migration {
    pub moves: Vec<Moved>,
    /// True when the root was already in the current layout.
    pub skipped: bool,
}

impl Migration {
    pub fn failures(&self) -> impl Iterator<Item = &Moved> {
        self.moves.iter().filter(|m| matches!(m, Moved::Failed { .. }))
    }

    pub fn succeeded(&self) -> usize {
        self.moves.iter().filter(|m| matches!(m, Moved::Ok { .. })).count()
    }
}

/// Files that are never bot configs, wherever they sit.
const NON_CONFIG_STEMS: [&str; 5] = ["credentials", "cookies", "sessions", "settings", "lang_prefs"];

/// Move a legacy flat root into the current layout.
///
/// `is_config` decides whether a loose `.json` file is a bot config (the caller
/// passes the real validator; tests pass their own). Nothing is ever deleted or
/// overwritten: a name that already exists at the destination is left alone, so
/// running this twice cannot lose data.
pub fn migrate_layout(root: &Path, is_config: &dyn Fn(&Path) -> bool) -> Migration {
    let mut migration = Migration::default();
    if !root.exists() {
        return migration;
    }
    if read_layout_version(root) >= LAYOUT_VERSION {
        migration.skipped = true;
        return migration;
    }

    // Fixed destinations for the files whose names we know.
    let fixed: [(&str, PathBuf); 6] = [
        ("lang_prefs.json", root.join("state")),
        ("settings.json", root.join("state")),
        ("credentials.json", root.join("auth")),
        ("rustypipe_cache.json", root.join("cache")),
        ("spotify_cache", root.join("cache")),
        ("TEAMTALK_DLL", root.join("cache")),
    ];
    for (name, dest_dir) in fixed {
        let from = root.join(name);
        if from.exists() {
            move_into(&from, &dest_dir, &mut migration);
        }
    }

    // Cookies and configs belong to the user.
    let cookies = root.join("cookies.txt");
    if cookies.exists() {
        move_into(&cookies, &root.join("config"), &mut migration);
    }
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if NON_CONFIG_STEMS.contains(&stem) || !is_config(&path) {
            continue;
        }
        move_into(&path, &root.join("config"), &mut migration);
    }

    // Only claim the new layout if everything landed; a partial move must be
    // retried next start rather than silently forgotten.
    if migration.failures().next().is_none() {
        write_layout_version(root);
    }
    migration
}

fn move_into(from: &Path, dest_dir: &Path, migration: &mut Migration) {
    let Some(name) = from.file_name() else {
        return;
    };
    let to = dest_dir.join(name);
    if to.exists() {
        // Already migrated by hand, or a leftover: never overwrite.
        return;
    }
    if let Err(e) = std::fs::create_dir_all(dest_dir) {
        migration.moves.push(Moved::Failed {
            from: from.to_path_buf(),
            reason: e.to_string(),
        });
        return;
    }
    match std::fs::rename(from, &to) {
        Ok(()) => migration.moves.push(Moved::Ok {
            from: from.to_path_buf(),
            to,
        }),
        Err(e) => migration.moves.push(Moved::Failed {
            from: from.to_path_buf(),
            reason: e.to_string(),
        }),
    }
}

fn read_layout_version(root: &Path) -> u32 {
    std::fs::read_to_string(root.join(VERSION_MARKER))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_layout_version(root: &Path) {
    let _ = std::fs::write(root.join(VERSION_MARKER), LAYOUT_VERSION.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ttspotify_paths_{}_{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    /// Everything is a config except the known non-config names.
    fn any_json_is_config(_: &Path) -> bool {
        true
    }

    #[test]
    fn a_flat_root_is_sorted_into_the_new_layout() {
        let root = scratch("flat");
        write(&root.join("myserver.json"), "{}");
        write(&root.join("cookies.txt"), "cookie");
        write(&root.join("lang_prefs.json"), "{}");
        write(&root.join("settings.json"), "{}");
        write(&root.join("credentials.json"), "{}");
        write(&root.join("rustypipe_cache.json"), "{}");
        write(&root.join("spotify_cache").join("audio").join("x"), "blob");
        write(&root.join("TEAMTALK_DLL").join("TEAMTALK_SDK_VERSION.txt"), "v5.19a");

        let report = migrate_layout(&root, &any_json_is_config);

        assert!(report.failures().next().is_none(), "{report:?}");
        assert!(root.join("config/myserver.json").exists());
        assert!(root.join("config/cookies.txt").exists());
        assert!(root.join("state/lang_prefs.json").exists());
        assert!(root.join("state/settings.json").exists());
        assert!(root.join("auth/credentials.json").exists());
        assert!(root.join("cache/rustypipe_cache.json").exists());
        assert!(root.join("cache/spotify_cache/audio/x").exists());
        assert!(root.join("cache/TEAMTALK_DLL/TEAMTALK_SDK_VERSION.txt").exists());
        // Nothing left loose.
        assert!(!root.join("myserver.json").exists());
        assert!(!root.join("credentials.json").exists());
    }

    #[test]
    fn a_second_run_does_nothing() {
        let root = scratch("twice");
        write(&root.join("myserver.json"), "{}");
        migrate_layout(&root, &any_json_is_config);

        let again = migrate_layout(&root, &any_json_is_config);
        assert!(again.skipped, "an already-migrated root must be left alone");
        assert_eq!(again.succeeded(), 0);
    }

    #[test]
    fn a_file_that_is_not_a_config_is_left_where_it_is() {
        // A stray JSON the bot does not recognise must not be swept into
        // config/, where it would then be read as a bot.
        let root = scratch("stray");
        write(&root.join("notes.json"), "{}");
        migrate_layout(&root, &|_| false);
        assert!(root.join("notes.json").exists(), "stray file should stay put");
        assert!(!root.join("config/notes.json").exists());
    }

    #[test]
    fn an_existing_destination_is_never_overwritten() {
        let root = scratch("collide");
        write(&root.join("myserver.json"), "old");
        write(&root.join("config").join("myserver.json"), "new");

        migrate_layout(&root, &any_json_is_config);

        assert_eq!(std::fs::read_to_string(root.join("config/myserver.json")).unwrap(), "new");
        assert!(root.join("myserver.json").exists(), "the loose copy is kept, not destroyed");
    }

    #[test]
    fn an_already_new_layout_is_left_alone() {
        let root = scratch("fresh");
        write(&root.join("config").join("myserver.json"), "{}");
        let report = migrate_layout(&root, &any_json_is_config);
        assert_eq!(report.succeeded(), 0);
        assert!(root.join("config/myserver.json").exists());
    }

    #[test]
    fn a_missing_root_is_not_an_error() {
        let root = scratch("gone");
        std::fs::remove_dir_all(&root).unwrap();
        let report = migrate_layout(&root, &any_json_is_config);
        assert_eq!(report.succeeded(), 0);
        assert!(report.failures().next().is_none());
    }

    #[test]
    fn the_layout_version_is_only_claimed_once_everything_moved() {
        let root = scratch("version");
        write(&root.join("myserver.json"), "{}");
        migrate_layout(&root, &any_json_is_config);
        assert_eq!(read_layout_version(&root), LAYOUT_VERSION);
    }
}

#[cfg(test)]
mod live_folder_test {
    //! Runs the migration against a copy of a real data folder when one is
    //! provided, so the rules are checked against something that actually
    //! existed rather than only fixtures the test wrote itself.

    use super::*;

    #[test]
    fn migrates_a_real_data_folder_when_one_is_supplied() {
        let Some(dir) = std::env::var_os("TTSPOTIFY_LIVE_DATA_DIR") else {
            return; // Nothing to check against; not a failure.
        };
        let root = PathBuf::from(dir);
        if !root.exists() {
            return;
        }
        let report = migrate_layout(&root, &crate::config::is_bot_config);
        assert!(report.failures().next().is_none(), "{report:?}");
        for loose in ["config.json", "credentials.json", "lang_prefs.json", "settings.json"] {
            assert!(!root.join(loose).exists(), "{loose} should have moved");
        }
        assert!(root.join("config/config.json").exists());
        assert!(root.join("auth/credentials.json").exists());
        assert!(root.join("state/lang_prefs.json").exists());
        assert!(root.join("cache/spotify_cache").exists());
        // Logs and translations were already in the right shape.
        assert!(root.join("logs").exists());
        assert!(root.join("lang").exists());
    }
}
