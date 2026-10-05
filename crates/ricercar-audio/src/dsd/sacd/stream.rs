//! Audio of a SACD area: sectors carry a packet stream (audio,
//! supplementary data, padding) from which frames of 1/75 s are rebuilt,
//! then decoded (DST) or passed through (plain DSD).
//!
//! Plain DSD frames are byte-interleaved, most significant bit first: the
//! layout of the DsdSource contract, so they need no conversion. The DST
//! decoder writes the same layout.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use dst_decoder::decoder::DstDecoder;

use super::{Area, FRAMES_PER_SEC, FrameFormat, SECTOR, SacdTrack, time_code};
use crate::dsd::source::{DsdFormat, DsdSource};
use crate::error::{AudioError, Result};

const PACKET_AUDIO: u8 = 2;
/// Sectors read at once while streaming.
const BATCH: u32 = 32;
/// How far a seek probe looks for a frame start, and the margin around a
/// track's listed sectors searched when seeking.
const PROBE: u32 = 64;
/// What a frame that cannot be read is replaced with.
const SILENCE: u8 = 0x69;

/// The header of an audio sector.
struct SectorHead {
    /// `(frame start, data type, length)` per packet.
    packets: [(bool, u8, usize); 7],
    packet_count: usize,
    /// Time codes of the frames starting in this sector.
    times: [u32; 7],
    time_count: usize,
    /// Where packet data begins.
    data: usize,
}

/// Header byte: packet count (3 bits), frame info count (3 bits), a
/// reserved bit, the DST flag. Then 2 bytes per packet (frame start,
/// reserved, data type: 3 bits, length: 11 bits), then per frame a
/// minutes/seconds/frames time code, followed by one more byte when the
/// sector is DST-coded.
fn parse_sector(s: &[u8]) -> Option<SectorHead> {
    let packet_count = (s[0] >> 5) as usize;
    let time_count = ((s[0] >> 2) & 7) as usize;
    let dst = s[0] & 1 != 0;
    let mut h = SectorHead {
        packets: [(false, 0, 0); 7],
        packet_count,
        times: [0; 7],
        time_count,
        data: 0,
    };
    let mut p = 1;
    for i in 0..packet_count {
        let v = u16::from_be_bytes([s[p], s[p + 1]]);
        h.packets[i] = (v & 0x8000 != 0, ((v >> 11) & 7) as u8, (v & 0x7ff) as usize);
        p += 2;
    }
    for i in 0..time_count {
        h.times[i] = time_code(&s[p..]);
        p += if dst { 4 } else { 3 };
    }
    h.data = p;
    let total: usize = h.packets[..packet_count].iter().map(|p| p.2).sum();
    (p + total <= SECTOR).then_some(h)
}

/// Rebuilds the frames of an area from its sectors.
struct FrameReader {
    file: File,
    /// Audio sectors of the area: `start..end`.
    start: u32,
    end: u32,
    /// Size of a complete plain DSD frame (DST frames end at the next
    /// frame start).
    plain_size: Option<usize>,
    next: u32,
    cache: Vec<u8>,
    cache_at: u32,
    cache_len: u32,
    building: Option<(u32, Vec<u8>)>,
    done: VecDeque<(u32, Vec<u8>)>,
    last_time: u32,
}

impl FrameReader {
    fn new(path: &Path, area: &Area) -> Result<FrameReader> {
        let format = area.info.format;
        let plain_size = (area.info.frame_format != FrameFormat::Dst)
            .then(|| frame_len(format) * format.channels as usize);
        Ok(FrameReader {
            file: File::open(path)?,
            start: area.start,
            end: area.end,
            plain_size,
            next: area.start,
            cache: Vec::new(),
            cache_at: 0,
            cache_len: 0,
            building: None,
            done: VecDeque::new(),
            last_time: 0,
        })
    }

    /// Offset of sector `lsn` in the cache, reading it if needed.
    fn load(&mut self, lsn: u32) -> io::Result<usize> {
        if !(self.cache_at..self.cache_at + self.cache_len).contains(&lsn) {
            let n = BATCH.min(self.end - lsn);
            self.cache.resize(n as usize * SECTOR, 0);
            self.file
                .read_exact_at(&mut self.cache, lsn as u64 * SECTOR as u64)?;
            self.cache_at = lsn;
            self.cache_len = n;
        }
        Ok((lsn - self.cache_at) as usize * SECTOR)
    }

    /// Continues from sector `lsn`, dropping any partial frame.
    fn goto(&mut self, lsn: u32) {
        self.next = lsn;
        self.building = None;
        self.done.clear();
    }

    /// The next complete frame and its time code, `None` at the end of
    /// the area.
    fn next_frame(&mut self) -> io::Result<Option<(u32, Vec<u8>)>> {
        loop {
            if let Some(f) = self.done.pop_front() {
                return Ok(Some(f));
            }
            if self.next >= self.end {
                return Ok(self.building.take());
            }
            let lsn = self.next;
            self.next += 1;
            let off = self.load(lsn)?;
            let s = &self.cache[off..off + SECTOR];
            let Some(h) = parse_sector(s) else {
                tracing::warn!("SACD: bad audio sector {lsn}");
                self.building = None;
                continue;
            };
            let mut p = h.data;
            let mut started = 0;
            for &(start, kind, len) in &h.packets[..h.packet_count] {
                let data = &s[p..p + len];
                p += len;
                if kind != PACKET_AUDIO {
                    continue;
                }
                if start {
                    if let Some(f) = self.building.take() {
                        self.done.push_back(f);
                    }
                    let time = if started < h.time_count {
                        h.times[started]
                    } else {
                        self.last_time + 1
                    };
                    started += 1;
                    self.last_time = time;
                    let cap = self.plain_size.unwrap_or(len);
                    self.building = Some((time, Vec::with_capacity(cap)));
                }
                if let Some((_, buf)) = &mut self.building {
                    buf.extend_from_slice(data);
                    if self.plain_size.is_some_and(|n| buf.len() >= n) {
                        self.done.extend(self.building.take());
                    }
                }
            }
        }
    }

    /// Time code of the first frame starting at or after sector `lsn`.
    fn time_from(&mut self, lsn: u32) -> io::Result<Option<u32>> {
        for lsn in lsn..(lsn + PROBE).min(self.end) {
            let off = self.load(lsn)?;
            if let Some(h) = parse_sector(&self.cache[off..off + SECTOR])
                && h.time_count > 0
            {
                return Ok(Some(h.times[0]));
            }
        }
        Ok(None)
    }

    /// The sector where frame `target` starts, searched in `lo..hi`.
    fn locate(&mut self, target: u32, lo: u32, hi: u32) -> io::Result<u32> {
        let before = |t: Option<u32>| t.is_some_and(|t| t <= target);
        let lo = if before(self.time_from(lo)?) {
            lo
        } else {
            self.start
        };
        let (mut a, mut b) = (lo, hi.max(lo + 1));
        while b - a > 1 {
            let m = a + (b - a) / 2;
            if before(self.time_from(m)?) {
                a = m;
            } else {
                b = m;
            }
        }
        Ok(a)
    }
}

/// Bytes per channel in a frame of 1/75 s (4704 at DSD64).
fn frame_len(format: DsdFormat) -> usize {
    (format.rate / 8 / FRAMES_PER_SEC) as usize
}

/// Sectors searched for a track's frames.
fn window(area: &Area, track: &SacdTrack) -> (u32, u32) {
    let lo = track.start_sector.saturating_sub(PROBE).max(area.start);
    let hi = (track
        .start_sector
        .saturating_add(track.sectors)
        .saturating_add(PROBE))
    .min(area.end);
    if lo < hi {
        (lo, hi)
    } else {
        (area.start, area.end)
    }
}

/// A track of the stereo area as a DSD stream.
pub(super) struct TrackStream {
    frames: FrameReader,
    format: DsdFormat,
    dst: Option<DstDecoder>,
    frame_len: usize,
    /// Track frames, as area time codes: `first..end`.
    first: u32,
    end: u32,
    window: (u32, u32),
    /// Next frame to play, and bytes per channel to drop from it.
    want: u32,
    skip: usize,
    out: Vec<u8>,
    out_pos: usize,
    eof: bool,
    tags: Vec<(String, String)>,
}

impl TrackStream {
    pub(super) fn open(
        path: &Path,
        area: &Area,
        track: &SacdTrack,
        tags: Vec<(String, String)>,
    ) -> Result<TrackStream> {
        let format = area.info.format;
        let dst = match area.info.frame_format {
            FrameFormat::Dst => Some(
                DstDecoder::new(format.channels as usize, format.rate as usize)
                    .map_err(|e| AudioError::Decode(format!("DST: {e}")))?,
            ),
            _ => None,
        };
        let mut s = TrackStream {
            frames: FrameReader::new(path, area)?,
            format,
            dst,
            frame_len: frame_len(format),
            first: track.start_frame,
            end: track.start_frame + track.frames,
            window: window(area, track),
            want: track.start_frame,
            skip: 0,
            out: Vec::new(),
            out_pos: 0,
            eof: false,
            tags,
        };
        s.seek(0)?;
        Ok(s)
    }

    fn decode(&mut self, data: Vec<u8>) {
        let size = self.frame_len * self.format.channels as usize;
        match &mut self.dst {
            Some(dst) => {
                self.out.resize(size, SILENCE);
                if let Err(e) = dst.decode_frame(&data, &mut self.out) {
                    tracing::warn!("SACD: DST frame {}: {e}", self.want);
                }
            }
            None => {
                self.out = data;
                if self.out.len() != size {
                    tracing::warn!("SACD: frame {} is {} bytes", self.want, self.out.len());
                    self.out.resize(size, SILENCE);
                }
            }
        }
    }
}

impl DsdSource for TrackStream {
    fn format(&self) -> DsdFormat {
        self.format
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let ch = self.format.channels as usize;
        let max = buf.len() / ch * ch;
        if max == 0 {
            return Ok(0);
        }
        loop {
            if self.out_pos < self.out.len() {
                let n = max.min(self.out.len() - self.out_pos);
                buf[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                return Ok(n);
            }
            if self.eof {
                return Ok(0);
            }
            match self.frames.next_frame()? {
                None => self.eof = true,
                Some((time, _)) if time < self.want => {}
                Some((time, _)) if time >= self.end => self.eof = true,
                Some((time, data)) => {
                    self.want = time;
                    self.decode(data);
                    self.out_pos = self.skip * ch;
                    self.skip = 0;
                    self.want = time + 1;
                }
            }
        }
    }

    fn seek(&mut self, pos: u64) -> io::Result<()> {
        self.out.clear();
        self.out_pos = 0;
        if pos >= self.len().unwrap_or(0) {
            self.eof = true;
            return Ok(());
        }
        let target = self.first + (pos / self.frame_len as u64) as u32;
        let (lo, hi) = self.window;
        let lsn = self.frames.locate(target, lo, hi)?;
        self.frames.goto(lsn);
        self.want = target;
        self.skip = (pos % self.frame_len as u64) as usize;
        self.eof = false;
        Ok(())
    }

    fn len(&self) -> Option<u64> {
        Some((self.end - self.first) as u64 * self.frame_len as u64)
    }

    fn tags(&self) -> Vec<(String, String)> {
        self.tags.clone()
    }
}

/// The frames of a track as stored on the disc, see
/// [`super::SacdImage::track_frames`].
pub struct RawFrames {
    frames: FrameReader,
    first: u32,
    end: u32,
    done: bool,
}

impl RawFrames {
    pub(super) fn open(path: &Path, area: &Area, track: &SacdTrack) -> Result<RawFrames> {
        let mut frames = FrameReader::new(path, area)?;
        let (lo, hi) = window(area, track);
        let lsn = frames.locate(track.start_frame, lo, hi)?;
        frames.goto(lsn);
        Ok(RawFrames {
            frames,
            first: track.start_frame,
            end: track.start_frame + track.frames,
            done: track.frames == 0,
        })
    }
}

impl Iterator for RawFrames {
    type Item = io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            match self.frames.next_frame() {
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
                Ok(Some((time, data))) if (self.first..self.end).contains(&time) => {
                    return Some(Ok(data));
                }
                Ok(Some((time, _))) if time < self.first => {}
                Ok(_) => self.done = true,
            }
        }
        None
    }
}
