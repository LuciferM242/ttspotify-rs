//! A buffer that is written and read at the same time.
//!
//! yt-dlp writes a track to us over a pipe while symphonia wants to decode it.
//! Waiting for the download to finish before starting the decoder is simple,
//! and it is what this used to do — at the cost of every YouTube track starting
//! only after its whole file had arrived.
//!
//! This lets the decoder start on the first bytes: reads past what has arrived
//! block until the downloader catches up, rather than reporting end-of-file.
//! Everything downloaded stays in memory, so seeking backwards is still
//! instant, and seeking forwards simply waits for the bytes.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Shared state behind both halves of the buffer.
#[derive(Debug, Default)]
struct Shared {
    data: Mutex<Vec<u8>>,
    /// Set when the download finished, failed, or was abandoned: readers past
    /// the end must stop waiting and report end-of-file.
    closed: AtomicBool,
    /// Set when the download failed, so a reader can fail rather than silently
    /// decode a truncated track.
    failed: Mutex<Option<String>>,
    /// Signalled whenever data is appended or the buffer closes.
    grew: Condvar,
}

/// The writing half, held by whatever is fetching the bytes.
#[derive(Debug, Clone)]
pub struct StreamWriter {
    shared: Arc<Shared>,
}

/// The reading half, handed to the decoder. Cheap to clone; each clone has its
/// own read position.
#[derive(Debug)]
pub struct StreamReader {
    shared: Arc<Shared>,
    pos: u64,
}

/// Create a connected writer/reader pair.
pub fn channel() -> (StreamWriter, StreamReader) {
    let shared = Arc::new(Shared::default());
    (
        StreamWriter { shared: shared.clone() },
        StreamReader { shared, pos: 0 },
    )
}

impl StreamWriter {
    /// Append downloaded bytes and wake any reader waiting for them.
    pub fn append(&self, bytes: &[u8]) {
        {
            let mut data = self.shared.data.lock().unwrap_or_else(|e| e.into_inner());
            data.extend_from_slice(bytes);
        }
        self.shared.grew.notify_all();
    }

    /// How much has been written so far.
    pub fn len(&self) -> usize {
        self.shared
            .data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The download finished: readers may now hit end-of-file.
    pub fn finish(&self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.grew.notify_all();
    }

    /// The download failed. Readers waiting for more data fail with `reason`
    /// instead of decoding a half-track as though it had ended normally.
    pub fn fail(&self, reason: impl Into<String>) {
        *self.shared.failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason.into());
        self.finish();
    }
}

impl StreamReader {
    /// Bytes available right now (not the eventual total, which is unknown
    /// until the download ends).
    pub fn available(&self) -> u64 {
        self.shared
            .data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len() as u64
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// Block until `self.pos` has data behind it, the buffer closes, or the
    /// download fails.
    fn wait_for_data(&self) -> io::Result<usize> {
        let mut data = self.shared.data.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(reason) = self.shared.failed.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                return Err(io::Error::other(reason));
            }
            if (self.pos as usize) < data.len() {
                return Ok(data.len());
            }
            if self.shared.closed.load(Ordering::SeqCst) {
                return Ok(data.len()); // Genuine end of file.
            }
            data = self
                .shared
                .grew
                .wait(data)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Read for StreamReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let len = self.wait_for_data()?;
        let start = self.pos as usize;
        if start >= len {
            return Ok(0); // End of a finished download.
        }
        let data = self.shared.data.lock().unwrap_or_else(|e| e.into_inner());
        let n = out.len().min(len - start);
        out[..n].copy_from_slice(&data[start..start + n]);
        drop(data);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(delta) => self.pos as i64 + delta,
            // The total size is only known once the download ends; waiting for
            // that is the honest answer to "seek from the end".
            SeekFrom::End(delta) => {
                let mut data = self.shared.data.lock().unwrap_or_else(|e| e.into_inner());
                while !self.shared.closed.load(Ordering::SeqCst) {
                    data = self
                        .shared
                        .grew
                        .wait(data)
                        .unwrap_or_else(|e| e.into_inner());
                }
                data.len() as i64 + delta
            }
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the stream",
            ));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

impl symphonia::core::io::MediaSource for StreamReader {
    fn is_seekable(&self) -> bool {
        // Everything downloaded is kept, and reads ahead of the download wait,
        // so seeking works in both directions.
        true
    }

    fn byte_len(&self) -> Option<u64> {
        // Only known once the download has finished; before that, saying
        // nothing is better than guessing.
        self.is_closed().then(|| self.available())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn a_reader_sees_bytes_appended_before_it_read() {
        let (writer, mut reader) = channel();
        writer.append(b"hello");
        writer.finish();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello");
    }

    #[test]
    fn a_read_past_the_download_waits_instead_of_reporting_end_of_file() {
        // The whole point: the decoder starts on partial data and blocks for
        // the rest rather than seeing a truncated file.
        let (writer, mut reader) = channel();
        writer.append(b"part one ");

        let w = writer.clone();
        let feeder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            w.append(b"part two");
            w.finish();
        });

        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        feeder.join().unwrap();
        assert_eq!(out, b"part one part two");
    }

    #[test]
    fn end_of_file_is_reported_once_the_download_finishes() {
        let (writer, mut reader) = channel();
        writer.append(b"abc");
        writer.finish();
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 3);
        assert_eq!(reader.read(&mut buf).unwrap(), 0, "second read is EOF");
    }

    #[test]
    fn a_failed_download_surfaces_as_a_read_error() {
        // Otherwise a half-downloaded track decodes as though it simply ended,
        // and the listener hears it cut off with no explanation.
        let (writer, mut reader) = channel();
        writer.append(b"abc");
        let w = writer.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            w.fail("yt-dlp died");
        });
        let mut out = Vec::new();
        let err = reader.read_to_end(&mut out).unwrap_err();
        assert!(err.to_string().contains("yt-dlp died"), "got: {err}");
    }

    #[test]
    fn seeking_backwards_into_downloaded_data_is_immediate() {
        let (writer, mut reader) = channel();
        writer.append(b"0123456789");
        writer.finish();
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"0123");
        reader.seek(SeekFrom::Start(1)).unwrap();
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"1234");
    }

    #[test]
    fn seeking_forwards_past_the_download_waits_for_the_bytes() {
        let (writer, mut reader) = channel();
        writer.append(b"0123");
        let w = writer.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            w.append(b"456789");
            w.finish();
        });
        reader.seek(SeekFrom::Start(6)).unwrap();
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"67");
    }

    #[test]
    fn seeking_from_the_end_waits_for_the_total_size() {
        let (writer, mut reader) = channel();
        let w = writer.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            w.append(b"0123456789");
            w.finish();
        });
        let pos = reader.seek(SeekFrom::End(-2)).unwrap();
        assert_eq!(pos, 8);
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"89");
    }

    #[test]
    fn a_negative_seek_is_an_error_not_a_panic() {
        let (writer, mut reader) = channel();
        writer.append(b"abc");
        writer.finish();
        assert!(reader.seek(SeekFrom::Current(-5)).is_err());
    }

    #[test]
    fn the_length_is_unknown_until_the_download_ends() {
        use symphonia::core::io::MediaSource;
        let (writer, reader) = channel();
        writer.append(b"abc");
        assert_eq!(reader.byte_len(), None, "still downloading");
        writer.finish();
        assert_eq!(reader.byte_len(), Some(3));
        assert!(reader.is_seekable());
    }

    #[test]
    fn a_reader_keeps_up_with_a_writer_feeding_it_in_small_chunks() {
        let (writer, mut reader) = channel();
        let w = writer.clone();
        let feeder = std::thread::spawn(move || {
            for i in 0..200u8 {
                w.append(&[i]);
                if i % 20 == 0 {
                    std::thread::yield_now();
                }
            }
            w.finish();
        });
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        feeder.join().unwrap();
        let expected: Vec<u8> = (0..200u8).collect();
        assert_eq!(out, expected, "bytes must arrive in order and complete");
    }
}
