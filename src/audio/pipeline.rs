use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use teamtalk::Client;

use crate::audio::volume::VolumeController;
use crate::config::BotConfig;
use crate::tt::audio_inject;

/// Use librespot's native sample rate - no resampling needed
const SAMPLE_RATE: i32 = 44100;
const CHANNELS: i32 = 2;
/// 20ms frames at 44100Hz stereo = 882 samples/channel × 2 channels = 1764 i16 values
const FRAME_SAMPLES: usize = 882;
const FRAME_SIZE: usize = FRAME_SAMPLES * CHANNELS as usize; // 1764

/// Block duration in microseconds (~20ms)
const BLOCK_DURATION_US: u64 = (FRAME_SAMPLES as u64 * 1_000_000) / SAMPLE_RATE as u64;

/// Accumulates incoming PCM and hands out fixed-size frames. Backed by a
/// `VecDeque` so consuming a frame is O(frame) with no O(remaining) memmove —
/// the previous `Vec::drain(..FRAME_SIZE)` shifted every leftover sample to the
/// front on every 20ms frame.
struct Framer {
    buf: VecDeque<i16>,
}

impl Framer {
    fn new(capacity: usize) -> Self {
        Self { buf: VecDeque::with_capacity(capacity) }
    }

    fn push(&mut self, samples: &[i16]) {
        self.buf.extend(samples.iter().copied());
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    fn clear(&mut self) {
        self.buf.clear();
    }

    /// Pop exactly `out.len()` samples into `out`. Returns false (leaving `out`
    /// untouched) if fewer than that are buffered.
    fn pop_frame(&mut self, out: &mut [i16]) -> bool {
        if self.buf.len() < out.len() {
            return false;
        }
        for slot in out.iter_mut() {
            *slot = self.buf.pop_front().unwrap();
        }
        true
    }
}

/// Number of consecutive empty 50ms channel polls after which a closed gate
/// opens anyway: the producer has gone quiet mid-fill (end of stream or a hard
/// stall), so play out whatever is buffered instead of holding it hostage.
const IDLE_POLLS_BEFORE_FLUSH: u32 = 6;

/// Holds back injection after a (re)start until `jitter_buffer_ms` worth of
/// audio is buffered, absorbing bursty producer starts. Latches open once
/// filled; `rearm` closes it again for the next track/seek. A target of 0
/// keeps the gate permanently open (previous pipeline behavior).
struct PrebufferGate {
    target_samples: usize,
    open: bool,
    idle_polls: u32,
}

impl PrebufferGate {
    fn new(jitter_buffer_ms: u32) -> Self {
        let target_samples =
            (SAMPLE_RATE as u64 * CHANNELS as u64 * jitter_buffer_ms as u64 / 1000) as usize;
        Self { target_samples, open: target_samples == 0, idle_polls: 0 }
    }

    /// Close the gate again (new track / seek): buffer must refill before
    /// injection resumes.
    fn rearm(&mut self) {
        self.open = self.target_samples == 0;
        self.idle_polls = 0;
    }

    /// Data arrived; `buffered` is the framer fill level. Opens the gate once
    /// the target is reached. Returns whether the gate is open.
    fn on_data(&mut self, buffered: usize) -> bool {
        self.idle_polls = 0;
        if buffered >= self.target_samples {
            self.open = true;
        }
        self.open
    }

    /// A channel poll timed out with no data. With samples stuck behind a
    /// closed gate this counts toward the flush threshold. Returns whether
    /// the gate is open.
    fn on_idle(&mut self, buffered: usize) -> bool {
        if !self.open && buffered > 0 {
            self.idle_polls += 1;
            if self.idle_polls >= IDLE_POLLS_BEFORE_FLUSH {
                self.open = true;
            }
        }
        self.open
    }
}

/// Monotonic, always-positive stream IDs. The previous millisecond-based scheme
/// could collide when two tracks started within the same millisecond and could
/// produce negative IDs once the value overflowed i32.
fn new_stream_id() -> i32 {
    static NEXT_STREAM_ID: AtomicI32 = AtomicI32::new(1);
    let id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);
    if id > 0 {
        id
    } else {
        // Wrapped past i32::MAX: restart the sequence at 1.
        NEXT_STREAM_ID.store(2, Ordering::Relaxed);
        1
    }
}

pub struct AudioPipeline {
    audio_rx: Receiver<Vec<i16>>,
    client: Arc<Client>,
    volume: Arc<AtomicU8>,
    max_volume: u8,
    reset_flag: Arc<AtomicBool>,
    timing_reset_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    /// Restart the injected stream in place (channel move): flush TeamTalk,
    /// drop buffered PCM, re-arm the jitter gate — but keep `stream_id`,
    /// `sample_index` and `pos_ms` so the playback position doesn't jump.
    stream_flush_flag: Arc<AtomicBool>,
    /// True while the pipeline has nothing buffered to play (channel empty,
    /// less than one frame accumulated). Read by the runner's end-of-track
    /// drain wait so the tail of a song finishes before the queue advances.
    drained_flag: Arc<AtomicBool>,
    shutdown_flag: Arc<AtomicBool>,
    volume_controller: VolumeController,
    framer: Framer,
    prebuffer: PrebufferGate,
    frame_buf: Vec<i16>,
    stream_id: i32,
    sample_index: u32,
    /// Milliseconds of audio actually injected since the last reset. Paced at
    /// realtime by frame injection, so it reflects true playback position (the
    /// YouTube player reads this to report position, rather than counting
    /// frames buffered ahead in the channel).
    pos_ms: Arc<AtomicU32>,
    next_block_time: Option<Instant>,
}

impl AudioPipeline {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        audio_rx: Receiver<Vec<i16>>,
        client: Arc<Client>,
        volume: Arc<AtomicU8>,
        reset_flag: Arc<AtomicBool>,
        timing_reset_flag: Arc<AtomicBool>,
        pause_flag: Arc<AtomicBool>,
        stream_flush_flag: Arc<AtomicBool>,
        drained_flag: Arc<AtomicBool>,
        shutdown_flag: Arc<AtomicBool>,
        pos_ms: Arc<AtomicU32>,
        config: &BotConfig,
    ) -> Self {
        let mut volume_controller = VolumeController::new(config.volume_ramp_step);
        volume_controller.set_target(config.volume, config.max_volume);

        Self {
            audio_rx,
            client,
            volume,
            max_volume: config.max_volume,
            reset_flag,
            timing_reset_flag,
            pause_flag,
            stream_flush_flag,
            drained_flag,
            shutdown_flag,
            pos_ms,
            volume_controller,
            framer: Framer::new(FRAME_SIZE * 4),
            prebuffer: PrebufferGate::new(config.jitter_buffer_ms),
            frame_buf: vec![0i16; FRAME_SIZE],
            stream_id: new_stream_id(),
            sample_index: 0,
            next_block_time: None,
        }
    }

    /// Run the audio pipeline loop. This blocks the current thread.
    pub fn run(&mut self) {
        tracing::info!("Audio pipeline started");

        loop {
            if self.shutdown_flag.load(Ordering::Relaxed) {
                tracing::info!("Audio pipeline shutting down");
                break;
            }

            // Publish whether anything is left to play (for the end-of-track
            // drain wait). Refreshed every iteration, including while paused.
            self.drained_flag.store(
                self.audio_rx.is_empty() && self.framer.len() < FRAME_SIZE,
                Ordering::Relaxed,
            );

            // Check if we need to reset (new track loaded)
            if self.reset_flag.swap(false, Ordering::Relaxed) {
                // Drain all old PCM from channel so stale audio isn't injected
                while self.audio_rx.try_recv().is_ok() {}
                // Flush any old audio from TeamTalk
                crate::tt::audio_inject::flush_audio(&self.client);
                // Ensure voice transmission is disabled (like Python bot does before each track)
                let _ = self.client.enable_voice_transmission(false);
                // New stream ID for new track (like Python bot: time-based)
                self.stream_id = new_stream_id();
                self.framer.clear();
                self.prebuffer.rearm();
                self.next_block_time = None;
                self.sample_index = 0;
                self.pos_ms.store(0, Ordering::Relaxed);
                tracing::info!("Audio pipeline reset for new track (stream_id={})", self.stream_id);
            }

            // Check timing-only reset (for resume from pause)
            if self.timing_reset_flag.swap(false, Ordering::Relaxed) {
                self.next_block_time = None;
                tracing::debug!("Audio pipeline timing reset (resume)");
            }

            // When paused, stop injecting but KEEP everything buffered: the
            // players stop producing while paused, and resume must continue
            // from the exact note the listener last heard. The old drain here
            // silently skipped the buffered seconds on every pause/resume.
            if self.pause_flag.load(Ordering::Relaxed) {
                self.next_block_time = None;
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }

            // In-place stream restart (channel move): end the stream in the
            // SDK, then carry on with the SAME stream_id, sample_index and —
            // unlike pause/play — the same buffered PCM. The garble the flush
            // cures lives in the SDK's per-channel stream state, not in our
            // buffered samples; dropping them (the first version of this fix)
            // skipped the several seconds of read-ahead the decoder had built
            // up on every move. Only what the SDK itself had queued but not
            // yet sent is lost.
            // Checked AFTER the pause branch (which `continue`s) so a move
            // that happens while paused leaves the flag set and the flush
            // runs when playback resumes — flushing mid-pause would be
            // consumed with nothing to restart.
            if self.stream_flush_flag.swap(false, Ordering::Relaxed) {
                crate::tt::audio_inject::flush_audio(&self.client);
                self.prebuffer.rearm();
                self.next_block_time = None;
                tracing::info!(
                    "Audio stream flushed after channel move (stream_id={} continues, buffer kept)",
                    self.stream_id
                );
            }

            // Receive PCM data from the sink (with timeout so reset flag is checked)
            match self.audio_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(pcm_data) => {
                    self.framer.push(&pcm_data);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // Producer idle: give a closed gate a chance to flush any
                    // trapped tail (short track / end of stream), otherwise
                    // loop back to check the reset flag.
                    if !self.prebuffer.on_idle(self.framer.len()) {
                        continue;
                    }
                    if self.framer.len() < FRAME_SIZE {
                        continue;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    tracing::info!("Audio pipeline channel closed, exiting");
                    break;
                }
            }

            // Drain any additional buffered data without blocking
            while let Ok(pcm_data) = self.audio_rx.try_recv() {
                self.framer.push(&pcm_data);
            }

            // Hold injection until the jitter buffer has filled (no-op at 0ms).
            if !self.prebuffer.on_data(self.framer.len()) {
                continue;
            }

            while self.framer.len() >= FRAME_SIZE {
                // Check reset, pause or stream-flush mid-injection so a stop,
                // pause or channel move interrupts a buffered backlog promptly.
                if self.reset_flag.load(Ordering::Relaxed)
                    || self.pause_flag.load(Ordering::Relaxed)
                    || self.stream_flush_flag.load(Ordering::Relaxed)
                {
                    break;
                }
                if !self.framer.pop_frame(&mut self.frame_buf) {
                    break;
                }

                if self.sample_index == 0 {
                    tracing::info!("First audio frame ready, injecting (stream_id={})", self.stream_id);
                }

                // Update volume
                let vol = self.volume.load(Ordering::Relaxed);
                self.volume_controller.set_target(vol, self.max_volume);
                self.volume_controller.apply(&mut self.frame_buf);

                // Timing: wait until it's time to inject this block
                self.wait_for_next_block();

                // Inject, retrying briefly on transient failure. Cap the total
                // stall at ~200ms (20 x 10ms) then drop the frame: a wedged TT
                // client must not block the audio thread for ~1s per frame,
                // which back-pressures the whole producer chain.
                const MAX_INJECT_RETRIES: u32 = 20;
                let mut retries = 0u32;
                while !audio_inject::inject_audio_block(
                    &self.client,
                    &self.frame_buf,
                    SAMPLE_RATE,
                    CHANNELS,
                    self.stream_id,
                    self.sample_index,
                ) {
                    retries += 1;
                    if self.shutdown_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    if retries == 1 {
                        tracing::warn!("insert_audio_block failed, retrying...");
                    }
                    if retries > MAX_INJECT_RETRIES {
                        tracing::error!("insert_audio_block failed {MAX_INJECT_RETRIES} times, skipping frame");
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }

                self.sample_index = self.sample_index.wrapping_add(FRAME_SAMPLES as u32);
                // Publish realtime playback position (ms injected since reset).
                self.pos_ms.store(
                    (self.sample_index as u64 * 1000 / SAMPLE_RATE as u64) as u32,
                    Ordering::Relaxed,
                );
            }
        }
    }

    /// Sleep until it's time to inject the next audio block.
    /// Matches Python bot's timing: next_block_time starts at now, sleep delay, then advance.
    fn wait_for_next_block(&mut self) {
        let now = Instant::now();
        let block_duration = Duration::from_micros(BLOCK_DURATION_US);

        if self.next_block_time.is_none() {
            self.next_block_time = Some(now);
        }

        let next_time = self.next_block_time.unwrap();
        if next_time > now {
            std::thread::sleep(next_time - now);
        } else if now.duration_since(next_time) > Duration::from_millis(200) {
            // Drift too large - reset
            tracing::debug!("Audio timing drift, resetting");
            self.next_block_time = Some(now);
        }

        // Advance for next block
        self.next_block_time = Some(self.next_block_time.unwrap() + block_duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framer_yields_full_frames_in_order() {
        let mut framer = Framer::new(16);
        framer.push(&[1, 2, 3, 4, 5]);
        framer.push(&[6, 7, 8]);
        assert_eq!(framer.len(), 8);

        let mut frame = [0i16; 4];
        assert!(framer.pop_frame(&mut frame));
        assert_eq!(frame, [1, 2, 3, 4]);
        assert_eq!(framer.len(), 4);

        assert!(framer.pop_frame(&mut frame));
        assert_eq!(frame, [5, 6, 7, 8]);
        assert_eq!(framer.len(), 0);
    }

    #[test]
    fn framer_pop_fails_when_underfull_and_leaves_data() {
        let mut framer = Framer::new(16);
        framer.push(&[1, 2, 3]);
        let mut frame = [9i16; 4];
        assert!(!framer.pop_frame(&mut frame));
        // Output untouched, samples still buffered.
        assert_eq!(frame, [9, 9, 9, 9]);
        assert_eq!(framer.len(), 3);
    }

    #[test]
    fn framer_clear_empties() {
        let mut framer = Framer::new(16);
        framer.push(&[1, 2, 3, 4, 5]);
        framer.clear();
        assert_eq!(framer.len(), 0);
        let mut frame = [0i16; 2];
        assert!(!framer.pop_frame(&mut frame));
    }

    #[test]
    fn stream_ids_are_positive_and_distinct() {
        let a = new_stream_id();
        let b = new_stream_id();
        assert!(a > 0 && b > 0);
        assert_ne!(a, b);
    }

    #[test]
    fn prebuffer_gate_zero_ms_is_always_open() {
        let mut gate = PrebufferGate::new(0);
        assert!(gate.on_data(0));
        assert!(gate.on_idle(0));
        gate.rearm();
        assert!(gate.on_data(0));
    }

    #[test]
    fn prebuffer_gate_holds_until_target_then_latches_open() {
        // 400ms at 44100Hz stereo = 35280 samples
        let mut gate = PrebufferGate::new(400);
        assert!(!gate.on_data(0));
        assert!(!gate.on_data(35279));
        assert!(gate.on_data(35280));
        // Latched: stays open even as the buffer drains below target.
        assert!(gate.on_data(100));
        assert!(gate.on_idle(0));
    }

    #[test]
    fn prebuffer_gate_rearm_closes_again() {
        let mut gate = PrebufferGate::new(100); // 8820 samples
        assert!(gate.on_data(8820));
        gate.rearm();
        assert!(!gate.on_data(8819));
        assert!(gate.on_data(8820));
    }

    #[test]
    fn prebuffer_gate_flushes_after_idle_polls_with_data() {
        let mut gate = PrebufferGate::new(400);
        assert!(!gate.on_data(5000));
        for _ in 0..IDLE_POLLS_BEFORE_FLUSH - 1 {
            assert!(!gate.on_idle(5000));
        }
        // Producer quiet with samples stuck behind the gate: flush.
        assert!(gate.on_idle(5000));
    }

    #[test]
    fn prebuffer_gate_stays_armed_while_idle_and_empty() {
        let mut gate = PrebufferGate::new(400);
        // Idle-while-empty is the normal waiting-for-track state; never open.
        for _ in 0..IDLE_POLLS_BEFORE_FLUSH * 3 {
            assert!(!gate.on_idle(0));
        }
        assert!(!gate.on_data(100));
    }

    #[test]
    fn prebuffer_gate_data_resets_idle_streak() {
        let mut gate = PrebufferGate::new(400);
        assert!(!gate.on_data(5000));
        for _ in 0..IDLE_POLLS_BEFORE_FLUSH - 1 {
            assert!(!gate.on_idle(5000));
        }
        // Fresh data below target resets the idle counter.
        assert!(!gate.on_data(6000));
        for _ in 0..IDLE_POLLS_BEFORE_FLUSH - 1 {
            assert!(!gate.on_idle(6000));
        }
        assert!(gate.on_idle(6000));
    }
}
