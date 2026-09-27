//! Layout of YouTube's fragmented MP4 audio: header length, encoder priming,
//! and where each fragment starts in bytes and in time.
//!
//! Fragment sample offsets are relative to their own `moof`, so playback can
//! start at any fragment by feeding the header followed by that fragment.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Bytes from the start of the file through the end of `moov`.
    pub init_len: u64,
    /// Units per second of `priming` and the segment times.
    pub timescale: u32,
    /// Encoder delay: decoded audio before presentation time zero, all of it
    /// in the first fragment. The index's times leave it out.
    pub priming: u64,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Byte offset of the fragment's `moof`.
    pub offset: u64,
    pub size: u64,
    /// Presentation time of the fragment's start, in timescale units.
    pub start: u64,
    pub duration: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// Call again with at least this many bytes from the start of the file.
    NeedMore(u64),
    /// Not a layout this module plays; read the file whole instead.
    Unsupported(String),
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutError::NeedMore(n) => write!(f, "need the first {n} bytes"),
            LayoutError::Unsupported(why) => write!(f, "unsupported layout: {why}"),
        }
    }
}

fn unsupported<T>(why: impl Into<String>) -> Result<T, LayoutError> {
    Err(LayoutError::Unsupported(why.into()))
}

fn be_u32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn be_u64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes([
        b[at], b[at + 1], b[at + 2], b[at + 3], b[at + 4], b[at + 5], b[at + 6], b[at + 7],
    ])
}

/// Read the layout from the first bytes of a file `total_len` bytes long.
pub fn parse_layout(head: &[u8], total_len: u64) -> Result<Layout, LayoutError> {
    let mut at: u64 = 0;
    let mut seen_ftyp = false;
    let mut moov: Option<(usize, usize)> = None;

    loop {
        if at + 8 > head.len() as u64 {
            if at + 8 > total_len {
                return unsupported("the file ends before a segment index");
            }
            return Err(LayoutError::NeedMore(at + 16));
        }
        let pos = at as usize;
        let small = be_u32(head, pos) as u64;
        let kind = &head[pos + 4..pos + 8];
        let (size, header_len) = match small {
            1 => {
                if at + 16 > head.len() as u64 {
                    return Err(LayoutError::NeedMore(at + 16));
                }
                (be_u64(head, pos + 8), 16)
            }
            0 => return unsupported("a box before the segment index runs to the end of the file"),
            n => (n, 8),
        };
        if size < header_len {
            return unsupported(format!("a box at byte {at} is shorter than its own header"));
        }
        let end = at
            .checked_add(size)
            .ok_or_else(|| LayoutError::Unsupported("box size overflows".into()))?;
        if end > total_len {
            return unsupported(format!("a box at byte {at} runs past the end of the file"));
        }

        match kind {
            b"ftyp" => seen_ftyp = true,
            b"moov" => {
                if !seen_ftyp {
                    return unsupported("moov comes before ftyp");
                }
                if end > head.len() as u64 {
                    return Err(LayoutError::NeedMore(end));
                }
                moov = Some((pos + header_len as usize, end as usize));
            }
            b"sidx" => {
                // A decoder that cannot seek stops at the index, so moov must come first.
                let Some((moov_start, moov_end)) = moov else {
                    return unsupported("the segment index comes before moov");
                };
                if end > head.len() as u64 {
                    return Err(LayoutError::NeedMore(end));
                }
                let body = &head[pos + header_len as usize..end as usize];
                let mut layout = parse_sidx(body, end, moov_end as u64, total_len)?;
                if let Some((media_time, media_timescale)) = edit_priming(&head[moov_start..moov_end]) {
                    layout.priming =
                        (media_time as u128 * layout.timescale as u128 / media_timescale as u128) as u64;
                }
                return Ok(layout);
            }
            b"moof" | b"mdat" => return unsupported("a fragment comes before any segment index"),
            _ => {}
        }
        at = end;
    }
}

/// `sidx_end` is the file offset just past the box: the index's offsets start there.
fn parse_sidx(body: &[u8], sidx_end: u64, init_len: u64, total_len: u64) -> Result<Layout, LayoutError> {
    let need = |n: usize| {
        if body.len() < n {
            unsupported("the segment index is truncated")
        } else {
            Ok(())
        }
    };
    need(12)?;
    let version = body[0];
    let timescale = be_u32(body, 8);
    if timescale == 0 {
        return unsupported("the segment index has a timescale of zero");
    }
    let (earliest, first_offset, mut at) = match version {
        0 => {
            need(20)?;
            (be_u32(body, 12) as u64, be_u32(body, 16) as u64, 20)
        }
        1 => {
            need(28)?;
            (be_u64(body, 12), be_u64(body, 20), 28)
        }
        v => return unsupported(format!("segment index version {v}")),
    };
    need(at + 4)?;
    let count = u16::from_be_bytes([body[at + 2], body[at + 3]]) as usize;
    at += 4;
    if count == 0 {
        return unsupported("the segment index lists no segments");
    }
    need(at + count * 12)?;

    let mut offset = sidx_end + first_offset;
    let mut start = earliest;
    let mut segments = Vec::with_capacity(count);
    for i in 0..count {
        let e = at + i * 12;
        let word = be_u32(body, e);
        if word & 0x8000_0000 != 0 {
            return unsupported("the segment index points at further indexes");
        }
        let size = (word & 0x7fff_ffff) as u64;
        let duration = be_u32(body, e + 4) as u64;
        if size == 0 {
            return unsupported(format!("segment {i} is empty"));
        }
        segments.push(Segment { offset, size, start, duration });
        offset += size;
        start += duration;
    }
    if segments[0].offset < init_len {
        return unsupported("the first segment overlaps the header");
    }
    if offset > total_len {
        return unsupported("the segments run past the end of the file");
    }
    Ok(Layout { init_len, timescale, priming: 0, segments })
}

/// The boxes in `data`, stopping at the first that does not fit.
fn boxes(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 8 <= data.len() {
        let small = be_u32(data, at) as usize;
        let (size, header) = match small {
            1 if at + 16 <= data.len() => (be_u64(data, at + 8) as usize, 16),
            n => (n, 8),
        };
        if size < header || at.checked_add(size).is_none_or(|end| end > data.len()) {
            break;
        }
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&data[at + 4..at + 8]);
        out.push((kind, &data[at + header..at + size]));
        at += size;
    }
    out
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).into_iter().find(|(k, _)| k == kind).map(|(_, body)| body)
}

/// The first track's encoder delay from its edit list, as (media time,
/// media timescale).
fn edit_priming(moov: &[u8]) -> Option<(u64, u32)> {
    let trak = child(moov, b"trak")?;
    let mdhd = child(child(trak, b"mdia")?, b"mdhd")?;
    let media_timescale = match mdhd.first()? {
        1 if mdhd.len() >= 24 => be_u32(mdhd, 20),
        0 if mdhd.len() >= 16 => be_u32(mdhd, 12),
        _ => return None,
    };
    if media_timescale == 0 {
        return None;
    }
    let elst = child(child(trak, b"edts")?, b"elst")?;
    let version = *elst.first()?;
    if elst.len() < 8 {
        return None;
    }
    let count = be_u32(elst, 4) as usize;
    let entry = if version == 1 { 20 } else { 12 };
    for i in 0..count {
        let e = 8 + i * entry;
        if e + entry > elst.len() {
            return None;
        }
        // An empty edit is -1: a gap, not a media position.
        let media_time = if version == 1 {
            be_u64(elst, e + 8) as i64
        } else {
            be_u32(elst, e + 4) as i32 as i64
        };
        if media_time >= 0 {
            return Some((media_time as u64, media_timescale));
        }
    }
    None
}

/// Encoder priming in decoded frames at `sample_rate`, for a file played whole.
/// Zero when the header declares none or cannot be read from `head`.
pub fn priming_frames(head: &[u8], sample_rate: u32) -> u64 {
    boxes(head)
        .into_iter()
        .find(|(k, _)| k == b"moov")
        .and_then(|(_, moov)| edit_priming(moov))
        .map(|(media_time, timescale)| (media_time as u128 * sample_rate as u128 / timescale as u128) as u64)
        .unwrap_or(0)
}

impl Layout {
    /// Rounds down.
    pub fn units_to_ms(&self, units: u64) -> u64 {
        (units as u128 * 1000 / self.timescale as u128) as u64
    }

    /// Rounds down and saturates, so an absurd target lands past the end.
    pub fn ms_to_units(&self, ms: u64) -> u64 {
        (ms as u128 * self.timescale as u128 / 1000).min(u64::MAX as u128) as u64
    }

    pub fn duration_ms(&self) -> u64 {
        let last = self.segments[self.segments.len() - 1];
        self.units_to_ms(last.start + last.duration)
    }

    /// The fragment playing at `ms`, and how much decoded audio of that
    /// fragment comes before it, in timescale units. `None` at or past the end.
    pub fn locate(&self, ms: u64) -> Option<(usize, u64)> {
        let units = self.ms_to_units(ms);
        let last = self.segments[self.segments.len() - 1];
        if units >= last.start + last.duration {
            return None;
        }
        let index = self.segments.partition_point(|s| s.start <= units).saturating_sub(1);
        let mut into = units.saturating_sub(self.segments[index].start);
        if index == 0 {
            into += self.priming;
        }
        Some((index, into))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    fn bx64(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = 1u32.to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(&((payload.len() + 16) as u64).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// A version 0 sidx with direct references of (size, duration).
    fn sidx_v0(timescale: u32, earliest: u32, first_offset: u32, refs: &[(u32, u32)]) -> Vec<u8> {
        let mut p = vec![0, 0, 0, 0];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&timescale.to_be_bytes());
        p.extend_from_slice(&earliest.to_be_bytes());
        p.extend_from_slice(&first_offset.to_be_bytes());
        p.extend_from_slice(&[0, 0]);
        p.extend_from_slice(&(refs.len() as u16).to_be_bytes());
        for (size, dur) in refs {
            p.extend_from_slice(&size.to_be_bytes());
            p.extend_from_slice(&dur.to_be_bytes());
            p.extend_from_slice(&0x9000_0000u32.to_be_bytes());
        }
        bx(b"sidx", &p)
    }

    /// ftyp + a garbage moov + sidx, the header length, and the file length.
    fn file_head(refs: &[(u32, u32)]) -> (Vec<u8>, u64, u64) {
        let mut head = bx(b"ftyp", b"dash\0\0\0\0iso6mp41");
        head.extend(bx(b"moov", &[7u8; 40]));
        let init_len = head.len() as u64;
        head.extend(sidx_v0(44_100, 0, 0, refs));
        let media: u64 = refs.iter().map(|(s, _)| *s as u64).sum();
        let total = head.len() as u64 + media;
        (head, init_len, total)
    }

    const TEN_SECONDS: u32 = 441_000;

    #[test]
    fn reads_the_header_length_and_every_segment() {
        let (head, init_len, total) = file_head(&[(1000, TEN_SECONDS), (2000, TEN_SECONDS), (500, 220_500)]);
        let layout = parse_layout(&head, total).unwrap();
        assert_eq!(layout.init_len, init_len);
        assert_eq!(layout.timescale, 44_100);
        let anchor = head.len() as u64;
        assert_eq!(
            layout.segments,
            vec![
                Segment { offset: anchor, size: 1000, start: 0, duration: TEN_SECONDS as u64 },
                Segment { offset: anchor + 1000, size: 2000, start: 441_000, duration: TEN_SECONDS as u64 },
                Segment { offset: anchor + 3000, size: 500, start: 882_000, duration: 220_500 },
            ]
        );
        assert_eq!(layout.duration_ms(), 25_000);
    }

    #[test]
    fn a_first_offset_moves_every_segment() {
        let mut head = bx(b"ftyp", b"dash");
        head.extend(bx(b"moov", &[0; 8]));
        head.extend(sidx_v0(1000, 0, 16, &[(100, 1000)]));
        let sidx_end = head.len() as u64;
        let layout = parse_layout(&head, sidx_end + 16 + 100).unwrap();
        assert_eq!(layout.segments[0].offset, sidx_end + 16);
    }

    #[test]
    fn version_one_indexes_and_64_bit_boxes_are_read() {
        let mut p = vec![1, 0, 0, 0];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&1000u32.to_be_bytes());
        p.extend_from_slice(&5_000u64.to_be_bytes());
        p.extend_from_slice(&0u64.to_be_bytes());
        p.extend_from_slice(&[0, 0]);
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&300u32.to_be_bytes());
        p.extend_from_slice(&2000u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());

        let mut head = bx(b"ftyp", b"dash");
        head.extend(bx64(b"moov", &[1; 20]));
        head.extend(bx(b"free", &[0; 4]));
        let moov_end = head.len() as u64 - 12;
        head.extend(bx(b"sidx", &p));
        let layout = parse_layout(&head, head.len() as u64 + 300).unwrap();
        assert_eq!(layout.init_len, moov_end, "the header ends at moov, not at a later free box");
        assert_eq!(layout.segments[0].start, 5_000);
        assert_eq!(layout.segments[0].duration, 2_000);
    }

    #[test]
    fn a_head_cut_short_asks_for_more() {
        let (head, _, total) = file_head(&[(1000, 10); 50]);
        let cut = &head[..head.len() - 20];
        assert_eq!(parse_layout(cut, total), Err(LayoutError::NeedMore(head.len() as u64)));
        assert_eq!(parse_layout(&head[..30], total), Err(LayoutError::NeedMore(24 + 16)));
        assert_eq!(parse_layout(&head[..40], total), Err(LayoutError::NeedMore(24 + 48)), "cut inside moov");
    }

    #[test]
    fn a_fragment_before_the_index_is_unsupported() {
        let mut head = bx(b"ftyp", b"dash");
        head.extend(bx(b"moov", &[0; 8]));
        head.extend(bx(b"moof", &[0; 8]));
        let total = head.len() as u64;
        assert!(matches!(parse_layout(&head, total), Err(LayoutError::Unsupported(_))));
    }

    #[test]
    fn an_index_before_the_header_is_unsupported() {
        let mut head = bx(b"ftyp", b"dash");
        head.extend(sidx_v0(1000, 0, 0, &[(10, 10)]));
        head.extend(bx(b"moov", &[0; 8]));
        let total = head.len() as u64 + 10;
        assert!(matches!(parse_layout(&head, total), Err(LayoutError::Unsupported(_))));
    }

    #[test]
    fn a_file_with_no_index_is_unsupported_not_a_request_for_more() {
        let mut head = bx(b"ftyp", b"M4A ");
        head.extend(bx(b"moov", &[0; 8]));
        head.extend(bx(b"mdat", &[0; 64]));
        let total = head.len() as u64;
        assert!(matches!(parse_layout(&head, total), Err(LayoutError::Unsupported(_))));
    }

    #[test]
    fn indirect_references_are_unsupported() {
        let mut head = bx(b"ftyp", b"dash");
        head.extend(bx(b"moov", &[0; 8]));
        let mut sidx = sidx_v0(1000, 0, 0, &[(100, 10)]);
        let len = sidx.len();
        sidx[len - 12] |= 0x80;
        head.extend(sidx);
        let total = head.len() as u64 + 100;
        assert!(matches!(parse_layout(&head, total), Err(LayoutError::Unsupported(_))));
    }

    #[test]
    fn segments_running_past_the_file_are_unsupported() {
        let (head, _, total) = file_head(&[(1000, 10), (1000, 10)]);
        assert!(matches!(parse_layout(&head, total - 1), Err(LayoutError::Unsupported(_))));
    }

    #[test]
    fn a_corrupt_box_size_is_unsupported_not_a_panic() {
        let mut head = bx(b"ftyp", b"dash");
        head.extend_from_slice(&4u32.to_be_bytes());
        head.extend_from_slice(b"moov");
        head.extend_from_slice(&[0; 16]);
        assert!(matches!(parse_layout(&head, 1 << 20), Err(LayoutError::Unsupported(_))));
        let mut zero = bx(b"ftyp", b"dash");
        zero.extend_from_slice(&0u32.to_be_bytes());
        zero.extend_from_slice(b"moov");
        zero.extend_from_slice(&[0; 16]);
        assert!(matches!(parse_layout(&zero, 1 << 20), Err(LayoutError::Unsupported(_))));
    }

    #[test]
    fn locate_finds_the_segment_and_the_distance_into_it() {
        let (head, _, total) = file_head(&[(1, TEN_SECONDS); 3]);
        let layout = parse_layout(&head, total).unwrap();
        assert_eq!(layout.locate(0), Some((0, 0)));
        assert_eq!(layout.locate(9_999), Some((0, layout.ms_to_units(9_999))));
        assert_eq!(layout.locate(10_000), Some((1, 0)), "a boundary belongs to the segment starting there");
        assert_eq!(layout.locate(15_500), Some((1, 5_500 * 441 / 10)));
        assert_eq!(layout.locate(29_999), Some((2, layout.ms_to_units(9_999))));
    }

    #[test]
    fn locate_at_or_past_the_end_is_none() {
        let (head, _, total) = file_head(&[(1, TEN_SECONDS); 3]);
        let layout = parse_layout(&head, total).unwrap();
        assert_eq!(layout.locate(30_000), None);
        assert_eq!(layout.locate(u64::MAX), None);
    }

    #[test]
    fn a_twenty_hour_index_fits_in_the_first_megabyte() {
        let refs = vec![(160_000u32, TEN_SECONDS); 7200];
        let (head, _, total) = file_head(&refs);
        assert!(head.len() < 1 << 20, "index is {} bytes", head.len());
        let layout = parse_layout(&head, total).unwrap();
        assert_eq!(layout.segments.len(), 7200);
        assert_eq!(layout.duration_ms(), 72_000_000);
        assert_eq!(layout.locate(5 * 3_600_000), Some((1800, 0)));
    }

    fn elst(version: u8, media_times: &[i64]) -> Vec<u8> {
        let mut p = vec![version, 0, 0, 0];
        p.extend_from_slice(&(media_times.len() as u32).to_be_bytes());
        for &t in media_times {
            if version == 1 {
                p.extend_from_slice(&0u64.to_be_bytes());
                p.extend_from_slice(&t.to_be_bytes());
            } else {
                p.extend_from_slice(&0u32.to_be_bytes());
                p.extend_from_slice(&(t as i32).to_be_bytes());
            }
            p.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        }
        bx(b"elst", &p)
    }

    fn mdhd(version: u8, timescale: u32) -> Vec<u8> {
        let mut p = vec![version, 0, 0, 0];
        let stamp_len = if version == 1 { 8 } else { 4 };
        p.extend(std::iter::repeat_n(0u8, 2 * stamp_len));
        p.extend_from_slice(&timescale.to_be_bytes());
        p.extend(std::iter::repeat_n(0u8, stamp_len + 4));
        bx(b"mdhd", &p)
    }

    /// ftyp, a moov holding one trak made of `trak_children`, and an index of
    /// three fragments, the first lasting `first_duration`.
    fn head_with_trak(trak_children: &[Vec<u8>], sidx_timescale: u32, first_duration: u32) -> (Vec<u8>, u64) {
        let mut head = bx(b"ftyp", b"dash");
        let mut trak = Vec::new();
        for c in trak_children {
            trak.extend_from_slice(c);
        }
        let mut moov = bx(b"mvhd", &[0u8; 100]);
        moov.extend(bx(b"trak", &trak));
        head.extend(bx(b"moov", &moov));
        head.extend(sidx_v0(sidx_timescale, 0, 0, &[(100, first_duration), (100, 441_000), (100, 441_000)]));
        let total = head.len() as u64 + 300;
        (head, total)
    }

    fn with_edit(version: u8, media_times: &[i64], mdhd_version: u8, media_timescale: u32) -> Vec<Vec<u8>> {
        vec![
            bx(b"tkhd", &[0; 84]),
            bx(b"edts", &elst(version, media_times)),
            bx(b"mdia", &mdhd(mdhd_version, media_timescale)),
        ]
    }

    #[test]
    fn an_edit_list_gives_the_priming() {
        // As in a measured YouTube "- Topic" upload.
        let (head, total) = head_with_trak(&with_edit(0, &[1600], 0, 44_100), 44_100, 441_000 - 1600);
        assert_eq!(parse_layout(&head, total).unwrap().priming, 1600);
    }

    #[test]
    fn no_edit_list_means_no_priming() {
        let (head, total) = head_with_trak(&[bx(b"mdia", &mdhd(0, 44_100))], 44_100, 441_000);
        assert_eq!(parse_layout(&head, total).unwrap().priming, 0);
    }

    #[test]
    fn priming_is_given_in_the_index_timescale() {
        let (head, total) = head_with_trak(&with_edit(0, &[2400], 0, 48_000), 44_100, 441_000);
        assert_eq!(parse_layout(&head, total).unwrap().priming, 2205);
    }

    #[test]
    fn an_empty_edit_is_passed_over() {
        let (head, total) = head_with_trak(&with_edit(0, &[-1, 1600], 0, 44_100), 44_100, 441_000);
        assert_eq!(parse_layout(&head, total).unwrap().priming, 1600);
    }

    #[test]
    fn version_one_edit_lists_and_media_headers_are_read() {
        let (head, total) = head_with_trak(&with_edit(1, &[1600], 1, 44_100), 44_100, 441_000);
        assert_eq!(parse_layout(&head, total).unwrap().priming, 1600);
    }

    #[test]
    fn a_malformed_moov_means_no_priming_not_an_error() {
        let (head, _, total) = file_head(&[(10, TEN_SECONDS)]);
        assert_eq!(parse_layout(&head, total).unwrap().priming, 0);
    }

    #[test]
    fn the_first_fragment_holds_the_priming_before_time_zero() {
        let (head, total) = head_with_trak(&with_edit(0, &[1600], 0, 44_100), 44_100, 441_000 - 1600);
        let layout = parse_layout(&head, total).unwrap();
        assert_eq!(layout.locate(0), Some((0, 1600)), "playback starts after the priming");
        assert_eq!(layout.locate(1000), Some((0, 44_100 + 1600)));
        // Fragment 1 starts at 439_400, so ten seconds is 1600 into it.
        assert_eq!(layout.locate(10_000), Some((1, 1600)));
    }

    #[test]
    fn priming_frames_reads_a_whole_file_header() {
        let (head, _) = head_with_trak(&with_edit(0, &[2400], 0, 48_000), 44_100, 441_000);
        assert_eq!(priming_frames(&head, 48_000), 2400);
        assert_eq!(priming_frames(&head, 44_100), 2205);
        assert_eq!(priming_frames(b"not an mp4 at all", 44_100), 0);
    }
}
