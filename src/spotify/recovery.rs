//! Session-recovery policy for the Spotify engine.
//!
//! librespot 0.8's `Session` is single-use: once its connection dies
//! (`session.is_invalid()` is permanently `true`), it can never be reconnected —
//! the connection sender is a `OnceLock`, so `session.connect()` on the same
//! Session always fails with `Session is not connected`. The only recovery is to
//! build a brand-new `Session` (and Player/metadata from it).
//!
//! This module holds the *policy* for that recovery — the backoff schedule, the
//! attempt cap, and the single-flight guard — as small, pure, testable pieces.
//! The async driver that actually rebuilds the engine lives in the runner and
//! consumes these.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Delay before each recovery attempt. The first is deliberately short so a
/// transient session blip (the common case) recovers in ~1s and is barely
/// noticeable; the delays then grow so a genuinely dead or rate-limiting Spotify
/// is never hammered (IP-block safety). The length of this array is the hard
/// attempt cap.
pub const RECOVERY_BACKOFF: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

/// Maximum number of rebuild attempts before giving up. After this the bot stops
/// trying automatically and waits for a manual/lazy re-trigger.
pub const MAX_ATTEMPTS: usize = RECOVERY_BACKOFF.len();

/// Result of a recovery cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// Session rebuilt and playback resumed.
    Recovered,
    /// All attempts exhausted; auto-recovery stops until a manual/lazy retrigger.
    GaveUp,
}

/// Delay to wait before attempt `attempt` (0-based), or `None` when the attempt
/// cap is exceeded (the caller should give up).
///
/// Attempt 0 is preceded by the first (short) delay rather than firing
/// instantly, so a transient blip gets a moment to settle before a full rebuild.
pub fn delay_before_attempt(attempt: usize) -> Option<Duration> {
    RECOVERY_BACKOFF.get(attempt).copied()
}

/// Single-flight guard: ensures only one recovery cycle runs at a time even when
/// both the supervisor poll and the `EndOfTrack` guard detect the death at once.
#[derive(Debug, Default)]
pub struct RecoveryGuard {
    active: AtomicBool,
}

impl RecoveryGuard {
    pub const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
        }
    }

    /// Try to become the one running recovery. Returns `true` for exactly one
    /// caller until [`finish`](Self::finish) is called; concurrent callers get
    /// `false` and should do nothing.
    pub fn try_begin(&self) -> bool {
        self.active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Whether a recovery cycle is currently in progress.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Release the guard so a future death can start a new cycle.
    pub fn finish(&self) {
        self.active.store(false, Ordering::Release);
    }
}

/// Resume seek target after a recovery: rewind slightly from where playback died
/// so the transition feels continuous rather than clipping forward.
pub const RESUME_REWIND_MS: u32 = 2000;

/// Compute the position to seek to when resuming the interrupted track.
pub fn resume_seek_ms(position_ms: u32) -> u32 {
    position_ms.saturating_sub(RESUME_REWIND_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule_is_fast_first_then_bounded() {
        assert_eq!(
            RECOVERY_BACKOFF,
            [
                Duration::from_secs(1),
                Duration::from_secs(3),
                Duration::from_secs(10),
                Duration::from_secs(30),
                Duration::from_secs(60),
            ]
        );
        // First attempt is quick (transient blips recover ~invisibly)...
        assert!(RECOVERY_BACKOFF[0] <= Duration::from_secs(1));
        // ...and delays only grow (never hammer a dead Spotify).
        for w in RECOVERY_BACKOFF.windows(2) {
            assert!(w[1] > w[0], "backoff must be monotonically increasing");
        }
    }

    #[test]
    fn max_attempts_is_schedule_length() {
        assert_eq!(MAX_ATTEMPTS, 5);
    }

    #[test]
    fn delay_before_each_attempt_then_give_up() {
        assert_eq!(delay_before_attempt(0), Some(Duration::from_secs(1)));
        assert_eq!(delay_before_attempt(1), Some(Duration::from_secs(3)));
        assert_eq!(delay_before_attempt(2), Some(Duration::from_secs(10)));
        assert_eq!(delay_before_attempt(3), Some(Duration::from_secs(30)));
        assert_eq!(delay_before_attempt(4), Some(Duration::from_secs(60)));
        // Attempt index 5 exceeds the cap -> give up.
        assert_eq!(delay_before_attempt(5), None);
        assert_eq!(delay_before_attempt(100), None);
    }

    #[test]
    fn guard_is_single_flight() {
        let g = RecoveryGuard::new();
        assert!(!g.is_active());
        assert!(g.try_begin(), "first caller wins");
        assert!(g.is_active());
        assert!(!g.try_begin(), "second concurrent caller is rejected");
        assert!(!g.try_begin());
        g.finish();
        assert!(!g.is_active());
        assert!(g.try_begin(), "after finish a new cycle can begin");
    }

    #[test]
    fn resume_seek_rewinds_but_never_underflows() {
        assert_eq!(resume_seek_ms(60_000), 58_000);
        assert_eq!(resume_seek_ms(2_000), 0);
        assert_eq!(resume_seek_ms(500), 0); // saturating, no underflow panic
        assert_eq!(resume_seek_ms(0), 0);
    }

    /// Documents the exact sequence the async driver walks: wait, attempt, ...
    /// five times, then give up.
    #[test]
    fn driver_sequence_is_five_bounded_attempts() {
        let mut delays = Vec::new();
        let mut attempt = 0;
        while let Some(d) = delay_before_attempt(attempt) {
            delays.push(d);
            attempt += 1;
        }
        assert_eq!(delays.len(), MAX_ATTEMPTS);
        assert_eq!(delays.last(), Some(&Duration::from_secs(60)));
    }
}
