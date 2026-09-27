//! YouTube audio player: the sidecar finds a track's address, `ranged` fetches
//! the file in blocks, and a blocking worker decodes it, resamples to 44.1k
//! stereo and feeds the audio pipeline.
//!
//! Indexed files play fragment by fragment, so starting or seeking anywhere
//! needs only the block holding that point. Cached files and files without an
//! index are decoded as one seekable file.

use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use parking_lot::Mutex;
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::bot::commands::BotCommand;
use crate::bot::state::SharedState;
use crate::player::MediaPlayer;
use crate::youtube::metadata::YouTubeMetadata;
use crate::youtube::mp4::{self, Layout, LayoutError};
use crate::youtube::ranged::{
    self, BlockStore, Coverage, HttpSource, RangeSource, Renewer, SegmentReader, StoreReader,
};
use crate::youtube::sidecar::{self, StreamInfo};

const PIPELINE_RATE: u32 = 44_100;
const CHANNELS: usize = 2;
const CHUNK_IN: usize = 1024;

/// The sidecar normally answers in about a second.
const SIDECAR_TIMEOUT: Duration = Duration::from_secs(60);

/// The cache sweep deletes a partial file untouched for an hour; a long pause
/// is not an abandoned download.
const PAUSED_KEEPALIVE: Duration = Duration::from_secs(10 * 60);

/// A track-end signal from an older load than the current one.
fn generation_is_stale(signal_gen: u64, current_gen: u64) -> bool {
    signal_gen != current_gen
}

/// Per-track control flags. Recreated on every `load`.
#[derive(Default)]
struct TrackControl {
    paused: AtomicBool,
    stopped: AtomicBool,
    position_ms: AtomicU32,
    seek_requested: AtomicBool,
    seek_to_ms: AtomicU32,
}

pub struct YouTubePlayer {
    audio_tx: Sender<Vec<i16>>,
    metadata: Arc<YouTubeMetadata>,
    http: reqwest::Client,
    cmd_tx: UnboundedSender<BotCommand>,
    state: SharedState,
    /// Milliseconds the pipeline has injected since its last reset; position
    /// is the last seek target plus this.
    pipeline_pos_ms: Arc<AtomicU32>,
    #[allow(clippy::type_complexity)]
    current: Arc<Mutex<Option<(JoinHandle<()>, Arc<TrackControl>)>>>,
    /// Bumped on every load and stop, so a stale end-of-track is recognised.
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
        let http = crate::net::stall_bounded_client().unwrap_or_else(|e| {
            tracing::warn!("YouTube: HTTP client setup failed ({e}); using defaults");
            reqwest::Client::new()
        });
        Self {
            audio_tx,
            metadata,
            http,
            cmd_tx,
            state,
            pipeline_pos_ms,
            current: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    fn spawn_track(&self, video_id: &str) {
        self.abort_current();
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;

        let audio_tx = self.audio_tx.clone();
        let metadata = self.metadata.clone();
        let http = self.http.clone();
        let cmd_tx = self.cmd_tx.clone();
        let state = self.state.clone();
        let pipeline_pos_ms = self.pipeline_pos_ms.clone();
        let video_id = video_id.to_string();
        let ctrl = Arc::new(TrackControl::default());
        let ctrl_for_task = ctrl.clone();

        let handle = tokio::spawn(async move {
            let error = match play_track(
                video_id.clone(),
                metadata,
                http,
                audio_tx,
                ctrl_for_task,
                state,
                pipeline_pos_ms,
            )
            .await
            {
                Ok(()) => None,
                Err(e) => {
                    tracing::error!("YouTube playback failed (video_id={video_id}): {e}");
                    Some(e)
                }
            };
            let _ = cmd_tx.send(BotCommand::TrackEnded { generation, error });
        });

        *self.current.lock() = Some((handle, ctrl));
    }

    pub fn is_stale_generation(&self, signal_gen: u64) -> bool {
        generation_is_stale(signal_gen, self.current_generation())
    }

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
        // Shutdown paths never call stop; the running track must not outlive the player.
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

    fn seek(&self, position_ms: u32) -> bool {
        // A finished track keeps a dead control until the next load: report the
        // seek unaccepted so the runner does not flush the buffered tail.
        match self.current.lock().as_ref() {
            Some((_, ctrl)) if !ctrl.stopped.load(Ordering::Relaxed) => {
                tracing::debug!("YouTube seek requested to {position_ms}ms");
                ctrl.seek_to_ms.store(position_ms, Ordering::Relaxed);
                ctrl.seek_requested.store(true, Ordering::Relaxed);
                true
            }
            _ => {
                tracing::debug!("YouTube seek ignored: no live track");
                false
            }
        }
    }

    fn preload(&self, _video_id: &str) {}
}

fn read_all(mut from: impl Read) -> String {
    let mut out = String::new();
    let _ = from.read_to_string(&mut out);
    out
}

/// Ask the sidecar where the track's audio is. Blocking. `Ok(None)` when
/// `stop` was set while waiting.
fn resolve_stream(
    metadata: &YouTubeMetadata,
    video_id: &str,
    stop: Option<&AtomicBool>,
) -> Result<Option<StreamInfo>, String> {
    let mut child = metadata
        .spawn_sidecar(video_id)
        .map_err(|e| format!("sidecar spawn: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "sidecar stdout was not piped".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "sidecar stderr was not piped".to_string())?;
    // A child blocked on a full pipe never exits, so drain both while waiting.
    let out = std::thread::spawn(move || read_all(stdout));
    let err = std::thread::spawn(move || read_all(stderr));

    let started = Instant::now();
    let status = loop {
        if stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out.join();
            let _ = err.join();
            return Ok(None);
        }
        if started.elapsed() > SIDECAR_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out.join();
            let said = sidecar::complaint(&err.join().unwrap_or_default());
            return Err(if said.is_empty() {
                "the sidecar did not answer within a minute".to_string()
            } else {
                format!("the sidecar did not answer within a minute ({said})")
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => {
                let _ = child.kill();
                return Err(format!("waiting for the sidecar: {e}"));
            }
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if !status.success() {
        let said = sidecar::complaint(&stderr);
        return Err(if said.is_empty() {
            format!("the sidecar failed ({status})")
        } else {
            said
        });
    }
    sidecar::parse_stream_info(&stdout).map(Some)
}

/// Closes the store however playback ends, including an aborted task, so its
/// readers wake and its workers stop.
struct CloseOnDrop(Arc<BlockStore>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

async fn play_track(
    video_id: String,
    metadata: Arc<YouTubeMetadata>,
    http: reqwest::Client,
    audio_tx: Sender<Vec<i16>>,
    ctrl: Arc<TrackControl>,
    state: SharedState,
    pipeline_pos_ms: Arc<AtomicU32>,
) -> Result<(), String> {
    if let Some(path) = crate::youtube::cache::cached(&video_id) {
        crate::youtube::cache::mark_played(&path);
        match std::fs::File::open(&path) {
            Ok(file) => {
                let decoded = {
                    let audio_tx = audio_tx.clone();
                    let ctrl = ctrl.clone();
                    let state = state.clone();
                    let pipeline_pos_ms = pipeline_pos_ms.clone();
                    tokio::task::spawn_blocking(move || {
                        decode_file(file, audio_tx, ctrl, state, pipeline_pos_ms)
                    })
                    .await
                    .map_err(|e| format!("decode worker join: {e}"))?
                };
                match decoded {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        // A file that opens but will not decode is truncated or
                        // corrupt; kept, it fails the same way on every replay.
                        tracing::warn!(
                            "YouTube: cached {video_id} did not decode ({e}); refetching"
                        );
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("YouTube: cached {video_id} could not be opened ({e}); refetching");
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    if ctrl.stopped.load(Ordering::Relaxed) {
        return Ok(());
    }
    let found = {
        let metadata = metadata.clone();
        let video_id = video_id.clone();
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || resolve_stream(&metadata, &video_id, Some(&ctrl.stopped)))
            .await
            .map_err(|e| format!("sidecar worker join: {e}"))??
    };
    let Some(info) = found else {
        return Ok(());
    };
    tracing::debug!(
        "YouTube: {video_id} is {} bytes, served to {}",
        info.content_length,
        info.client
    );
    if info.signed_in {
        tracing::info!("YouTube: {video_id} was refused without a sign-in and is played with the cookies file");
    }

    let part = crate::youtube::cache::partial_path(&video_id)
        .ok_or_else(|| format!("unusable video id {video_id}"))?;
    let coverage = Coverage::for_size(info.content_length);
    let store = BlockStore::create(&part, info.content_length, coverage)
        .map_err(|e| format!("create {}: {e}", part.display()))?;
    if coverage == Coverage::Full {
        if let Some(target) = crate::youtube::cache::track_path(&video_id) {
            store.publish_on_complete(target);
        }
    }
    let _close = CloseOnDrop(store.clone());

    // The address expires during a long track; the sidecar gives a fresh one.
    let renewer: Renewer = {
        let metadata = metadata.clone();
        let video_id = video_id.clone();
        let ctrl = ctrl.clone();
        Box::new(move || {
            let metadata = metadata.clone();
            let video_id = video_id.clone();
            // Watch the stop flag here too: spawn_blocking cannot be aborted,
            // so without it a stop leaves the sidecar running to its timeout.
            let ctrl = ctrl.clone();
            Box::pin(async move {
                let found = tokio::task::spawn_blocking(move || {
                    resolve_stream(&metadata, &video_id, Some(&ctrl.stopped))
                })
                    .await
                    .map_err(|e| format!("sidecar worker join: {e}"))??;
                let info = found.ok_or_else(|| "the sidecar was stopped".to_string())?;
                Ok((info.url, info.content_length))
            })
        })
    };
    let source: Arc<dyn RangeSource> =
        Arc::new(HttpSource::new(http, info.url, info.content_length, renewer));
    ranged::spawn_workers(store.clone(), source, ranged::WORKERS);

    tokio::task::spawn_blocking(move || decode_ranged(store, audio_tx, ctrl, state, pipeline_pos_ms))
        .await
        .map_err(|e| format!("decode worker join: {e}"))?
}

/// What the start of a file says about how to play it. Both carry the bytes read.
enum Head {
    Indexed(Layout, Vec<u8>),
    Unindexed(String, Vec<u8>),
}

fn read_head(store: &BlockStore) -> Result<Head, String> {
    let total = store.total();
    let mut head: Vec<u8> = Vec::new();
    let mut want = ranged::BLOCK.min(total);
    loop {
        while (head.len() as u64) < want {
            let mut buf = vec![0u8; (want - head.len() as u64) as usize];
            let n = store
                .read_at(head.len() as u64, &mut buf)
                .map_err(|e| format!("reading the header: {e}"))?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        match mp4::parse_layout(&head, total) {
            Ok(layout) => return Ok(Head::Indexed(layout, head)),
            Err(LayoutError::Unsupported(why)) => return Ok(Head::Unindexed(why, head)),
            Err(LayoutError::NeedMore(n)) if n > want && n <= total => want = n,
            Err(LayoutError::NeedMore(n)) => {
                let why = format!("the header wants {n} bytes of a {total}-byte file");
                return Ok(Head::Unindexed(why, head));
            }
        }
    }
}

/// Decode a cached file. Blocking.
fn decode_file(
    mut file: std::fs::File,
    audio_tx: Sender<Vec<i16>>,
    ctrl: Arc<TrackControl>,
    state: SharedState,
    pipeline_pos_ms: Arc<AtomicU32>,
) -> Result<(), String> {
    let mut head = Vec::new();
    (&mut file)
        .take(ranged::BLOCK)
        .read_to_end(&mut head)
        .map_err(|e| format!("reading the cached file: {e}"))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("reading the cached file: {e}"))?;
    let first = open_decoding(Box::new(file))?;
    let priming = mp4::priming_frames(&head, first.src_rate);
    run_decode(first, Seeking::Native { priming }, priming, None, audio_tx, ctrl, state, pipeline_pos_ms)
}

/// Decode a track being fetched into `store`. Blocking.
fn decode_ranged(
    store: Arc<BlockStore>,
    audio_tx: Sender<Vec<i16>>,
    ctrl: Arc<TrackControl>,
    state: SharedState,
    pipeline_pos_ms: Arc<AtomicU32>,
) -> Result<(), String> {
    let stopped = |ctrl: &TrackControl| ctrl.stopped.load(Ordering::Relaxed);
    let head = match read_head(&store) {
        Ok(h) => h,
        Err(_) if stopped(&ctrl) => return Ok(()),
        Err(e) => return Err(e),
    };
    let keepalive = Some(store.clone());
    match head {
        Head::Indexed(layout, bytes) => {
            let init: Arc<[u8]> = Arc::from(&bytes[..layout.init_len as usize]);
            let reader = SegmentReader::new(init.clone(), store.clone(), layout.segments[0].offset);
            let first = match open_decoding(Box::new(reader)) {
                Ok(d) => d,
                Err(_) if stopped(&ctrl) => return Ok(()),
                Err(e) => return Err(e),
            };
            // Starting is a seek to zero: it skips the priming.
            let skip_units = layout.locate(0).map(|(_, into)| into).unwrap_or(layout.priming);
            let start_skip = skip_units.saturating_mul(first.src_rate as u64) / layout.timescale as u64;
            let seeking = Seeking::Segments { layout, init, store };
            run_decode(first, seeking, start_skip, keepalive, audio_tx, ctrl, state, pipeline_pos_ms)
        }
        Head::Unindexed(why, bytes) => {
            tracing::info!("YouTube: {why}; reading the track as one file");
            let first = match open_decoding(Box::new(StoreReader::new(store.clone(), 0))) {
                Ok(d) => d,
                Err(_) if stopped(&ctrl) => return Ok(()),
                Err(e) => return Err(e),
            };
            let priming = mp4::priming_frames(&bytes, first.src_rate);
            let seeking = Seeking::Native { priming };
            run_decode(first, seeking, priming, keepalive, audio_tx, ctrl, state, pipeline_pos_ms)
        }
    }
}

struct Decoding {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    src_rate: u32,
    time_base: Option<TimeBase>,
}

impl Decoding {
    /// A duration in the track's timestamps, as decoded frames.
    fn ts_to_frames(&self, ts: u64) -> u64 {
        match self.time_base {
            Some(tb) if tb.denom > 0 => {
                (ts as u128 * self.src_rate as u128 * tb.numer as u128 / tb.denom as u128) as u64
            }
            _ => ts,
        }
    }
}

fn open_decoding(source: Box<dyn MediaSource>) -> Result<Decoding, String> {
    let mss = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;
    let format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "no default track".to_string())?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let src_rate = codec_params
        .sample_rate
        .ok_or_else(|| "missing sample_rate".to_string())?;
    let decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder make: {e}"))?;
    Ok(Decoding { format, decoder, track_id, src_rate, time_base: codec_params.time_base })
}

enum Seeking {
    /// One seekable file. `priming` is in decoded frames: symphonia's timeline
    /// includes it, positions do not.
    Native { priming: u64 },
    /// A fragmented stream: reopen at the fragment holding the target.
    Segments { layout: Layout, init: Arc<[u8]>, store: Arc<BlockStore> },
}

fn make_resampler(src_rate: u32) -> Result<Option<SincFixedIn<f32>>, String> {
    if src_rate == PIPELINE_RATE {
        return Ok(None);
    }
    let params = SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };
    SincFixedIn::<f32>::new(PIPELINE_RATE as f64 / src_rate as f64, 2.0, params, CHUNK_IN, CHANNELS)
        .map(Some)
        .map_err(|e| format!("resampler new: {e}"))
}

fn output_buffers(resampler: Option<&SincFixedIn<f32>>) -> (Vec<f32>, Vec<f32>) {
    let cap = resampler.map(|rs| rs.output_frames_max()).unwrap_or(0);
    (vec![0.0; cap], vec![0.0; cap])
}

/// Discard up to `skip` frames of what was appended after index `from`.
/// Returns how many are still to discard.
fn drop_leading(buf_l: &mut Vec<f32>, buf_r: &mut Vec<f32>, from: usize, skip: u64) -> u64 {
    if skip == 0 {
        return 0;
    }
    let added = buf_l.len().saturating_sub(from);
    let dropped = skip.min(added as u64) as usize;
    buf_l.drain(from..from + dropped);
    buf_r.drain(from..from + dropped);
    skip - dropped as u64
}

/// Decode, resample and send until the track ends, fails or is stopped.
/// `start_skip` decoded frames are discarded first.
#[allow(clippy::too_many_arguments)]
fn run_decode(
    mut dec: Decoding,
    seeking: Seeking,
    start_skip: u64,
    keepalive: Option<Arc<BlockStore>>,
    audio_tx: Sender<Vec<i16>>,
    ctrl: Arc<TrackControl>,
    state: SharedState,
    pipeline_pos_ms: Arc<AtomicU32>,
) -> Result<(), String> {
    let mut resampler = make_resampler(dec.src_rate)?;
    let mut buf_l: Vec<f32> = Vec::with_capacity(CHUNK_IN * 4);
    let mut buf_r: Vec<f32> = Vec::with_capacity(CHUNK_IN * 4);
    let mut in_l: Vec<f32> = Vec::with_capacity(CHUNK_IN);
    let mut in_r: Vec<f32> = Vec::with_capacity(CHUNK_IN);
    let (mut out_l, mut out_r) = output_buffers(resampler.as_ref());
    let mut base_ms: u64 = 0;
    let mut skip_frames = start_skip;
    // Only this thread knows audio is really playing; `p` answered "loading" forever without it.
    let mut reported_playing = false;
    let mut last_touch = Instant::now();

    loop {
        if ctrl.stopped.load(Ordering::Relaxed) {
            return Ok(());
        }
        while ctrl.paused.load(Ordering::Relaxed) {
            if ctrl.stopped.load(Ordering::Relaxed) {
                return Ok(());
            }
            // A seek while paused applies now: the runner has already reported it.
            if ctrl.seek_requested.load(Ordering::Relaxed) {
                break;
            }
            if let Some(store) = &keepalive {
                if last_touch.elapsed() >= PAUSED_KEEPALIVE {
                    store.touch();
                    last_touch = Instant::now();
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        if ctrl.seek_requested.swap(false, Ordering::Relaxed) {
            let target = ctrl.seek_to_ms.load(Ordering::Relaxed);
            let landed = match &seeking {
                Seeking::Native { priming } => {
                    let rate = dec.src_rate as u64;
                    let frames = target as u64 * rate / 1000 + priming;
                    let time = Time { seconds: frames / rate, frac: (frames % rate) as f64 / rate as f64 };
                    match dec.format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(dec.track_id) }) {
                        Ok(seeked) => {
                            dec.decoder.reset();
                            // The seek lands on a packet boundary before the target.
                            skip_frames = dec.ts_to_frames(seeked.required_ts.saturating_sub(seeked.actual_ts));
                            true
                        }
                        Err(SymphoniaError::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                            tracing::debug!("YouTube seek to {target}ms is past the end; ending track");
                            return Ok(());
                        }
                        Err(_) if ctrl.stopped.load(Ordering::Relaxed) => return Ok(()),
                        Err(e) => {
                            tracing::warn!("YouTube seek to {target}ms failed: {e}");
                            false
                        }
                    }
                }
                Seeking::Segments { layout, init, store } => match layout.locate(target as u64) {
                    None => {
                        tracing::debug!("YouTube seek to {target}ms is past the end; ending track");
                        return Ok(());
                    }
                    Some((index, into)) => {
                        let fragment = layout.segments[index];
                        // Before opening, so the fragment's own block is fetched first.
                        store.set_playhead((fragment.offset / ranged::BLOCK) as usize);
                        let reader = SegmentReader::new(init.clone(), store.clone(), fragment.offset);
                        match open_decoding(Box::new(reader)) {
                            Ok(next) => {
                                skip_frames = into.saturating_mul(next.src_rate as u64) / layout.timescale as u64;
                                dec = next;
                                tracing::debug!("YouTube seek to {target}ms: fragment {index}, skipping {skip_frames} frames");
                                true
                            }
                            Err(_) if ctrl.stopped.load(Ordering::Relaxed) => return Ok(()),
                            Err(e) => {
                                tracing::warn!("YouTube seek to {target}ms failed: {e}");
                                false
                            }
                        }
                    }
                },
            };
            if landed {
                buf_l.clear();
                buf_r.clear();
                // The old resampler holds audio from before the seek.
                resampler = make_resampler(dec.src_rate)?;
                (out_l, out_r) = output_buffers(resampler.as_ref());
                base_ms = target as u64;
                pipeline_pos_ms.store(0, Ordering::Relaxed);
                ctrl.position_ms.store(target, Ordering::Relaxed);
                state.lock().position_ms = target;
            } else {
                // The runner already wrote the target; decoding carries on from
                // the old position, so the position must too.
                let cur = ctrl.position_ms.load(Ordering::Relaxed);
                base_ms = cur as u64;
                state.lock().position_ms = cur;
            }
        }

        let packet = match dec.format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                flush_remaining(resampler.as_mut(), &mut buf_l, &mut buf_r, &audio_tx, CHUNK_IN, &ctrl);
                return Ok(());
            }
            // A stop closes the download, which fails the read waiting on it.
            Err(_) if ctrl.stopped.load(Ordering::Relaxed) => return Ok(()),
            Err(e) => return Err(format!("next_packet: {e}")),
        };

        if packet.track_id() != dec.track_id {
            continue;
        }

        let before = buf_l.len();
        {
            let decoded = match dec.decoder.decode(&packet) {
                Ok(d) => d,
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(e) => return Err(format!("decode: {e}")),
            };
            if !extend_planar(&decoded, &mut buf_l, &mut buf_r) {
                tracing::warn!("YouTube: unsupported sample format {:?}", std::mem::discriminant(&decoded));
                continue;
            }
        }
        skip_frames = drop_leading(&mut buf_l, &mut buf_r, before, skip_frames);

        while buf_l.len() >= CHUNK_IN {
            in_l.clear();
            in_r.clear();
            in_l.extend_from_slice(&buf_l[..CHUNK_IN]);
            in_r.extend_from_slice(&buf_r[..CHUNK_IN]);
            buf_l.drain(..CHUNK_IN);
            buf_r.drain(..CHUNK_IN);

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

            // Serve a seek before sending more audio from the old position.
            if ctrl.seek_requested.load(Ordering::Relaxed) {
                buf_l.clear();
                buf_r.clear();
                break;
            }

            // Never block on the bounded channel, so a stop is noticed within ~10ms.
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

            let pos = (base_ms + pipeline_pos_ms.load(Ordering::Relaxed) as u64)
                .min(u32::MAX as u64) as u32;
            ctrl.position_ms.store(pos, Ordering::Relaxed);
            {
                let mut s = state.lock();
                s.position_ms = pos;
                if !reported_playing {
                    // Never overwrite a user's pause or a stop.
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
    // Never block: a pause right at the end would park this thread indefinitely.
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

/// Append a decoded buffer to the planar accumulators, mono on both sides.
/// False for an unhandled sample format.
///
/// The channel count must come from the buffer: `codec_params.channels` is
/// `None` for AAC in MP4, and assuming stereo aborted the process on mono.
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
        assert!(!generation_is_stale(5, 5));
        assert!(generation_is_stale(4, 5));
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
        let out = interleave_to_i16(&[2.0, -2.0], &[-2.0, 2.0]);
        assert_eq!(out, vec![32767, -32767, -32767, 32767]);
    }

    #[test]
    fn interleave_truncates_to_shorter_channel() {
        assert_eq!(interleave_to_i16(&[0.1, 0.2, 0.3], &[0.4]).len(), 2);
    }

    #[test]
    fn interleave_empty_returns_empty() {
        assert!(interleave_to_i16(&[], &[]).is_empty());
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
        let buf = f32_buffer(Channels::FRONT_LEFT, &[&[0.1, 0.2, 0.3]]);
        let decoded = AudioBufferRef::F32(std::borrow::Cow::Owned(buf));
        let (mut l, mut r) = (Vec::new(), Vec::new());
        assert!(extend_planar(&decoded, &mut l, &mut r));
        assert_eq!(l, vec![0.1, 0.2, 0.3]);
        assert_eq!(r, l);
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

    #[test]
    fn leading_frames_are_dropped_from_new_audio_only() {
        let mut l = vec![1.0, 2.0, 10.0, 11.0, 12.0];
        let mut r = vec![-1.0, -2.0, -10.0, -11.0, -12.0];
        assert_eq!(drop_leading(&mut l, &mut r, 2, 2), 0);
        assert_eq!(l, vec![1.0, 2.0, 12.0]);
        assert_eq!(r, vec![-1.0, -2.0, -12.0]);
    }

    #[test]
    fn a_skip_longer_than_one_packet_carries_over() {
        let mut l = vec![0.0; 1024];
        let mut r = vec![0.0; 1024];
        assert_eq!(drop_leading(&mut l, &mut r, 0, 3000), 1976);
        assert!(l.is_empty() && r.is_empty());
        let mut l2 = vec![7.0; 8];
        let mut r2 = vec![7.0; 8];
        assert_eq!(drop_leading(&mut l2, &mut r2, 0, 0), 0);
        assert_eq!(l2.len(), 8);
    }

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    /// ftyp, a moov of `moov_len` bytes, an index of `count` fragments of
    /// `size` bytes, then the fragments.
    fn indexed_file(moov_len: usize, count: usize, size: u32) -> Vec<u8> {
        let mut file = bx(b"ftyp", b"dash");
        file.extend(bx(b"moov", &vec![0; moov_len]));
        let mut p = vec![0, 0, 0, 0];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&44_100u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&[0, 0]);
        p.extend_from_slice(&(count as u16).to_be_bytes());
        for _ in 0..count {
            p.extend_from_slice(&size.to_be_bytes());
            p.extend_from_slice(&441_000u32.to_be_bytes());
            p.extend_from_slice(&0u32.to_be_bytes());
        }
        file.extend(bx(b"sidx", &p));
        file.extend(std::iter::repeat_n(0u8, count * size as usize));
        file
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ttspotify_player_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_indexed_file_is_played_by_fragment() {
        let dir = scratch("head_indexed");
        let file = indexed_file(16, 3, 5000);
        let store = BlockStore::filled_for_test(&dir.join("t.part"), &file);
        match read_head(&store).unwrap() {
            Head::Indexed(layout, bytes) => {
                assert_eq!(layout.segments.len(), 3);
                assert!(bytes.len() as u64 >= layout.init_len);
            }
            Head::Unindexed(why, _) => panic!("expected an index: {why}"),
        }
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_header_running_past_the_first_block_is_still_read() {
        // An index alone cannot outgrow a block (16-bit entry count), but a large moov can push it past.
        let dir = scratch("head_big");
        let file = indexed_file(900_000, 20_000, 1);
        assert!(file.len() - 20_000 > ranged::BLOCK as usize);
        let store = BlockStore::filled_for_test(&dir.join("t.part"), &file);
        match read_head(&store).unwrap() {
            Head::Indexed(layout, _) => assert_eq!(layout.segments.len(), 20_000),
            Head::Unindexed(why, _) => panic!("expected an index: {why}"),
        }
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_without_an_index_is_played_whole() {
        let dir = scratch("head_plain");
        let mut file = bx(b"ftyp", b"M4A ");
        file.extend(bx(b"moov", &[0; 16]));
        file.extend(bx(b"mdat", &[0; 4000]));
        let store = BlockStore::filled_for_test(&dir.join("t.part"), &file);
        assert!(matches!(read_head(&store).unwrap(), Head::Unindexed(..)));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn live_metadata() -> Option<Arc<YouTubeMetadata>> {
        crate::youtube::metadata::for_tests().map(Arc::new)
    }

    fn live_store(dir: &std::path::Path, info: &StreamInfo, coverage: Coverage) -> Arc<BlockStore> {
        let store = BlockStore::create(&dir.join("t.part"), info.content_length, coverage).unwrap();
        let renewer: Renewer = Box::new(|| Box::pin(async { Err("no renewal in this test".to_string()) }));
        let source: Arc<dyn RangeSource> = Arc::new(HttpSource::new(
            reqwest::Client::new(),
            info.url.clone(),
            info.content_length,
            renewer,
        ));
        ranged::spawn_workers(store.clone(), source, ranged::WORKERS);
        store
    }

    /// `count` frames of the left channel, after discarding `skip`.
    fn decode_frames(source: Box<dyn MediaSource>, skip: u64, count: usize) -> Vec<f32> {
        let mut dec = open_decoding(source).expect("open");
        let (mut l, mut r) = (Vec::new(), Vec::new());
        let mut skip = skip;
        while l.len() < count {
            let packet = match dec.format.next_packet() {
                Ok(p) => p,
                Err(_) => break,
            };
            if packet.track_id() != dec.track_id {
                continue;
            }
            let before = l.len();
            let decoded = dec.decoder.decode(&packet).expect("decode");
            assert!(extend_planar(&decoded, &mut l, &mut r));
            skip = drop_leading(&mut l, &mut r, before, skip);
        }
        l.truncate(count);
        l
    }

    async fn live_stream(meta: &Arc<YouTubeMetadata>, id: &'static str) -> StreamInfo {
        let m = meta.clone();
        tokio::task::spawn_blocking(move || resolve_stream(&m, id, None))
            .await
            .unwrap()
            .expect("the sidecar found no stream")
            .expect("not stopped")
    }

    /// Jumping to a fragment gives the same audio the whole file gives at that
    /// moment. `cargo test --lib -- --ignored a_fragment_decodes --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "hits the network and needs Deno"]
    async fn a_fragment_decodes_to_the_same_audio_as_the_whole_file() {
        let Some(meta) = live_metadata() else {
            println!("skipped: no Deno found");
            return;
        };
        // A "- Topic" upload: its header declares encoder priming.
        let info = live_stream(&meta, "5oWyMakvQew").await;
        let dir = scratch("live_same_audio");
        let store = live_store(&dir, &info, Coverage::Full);
        let reader_store = store.clone();
        let worst = tokio::task::spawn_blocking(move || {
            let Head::Indexed(layout, head) = read_head(&reader_store).unwrap() else {
                panic!("expected an index");
            };
            assert!(layout.priming > 0, "this track was chosen for its priming");
            let init: Arc<[u8]> = Arc::from(&head[..layout.init_len as usize]);
            let later = layout.units_to_ms(layout.segments[4].start) + 3_000;
            let mut worst = 0.0f32;
            for ms in [3_000u64, later] {
                let (index, into) = layout.locate(ms).unwrap();
                // The whole-file decode includes the priming.
                let target_frame = (layout.ms_to_units(ms) + layout.priming) as usize;
                let count = 44_100;
                let whole = decode_frames(Box::new(StoreReader::new(reader_store.clone(), 0)), 0, target_frame + count);
                let fragment = layout.segments[index];
                let jumped = decode_frames(
                    Box::new(SegmentReader::new(init.clone(), reader_store.clone(), fragment.offset)),
                    into,
                    count,
                );
                assert_eq!(jumped.len(), count);
                let diff = whole[target_frame..]
                    .iter()
                    .zip(&jumped)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0f32, f32::max);
                println!("{ms}ms: fragment {index}, largest difference {diff}");
                worst = worst.max(diff);
            }
            worst
        })
        .await
        .unwrap();
        assert!(worst < 1e-3, "jumping to a fragment gave different audio (largest difference {worst})");
        store.close();
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "hits the network and needs Deno"]
    async fn a_seek_an_hour_in_fetches_only_that_part() {
        let Some(meta) = live_metadata() else {
            println!("skipped: no Deno found");
            return;
        };
        let info = live_stream(&meta, "HSOtku1j600").await;
        let dir = scratch("live_hour_in");
        let store = live_store(&dir, &info, Coverage::Window(2));
        let reader_store = store.clone();
        let started = Instant::now();
        let frames = tokio::task::spawn_blocking(move || {
            let Head::Indexed(layout, head) = read_head(&reader_store).unwrap() else {
                panic!("expected an index");
            };
            let init: Arc<[u8]> = Arc::from(&head[..layout.init_len as usize]);
            let (index, into) = layout.locate(3_600_000).expect("an hour is inside a two-hour video");
            let fragment = layout.segments[index];
            reader_store.set_playhead((fragment.offset / ranged::BLOCK) as usize);
            decode_frames(Box::new(SegmentReader::new(init, reader_store.clone(), fragment.offset)), into, 44_100)
        })
        .await
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(frames.len(), 44_100);
        let blocks = ranged::block_count(info.content_length);
        let fetched = (0..blocks).filter(|&b| store.has_block(b)).count();
        println!("one second at 1:00:00 after {elapsed:?}, {fetched} of {blocks} blocks fetched");
        assert!(fetched <= 8, "fetched {fetched} blocks to play one second an hour in");
        store.close();
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "hits the network and needs Deno"]
    async fn the_player_seeks_forward_and_back_while_downloading() {
        let Some(meta) = live_metadata() else {
            println!("skipped: no Deno found");
            return;
        };
        let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<i16>>(256);
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let state: SharedState = Arc::new(Mutex::new(crate::bot::state::PlayerState::new()));
        let pipeline_pos = Arc::new(AtomicU32::new(0));
        let received = Arc::new(AtomicU64::new(0));
        let drain_count = received.clone();
        std::thread::spawn(move || {
            while let Ok(frame) = audio_rx.recv() {
                drain_count.fetch_add(frame.len() as u64, Ordering::Relaxed);
            }
        });
        let player = YouTubePlayer::new(audio_tx, meta, cmd_tx, state.clone(), pipeline_pos);

        async fn until(what: &str, mut done: impl FnMut() -> bool) -> Duration {
            let started = Instant::now();
            while !done() {
                assert!(started.elapsed() < Duration::from_secs(60), "timed out waiting for {what}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            started.elapsed()
        }

        let started = Instant::now();
        player.load("HSOtku1j600");
        until("audio", || received.load(Ordering::Relaxed) > 0).await;
        println!("first audio after {:?}", started.elapsed());

        for target in [3_600_000u32, 600_000] {
            assert!(player.seek(target));
            let took = until("the seek to land", || state.lock().position_ms == target).await;
            let at = received.load(Ordering::Relaxed);
            until("audio after the seek", || received.load(Ordering::Relaxed) > at + 88_200).await;
            println!("seek to {target}ms landed after {took:?}");
        }
        player.stop();
    }
}
