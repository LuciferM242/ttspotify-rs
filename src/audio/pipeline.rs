use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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

pub struct AudioPipeline {
    audio_rx: Receiver<Vec<i16>>,
    client: Arc<Client>,
    volume: Arc<AtomicU8>,
    max_volume: u8,
    reset_flag: Arc<AtomicBool>,
    timing_reset_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    volume_controller: VolumeController,
    accumulator: Vec<i16>,
    stream_id: i32,
    sample_index: u32,
    next_block_time: Option<Instant>,
}

impl AudioPipeline {
    pub fn new(
        audio_rx: Receiver<Vec<i16>>,
        client: Arc<Client>,
        volume: Arc<AtomicU8>,
        reset_flag: Arc<AtomicBool>,
        timing_reset_flag: Arc<AtomicBool>,
        pause_flag: Arc<AtomicBool>,
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
            volume_controller,
            accumulator: Vec::with_capacity(FRAME_SIZE * 4),
            stream_id: (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() & 0xFFFFFFFF) as i32,
            sample_index: 0,
            next_block_time: None,
        }
    }

    /// Run the audio pipeline loop. This blocks the current thread.
    pub fn run(&mut self) {
        tracing::info!("Audio pipeline started");

        loop {
            // Check if we need to reset (new track loaded)
            if self.reset_flag.swap(false, Ordering::Relaxed) {
                // Drain all old PCM from channel so stale audio isn't injected
                while self.audio_rx.try_recv().is_ok() {}
                // Flush any old audio from TeamTalk
                crate::tt::audio_inject::flush_audio(&self.client);
                // Ensure voice transmission is disabled (like Python bot does before each track)
                self.client.enable_voice_transmission(false);
                // New stream ID for new track (like Python bot: time-based)
                self.stream_id = (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() & 0xFFFFFFFF) as i32;
                self.accumulator.clear();
                self.next_block_time = None;
                self.sample_index = 0;
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
                self.accumulator.clear();
                self.next_block_time = None;
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }

            // Receive PCM data from the sink (with timeout so reset flag is checked)
            match self.audio_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(pcm_data) => {
                    self.accumulator.extend_from_slice(&pcm_data);
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
                self.accumulator.extend_from_slice(&pcm_data);
            }

            // Inject complete frames
            while self.accumulator.len() >= FRAME_SIZE {
                // Check reset or pause mid-injection (for instant stop/pause)
                if self.reset_flag.load(Ordering::Relaxed) || self.pause_flag.load(Ordering::Relaxed) {
                    break;
                }
                let mut frame: Vec<i16> = self.accumulator.drain(..FRAME_SIZE).collect();
                if frame.len() < FRAME_SIZE {
                    break;
                }

                if self.sample_index == 0 {
                    tracing::info!("First audio frame ready, injecting (stream_id={})", self.stream_id);
                }

                // Update volume
                let vol = self.volume.load(Ordering::Relaxed);
                self.volume_controller.set_target(vol, self.max_volume);
                self.volume_controller.apply(&mut frame);

                // Timing: wait until it's time to inject this block
                self.wait_for_next_block();

                // Inject (retry until success, like Python bot)
                let mut retries = 0u32;
                while !audio_inject::inject_audio_block(
                    &self.client,
                    &frame,
                    SAMPLE_RATE,
                    CHANNELS,
                    self.stream_id,
                    self.sample_index,
                ) {
                    retries += 1;
                    if retries == 1 {
                        tracing::warn!("insert_audio_block failed, retrying...");
                    }
                    if retries > 100 {
                        tracing::error!("insert_audio_block failed 100 times, skipping frame");
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }

                self.sample_index = self.sample_index.wrapping_add(FRAME_SAMPLES as u32);
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
