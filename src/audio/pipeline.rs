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
    shutdown_flag: Arc<AtomicBool>,
    volume_controller: VolumeController,
    framer: Framer,
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
            shutdown_flag,
            pos_ms,
            volume_controller,
            framer: Framer::new(FRAME_SIZE * 4),
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

            // Check if we need to reset (new track loaded)
            if self.reset_flag.swap(false, Ordering::Relaxed) {
                // Drain all old PCM from channel so stale audio isn't injected
                while self.audio_rx.try_recv().is_ok() {}
                // Flush any old audio from TeamTalk
                crate::tt::audio_inject::flush_audio(&self.client);
                // Ensure voice transmission is disabled (like Python bot does before each track)
                self.client.enable_voice_transmission(false);
                // New stream ID for new track (like Python bot: time-based)
                self.stream_id = new_stream_id();
                self.framer.clear();
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

            // When paused, drain all buffered audio and skip injection
            if self.pause_flag.load(Ordering::Relaxed) {
                // Drain channel to prevent backpressure on the sink
                while self.audio_rx.try_recv().is_ok() {}
                self.framer.clear();
                self.next_block_time = None;
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }

            // Receive PCM data from the sink (with timeout so reset flag is checked)
            match self.audio_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(pcm_data) => {
                    self.framer.push(&pcm_data);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    continue; // Loop back to check reset flag
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

            while self.framer.len() >= FRAME_SIZE {
                // Check reset or pause mid-injection (for instant stop/pause)
                if self.reset_flag.load(Ordering::Relaxed) || self.pause_flag.load(Ordering::Relaxed) {
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
}
