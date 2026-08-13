//! YouTube audio player.
//!
//! Per loaded track: spawns a tokio task that runs yt-dlp, downloads the whole
//! compressed m4a into memory, then decodes it with symphonia on a blocking
//! worker, resamples to 44.1k stereo via rubato, and pushes Vec<i16> frames
//! into the same crossbeam channel the audio pipeline consumes. Buffering the
//! full file (this bot never plays livestreams, so downloads are finite) makes
//! the source seekable, so seek is a native symphonia call — instant in both
//! directions with no yt-dlp respawn.

use std::io::Read;
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

impl Drop for YouTubePlayer {
    fn drop(&mut self) {
        // External shutdown paths (tray stop, reconnect-exhausted) never call
        // MediaPlayer::stop, which left an in-flight yt-dlp download orphaned:
        // the watcher never killed the child and the runtime drop then joined
        // the still-reading download worker until the file finished. Dropping
        // the player (the command processor owns it) now aborts whatever runs.
        self.abort_current();
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

/// A yt-dlp download that has proven it is producing audio.
struct Started {
    stdout: std::process::ChildStdout,
    /// The bytes already read while proving it. They must be fed to the
    /// decoder before anything read afterwards.
    first_chunk: Vec<u8>,
    stderr_handle: std::thread::JoinHandle<String>,
    watcher_handle: std::thread::JoinHandle<Option<std::process::ExitStatus>>,
}

/// Spawn yt-dlp through `client` and wait for the first audio bytes.
///
/// Waiting for real bytes is the whole point: a refused URL exits with 403
/// having produced none, and that is only knowable by reading. Returning after
/// the first chunk means a failure is detected in about the time the attempt
/// would have taken anyway, leaving room to try another client.
async fn begin_download(
    metadata: &Arc<YouTubeMetadata>,
    video_id: &str,
    client: &str,
    ctrl: &Arc<TrackControl>,
) -> Result<Started, String> {
    let mut child = metadata
        .spawn_ytdlp_with_client(video_id, client)
        .map_err(|e| format!("yt-dlp spawn: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "yt-dlp stdout was not piped".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "yt-dlp stderr was not piped".to_string())?;

    // Drain stderr in the background so yt-dlp never blocks on a full pipe,
    // and so its complaint can be reported if this attempt fails.
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

    // Blocking read, on the blocking pool: this waits on the network for as
    // long as the extraction takes.
    let ctrl_for_read = ctrl.clone();
    let (stdout, first_chunk) = tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; 64 * 1024];
        if ctrl_for_read.stopped.load(Ordering::Relaxed) {
            return (stdout, Vec::new());
        }
        // One read. It blocks until yt-dlp has either produced audio or given
        // up, which is exactly the question being asked.
        match stdout.read(&mut buf) {
            Ok(0) => (stdout, Vec::new()), // exited without ever sending audio
            Ok(n) => {
                buf.truncate(n);
                (stdout, buf)
            }
            Err(_) => (stdout, Vec::new()),
        }
    })
    .await
    .map_err(|e| format!("download worker join: {e}"))?;

    if first_chunk.is_empty() {
        // Nothing came back. Report what yt-dlp said, since that is the only
        // account of why - a 403, a missing format, a bot check.
        let complaint = stderr_handle.join().unwrap_or_default();
        let _ = watcher_handle.join();
        let reason = complaint
            .lines()
            .find(|l| l.to_lowercase().contains("error"))
            .unwrap_or_else(|| complaint.lines().last().unwrap_or("no output"));
        return Err(reason.chars().take(300).collect());
    }

    Ok(Started { stdout, first_chunk, stderr_handle, watcher_handle })
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
    // Try the fast client, then the reliable one. YouTube refuses the fast
    // client's URLs when it distrusts an address - a datacenter IP more often
    // than a home one - and the refusal arrives before any audio, so a second
    // attempt through a PO-token client costs nothing on the tracks that were
    // going to work anyway. Measured on one address: 2/6 through the fast
    // client, 6/6 through the fallback.
    let mut started = None;
    let mut last_error = String::new();
    for (attempt, client) in [
        crate::youtube::metadata::PRIMARY_CLIENT,
        crate::youtube::metadata::FALLBACK_CLIENT,
    ]
    .into_iter()
    .enumerate()
    {
        if ctrl.stopped.load(Ordering::Relaxed) {
            return Ok(());
        }
        match begin_download(&metadata, &video_id, client, &ctrl).await {
            Ok(begun) => {
                if attempt > 0 {
                    tracing::info!(
                        "YouTube: {video_id} needed the {client} player after the faster one was refused"
                    );
                }
                started = Some(begun);
                break;
            }
            Err(e) => {
                tracing::warn!("YouTube: {client} player could not start {video_id}: {e}");
                last_error = e;
            }
        }
    }
    let Started { mut stdout, first_chunk, stderr_handle, watcher_handle } = match started {
        Some(s) => s,
        None => return Err(last_error),
    };
    let _ = &stderr_handle;
    let _ = &watcher_handle;
    // Pump yt-dlp's output into a buffer the decoder reads at the same time,
    // so playback starts on the first bytes instead of after the whole track
    // has downloaded. Everything downloaded is kept, so seeking back is still
    // instant; seeking forward waits for the bytes.
    let (writer, reader) = crate::youtube::stream::channel();
    // The bytes read to prove this attempt works come first, or the decoder
    // starts mid-file.
    writer.append(&first_chunk);
    let ctrl_for_read = ctrl.clone();
    let download_writer = writer.clone();
    let download = tokio::task::spawn_blocking(move || {
        let mut chunk = [0u8; 64 * 1024];
        let mut total = 0usize;
        loop {
            if ctrl_for_read.stopped.load(Ordering::Relaxed) {
                download_writer.finish();
                return;
            }
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    if total > MAX_TRACK_BYTES {
                        download_writer.fail("track exceeds maximum buffer size");
                        return;
                    }
                    download_writer.append(&chunk[..n]);
                }
                Err(e) => {
                    download_writer.fail(format!("read yt-dlp output: {e}"));
                    return;
                }
            }
        }
        download_writer.finish();
    });

    // Decoding runs alongside the download, not after it.
    let ctrl_for_cleanup = ctrl.clone();
    let decode = tokio::task::spawn_blocking(move || {
        decode_and_stream(reader, audio_tx, ctrl, state, pipeline_pos_ms)
    });

    let decode_result = decode.await.map_err(|e| format!("decode worker join: {e}"))?;
    // Decoding is over either way, so nothing is waiting on the rest of the
    // download: tell it to stop so a failure is reported now rather than after
    // the remaining minutes of audio have been fetched. Snapshot whether a
    // user stop had already happened — after this point a non-zero yt-dlp exit
    // is expected (our own kill), before it it means a truncated download.
    let was_stopped = ctrl_for_cleanup.stopped.swap(true, Ordering::Relaxed);
    let _ = download.await;

    let exit_status = watcher_handle.join().ok().flatten();
    let stderr_text = stderr_handle.join().unwrap_or_default();

    if let Err(e) = decode_result {
        // A decode error is usually yt-dlp's error wearing a disguise: it
        // failed, we got a truncated stream, and symphonia complained about
        // that instead. Report what yt-dlp said.
        let yt_err = stderr_text
            .lines()
            .find(|l| l.to_lowercase().contains("error"))
            .unwrap_or_else(|| stderr_text.lines().last().unwrap_or(""));
        let exit_code = exit_status.and_then(|s| s.code()).unwrap_or(-1);
        if yt_err.is_empty() {
            return Err(e);
        }
        return Err(format!(
            "{e} (yt-dlp exit={exit_code}, stderr: {})",
            yt_err.chars().take(300).collect::<String>()
        ));
    }
    // Decode reached a clean end-of-stream, but if yt-dlp itself failed and
    // the user never stopped the track, that "end" is a truncation that
    // happened to land on an mp4 atom boundary: the song cut short, silently,
    // and it counted as a success (resetting the failure brake). Surface it.
    if !was_stopped {
        if let Some(status) = exit_status {
            if !status.success() {
                let yt_err = stderr_text
                    .lines()
                    .find(|l| l.to_lowercase().contains("error"))
                    .unwrap_or_else(|| stderr_text.lines().last().unwrap_or("no output"));
                return Err(format!(
                    "yt-dlp exited with {status} mid-download; track truncated (stderr: {})",
                    yt_err.chars().take(300).collect::<String>()
                ));
            }
        }
    }
    Ok(())
}

/// Decode + resample the compressed audio as it arrives. Runs on a blocking
/// worker. The source keeps everything it has read, so `ctrl.seek_requested` is
/// served by a native symphonia seek: instantly backwards, and forwards as soon
/// as the download reaches that point.
fn decode_and_stream(
    reader: crate::youtube::stream::StreamReader,
    audio_tx: Sender<Vec<i16>>,
    ctrl: Arc<TrackControl>,
    state: SharedState,
    pipeline_pos_ms: Arc<AtomicU32>,
) -> Result<(), String> {
    // Wait for the download before probing. symphonia will not seek a source
    // of unknown length, and finding the MP4 moov atom needs a seek, so a probe
    // started mid-download fails with "stream is not seekable" - which is every
    // track. The bytes are kept, so decoding still streams from memory once it
    // starts; only the probe waits.
    reader.wait_until_complete();
    let source: Box<dyn MediaSource> = Box::new(reader);
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

    // Nothing else on the YouTube path reports "actually playing" (librespot's
    // event loop only speaks for Spotify), so the status stayed Loading for
    // the whole track: bare `p` answered "loading" forever and pause/resume
    // was unreachable. Promote Loading -> Playing on the first chunk handed
    // to the pipeline; the guard keeps a user's Paused (or a stop's Idle)
    // from being overwritten by this thread.
    let mut reported_playing = false;

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
                Err(SymphoniaError::IoError(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    // Seeking past the end of the track: treat it as the track
                    // finishing, the way every player's UI does. Returning Ok
                    // raises a natural TrackEnded and the queue advances.
                    tracing::debug!("YouTube seek to {target}ms is past the end; ending track");
                    return Ok(());
                }
                Err(e) => {
                    // The runner already wrote the optimistic target into
                    // state.position_ms and reset the pipeline (pos_ms = 0).
                    // Decoding continues from the OLD file position, so rebase
                    // the position bookkeeping there — leaving base_ms stale
                    // made `c` read near 0:00 for the rest of the track and
                    // corrupted every later relative seek.
                    let cur = ctrl.position_ms.load(Ordering::Relaxed);
                    base_ms = cur as u64;
                    state.lock().position_ms = cur;
                    tracing::warn!("YouTube seek to {target}ms failed: {e}; continuing at {cur}ms");
                }
            }
        }

        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Drain any remaining buffered samples through the resampler.
                flush_remaining(resampler.as_mut(), &mut buf_l, &mut buf_r, &audio_tx, chunk_in, &ctrl);
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

        if !extend_planar(&decoded, &mut buf_l, &mut buf_r) {
            tracing::warn!("YouTube: unsupported sample format {:?}", std::mem::discriminant(&decoded));
            continue;
        }

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
            {
                let mut s = state.lock();
                s.position_ms = pos;
                if !reported_playing {
                    if s.status == crate::bot::state::PlaybackStatus::Loading {
                        s.status = crate::bot::state::PlaybackStatus::Playing;
                    }
                    reported_playing = true;
                }
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
    ctrl: &TrackControl,
) {
    if buf_l.is_empty() {
        return;
    }
    // Pad with zeros up to chunk_in so the resampler can complete one final block.
    let frame = if let Some(rs) = resampler {
        if buf_l.len() < chunk_in {
            buf_l.resize(chunk_in, 0.0);
            buf_r.resize(chunk_in, 0.0);
        }
        let in_l: Vec<f32> = buf_l.drain(..chunk_in).collect();
        let in_r: Vec<f32> = buf_r.drain(..chunk_in).collect();
        match rs.process(&[in_l, in_r], None) {
            Ok(out) => interleave_to_i16(&out[0], &out[1]),
            Err(_) => return,
        }
    } else {
        let out = interleave_to_i16(buf_l, buf_r);
        buf_l.clear();
        buf_r.clear();
        out
    };
    // Same never-block contract as the decode loop's sends: a blocking send
    // here could park this thread until the pipeline drains — indefinitely if
    // the user pauses right at the end of the track.
    let mut frame = Some(frame);
    loop {
        if ctrl.stopped.load(Ordering::Relaxed) {
            return;
        }
        match audio_tx.try_send(frame.take().expect("set before loop")) {
            Ok(()) => return,
            Err(crossbeam_channel::TrySendError::Full(returned)) => {
                frame = Some(returned);
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => return,
        }
    }
}

/// Append a decoded buffer's samples to the planar f32 accumulators, mono
/// duplicated to both sides. Returns false for a sample format we don't
/// handle.
///
/// The channel count MUST come from the buffer itself (`spec()`), not from
/// `codec_params.channels` captured before the loop: that field is `None` for
/// every AAC-in-MP4 stream, and defaulting it to 2 made `buf.chan(1)` trip
/// symphonia's channel assert on the first packet of a mono track — which
/// with panic = "abort" took down the whole process.
fn extend_planar(decoded: &AudioBufferRef<'_>, buf_l: &mut Vec<f32>, buf_r: &mut Vec<f32>) -> bool {
    match decoded {
        AudioBufferRef::F32(buf) => {
            let n = buf.frames();
            let l = buf.chan(0);
            let r = if buf.spec().channels.count() >= 2 { buf.chan(1) } else { l };
            buf_l.extend_from_slice(&l[..n]);
            buf_r.extend_from_slice(&r[..n]);
            true
        }
        AudioBufferRef::S16(buf) => {
            let n = buf.frames();
            let l = buf.chan(0);
            let r = if buf.spec().channels.count() >= 2 { buf.chan(1) } else { l };
            buf_l.extend(l[..n].iter().map(|&s| s as f32 / 32768.0));
            buf_r.extend(r[..n].iter().map(|&s| s as f32 / 32768.0));
            true
        }
        AudioBufferRef::S32(buf) => {
            let n = buf.frames();
            let l = buf.chan(0);
            let r = if buf.spec().channels.count() >= 2 { buf.chan(1) } else { l };
            buf_l.extend(l[..n].iter().map(|&s| s as f32 / 2147483648.0));
            buf_r.extend(r[..n].iter().map(|&s| s as f32 / 2147483648.0));
            true
        }
        _ => false,
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

    use symphonia::core::audio::{AudioBuffer, Channels, SignalSpec};

    fn f32_buffer(channels: Channels, frames: &[&[f32]]) -> AudioBuffer<f32> {
        let spec = SignalSpec::new(44_100, channels);
        let n = frames[0].len();
        let mut buf = AudioBuffer::<f32>::new(n as u64, spec);
        buf.render_reserved(Some(n));
        for (ch, samples) in frames.iter().enumerate() {
            buf.chan_mut(ch).copy_from_slice(samples);
        }
        buf
    }

    #[test]
    fn mono_buffer_duplicates_the_single_channel_instead_of_panicking() {
        // AAC-in-MP4 reports no channel layout in codec_params, so the count
        // must come from the decoded buffer itself. A mono buffer used to hit
        // `buf.chan(1)` and abort the process.
        let buf = f32_buffer(Channels::FRONT_LEFT, &[&[0.1, 0.2, 0.3]]);
        let decoded = AudioBufferRef::F32(std::borrow::Cow::Owned(buf));
        let (mut l, mut r) = (Vec::new(), Vec::new());
        assert!(extend_planar(&decoded, &mut l, &mut r));
        assert_eq!(l, vec![0.1, 0.2, 0.3]);
        assert_eq!(r, l, "mono plays on both sides");
    }

    #[test]
    fn stereo_buffer_keeps_left_and_right_separate() {
        let buf = f32_buffer(
            Channels::FRONT_LEFT | Channels::FRONT_RIGHT,
            &[&[0.1, 0.2], &[0.3, 0.4]],
        );
        let decoded = AudioBufferRef::F32(std::borrow::Cow::Owned(buf));
        let (mut l, mut r) = (Vec::new(), Vec::new());
        assert!(extend_planar(&decoded, &mut l, &mut r));
        assert_eq!(l, vec![0.1, 0.2]);
        assert_eq!(r, vec![0.3, 0.4]);
    }

    #[test]
    fn extend_planar_accumulates_across_calls() {
        let (mut l, mut r) = (Vec::new(), Vec::new());
        for _ in 0..2 {
            let buf = f32_buffer(Channels::FRONT_LEFT, &[&[0.5]]);
            let decoded = AudioBufferRef::F32(std::borrow::Cow::Owned(buf));
            assert!(extend_planar(&decoded, &mut l, &mut r));
        }
        assert_eq!(l.len(), 2);
        assert_eq!(r.len(), 2);
    }
}
