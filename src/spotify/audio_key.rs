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

use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// How long after a key failure a skipped track is still blamed on it. Long
/// enough to cover the decode attempt that follows (about 2s in practice),
/// short enough that an unrelated skip later is not mislabelled.
pub const BLAME_WINDOW: Duration = Duration::from_secs(15);

static LAST_FAILURE: Mutex<Option<Instant>> = Mutex::new(None);

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

/// Record that Spotify refused an audio key.
pub fn note_failure() {
    *LAST_FAILURE.lock() = Some(Instant::now());
}

/// Whether a key failure happened recently enough to explain a skip now.
pub fn failed_recently() -> bool {
    is_recent(*LAST_FAILURE.lock(), Instant::now(), BLAME_WINDOW)
}

/// Clear the recorded failure. Tests share this global, so each must start
/// from a known state.
#[cfg(test)]
pub fn reset_for_test() {
    *LAST_FAILURE.lock() = None;
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
    fn the_skip_message_names_the_real_cause_only_after_a_key_failure() {
        let blamed = skip_reason(true);
        assert!(blamed.contains("audio key"));
        assert!(blamed.contains("premium"), "must name the likeliest cause");
        assert!(blamed.contains("another device"), "must name the other cause");

        let plain = skip_reason(false);
        assert!(!plain.contains("audio key"), "do not blame keys without evidence");
    }
}
