//! YouTube audio player.
//!
//! Per loaded track: spawns a tokio task that resolves the audio URL,
//! downloads the full M4A blob, decodes it with symphonia on a blocking
//! worker, resamples to 44.1k stereo via rubato, and pushes Vec<i16>
//! frames into the same crossbeam channel the audio pipeline consumes.
//! From the pipeline's perspective YouTube and Spotify are indistinguishable
//! producers.

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;
use parking_lot::Mutex;
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::bot::commands::BotCommand;
use crate::player::MediaPlayer;
use crate::youtube::metadata::YouTubeMetadata;

/// Audio pipeline expects this rate. librespot/Spotify side already produces 44.1k.
const PIPELINE_RATE: u32 = 44_100;
const CHANNELS: usize = 2;

/// Per-track control flags. Recreated on every `load`.
#[derive(Default)]
struct TrackControl {
    paused: AtomicBool,
    stopped: AtomicBool,
}

pub struct YouTubePlayer {
    audio_tx: Sender<Vec<i16>>,
    metadata: Arc<YouTubeMetadata>,
    /// Used to send EndOfTrack-equivalent (`BotCommand::Next { user_id: 0 }`)
    /// when the stream finishes naturally.
    cmd_tx: UnboundedSender<BotCommand>,
    /// Active track's task + control. `None` when idle.
    current: Arc<Mutex<Option<(JoinHandle<()>, Arc<TrackControl>)>>>,
}

impl YouTubePlayer {
    pub fn new(
        audio_tx: Sender<Vec<i16>>,
        metadata: Arc<YouTubeMetadata>,
        cmd_tx: UnboundedSender<BotCommand>,
    ) -> Self {
        Self {
            audio_tx,
            metadata,
            cmd_tx,
            current: Arc::new(Mutex::new(None)),
        }
    }

    /// Stop and abort any currently-running track task.
    fn abort_current(&self) {
        let mut cur = self.current.lock();
        if let Some((handle, ctrl)) = cur.take() {
            ctrl.stopped.store(true, Ordering::Relaxed);
            handle.abort();
        }
    }
}

impl MediaPlayer for YouTubePlayer {
    fn load(&self, video_id: &str) {
        self.abort_current();

        let audio_tx = self.audio_tx.clone();
        let metadata = self.metadata.clone();
        let cmd_tx = self.cmd_tx.clone();
        let video_id = video_id.to_string();
        let ctrl = Arc::new(TrackControl::default());
        let ctrl_for_task = ctrl.clone();

        let handle = tokio::spawn(async move {
            if let Err(e) = play_track(video_id.clone(), metadata, audio_tx, ctrl_for_task).await {
                tracing::error!("YouTube playback failed (video_id={video_id}): {e}");
            }
            // Tell the runner to advance regardless of whether playback ended
            // naturally or errored — same contract as Spotify's EndOfTrack.
            let _ = cmd_tx.send(BotCommand::Next { user_id: 0 });
        });

        *self.current.lock() = Some((handle, ctrl));
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

    fn seek(&self, _position_ms: u32) {
        // YouTube seeking requires re-issuing the HTTP range request.
        // Wired up in the decode pipeline commit.
        tracing::warn!("YouTubePlayer::seek not yet implemented");
    }

    fn preload(&self, _video_id: &str) {
        // No-op: YouTube preload would mean opening a second HTTP stream.
        // Skipped for now; gapless playback is a Phase-4 concern.
    }
}

/// Resolve the audio URL, download, decode + resample on a blocking worker,
/// stream Vec<i16> frames into `audio_tx`. Honors `ctrl.stopped` and
/// `ctrl.paused` flags. Returns once the track ends or is cancelled.
async fn play_track(
    video_id: String,
    metadata: Arc<YouTubeMetadata>,
    audio_tx: Sender<Vec<i16>>,
    ctrl: Arc<TrackControl>,
) -> Result<(), String> {
    // 1. Resolve direct stream URL.
    let url = metadata.get_audio_url(&video_id).await
        .map_err(|e| format!("URL resolve: {e}"))?;

    // 2. Download the whole M4A blob. Music tracks are 3-10 MB; fits in memory.
    let bytes = reqwest::get(&url).await
        .map_err(|e| format!("HTTP fetch: {e}"))?
        .bytes().await
        .map_err(|e| format!("HTTP body read: {e}"))?
        .to_vec();

    if ctrl.stopped.load(Ordering::Relaxed) {
        return Ok(());
    }

    // 3. Decode + resample on a blocking worker.
    tokio::task::spawn_blocking(move || decode_and_stream(bytes, audio_tx, ctrl))
        .await
        .map_err(|e| format!("decode worker join: {e}"))?
}

/// Sync decode + resample loop. Runs on a blocking worker thread.
fn decode_and_stream(
    bytes: Vec<u8>,
    audio_tx: Sender<Vec<i16>>,
    ctrl: Arc<TrackControl>,
) -> Result<(), String> {
    let cursor = Cursor::new(bytes);
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

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
            let in_l: Vec<f32> = buf_l.drain(..chunk_in).collect();
            let in_r: Vec<f32> = buf_r.drain(..chunk_in).collect();

            let frame = if let Some(ref mut rs) = resampler {
                let out = rs.process(&[in_l, in_r], None)
                    .map_err(|e| format!("resample: {e}"))?;
                interleave_to_i16(&out[0], &out[1])
            } else {
                interleave_to_i16(&in_l, &in_r)
            };

            if ctrl.stopped.load(Ordering::Relaxed) {
                return Ok(());
            }
            // bounded(256) — block briefly if pipeline is full. If the
            // receiver dropped, the track is gone; just exit.
            if audio_tx.send(frame).is_err() {
                return Ok(());
            }
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
