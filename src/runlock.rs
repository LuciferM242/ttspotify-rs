//! One bot per config, per machine.
//!
//! Nothing used to stop the same bot running twice: start it as a service,
//! then open a terminal and `run` it to see what it is doing, and two processes
//! log in to TeamTalk under one account. Most servers drop the older session
//! when the newer one arrives, so the pair knock each other offline in turn,
//! both inject audio into the channel, and both write the config when the
//! volume changes. From the outside the bot looks like it is disconnecting at
//! random and changing its own volume, and neither copy can say why — it does
//! not know the other exists.
//!
//! The lock is an `flock` on a file beside the config. The kernel drops it when
//! the process ends, however it ends, so a crash or a kill never leaves a stale
//! lock that blocks the next start — which is the failure mode that makes
//! pid-file locking worse than none at all.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// Held for as long as the bot runs. Dropping it (or exiting) frees the lock.
#[derive(Debug)]
pub struct RunLock {
    _file: File,
    path: PathBuf,
}

impl RunLock {
    /// The lock file's path, for messages.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Why a lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another process is running this bot.
    AlreadyRunning,
    /// The lock itself could not be created — a read-only data directory, say.
    /// The bot should carry on: refusing to start because a lock file cannot be
    /// written would be a worse failure than the one being prevented.
    Unavailable(String),
}

/// Where a config's lock lives: in the state directory, named after the config
/// and the full path it came from.
///
/// The name alone is not enough. Two files can both be called `home.json` in
/// different folders — one in the data root, one someone keeps elsewhere and
/// runs with `--config` — and those are different bots, quite possibly on
/// different accounts. Keying on the whole path means only the same file locks
/// out the same file, while the readable stem keeps the directory listing
/// meaningful.
pub fn lock_path(config_path: &Path) -> PathBuf {
    let name = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("config");
    // The canonical path when it resolves, so `./home.json` and an absolute
    // path to the same file share a lock; the path as given when it does not.
    let full = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    crate::paths::state_dir().join(format!("{name}-{:016x}.lock", path_digest(&full)))
}

/// A digest of the path that two builds will always agree on.
///
/// Not `DefaultHasher`: its output is explicitly allowed to change between Rust
/// releases, and the whole point of the lock is that two processes compute the
/// same file name. A bot started by a service built with one toolchain and a
/// second copy started by hand from a newer build would each take their own
/// lock and both log in — exactly the collision this module exists to stop.
/// FNV-1a is a handful of lines and fixed forever.
fn path_digest(path: &Path) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Take the lock for this config, or say who is already holding it.
pub fn acquire(config_path: &Path) -> Result<RunLock, LockError> {
    let path = lock_path(config_path);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(LockError::Unavailable(e.to_string()));
        }
    }

    let file = match File::create(&path) {
        Ok(file) => file,
        Err(e) => return Err(LockError::Unavailable(e.to_string())),
    };

    // LOCK_EX | LOCK_NB: take it if free, tell us straight away if not, rather
    // than waiting for the other bot to exit.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if taken == 0 {
        return Ok(RunLock { _file: file, path });
    }

    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Err(LockError::AlreadyRunning),
        _ => Err(LockError::Unavailable(err.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ttspotify_lock_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn each_config_gets_its_own_lock_file() {
        let dir = Path::new("/x/config");
        assert_ne!(
            lock_path(&dir.join("apple.json")),
            lock_path(&dir.join("pear.json"))
        );
        // Readable at a glance in the state directory.
        assert!(lock_path(&dir.join("apple.json"))
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("apple-"));
    }

    #[test]
    fn the_digest_is_fixed_so_two_builds_agree() {
        // Pinned values: if this ever changes, a bot started by an older build
        // and one started by a newer build would take different locks and both
        // log in on the same account.
        // Cross-checked against an independent FNV-1a implementation, not
        // copied from this code's own output.
        assert_eq!(
            path_digest(Path::new("/home/u/.config/ttspotify/config/apple.json")),
            0x625b_fed0_ab62_3d77
        );
        assert_eq!(path_digest(Path::new("")), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn the_same_name_in_two_folders_is_two_different_bots() {
        // A `home.json` in the data root and one someone keeps elsewhere and
        // runs with --config are different bots, possibly on different
        // accounts, so one must not lock the other out.
        assert_ne!(
            lock_path(Path::new("/a/home.json")),
            lock_path(Path::new("/b/home.json"))
        );
    }

    #[test]
    fn a_second_lock_on_the_same_file_is_refused() {
        // Two locks in one process still contend, because flock is per open
        // file description and each `acquire` opens its own.
        let dir = scratch("contend");
        let config = dir.join("lock-contend.json");
        std::fs::write(&config, "{}").unwrap();

        let first = acquire(&config).expect("first lock");
        match acquire(&config) {
            Err(LockError::AlreadyRunning) => {}
            other => panic!("second lock should have been refused, got {other:?}"),
        }

        // Dropping the first frees it, which is what makes a crashed bot
        // recoverable without cleaning anything up by hand.
        drop(first);
        assert!(acquire(&config).is_ok(), "the lock should be free again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_configs_do_not_block_each_other() {
        let dir = scratch("separate");
        let apple = dir.join("lock-sep-a.json");
        let pear = dir.join("lock-sep-b.json");
        std::fs::write(&apple, "{}").unwrap();
        std::fs::write(&pear, "{}").unwrap();

        let _a = acquire(&apple).expect("apple");
        assert!(acquire(&pear).is_ok(), "a different bot must not be blocked");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
