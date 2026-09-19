//! Which bot the current thread belongs to.
//!
//! The tray runs every bot in one process, so anything shared across them -
//! a log file, a record of "a key failed recently" - needs to know whose work
//! a thread is doing. Each bot builds its own tokio runtime and its own
//! threads, so every thread belongs to exactly one bot, and a tracing layer
//! runs on the thread that emitted the event.
//!
//! Zero means "not tagged": the single-bot CLI, library threads the bot does
//! not spawn itself, and every test.

use std::sync::atomic::{AtomicU64, Ordering};

thread_local! {
    static CURRENT_BOT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Hands out bot ids. Starts at 1 so it never collides with the untagged 0.
static NEXT_BOT_ID: AtomicU64 = AtomicU64::new(1);

/// Claim a fresh bot id, to be given to every thread that bot runs on.
pub fn next_bot_id() -> u64 {
    NEXT_BOT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Tag this thread as belonging to `id`. Call on the bot's own thread, from
/// its runtime's `on_thread_start`, and on any thread it spawns itself.
pub fn set_current_bot(id: u64) {
    CURRENT_BOT.with(|c| c.set(id));
}

/// The bot this thread belongs to, or 0 when untagged.
pub fn current_bot() -> u64 {
    CURRENT_BOT.with(|c| c.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untagged_thread_belongs_to_no_bot() {
        std::thread::spawn(|| assert_eq!(current_bot(), 0)).join().unwrap();
    }

    #[test]
    fn a_tag_stays_on_the_thread_that_set_it() {
        let id = next_bot_id();
        set_current_bot(id);
        let elsewhere = std::thread::spawn(current_bot).join().unwrap();
        assert_eq!(current_bot(), id);
        assert_eq!(elsewhere, 0, "a new thread starts untagged");
        set_current_bot(0);
    }

    #[test]
    fn ids_are_never_handed_out_twice() {
        let first = next_bot_id();
        let second = next_bot_id();
        assert_ne!(first, second);
        assert!(first > 0 && second > 0, "zero is reserved for untagged threads");
    }
}
