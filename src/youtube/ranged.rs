//! Fetching a remote file in byte ranges into a partly filled local copy.
//!
//! The file is cut into blocks fetched several at a time. A reader that reaches
//! a missing block asks for it first and waits, so playback can start or jump
//! anywhere after one block. Small files download completely and are cached;
//! past `FULL_DOWNLOAD_MAX` only a window ahead of playback is fetched.

use std::collections::VecDeque;
use std::fs::File;
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

/// YouTube refuses much larger ranges on some tracks.
pub const BLOCK: u64 = 1 << 20;

/// Files up to this size download completely and can be cached.
pub const FULL_DOWNLOAD_MAX: u64 = 256 << 20;

/// Blocks kept ahead of playback for larger files: about half an hour of audio.
pub const READ_AHEAD_BLOCKS: usize = 32;

/// Four at a time was measured about four times faster than one; eight gained little.
pub const WORKERS: usize = 4;

const TRANSIENT_ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// Upper bound on a missed wake-up for a waiting reader or an idle worker.
const RECHECK: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// Every block, from playback onwards and then from the start.
    Full,
    /// Only this many blocks from playback onwards.
    Window(usize),
}

impl Coverage {
    pub fn for_size(total: u64) -> Coverage {
        if total <= FULL_DOWNLOAD_MAX {
            Coverage::Full
        } else {
            Coverage::Window(READ_AHEAD_BLOCKS)
        }
    }
}

pub fn block_count(total: u64) -> usize {
    total.div_ceil(BLOCK) as usize
}

/// Byte range `start..end` of block `index`.
pub fn block_bounds(total: u64, index: usize) -> (u64, u64) {
    let start = index as u64 * BLOCK;
    (start, (start + BLOCK).min(total))
}

/// The next block to fetch: blocks readers wait on first, in order asked, then
/// forwards from playback as far as the coverage reaches.
pub fn pick_block(
    have: &[bool],
    inflight: &[bool],
    demand: &VecDeque<usize>,
    playhead: usize,
    coverage: Coverage,
) -> Option<usize> {
    let free = |i: usize| i < have.len() && !have[i] && !inflight[i];
    if let Some(&d) = demand.iter().find(|&&d| free(d)) {
        return Some(d);
    }
    let n = have.len();
    let playhead = playhead.min(n);
    let end = match coverage {
        Coverage::Full => n,
        Coverage::Window(w) => playhead.saturating_add(w).min(n),
    };
    if let Some(i) = (playhead..end).find(|&i| free(i)) {
        return Some(i);
    }
    match coverage {
        Coverage::Full => (0..playhead).find(|&i| free(i)),
        Coverage::Window(_) => None,
    }
}

struct State {
    have: Vec<bool>,
    inflight: Vec<bool>,
    demand: VecDeque<usize>,
    playhead: usize,
    coverage: Coverage,
    missing: usize,
    failure: Option<String>,
    closed: bool,
    publish_to: Option<PathBuf>,
}

/// A local file being filled block by block.
pub struct BlockStore {
    // Option so Drop can close it before renaming: Windows will not rename an open file.
    file: Option<File>,
    path: PathBuf,
    total: u64,
    state: Mutex<State>,
    arrived: Condvar,
    work: tokio::sync::Notify,
}

impl BlockStore {
    /// Create `path` at the full `total` length, holding nothing yet.
    pub fn create(path: &Path, total: u64, coverage: Coverage) -> io::Result<Arc<BlockStore>> {
        if total == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "an empty file has nothing to fetch"));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = File::options().read(true).write(true).create(true).truncate(true).open(path)?;
        if let Err(e) = mark_sparse(&file) {
            tracing::debug!("could not make {} sparse: {e}", path.display());
        }
        file.set_len(total)?;
        let blocks = block_count(total);
        Ok(Arc::new(BlockStore {
            file: Some(file),
            path: path.to_path_buf(),
            total,
            state: Mutex::new(State {
                have: vec![false; blocks],
                inflight: vec![false; blocks],
                demand: VecDeque::new(),
                playhead: 0,
                coverage,
                missing: blocks,
                failure: None,
                closed: false,
                publish_to: None,
            }),
            arrived: Condvar::new(),
            work: tokio::sync::Notify::new(),
        }))
    }

    fn file(&self) -> &File {
        self.file.as_ref().expect("the file stays open until the store is dropped")
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_complete(&self) -> bool {
        self.state.lock().missing == 0
    }

    pub fn has_block(&self, index: usize) -> bool {
        self.state.lock().have.get(index).copied().unwrap_or(false)
    }

    /// Dropped complete, move the file to `target` instead of deleting it.
    pub fn publish_on_complete(&self, target: PathBuf) {
        self.state.lock().publish_to = Some(target);
    }

    /// The block playback is reading; the read-ahead follows it.
    pub fn set_playhead(&self, block: usize) {
        let mut s = self.state.lock();
        if s.playhead != block {
            s.playhead = block;
            drop(s);
            self.work.notify_one();
        }
    }

    /// Wake waiting readers with an error and let the workers go.
    pub fn close(&self) {
        self.state.lock().closed = true;
        self.arrived.notify_all();
        self.work.notify_waiters();
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().closed
    }

    /// Stop downloading. Arrived blocks still read; a missing one reports `why`.
    pub fn fail(&self, why: String) {
        {
            let mut s = self.state.lock();
            if s.failure.is_none() {
                s.failure = Some(why);
            }
        }
        self.arrived.notify_all();
        self.work.notify_waiters();
    }

    fn failure(&self) -> Option<String> {
        self.state.lock().failure.clone()
    }

    /// Mark the file in use, so the abandoned-download sweep leaves it.
    pub fn touch(&self) {
        let _ = crate::audio_cache::touch(&self.path);
    }

    fn claim(&self) -> Option<usize> {
        let mut s = self.state.lock();
        if s.closed || s.failure.is_some() {
            return None;
        }
        let pick = pick_block(&s.have, &s.inflight, &s.demand, s.playhead, s.coverage)?;
        s.inflight[pick] = true;
        Some(pick)
    }

    fn release(&self, index: usize) {
        self.state.lock().inflight[index] = false;
    }

    fn finish(&self, index: usize, bytes: &[u8]) -> io::Result<()> {
        let (start, end) = block_bounds(self.total, index);
        if bytes.len() as u64 != end - start {
            self.release(index);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("block {index} is {} bytes, expected {}", bytes.len(), end - start),
            ));
        }
        if let Err(e) = write_all_at(self.file(), bytes, start) {
            self.release(index);
            return Err(e);
        }
        {
            let mut s = self.state.lock();
            s.inflight[index] = false;
            if !s.have[index] {
                s.have[index] = true;
                s.missing -= 1;
            }
            s.demand.retain(|&d| d != index);
        }
        self.arrived.notify_all();
        Ok(())
    }

    /// Read from `pos`, waiting for its block. Returns at most the rest of that
    /// block, and 0 at the end of the file.
    pub fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        if pos >= self.total || buf.is_empty() {
            return Ok(0);
        }
        let index = (pos / BLOCK) as usize;
        {
            let mut s = self.state.lock();
            loop {
                if s.closed {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "the download was stopped"));
                }
                if s.have[index] {
                    break;
                }
                if let Some(why) = &s.failure {
                    return Err(io::Error::other(why.clone()));
                }
                if !s.demand.contains(&index) {
                    s.demand.push_back(index);
                    self.work.notify_one();
                }
                self.arrived.wait_for(&mut s, RECHECK);
            }
        }
        let (_, end) = block_bounds(self.total, index);
        let want = buf.len().min((end - pos) as usize);
        read_exact_at(self.file(), &mut buf[..want], pos)?;
        Ok(want)
    }
}

#[cfg(test)]
impl BlockStore {
    /// A store already holding all of `data`.
    pub(crate) fn filled_for_test(path: &Path, data: &[u8]) -> Arc<BlockStore> {
        let total = data.len() as u64;
        let store = BlockStore::create(path, total, Coverage::Full).expect("create a test store");
        for index in 0..block_count(total) {
            assert_eq!(store.claim(), Some(index));
            let (start, end) = block_bounds(total, index);
            store.finish(index, &data[start as usize..end as usize]).expect("fill a test store");
        }
        store
    }
}

impl Drop for BlockStore {
    fn drop(&mut self) {
        let (complete, target) = {
            let s = self.state.get_mut();
            (s.missing == 0, s.publish_to.take())
        };
        drop(self.file.take());
        if let (true, Some(target)) = (complete, target) {
            match std::fs::rename(&self.path, &target) {
                Ok(()) => return,
                Err(e) => tracing::warn!("could not cache {}: {e}", target.display()),
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// Asking the same address again will not help; renewing it might.
    Refused { why: String, generation: u64 },
    /// May pass: a dropped connection, a server error.
    Transient(String),
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Produces a fresh address for the same file, and the length it serves.
pub type Renewer = Box<dyn Fn() -> BoxFuture<'static, Result<(String, u64), String>> + Send + Sync>;

pub trait RangeSource: Send + Sync + 'static {
    /// Bytes `start..end` of the file.
    fn fetch(&self, start: u64, end: u64) -> BoxFuture<'_, Result<Vec<u8>, FetchError>>;

    /// Replace the address refused at `generation`; a no-op if already replaced.
    fn renew(&self, generation: u64) -> BoxFuture<'_, Result<(), String>>;
}

/// Retries what may pass; renews a refused address once.
async fn fetch_block(source: &dyn RangeSource, start: u64, end: u64) -> Result<Vec<u8>, String> {
    let mut transient = 0;
    let mut renewed = false;
    loop {
        match source.fetch(start, end).await {
            Ok(bytes) => return Ok(bytes),
            Err(FetchError::Transient(why)) => {
                transient += 1;
                if transient >= TRANSIENT_ATTEMPTS {
                    return Err(why);
                }
                tokio::time::sleep(RETRY_DELAY * transient).await;
            }
            Err(FetchError::Refused { why, generation }) => {
                if renewed {
                    return Err(why);
                }
                renewed = true;
                source
                    .renew(generation)
                    .await
                    .map_err(|e| format!("{why}; getting a new address failed: {e}"))?;
            }
        }
    }
}

/// Start workers filling `store`. They stop when it is complete, closed or failed.
pub fn spawn_workers(
    store: Arc<BlockStore>,
    source: Arc<dyn RangeSource>,
    workers: usize,
) -> Vec<tokio::task::JoinHandle<()>> {
    (0..workers)
        .map(|_| {
            let store = store.clone();
            let source = source.clone();
            tokio::spawn(async move { worker(store, source).await })
        })
        .collect()
}

async fn worker(store: Arc<BlockStore>, source: Arc<dyn RangeSource>) {
    loop {
        match store.claim() {
            Some(index) => {
                let (start, end) = block_bounds(store.total(), index);
                let result = fetch_block(source.as_ref(), start, end).await;
                if store.is_closed() {
                    store.release(index);
                    return;
                }
                match result {
                    Ok(bytes) => {
                        if let Err(e) = store.finish(index, &bytes) {
                            store.fail(format!("could not store block {index}: {e}"));
                            return;
                        }
                    }
                    Err(why) => {
                        store.release(index);
                        store.fail(why);
                        return;
                    }
                }
            }
            None => {
                if store.is_closed() || store.is_complete() || store.failure().is_some() {
                    return;
                }
                let _ = tokio::time::timeout(RECHECK, store.work.notified()).await;
            }
        }
    }
}

/// Byte ranges of a URL over HTTP.
pub struct HttpSource {
    client: reqwest::Client,
    url: parking_lot::RwLock<String>,
    generation: AtomicU64,
    total: u64,
    renew_lock: tokio::sync::Mutex<()>,
    renewer: Renewer,
}

impl HttpSource {
    /// A renewed address serving a different length is refused.
    pub fn new(client: reqwest::Client, url: String, total: u64, renewer: Renewer) -> HttpSource {
        HttpSource {
            client,
            url: parking_lot::RwLock::new(url),
            generation: AtomicU64::new(0),
            total,
            renew_lock: tokio::sync::Mutex::new(()),
            renewer,
        }
    }
}

/// `None` for a served range, `Some(true)` when retrying cannot help, `Some(false)` when it may.
fn classify_status(status: u16) -> Option<bool> {
    match status {
        206 => None,
        429 | 500..=599 => Some(false),
        // Includes 200: the server ignored the range and is sending the whole file.
        _ => Some(true),
    }
}

impl RangeSource for HttpSource {
    fn fetch(&self, start: u64, end: u64) -> BoxFuture<'_, Result<Vec<u8>, FetchError>> {
        Box::pin(async move {
            let generation = self.generation.load(Ordering::Relaxed);
            let url = self.url.read().clone();
            let response = self
                .client
                .get(&url)
                .header("accept", "*/*")
                .header("origin", "https://www.youtube.com")
                .header("referer", "https://www.youtube.com")
                .header("range", format!("bytes={}-{}", start, end - 1))
                .send()
                .await
                .map_err(|e| FetchError::Transient(format!("range at byte {start}: {e}")))?;
            let status = response.status().as_u16();
            match classify_status(status) {
                None => {}
                Some(true) => {
                    return Err(FetchError::Refused {
                        why: format!("HTTP {status} at byte {start}"),
                        generation,
                    })
                }
                Some(false) => return Err(FetchError::Transient(format!("HTTP {status} at byte {start}"))),
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|e| FetchError::Transient(format!("reading the range at byte {start}: {e}")))?;
            if bytes.len() as u64 != end - start {
                return Err(FetchError::Transient(format!(
                    "the range at byte {start} was {} bytes, expected {}",
                    bytes.len(),
                    end - start
                )));
            }
            Ok(bytes.to_vec())
        })
    }

    fn renew(&self, generation: u64) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let _guard = self.renew_lock.lock().await;
            if self.generation.load(Ordering::Relaxed) != generation {
                return Ok(());
            }
            let (url, total) = (self.renewer)().await?;
            if total != self.total {
                return Err(format!("the new address serves {total} bytes, not {}", self.total));
            }
            *self.url.write() = url;
            self.generation.fetch_add(1, Ordering::Relaxed);
            tracing::info!("YouTube: renewed the audio address");
            Ok(())
        })
    }
}

/// Reads a store from any position, waiting for blocks. Seekable, for files
/// without a segment index.
pub struct StoreReader {
    store: Arc<BlockStore>,
    pos: u64,
}

impl StoreReader {
    pub fn new(store: Arc<BlockStore>, pos: u64) -> StoreReader {
        StoreReader { store, pos }
    }
}

impl Read for StoreReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.store.read_at(self.pos, buf)?;
        self.pos += n as u64;
        self.store.set_playhead((self.pos.min(self.store.total().saturating_sub(1)) / BLOCK) as usize);
        Ok(n)
    }
}

impl Seek for StoreReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let total = self.store.total() as i128;
        let next = match to {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(d) => total + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if next < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}

impl symphonia::core::io::MediaSource for StoreReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.store.total())
    }
}

/// A file's header followed by everything from one fragment onwards.
///
/// Unseekable on purpose: the decoder then reads fragments in order instead of
/// scanning the whole file first.
pub struct SegmentReader {
    init: Arc<[u8]>,
    init_pos: usize,
    body: StoreReader,
}

impl SegmentReader {
    /// `init` is the header; playback continues from byte `from` of the file.
    pub fn new(init: Arc<[u8]>, store: Arc<BlockStore>, from: u64) -> SegmentReader {
        SegmentReader { init, init_pos: 0, body: StoreReader::new(store, from) }
    }
}

impl Read for SegmentReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.init_pos < self.init.len() {
            let n = buf.len().min(self.init.len() - self.init_pos);
            buf[..n].copy_from_slice(&self.init[self.init_pos..self.init_pos + n]);
            self.init_pos += n;
            return Ok(n);
        }
        self.body.read(buf)
    }
}

impl Seek for SegmentReader {
    fn seek(&mut self, _to: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "a fragment stream is read in order"))
    }
}

impl symphonia::core::io::MediaSource for SegmentReader {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        // Positional: readers and workers sharing the handle cannot move each other's offset.
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read")),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "short write")),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Without this, NTFS zero-fills everything before a block written far into a new file.
#[cfg(windows)]
fn mark_sparse(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
    #[link(name = "kernel32")]
    extern "system" {
        fn DeviceIoControl(
            device: *mut std::ffi::c_void,
            code: u32,
            in_buf: *const std::ffi::c_void,
            in_len: u32,
            out_buf: *mut std::ffi::c_void,
            out_len: u32,
            returned: *mut u32,
            overlapped: *mut std::ffi::c_void,
        ) -> i32;
    }
    let mut returned = 0u32;
    // SAFETY: the handle belongs to `file`, which outlives the call; both
    // buffers are null with zero lengths, which FSCTL_SET_SPARSE accepts; the
    // handle is not overlapped, so no OVERLAPPED structure is needed.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn mark_sparse(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn scratch(name: &str) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ttspotify_ranged_{}_{}_{}",
            name,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pattern(len: u64) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn q(items: &[usize]) -> VecDeque<usize> {
        items.iter().copied().collect()
    }

    #[test]
    fn the_last_block_is_the_remainder() {
        let total = 2 * BLOCK + 10;
        assert_eq!(block_count(total), 3);
        assert_eq!(block_bounds(total, 0), (0, BLOCK));
        assert_eq!(block_bounds(total, 2), (2 * BLOCK, total));
        assert_eq!(block_count(BLOCK), 1);
    }

    #[test]
    fn small_files_download_completely_and_large_ones_keep_a_window() {
        assert_eq!(Coverage::for_size(4 << 20), Coverage::Full);
        assert_eq!(Coverage::for_size(FULL_DOWNLOAD_MAX), Coverage::Full);
        assert_eq!(Coverage::for_size(FULL_DOWNLOAD_MAX + 1), Coverage::Window(READ_AHEAD_BLOCKS));
    }

    #[test]
    fn a_waiting_reader_is_served_before_the_read_ahead() {
        let have = [false; 10];
        let inflight = [false; 10];
        assert_eq!(pick_block(&have, &inflight, &q(&[7]), 0, Coverage::Full), Some(7));
        assert_eq!(pick_block(&have, &inflight, &q(&[8, 3]), 0, Coverage::Full), Some(8));
    }

    #[test]
    fn a_demand_already_taken_care_of_is_passed_over() {
        let mut have = [false; 10];
        let mut inflight = [false; 10];
        have[7] = true;
        inflight[8] = true;
        assert_eq!(pick_block(&have, &inflight, &q(&[7, 8, 9]), 0, Coverage::Full), Some(9));
    }

    #[test]
    fn read_ahead_runs_forwards_from_playback() {
        let mut have = [false; 10];
        let inflight = [false; 10];
        have[4] = true;
        assert_eq!(pick_block(&have, &inflight, &q(&[]), 4, Coverage::Full), Some(5));
    }

    #[test]
    fn a_full_download_wraps_round_to_the_start() {
        let mut have = [true; 10];
        let inflight = [false; 10];
        have[1] = false;
        assert_eq!(pick_block(&have, &inflight, &q(&[]), 6, Coverage::Full), Some(1));
    }

    #[test]
    fn a_window_stops_at_its_edge_and_never_looks_behind() {
        let mut have = [false; 10];
        let inflight = [false; 10];
        have[5] = true;
        have[6] = true;
        assert_eq!(pick_block(&have, &inflight, &q(&[]), 5, Coverage::Window(2)), None);
        assert_eq!(pick_block(&have, &inflight, &q(&[]), 5, Coverage::Window(3)), Some(7));
        assert_eq!(pick_block(&have, &inflight, &q(&[0]), 5, Coverage::Window(2)), Some(0), "demand ignores the window");
    }

    #[test]
    fn nothing_to_do_when_everything_is_here_or_on_its_way() {
        let have = [true, false];
        let inflight = [false, true];
        assert_eq!(pick_block(&have, &inflight, &q(&[1]), 0, Coverage::Full), None);
    }

    #[test]
    fn a_playhead_past_the_end_does_not_panic() {
        let have = [false; 3];
        let inflight = [false; 3];
        assert_eq!(pick_block(&have, &inflight, &q(&[]), 99, Coverage::Window(4)), None);
        assert_eq!(pick_block(&have, &inflight, &q(&[]), 99, Coverage::Full), Some(0));
    }

    #[test]
    fn a_stored_block_reads_back() {
        let dir = scratch("readback");
        let total = BLOCK + 100;
        let data = pattern(total);
        let store = BlockStore::create(&dir.join("t.part"), total, Coverage::Full).unwrap();
        for i in 0..2 {
            assert_eq!(store.claim(), Some(i));
            let (s, e) = block_bounds(total, i);
            store.finish(i, &data[s as usize..e as usize]).unwrap();
        }
        assert!(store.is_complete());
        let mut buf = vec![0u8; 200];
        assert_eq!(store.read_at(BLOCK - 50, &mut buf).unwrap(), 50, "a read stops at its block's end");
        assert_eq!(&buf[..50], &data[(BLOCK - 50) as usize..BLOCK as usize]);
        assert_eq!(store.read_at(total, &mut buf).unwrap(), 0);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_block_of_the_wrong_length_is_refused_and_can_be_fetched_again() {
        let dir = scratch("wronglen");
        let store = BlockStore::create(&dir.join("t.part"), BLOCK, Coverage::Full).unwrap();
        assert_eq!(store.claim(), Some(0));
        assert!(store.finish(0, &[1, 2, 3]).is_err());
        assert!(!store.has_block(0));
        assert_eq!(store.claim(), Some(0));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reader_waits_for_its_block_and_asks_for_it() {
        let dir = scratch("wait");
        let total = 3 * BLOCK;
        let data = pattern(total);
        let store = BlockStore::create(&dir.join("t.part"), total, Coverage::Window(0)).unwrap();
        let reader = {
            let store = store.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 10];
                let n = store.read_at(2 * BLOCK + 5, &mut buf).unwrap();
                buf.truncate(n);
                buf
            })
        };
        let claimed = loop {
            if let Some(b) = store.claim() {
                break b;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(claimed, 2, "the block the reader needs is the one fetched");
        let (s, e) = block_bounds(total, 2);
        store.finish(2, &data[s as usize..e as usize]).unwrap();
        let got = reader.join().unwrap();
        assert_eq!(got, data[(2 * BLOCK + 5) as usize..(2 * BLOCK + 15) as usize]);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_wakes_a_waiting_reader_with_an_error() {
        let dir = scratch("close");
        let store = BlockStore::create(&dir.join("t.part"), BLOCK, Coverage::Full).unwrap();
        let reader = {
            let store = store.clone();
            std::thread::spawn(move || store.read_at(0, &mut [0u8; 4]))
        };
        std::thread::sleep(Duration::from_millis(50));
        store.close();
        let err = reader.join().unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn after_a_failure_what_arrived_still_plays_and_the_rest_errors() {
        let dir = scratch("fail");
        let total = 2 * BLOCK;
        let store = BlockStore::create(&dir.join("t.part"), total, Coverage::Full).unwrap();
        assert_eq!(store.claim(), Some(0));
        store.finish(0, &pattern(BLOCK)).unwrap();
        store.fail("HTTP 403 at byte 1048576".into());
        assert!(store.read_at(10, &mut [0u8; 4]).is_ok());
        let err = store.read_at(BLOCK + 1, &mut [0u8; 4]).unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_complete_download_is_published_when_dropped() {
        let dir = scratch("publish");
        let part = dir.join("t.part");
        let target = dir.join("t.m4a");
        let store = BlockStore::create(&part, 10, Coverage::Full).unwrap();
        store.publish_on_complete(target.clone());
        assert_eq!(store.claim(), Some(0));
        store.finish(0, &pattern(10)).unwrap();
        drop(store);
        assert!(!part.exists());
        assert_eq!(std::fs::read(&target).unwrap(), pattern(10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_incomplete_or_unpublished_download_leaves_nothing_behind() {
        let dir = scratch("discard");
        let part = dir.join("t.part");
        let target = dir.join("t.m4a");
        let store = BlockStore::create(&part, 2 * BLOCK, Coverage::Full).unwrap();
        store.publish_on_complete(target.clone());
        drop(store);
        assert!(!part.exists());
        assert!(!target.exists(), "a half download must never become a cache entry");

        let complete = BlockStore::create(&part, 10, Coverage::Full).unwrap();
        assert_eq!(complete.claim(), Some(0));
        complete.finish(0, &pattern(10)).unwrap();
        drop(complete);
        assert!(!part.exists() && !target.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_segment_reader_gives_the_header_then_the_file_from_the_fragment() {
        let dir = scratch("segment");
        let total = BLOCK + 500;
        let data = pattern(total);
        let store = BlockStore::create(&dir.join("t.part"), total, Coverage::Full).unwrap();
        for i in 0..2 {
            store.claim();
            let (s, e) = block_bounds(total, i);
            store.finish(i, &data[s as usize..e as usize]).unwrap();
        }
        let init: Arc<[u8]> = Arc::from(&b"HEADER"[..]);
        let from = BLOCK - 20;
        let mut reader = SegmentReader::new(init, store.clone(), from);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        let mut expected = b"HEADER".to_vec();
        expected.extend_from_slice(&data[from as usize..]);
        assert_eq!(out, expected);
        use symphonia::core::io::MediaSource;
        assert!(!reader.is_seekable());
        assert!(reader.seek(SeekFrom::Start(0)).is_err());
        drop(reader);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reading_moves_the_playhead_the_read_ahead_follows() {
        let dir = scratch("playhead");
        let total = 4 * BLOCK;
        let store = BlockStore::create(&dir.join("t.part"), total, Coverage::Window(1)).unwrap();
        for i in 0..4 {
            store.claim();
            let (s, e) = block_bounds(total, i);
            store.finish(i, &pattern(e - s)).unwrap();
        }
        let mut reader = StoreReader::new(store.clone(), 0);
        reader.seek(SeekFrom::Start(2 * BLOCK + 1)).unwrap();
        reader.read_exact(&mut [0u8; 8]).unwrap();
        assert_eq!(store.state.lock().playhead, 2);
        drop(reader);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_statuses_are_sorted_into_passing_and_final() {
        assert_eq!(classify_status(206), None);
        assert_eq!(classify_status(503), Some(false));
        assert_eq!(classify_status(429), Some(false));
        assert_eq!(classify_status(403), Some(true));
        assert_eq!(classify_status(410), Some(true));
        assert_eq!(classify_status(200), Some(true));
    }

    /// An in-memory file with scripted failures.
    struct FakeSource {
        data: Vec<u8>,
        fetched: Mutex<Vec<u64>>,
        transient_failures: AtomicUsize,
        refusals: AtomicUsize,
        renewals: AtomicUsize,
        renew_ok: bool,
    }

    impl FakeSource {
        fn new(data: Vec<u8>) -> FakeSource {
            FakeSource {
                data,
                fetched: Mutex::new(Vec::new()),
                transient_failures: AtomicUsize::new(0),
                refusals: AtomicUsize::new(0),
                renewals: AtomicUsize::new(0),
                renew_ok: true,
            }
        }
    }

    impl RangeSource for FakeSource {
        fn fetch(&self, start: u64, end: u64) -> BoxFuture<'_, Result<Vec<u8>, FetchError>> {
            Box::pin(async move {
                if self.transient_failures.load(Ordering::Relaxed) > 0 {
                    self.transient_failures.fetch_sub(1, Ordering::Relaxed);
                    return Err(FetchError::Transient("connection reset".into()));
                }
                if self.refusals.load(Ordering::Relaxed) > 0 {
                    self.refusals.fetch_sub(1, Ordering::Relaxed);
                    return Err(FetchError::Refused { why: "HTTP 403".into(), generation: 0 });
                }
                self.fetched.lock().push(start);
                Ok(self.data[start as usize..end as usize].to_vec())
            })
        }

        fn renew(&self, _generation: u64) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                self.renewals.fetch_add(1, Ordering::Relaxed);
                if self.renew_ok {
                    Ok(())
                } else {
                    Err("the sidecar failed".into())
                }
            })
        }
    }

    async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..400 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn workers_fill_a_small_file_completely_and_exactly() {
        let dir = scratch("workers_full");
        let total = 5 * BLOCK + 123;
        let data = pattern(total);
        let source = Arc::new(FakeSource::new(data.clone()));
        let store = BlockStore::create(&dir.join("t.part"), total, Coverage::Full).unwrap();
        spawn_workers(store.clone(), source.clone(), WORKERS);
        wait_until("the download to complete", || store.is_complete()).await;
        let read_store = store.clone();
        let copy = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            StoreReader::new(read_store, 0).read_to_end(&mut out).unwrap();
            out
        })
        .await
        .unwrap();
        assert_eq!(copy, data);
        assert_eq!(source.fetched.lock().len(), 6, "each block fetched once");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_large_file_fetches_only_what_playback_needs() {
        let dir = scratch("workers_window");
        let total = 100 * BLOCK;
        let data = pattern(total);
        let source = Arc::new(FakeSource::new(data.clone()));
        let store = BlockStore::create(&dir.join("t.part"), total, Coverage::Window(3)).unwrap();
        spawn_workers(store.clone(), source.clone(), WORKERS);

        let read_store = store.clone();
        let got = tokio::task::spawn_blocking(move || {
            let mut reader = StoreReader::new(read_store, 0);
            reader.seek(SeekFrom::Start(60 * BLOCK + 7)).unwrap();
            let mut buf = [0u8; 16];
            reader.read_exact(&mut buf).unwrap();
            buf
        })
        .await
        .unwrap();
        assert_eq!(&got[..], &data[(60 * BLOCK + 7) as usize..(60 * BLOCK + 23) as usize]);

        wait_until("the window after the seek", || (60..63).all(|b| store.has_block(b))).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let fetched = source.fetched.lock().clone();
        assert!(
            fetched.len() <= 6,
            "a window of 3 should not download the file; fetched {} blocks: {:?}",
            fetched.len(),
            fetched.iter().map(|s| s / BLOCK).collect::<Vec<_>>()
        );
        assert!(!store.has_block(99));
        store.close();
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_passing_failure_is_retried() {
        let dir = scratch("retry");
        let source = Arc::new(FakeSource::new(pattern(BLOCK)));
        source.transient_failures.store(2, Ordering::Relaxed);
        let store = BlockStore::create(&dir.join("t.part"), BLOCK, Coverage::Full).unwrap();
        spawn_workers(store.clone(), source.clone(), 1);
        wait_until("the retried block", || store.is_complete()).await;
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_address_is_renewed_once_and_the_download_carries_on() {
        let dir = scratch("renew");
        let source = Arc::new(FakeSource::new(pattern(BLOCK)));
        source.refusals.store(1, Ordering::Relaxed);
        let store = BlockStore::create(&dir.join("t.part"), BLOCK, Coverage::Full).unwrap();
        spawn_workers(store.clone(), source.clone(), 1);
        wait_until("the block after renewal", || store.is_complete()).await;
        assert_eq!(source.renewals.load(Ordering::Relaxed), 1);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_address_refused_again_after_renewal_fails_the_reader() {
        let dir = scratch("refused_twice");
        let source = Arc::new(FakeSource::new(pattern(BLOCK)));
        source.refusals.store(2, Ordering::Relaxed);
        let store = BlockStore::create(&dir.join("t.part"), BLOCK, Coverage::Full).unwrap();
        spawn_workers(store.clone(), source.clone(), 1);
        let read_store = store.clone();
        let result = tokio::task::spawn_blocking(move || read_store.read_at(0, &mut [0u8; 4]))
            .await
            .unwrap();
        assert!(result.unwrap_err().to_string().contains("403"));
        assert_eq!(source.renewals.load(Ordering::Relaxed), 1, "renewed once, not forever");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_renewal_fails_the_reader_with_both_reasons() {
        let dir = scratch("renew_fails");
        let mut fake = FakeSource::new(pattern(BLOCK));
        fake.renew_ok = false;
        let source = Arc::new(fake);
        source.refusals.store(1, Ordering::Relaxed);
        let store = BlockStore::create(&dir.join("t.part"), BLOCK, Coverage::Full).unwrap();
        spawn_workers(store.clone(), source.clone(), 1);
        let read_store = store.clone();
        let err = tokio::task::spawn_blocking(move || read_store.read_at(0, &mut [0u8; 4]))
            .await
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(err.contains("403") && err.contains("sidecar"), "{err}");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_stops_the_workers() {
        let dir = scratch("stop");
        let total = 100 * BLOCK;
        let source = Arc::new(FakeSource::new(pattern(total)));
        let store = BlockStore::create(&dir.join("t.part"), total, Coverage::Window(2)).unwrap();
        let handles = spawn_workers(store.clone(), source, WORKERS);
        store.close();
        for h in handles {
            tokio::time::timeout(Duration::from_secs(2), h)
                .await
                .expect("a worker kept running after close")
                .unwrap();
        }
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
