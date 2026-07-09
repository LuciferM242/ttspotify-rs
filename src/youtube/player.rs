//! YouTube audio player.
//!
//! Per loaded track: spawns a tokio task that resolves the audio URL,
//! downloads the full M4A blob, decodes it with symphonia on a blocking
//! worker, resamples to 44.1k stereo via rubato, and pushes Vec<i16>
//! frames into the same crossbeam channel the audio pipeline consumes.
//! From the pipeline's perspective YouTube and Spotify are indistinguishable
//! producers.

use std::io::{BufReader, Read, Seek, SeekFrom};
use std::process::ChildStdout;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;
use parking_lot::Mutex;
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::bot::commands::BotCommand;
use crate::bot::state::SharedState;
use crate::player::MediaPlayer;
use crate::youtube::metadata::YouTubeMetadata;

/// Audio pipeline expects this rate. librespot/Spotify side already produces 44.1k.
const PIPELINE_RATE: u32 = 44_100;
const CHANNELS: usize = 2;

/// A track-end signal is stale when its generation no longer matches the
/// currently-active one — i.e. the user skipped/stopped/replaced the track
/// before its natural end reached the command processor.
fn generation_is_stale(signal_gen: u64, current_gen: u64) -> bool {
    signal_gen != current_gen
}

/// Per-track control flags. Recreated on every `load`.
#[derive(Default)]
struct TrackControl {
    paused: AtomicBool,
    stopped: AtomicBool,
    /// Current playback position in milliseconds, updated by the decode loop.
    /// Seeded with the seek offset on a seek-triggered reload.
    position_ms: AtomicU32,
}

pub struct YouTubePlayer {
    audio_tx: Sender<Vec<i16>>,
    metadata: Arc<YouTubeMetadata>,
    /// Signals end-of-track (`BotCommand::TrackEnded`) when the stream finishes.
    cmd_tx: UnboundedSender<BotCommand>,
    /// Shared player state; the decode loop writes `position_ms` here so the
    /// `c` command and seek arithmetic see live YouTube positions.
    state: SharedState,
    /// Active track's task + control. `None` when idle.
    current: Arc<Mutex<Option<(JoinHandle<()>, Arc<TrackControl>)>>>,
    /// Video id of the currently-loaded track, so `seek` can re-spawn it at a
    /// new offset (yt-dlp/symphonia can't seek a non-seekable pipe).
    current_video_id: Arc<Mutex<Option<String>>>,
    /// Monotonic token identifying the current load. Bumped on every load and
    /// on stop/abort so a stale task's end-of-track signal can be recognized
    /// and discarded instead of double-advancing the queue.
    generation: Arc<AtomicU64>,
}

impl YouTubePlayer {
    pub fn new(
        audio_tx: Sender<Vec<i16>>,
        metadata: Arc<YouTubeMetadata>,
        cmd_tx: UnboundedSender<BotCommand>,
        state: SharedState,
    ) -> Self {
        Self {
            audio_tx,
            metadata,
            cmd_tx,
            state,
            current: Arc::new(Mutex::new(None)),
            current_video_id: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The generation of the currently-loaded track. A `TrackEnded` whose
    /// generation differs from this is stale and must be ignored.
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Start playing `video_id` from `start_ms`. `load` is `load_at(.., 0)`;
    /// `seek` re-invokes this to jump within the current track.
    fn load_at(&self, video_id: &str, start_ms: u32) {
        self.abort_current();
        // This track's generation token (abort_current bumped past the old one).
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        *self.current_video_id.lock() = Some(video_id.to_string());

        let audio_tx = self.audio_tx.clone();
        let metadata = self.metadata.clone();
        let cmd_tx = self.cmd_tx.clone();
        let state = self.state.clone();
        let video_id = video_id.to_string();
        let ctrl = Arc::new(TrackControl::default());
        ctrl.position_ms.store(start_ms, Ordering::Relaxed);
        let ctrl_for_task = ctrl.clone();

        let handle = tokio::spawn(async move {
            let error = match play_track(video_id.clone(), start_ms, metadata, audio_tx, ctrl_for_task, state).await {
                Ok(()) => None,
                Err(e) => {
                    tracing::error!("YouTube playback failed (video_id={video_id}): {e}");
                    Some(e)
                }
            };
            // Signal end-of-track tagged with this generation. The processor
            // drops it if a newer load/stop has since bumped the generation.
            let _ = cmd_tx.send(BotCommand::TrackEnded { generation, error });
        });

        *self.current.lock() = Some((handle, ctrl));
    }

    /// Whether a `TrackEnded` tagged with `signal_gen` is stale (belongs to an
    /// older load than what is currently active), given the player's current
    /// generation. Extracted for testing.
    pub fn is_stale_generation(&self, signal_gen: u64) -> bool {
        generation_is_stale(signal_gen, self.current_generation())
    }

    /// Stop and abort any currently-running track task, invalidating any
    /// end-of-track signal still in flight from it.
    fn abort_current(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        let mut cur = self.current.lock();
        if let Some((handle, ctrl)) = cur.take() {
            ctrl.stopped.store(true, Ordering::Relaxed);
            handle.abort();
        }
    }
}

impl MediaPlayer for YouTubePlayer {
    fn load(&self, video_id: &str) {
        self.load_at(video_id, 0);
    }

    fn play(&self) {
        if let Some((_, ctrl)) = self.current.lock().as_ref() {
            ctrl.paused.store(false, Ordering::Relaxed);
        }
    }

    fn pause(&self) {
        if let Some((_, ctrl)) = self.current.lock().as_ref() {
            ctrl.paused.store(true, Ordering::Relaxed);
        }
    }

    fn stop(&self) {
        self.abort_current();
        *self.current_video_id.lock() = None;
    }

    fn seek(&self, position_ms: u32) {
        // A pipe can't be seeked, so re-spawn yt-dlp and decode-skip to the
        // target offset. Generation bump makes the old track's TrackEnded stale.
        let video_id = self.current_video_id.lock().clone();
        match video_id {
            Some(id) => self.load_at(&id, position_ms),
            None => tracing::debug!("YouTube seek ignored: no track loaded"),
        }
    }

    fn preload(&self, _video_id: &str) {
        // No-op: YouTube preload would mean opening a second HTTP stream.
        // Skipped for now; gapless playback is a Phase-4 concern.
    }
}

/// Spawn yt-dlp, then decode + resample its stdout on a blocking worker as
/// bytes arrive. Audio starts playing within a second; livestreams and
/// hour-long videos work without buffering the whole file.
///
/// `ctrl.stopped` set during playback kills the yt-dlp subprocess.
async fn play_track(
    video_id: String,
    start_ms: u32,
    metadata: Arc<YouTubeMetadata>,
    audio_tx: Sender<Vec<i16>>,
    ctrl: Arc<TrackControl>,
    state: SharedState,
) -> Result<(), String> {
    let mut child = metadata.spawn_ytdlp(&video_id)
        .map_err(|e| format!("yt-dlp spawn: {e}"))?;
    let stdout = child.stdout.take()
        .ok_or_else(|| "yt-dlp stdout was not piped".to_string())?;
    let stderr = child.stderr.take()
        .ok_or_else(|| "yt-dlp stderr was not piped".to_string())?;

    // Drain stderr in the background so yt-dlp doesn't block on a full pipe,
    // and so we can surface its output on errors.
    let stderr_handle = std::thread::spawn(move || -> String {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::BufReader::new(stderr), &mut buf);
        buf
    });

    let ctrl_for_kill = ctrl.clone();
    let mut child_for_kill = child;
    let watcher_handle = std::thread::spawn(move || -> Option<std::process::ExitStatus> {
        loop {
            if ctrl_for_kill.stopped.load(Ordering::Relaxed) {
                let _ = child_for_kill.kill();
                return child_for_kill.wait().ok();
            }
            match child_for_kill.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => return None,
            }
        }
    });

    let decode_result = tokio::task::spawn_blocking(move || decode_and_stream(stdout, audio_tx, ctrl, start_ms, state))
        .await
        .map_err(|e| format!("decode worker join: {e}"))?;

    let exit_status = watcher_handle.join().ok().flatten();
    let stderr_text = stderr_handle.join().unwrap_or_default();

    // If decode failed, surface yt-dlp's stderr — most likely the real cause.
    if let Err(decode_err) = &decode_result {
        let yt_err = stderr_text.lines()
            .find(|l| l.to_lowercase().contains("error"))
            .unwrap_or_else(|| stderr_text.lines().last().unwrap_or(""));
        let exit_code = exit_status.and_then(|s| s.code()).unwrap_or(-1);
        return Err(format!(
            "{decode_err} (yt-dlp exit={exit_code}, stderr: {})",
            yt_err.chars().take(300).collect::<String>()
        ));
    }
    decode_result
}

/// Non-seekable wrapper around yt-dlp's stdout pipe. Symphonia accepts
/// non-seekable sources for fragmented/streaming MP4.
struct PipeSource {
    inner: BufReader<ChildStdout>,
}

impl Read for PipeSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for PipeSource {
    fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "yt-dlp pipe is not seekable",
        ))
    }
}

impl MediaSource for PipeSource {
    fn is_seekable(&self) -> bool { false }
    fn byte_len(&self) -> Option<u64> { None }
}

/// Sync decode + resample loop. Runs on a blocking worker thread, reading
/// from the yt-dlp child stdout pipe as bytes arrive.
fn decode_and_stream(
    stdout: ChildStdout,
    audio_tx: Sender<Vec<i16>>,
    ctrl: Arc<TrackControl>,
    start_ms: u32,
    state: SharedState,
) -> Result<(), String> {
    let source = PipeSource { inner: BufReader::with_capacity(64 * 1024, stdout) };
    let mss = MediaSourceStream::new(Box::new(source), Default::default());

    let mut hint = Hint::new();
    hint.with_extension("m4a");

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;

    let mut format = probed.format;
    let track = format.default_track()
        .ok_or_else(|| "no default track".to_string())?;
    let track_id = track.id;

    let codec_params = track.codec_params.clone();
    let src_rate = codec_params.sample_rate.ok_or_else(|| "missing sample_rate".to_string())?;
    let src_channels = codec_params.channels
        .map(|c| c.count())
        .unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder make: {e}"))?;

    // Resampler chunk size: pick something that maps nicely to common rates.
    // 1024 input frames -> 1024 * 44100/48000 ~= 940 output frames at worst.
    let chunk_in: usize = 1024;
    let mut resampler = if src_rate == PIPELINE_RATE {
        None
    } else {
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        Some(SincFixedIn::<f32>::new(
            PIPELINE_RATE as f64 / src_rate as f64,
            2.0,
            params,
            chunk_in,
            CHANNELS,
        ).map_err(|e| format!("resampler new: {e}"))?)
    };

    // Per-channel scratch buffers we feed the resampler.
    let mut buf_l: Vec<f32> = Vec::with_capacity(chunk_in * 4);
    let mut buf_r: Vec<f32> = Vec::with_capacity(chunk_in * 4);

    // Seek support: decode-and-discard source frames until `start_ms` is
    // reached, then begin sending. Decoding is far faster than realtime, so a
    // skip costs a brief download, not playback lag.
    let skip_src_frames: u64 = start_ms as u64 * src_rate as u64 / 1000;
    let mut decoded_src_frames: u64 = 0;
    let mut sent_output_frames: u64 = 0;

    loop {
        if ctrl.stopped.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Pause: spin-wait at coarse granularity. Acceptable since the audio
        // pipeline already drains its buffer when paused (TT side flushes).
        while ctrl.paused.load(Ordering::Relaxed) {
            if ctrl.stopped.load(Ordering::Relaxed) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Drain any remaining buffered samples through the resampler.
                flush_remaining(resampler.as_mut(), &mut buf_l, &mut buf_r, &audio_tx, chunk_in);
                return Ok(());
            }
            Err(e) => return Err(format!("next_packet: {e}")),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(_)) => continue, // skip bad packet
            Err(e) => return Err(format!("decode: {e}")),
        };

        // Pull planar f32 channels from whatever sample format symphonia hands us.
        // `skip` is the per-packet offset that discards frames before `start_ms`.
        match decoded {
            AudioBufferRef::F32(buf) => {
                let n = buf.frames();
                let skip = skip_offset(decoded_src_frames, skip_src_frames, n);
                let l = buf.chan(0);
                let r = if src_channels >= 2 { buf.chan(1) } else { l };
                buf_l.extend_from_slice(&l[skip..n]);
                buf_r.extend_from_slice(&r[skip..n]);
                decoded_src_frames += n as u64;
            }
            AudioBufferRef::S16(buf) => {
                let n = buf.frames();
                let skip = skip_offset(decoded_src_frames, skip_src_frames, n);
                let l = buf.chan(0);
                let r = if src_channels >= 2 { buf.chan(1) } else { l };
                buf_l.extend(l[skip..n].iter().map(|&s| s as f32 / 32768.0));
                buf_r.extend(r[skip..n].iter().map(|&s| s as f32 / 32768.0));
                decoded_src_frames += n as u64;
            }
            AudioBufferRef::S32(buf) => {
                let n = buf.frames();
                let skip = skip_offset(decoded_src_frames, skip_src_frames, n);
                let l = buf.chan(0);
                let r = if src_channels >= 2 { buf.chan(1) } else { l };
                buf_l.extend(l[skip..n].iter().map(|&s| s as f32 / 2147483648.0));
                buf_r.extend(r[skip..n].iter().map(|&s| s as f32 / 2147483648.0));
                decoded_src_frames += n as u64;
            }
            other => {
                tracing::warn!("YouTube: unsupported sample format {:?}", std::mem::discriminant(&other));
                continue;
            }
        };

        // Drain in chunk_in-sized slices through the resampler.
        while buf_l.len() >= chunk_in {
            let in_l: Vec<f32> = buf_l.drain(..chunk_in).collect();
            let in_r: Vec<f32> = buf_r.drain(..chunk_in).collect();

            let frame = if let Some(ref mut rs) = resampler {
                let out = rs.process(&[in_l, in_r], None)
                    .map_err(|e| format!("resample: {e}"))?;
                interleave_to_i16(&out[0], &out[1])
            } else {
                interleave_to_i16(&in_l, &in_r)
            };

            // Advance the reported playback position by this frame's duration.
            let out_frames = (frame.len() / 2) as u64;

            // Send through the bounded channel without ever blocking, so a
            // paused or stopped track exits within ~50ms instead of stalling
            // until the audio pipeline drains.
            let mut frame = Some(frame);
            loop {
                if ctrl.stopped.load(Ordering::Relaxed) {
                    return Ok(());
                }
                match audio_tx.try_send(frame.take().expect("set in this loop")) {
                    Ok(()) => break,
                    Err(crossbeam_channel::TrySendError::Full(returned)) => {
                        frame = Some(returned);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(crossbeam_channel::TrySendError::Disconnected(_)) => return Ok(()),
                }
            }

            sent_output_frames += out_frames;
            let pos = (start_ms as u64 + sent_output_frames * 1000 / PIPELINE_RATE as u64)
                .min(u32::MAX as u64) as u32;
            ctrl.position_ms.store(pos, Ordering::Relaxed);
            state.lock().position_ms = pos;
        }
    }
}

/// Per-packet offset that skips source frames still before the seek target.
/// `decoded_before` is how many frames were decoded prior to this packet.
fn skip_offset(decoded_before: u64, skip_target: u64, packet_frames: usize) -> usize {
    if decoded_before >= skip_target {
        0
    } else {
        (skip_target - decoded_before).min(packet_frames as u64) as usize
    }
}

fn flush_remaining(
    resampler: Option<&mut SincFixedIn<f32>>,
    buf_l: &mut Vec<f32>,
    buf_r: &mut Vec<f32>,
    audio_tx: &Sender<Vec<i16>>,
    chunk_in: usize,
) {
    if buf_l.is_empty() {
        return;
    }
    // Pad with zeros up to chunk_in so the resampler can complete one final block.
    if let Some(rs) = resampler {
        if buf_l.len() < chunk_in {
            buf_l.resize(chunk_in, 0.0);
            buf_r.resize(chunk_in, 0.0);
        }
        let in_l: Vec<f32> = buf_l.drain(..chunk_in).collect();
        let in_r: Vec<f32> = buf_r.drain(..chunk_in).collect();
        if let Ok(out) = rs.process(&[in_l, in_r], None) {
            let _ = audio_tx.send(interleave_to_i16(&out[0], &out[1]));
        }
    } else {
        let _ = audio_tx.send(interleave_to_i16(buf_l, buf_r));
        buf_l.clear();
        buf_r.clear();
    }
}

fn interleave_to_i16(l: &[f32], r: &[f32]) -> Vec<i16> {
    let n = l.len().min(r.len());
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        out.push((l[i].clamp(-1.0, 1.0) * 32767.0) as i16);
        out.push((r[i].clamp(-1.0, 1.0) * 32767.0) as i16);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_offset_discards_frames_before_target() {
        // Target not yet reached: skip whole packet.
        assert_eq!(skip_offset(0, 1000, 512), 512);
        // Partially into the target: skip only the remaining frames.
        assert_eq!(skip_offset(900, 1000, 512), 100);
        // Target already passed: skip nothing.
        assert_eq!(skip_offset(1000, 1000, 512), 0);
        assert_eq!(skip_offset(2000, 1000, 512), 0);
        // No seek (target 0): never skip.
        assert_eq!(skip_offset(0, 0, 512), 0);
    }

    #[test]
    fn generation_matches_are_fresh_mismatches_are_stale() {
        // Same generation => the signal belongs to the active track.
        assert!(!generation_is_stale(5, 5));
        // Older generation => a track the user already moved past.
        assert!(generation_is_stale(4, 5));
        // Any difference is stale, even a (never-expected) newer one.
        assert!(generation_is_stale(6, 5));
    }

    #[test]
    fn interleave_pairs_left_and_right() {
        let l = [0.5, -0.5, 0.0];
        let r = [-0.5, 0.5, 1.0];
        let out = interleave_to_i16(&l, &r);
        assert_eq!(out.len(), 6);
        assert_eq!(out[0], (0.5 * 32767.0) as i16);
        assert_eq!(out[1], (-0.5 * 32767.0) as i16);
        assert_eq!(out[2], (-0.5 * 32767.0) as i16);
        assert_eq!(out[3], (0.5 * 32767.0) as i16);
        assert_eq!(out[4], 0);
        assert_eq!(out[5], 32767);
    }

    #[test]
    fn interleave_clamps_overflow() {
        let l = [2.0, -2.0];
        let r = [-2.0, 2.0];
        let out = interleave_to_i16(&l, &r);
        assert_eq!(out, vec![32767, -32767, -32767, 32767]);
    }

    #[test]
    fn interleave_truncates_to_shorter_channel() {
        let l = [0.1, 0.2, 0.3];
        let r = [0.4];
        let out = interleave_to_i16(&l, &r);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn interleave_empty_returns_empty() {
        let out = interleave_to_i16(&[], &[]);
        assert!(out.is_empty());
    }
}
