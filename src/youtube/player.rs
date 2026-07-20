//! YouTube audio player.
//!
//! Per loaded track: spawns a tokio task that runs yt-dlp, downloads the whole
//! compressed m4a into memory, then decodes it with symphonia on a blocking
//! worker, resamples to 44.1k stereo via rubato, and pushes Vec<i16> frames
//! into the same crossbeam channel the audio pipeline consumes. Buffering the
//! full file (this bot never plays livestreams, so downloads are finite) makes
//! the source seekable, so seek is a native symphonia call — instant in both
//! directions with no yt-dlp respawn.

use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;
use parking_lot::Mutex;
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::bot::commands::BotCommand;
use crate::bot::state::SharedState;
use crate::player::MediaPlayer;
use crate::youtube::metadata::YouTubeMetadata;

/// Audio pipeline expects this rate. librespot/Spotify side already produces 44.1k.
const PIPELINE_RATE: u32 = 44_100;
const CHANNELS: usize = 2;

/// Safety cap on how much compressed audio we buffer for a single track. The
/// bot never plays livestreams, so real downloads are far below this; it only
/// guards against a runaway/unexpected infinite stream exhausting memory.
const MAX_TRACK_BYTES: usize = 512 * 1024 * 1024;

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
    position_ms: AtomicU32,
    /// Set by `seek`; the decode loop performs a native symphonia seek to
    /// `seek_to_ms` (both directions, since the whole file is buffered) and
    /// clears this.
    seek_requested: AtomicBool,
    seek_to_ms: AtomicU32,
}

pub struct YouTubePlayer {
    audio_tx: Sender<Vec<i16>>,
    metadata: Arc<YouTubeMetadata>,
    /// Signals end-of-track (`BotCommand::TrackEnded`) when the stream finishes.
    cmd_tx: UnboundedSender<BotCommand>,
    /// Shared player state; the decode loop writes `position_ms` here so the
    /// `c` command and seek arithmetic see live YouTube positions.
    state: SharedState,
    /// Realtime playback position (ms injected) published by the audio pipeline.
    /// Position = seek base + this, so it reflects audio actually played rather
    /// than frames buffered ahead in the channel.
    pipeline_pos_ms: Arc<AtomicU32>,
    /// Active track's task + control. `None` when idle.
    #[allow(clippy::type_complexity)]
    current: Arc<Mutex<Option<(JoinHandle<()>, Arc<TrackControl>)>>>,
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
        pipeline_pos_ms: Arc<AtomicU32>,
    ) -> Self {
        Self {
            audio_tx,
            metadata,
            cmd_tx,
            state,
            pipeline_pos_ms,
            current: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The generation of the currently-loaded track. A `TrackEnded` whose
    /// generation differs from this is stale and must be ignored.
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Download + play `video_id` from the start.
    fn spawn_track(&self, video_id: &str) {
        self.abort_current();
        // This track's generation token (abort_current bumped past the old one).
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;

        let audio_tx = self.audio_tx.clone();
        let metadata = self.metadata.clone();
        let cmd_tx = self.cmd_tx.clone();
        let state = self.state.clone();
        let pipeline_pos_ms = self.pipeline_pos_ms.clone();
        let video_id = video_id.to_string();
        let ctrl = Arc::new(TrackControl::default());
        let ctrl_for_task = ctrl.clone();

        let handle = tokio::spawn(async move {
            let error = match play_track(video_id.clone(), metadata, audio_tx, ctrl_for_task, state, pipeline_pos_ms).await {
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
        self.spawn_track(video_id);
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
    }

    fn seek(&self, position_ms: u32) {
        // Native symphonia seek on the buffered file: instant, both directions,
        // no respawn. The decode loop picks up the request on its next iteration.
        if let Some((_, ctrl)) = self.current.lock().as_ref() {
            tracing::debug!("YouTube seek requested to {position_ms}ms");
            ctrl.seek_to_ms.store(position_ms, Ordering::Relaxed);
            ctrl.seek_requested.store(true, Ordering::Relaxed);
        } else {
            tracing::debug!("YouTube seek ignored: no track loaded");
        }
    }

    fn preload(&self, _video_id: &str) {
        // No-op: YouTube preload would mean opening a second HTTP stream.
        // Skipped for now; gapless playback is a Phase-4 concern.
    }
}

/// Run yt-dlp, download the whole compressed m4a into memory, then decode +
/// resample it from a seekable buffer. Buffering the full file is what makes
/// seek work in both directions (symphonia can't open a partial fragmented
/// mp4). The bot never plays livestreams, so the download always terminates.
///
/// `ctrl.stopped` set during playback kills the yt-dlp subprocess.
async fn play_track(
    video_id: String,
    metadata: Arc<YouTubeMetadata>,
    audio_tx: Sender<Vec<i16>>,
    ctrl: Arc<TrackControl>,
    state: SharedState,
    pipeline_pos_ms: Arc<AtomicU32>,
) -> Result<(), String> {
    let mut child = metadata.spawn_ytdlp(&video_id)
        .map_err(|e| format!("yt-dlp spawn: {e}"))?;
    let mut stdout = child.stdout.take()
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

    // Kill the child promptly if the track is stopped mid-download.
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

    // Read the entire compressed stream into memory, honoring stop and a size cap.
    let ctrl_for_read = ctrl.clone();
    let download = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            if ctrl_for_read.stopped.load(Ordering::Relaxed) {
                return Ok(Vec::new());
            }
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() + n > MAX_TRACK_BYTES {
                        return Err("track exceeds maximum buffer size".to_string());
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(e) => return Err(format!("read yt-dlp output: {e}")),
            }
        }
        Ok(buf)
    })
    .await
    .map_err(|e| format!("download worker join: {e}"))?;

    let exit_status = watcher_handle.join().ok().flatten();
    let stderr_text = stderr_handle.join().unwrap_or_default();

    let bytes = match download {
        Ok(b) => b,
        Err(e) => {
            let yt_err = stderr_text.lines()
                .find(|l| l.to_lowercase().contains("error"))
                .unwrap_or_else(|| stderr_text.lines().last().unwrap_or(""));
            let exit_code = exit_status.and_then(|s| s.code()).unwrap_or(-1);
            return Err(format!(
                "{e} (yt-dlp exit={exit_code}, stderr: {})",
                yt_err.chars().take(300).collect::<String>()
            ));
        }
    };

    // Stopped mid-download, or nothing came back.
    if ctrl.stopped.load(Ordering::Relaxed) || bytes.is_empty() {
        return Ok(());
    }

    tokio::task::spawn_blocking(move || decode_and_stream(bytes, audio_tx, ctrl, state, pipeline_pos_ms))
        .await
        .map_err(|e| format!("decode worker join: {e}"))?
}

/// Decode + resample the buffered compressed audio. Runs on a blocking worker.
/// The source is a seekable in-memory buffer, so `ctrl.seek_requested` is served
/// by a native symphonia seek (both directions, instant).
fn decode_and_stream(
    bytes: Vec<u8>,
    audio_tx: Sender<Vec<i16>>,
    ctrl: Arc<TrackControl>,
    state: SharedState,
    pipeline_pos_ms: Arc<AtomicU32>,
) -> Result<(), String> {
    let source: Box<dyn MediaSource> = Box::new(Cursor::new(bytes));
    let mss = MediaSourceStream::new(source, Default::default());

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

    // Per-channel accumulators feeding the resampler.
    let mut buf_l: Vec<f32> = Vec::with_capacity(chunk_in * 4);
    let mut buf_r: Vec<f32> = Vec::with_capacity(chunk_in * 4);

    // Reusable scratch to avoid per-chunk heap allocations on the decode hot
    // path: fixed-size resampler input, and preallocated output buffers fed to
    // `process_into_buffer` (plain `process` allocates fresh output Vecs each
    // call). Output is sized to the resampler's max so validation always passes.
    let mut in_l: Vec<f32> = Vec::with_capacity(chunk_in);
    let mut in_r: Vec<f32> = Vec::with_capacity(chunk_in);
    let out_cap = resampler.as_ref().map(|rs| rs.output_frames_max()).unwrap_or(0);
    let mut out_l: Vec<f32> = vec![0.0; out_cap];
    let mut out_r: Vec<f32> = vec![0.0; out_cap];

    // Playback position = base_ms (last seek target, or 0) plus the pipeline's
    // realtime injected-ms since its last reset. This tracks audio actually
    // *played*, not frames buffered ahead, so it never lurches on a seek.
    let mut base_ms: u64 = 0;

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
            // A seek issued while paused must apply now, not on resume — the
            // runner has already reported the new position to the user. Drop
            // to the seek block below; the next loop iteration re-enters this
            // wait since we're still paused.
            if ctrl.seek_requested.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Serve a pending seek with a native symphonia seek (both directions).
        if ctrl.seek_requested.swap(false, Ordering::Relaxed) {
            let target = ctrl.seek_to_ms.load(Ordering::Relaxed);
            let time = Time { seconds: (target / 1000) as u64, frac: (target % 1000) as f64 / 1000.0 };
            match format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(track_id) }) {
                Ok(seeked) => {
                    buf_l.clear();
                    buf_r.clear();
                    decoder.reset();
                    // Rebase position at the seek target; the pipeline's pos_ms
                    // resets to 0 when the runner's audio_reset takes effect, so
                    // position climbs from `target` as post-seek audio plays.
                    base_ms = target as u64;
                    pipeline_pos_ms.store(0, Ordering::Relaxed);
                    ctrl.position_ms.store(target, Ordering::Relaxed);
                    state.lock().position_ms = target;
                    tracing::debug!("YouTube native seek to {target}ms (actual_ts={})", seeked.actual_ts);
                }
                Err(e) => tracing::warn!("YouTube seek to {target}ms failed: {e}"),
            }
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
        match decoded {
            AudioBufferRef::F32(buf) => {
                let n = buf.frames();
                let l = buf.chan(0);
                let r = if src_channels >= 2 { buf.chan(1) } else { l };
                buf_l.extend_from_slice(&l[..n]);
                buf_r.extend_from_slice(&r[..n]);
            }
            AudioBufferRef::S16(buf) => {
                let n = buf.frames();
                let l = buf.chan(0);
                let r = if src_channels >= 2 { buf.chan(1) } else { l };
                buf_l.extend(l[..n].iter().map(|&s| s as f32 / 32768.0));
                buf_r.extend(r[..n].iter().map(|&s| s as f32 / 32768.0));
            }
            AudioBufferRef::S32(buf) => {
                let n = buf.frames();
                let l = buf.chan(0);
                let r = if src_channels >= 2 { buf.chan(1) } else { l };
                buf_l.extend(l[..n].iter().map(|&s| s as f32 / 2147483648.0));
                buf_r.extend(r[..n].iter().map(|&s| s as f32 / 2147483648.0));
            }
            other => {
                tracing::warn!("YouTube: unsupported sample format {:?}", std::mem::discriminant(&other));
                continue;
            }
        };

        // Drain in chunk_in-sized slices through the resampler.
        while buf_l.len() >= chunk_in {
            in_l.clear();
            in_r.clear();
            in_l.extend_from_slice(&buf_l[..chunk_in]);
            in_r.extend_from_slice(&buf_r[..chunk_in]);
            buf_l.drain(..chunk_in);
            buf_r.drain(..chunk_in);

            let frame = if let Some(ref mut rs) = resampler {
                let (_, written) = rs
                    .process_into_buffer(
                        &[&in_l, &in_r],
                        &mut [out_l.as_mut_slice(), out_r.as_mut_slice()],
                        None,
                    )
                    .map_err(|e| format!("resample: {e}"))?;
                interleave_to_i16(&out_l[..written], &out_r[..written])
            } else {
                interleave_to_i16(&in_l, &in_r)
            };

            // A seek arrived mid-drain: drop the rest of this decoded buffer and
            // loop back so the seek is served before we send more stale audio.
            if ctrl.seek_requested.load(Ordering::Relaxed) {
                buf_l.clear();
                buf_r.clear();
                break;
            }

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

            // Report position from real playback (pipeline injection), not from
            // frames buffered ahead in the channel.
            let pos = (base_ms + pipeline_pos_ms.load(Ordering::Relaxed) as u64)
                .min(u32::MAX as u64) as u32;
            ctrl.position_ms.store(pos, Ordering::Relaxed);
            state.lock().position_ms = pos;
        }
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
