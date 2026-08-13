//! Noticing when Spotify refuses an audio key.
//!
//! librespot does not report this as a player event. It logs the failure, warns
//! that it is "continuing without decryption", and streams the encrypted bytes
//! anyway. Those bytes reach the decoder as noise, so what finally surfaces is
//! `PlayerEvent::Unavailable` — which the bot used to report as "Track
//! unavailable", blaming the track for an account or session problem.
//!
//! Watching librespot's own log for the failure lets the skip message say what
//! actually went wrong. It is indirect, but the alternative is patching
//! librespot: the reason never reaches us any other way.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// How long after a key failure a skipped track is still blamed on it. Long
/// enough to cover the decode attempt that follows (about 2s in practice),
/// short enough that an unrelated skip later is not mislabelled.
pub const BLAME_WINDOW: Duration = Duration::from_secs(15);

// Which bot the current thread belongs to.
//
// The tray runs every bot in one process, so a single shared record of "a key
// failed recently" let one bot's failure explain another bot's unrelated skip.
// Each bot builds its own tokio runtime, so every thread that can log a key
// failure or handle a skip belongs to exactly one bot, and the layer runs on
// the thread that emitted the event. Tagging those threads keeps the two apart.
//
// Zero means "not tagged": the single-bot CLI, and every test.
thread_local! {
    static CURRENT_BOT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Hands out bot ids. Starts at 1 so it never collides with the untagged 0.
static NEXT_BOT_ID: AtomicU64 = AtomicU64::new(1);

/// The last key failure per bot. A `Vec` rather than a map because there are as
/// many entries as configured bots - a handful, scanned rarely.
static LAST_FAILURE: Mutex<Vec<(u64, Instant)>> = Mutex::new(Vec::new());

/// Claim a fresh bot id, to be given to every thread that bot runs on.
pub fn next_bot_id() -> u64 {
    NEXT_BOT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Tag this thread as belonging to `id`. Call on the bot's own thread and from
/// its runtime's `on_thread_start`.
pub fn set_current_bot(id: u64) {
    CURRENT_BOT.with(|c| c.set(id));
}

/// The bot this thread belongs to, or 0 when untagged.
pub fn current_bot() -> u64 {
    CURRENT_BOT.with(|c| c.get())
}

/// Whether `last` is recent enough to explain something happening at `now`.
pub fn is_recent(last: Option<Instant>, now: Instant, within: Duration) -> bool {
    match last {
        // `checked_duration_since` rather than subtraction: a failure stamped
        // marginally after `now` (two threads reading the clock) would panic on
        // a plain subtraction.
        Some(t) => now.checked_duration_since(t).is_some_and(|age| age <= within),
        None => false,
    }
}

/// Record that Spotify refused an audio key, against the bot whose thread
/// reported it.
pub fn note_failure() {
    let id = current_bot();
    let now = Instant::now();
    let mut failures = LAST_FAILURE.lock();
    match failures.iter_mut().find(|(bot, _)| *bot == id) {
        Some((_, at)) => *at = now,
        None => failures.push((id, now)),
    }
}

/// Whether this bot hit a key failure recently enough to explain a skip now.
/// Another bot's failure is not this bot's explanation.
pub fn failed_recently() -> bool {
    let id = current_bot();
    let last = LAST_FAILURE
        .lock()
        .iter()
        .find(|(bot, _)| *bot == id)
        .map(|(_, at)| *at);
    is_recent(last, Instant::now(), BLAME_WINDOW)
}

/// Serialises every test that touches the process-wide failure record.
///
/// It lives here, beside the state, because the tests that touch it are in two
/// modules: this one and the logging layer's. A lock per module would guard
/// nothing - they would run concurrently and clear each other's state.
#[cfg(test)]
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Take exclusive use of the failure record, and start from a clean one.
#[cfg(test)]
pub fn test_guard() -> parking_lot::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock();
    reset_for_test();
    guard
}

/// Clear every recorded failure and untag this thread. Tests share these
/// globals, so each must start from a known state.
#[cfg(test)]
pub fn reset_for_test() {
    LAST_FAILURE.lock().clear();
    set_current_bot(0);
}

/// Whether a log line is librespot reporting a refused audio key.
///
/// Matches the two shapes it produces: the raw `error audio key <n> <n>` from
/// the channel, and the `audio key error` inside the "continuing without
/// decryption" warning.
pub fn is_audio_key_failure(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("error audio key") || m.contains("audio key error")
}

/// What to tell the user when a track is skipped.
pub fn skip_reason(after_key_failure: bool) -> &'static str {
    if after_key_failure {
        // Deliberately lists both causes: from here the two are
        // indistinguishable, and naming only one sends people hunting wrong.
        "Spotify refused the audio key for this track, so it could not be \
         played. This usually means the account is not premium, or it is \
         streaming on another device."
    } else {
        "Track unavailable, skipping."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_inside_the_window_explains_a_skip() {
        let now = Instant::now();
        let failed = now - Duration::from_secs(2);
        assert!(is_recent(Some(failed), now, BLAME_WINDOW));
    }

    #[test]
    fn an_old_failure_does_not_explain_a_later_skip() {
        // A key failure ten minutes ago must not relabel an unrelated skip.
        let now = Instant::now();
        let failed = now - Duration::from_secs(600);
        assert!(!is_recent(Some(failed), now, BLAME_WINDOW));
    }

    #[test]
    fn no_failure_at_all_explains_nothing() {
        assert!(!is_recent(None, Instant::now(), BLAME_WINDOW));
    }

    #[test]
    fn a_timestamp_from_the_future_does_not_panic() {
        // Two threads reading the clock can order stamps unexpectedly; plain
        // subtraction would panic here.
        let now = Instant::now();
        let future = now + Duration::from_secs(1);
        assert!(!is_recent(Some(future), now, BLAME_WINDOW));
    }

    #[test]
    fn librespot_key_failures_are_recognised() {
        // Both shapes seen in a real log.
        assert!(is_audio_key_failure("error audio key 0 1"));
        assert!(is_audio_key_failure(
            "Unable to load key, continuing without decryption: Service unavailable { audio key error }"
        ));
    }

    #[test]
    fn unrelated_log_lines_are_not_mistaken_for_key_failures() {
        assert!(!is_audio_key_failure("Loading <Molinos De Viento>"));
        assert!(!is_audio_key_failure("invalid mpeg audio header"));
        assert!(!is_audio_key_failure("skipping junk at 4096 bytes"));
        // "audio" and "key" both present, but not a key failure.
        assert!(!is_audio_key_failure("audio pipeline keyed to stream 3"));
    }

    #[test]
    fn one_bots_key_failure_does_not_explain_another_bots_skip() {
        // The tray runs every bot in one process. Before this, bot A hitting a
        // key failure made bot B's unrelated skip report the same cause - the
        // exact misleading diagnostic this module exists to remove.
        let _guard = test_guard();

        set_current_bot(1);
        note_failure();
        assert!(failed_recently(), "the bot that failed should see it");

        set_current_bot(2);
        assert!(
            !failed_recently(),
            "a sibling bot was blamed for a failure that was not its own"
        );

        set_current_bot(1);
        assert!(failed_recently(), "the failure still belongs to bot 1");
        reset_for_test();
    }

    #[test]
    fn a_failure_is_seen_across_the_threads_of_the_same_bot() {
        // librespot logs the failure on a runtime worker; the skip is handled
        // on another thread of the same runtime. Both carry the same tag, so
        // the failure has to cross between them.
        let _guard = test_guard();

        let logger = std::thread::spawn(|| {
            set_current_bot(7);
            note_failure();
        });
        logger.join().unwrap();

        let handler = std::thread::spawn(|| {
            set_current_bot(7);
            failed_recently()
        });
        assert!(
            handler.join().unwrap(),
            "a failure logged on one thread of a bot was invisible on another"
        );
        reset_for_test();
    }

    #[test]
    fn an_untagged_process_still_works() {
        // The CLI runs one bot and never tags anything; it must behave as it
        // always did rather than silently never blaming the key.
        let _guard = test_guard();
        assert_eq!(current_bot(), 0);
        assert!(!failed_recently());
        note_failure();
        assert!(failed_recently());
        reset_for_test();
    }

    #[test]
    fn repeated_failures_from_one_bot_do_not_grow_the_record() {
        // The record is keyed by bot, not by occurrence. A bot that fails
        // every track for an hour must leave one entry, not thousands. The
        // matching half of this - that a restart reuses its bot's id rather
        // than claiming a new one - is why the id lives on BotInstance.
        let _guard = test_guard();

        set_current_bot(42);
        for _ in 0..500 {
            note_failure();
        }
        assert_eq!(LAST_FAILURE.lock().len(), 1);

        set_current_bot(43);
        note_failure();
        assert_eq!(LAST_FAILURE.lock().len(), 2, "a second bot adds one entry");
        reset_for_test();
    }

    #[test]
    fn every_bot_gets_a_distinct_id_and_never_the_untagged_one() {
        let a = next_bot_id();
        let b = next_bot_id();
        assert_ne!(a, b);
        assert!(a > 0 && b > 0, "0 means untagged and must not be handed out");
    }

    #[test]
    fn the_skip_message_names_the_real_cause_only_after_a_key_failure() {
        let blamed = skip_reason(true);
        assert!(blamed.contains("audio key"));
        assert!(blamed.contains("premium"), "must name the likeliest cause");
        assert!(blamed.contains("another device"), "must name the other cause");

        let plain = skip_reason(false);
        assert!(!plain.contains("audio key"), "do not blame keys without evidence");
    }
}
