//! DSF (Sony DSD Stream File): a `DSD ` header, a `fmt ` chunk, a `data`
//! chunk of blocks (4096 bytes of one channel, then the next channel…), and
//! an optional ID3v2 tag at the end. Little-endian; the bits of each byte
//! are usually stored LSB first, which `read` turns around.

use std::io::{self, Read, Seek, SeekFrom};

use super::source::{DsdFormat, DsdSource};
use super::{SILENCE, bad, check_format};

/// Largest ID3 tag read (covers included).
const MAX_TAG: u64 = 16 << 20;

pub struct DsfReader<R> {
    r: R,
    format: DsdFormat,
    /// Bytes per channel of audio (padding of the last block excluded).
    len: u64,
    block: usize,
    data_start: u64,
    lsb_first: bool,
    pos: u64,
    cache: Vec<u8>,
    cached: Option<u64>,
    id3: Vec<u8>,
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn u64le(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8]))
}

impl<R: Read + Seek + Send> DsfReader<R> {
    /// Parse the headers; refuses formats the player cannot play.
    pub fn new(r: R) -> io::Result<Self> {
        let me = Self::new_unchecked(r)?;
        check_format(me.format)?;
        Ok(me)
    }

    /// Parse the headers whatever the channel count (library scans).
    pub fn new_unchecked(mut r: R) -> io::Result<Self> {
        let mut head = [0u8; 28];
        r.read_exact(&mut head)?;
        if &head[..4] != b"DSD " {
            return Err(bad("not a DSF file"));
        }
        let meta_at = u64le(&head[20..]);
        let fmt_at = u64le(&head[4..]).max(28);
        r.seek(SeekFrom::Start(fmt_at))?;
        let mut fmt = [0u8; 52];
        r.read_exact(&mut fmt)?;
        if &fmt[..4] != b"fmt " {
            return Err(bad("DSF: no fmt chunk"));
        }
        let fmt_size = u64le(&fmt[4..]);
        let format_id = u32le(&fmt[16..]);
        let channels = u32le(&fmt[24..]);
        let rate = u32le(&fmt[28..]);
        let bits = u32le(&fmt[32..]);
        let samples = u64le(&fmt[36..]);
        let block = u32le(&fmt[44..]) as usize;
        if format_id != 0 {
            return Err(bad("DSF: compressed data is not supported"));
        }
        if !(bits == 1 || bits == 8) || block == 0 || block > 1 << 20 || channels > 16 {
            return Err(bad("DSF: malformed fmt chunk"));
        }
        let data_at = fmt_at + fmt_size.max(52);
        r.seek(SeekFrom::Start(data_at))?;
        let mut data = [0u8; 12];
        r.read_exact(&mut data)?;
        if &data[..4] != b"data" {
            return Err(bad("DSF: no data chunk"));
        }
        let data_bytes = u64le(&data[4..]).saturating_sub(12);
        let ch = channels.max(1) as u64;
        let blocks = data_bytes / (block as u64 * ch);
        let len = samples.div_ceil(8).min(blocks * block as u64);

        let mut id3 = Vec::new();
        if meta_at > 0 {
            let end = r.seek(SeekFrom::End(0))?;
            if meta_at < end {
                r.seek(SeekFrom::Start(meta_at))?;
                (&mut r).take(MAX_TAG).read_to_end(&mut id3)?;
            }
        }
        Ok(DsfReader {
            r,
            format: DsdFormat {
                rate,
                channels: channels as u16,
            },
            len,
            block,
            data_start: data_at + 12,
            lsb_first: bits == 1,
            pos: 0,
            cache: Vec::new(),
            cached: None,
            id3,
        })
    }

    /// The raw ID3v2 tag at the end of the file (empty without one).
    pub fn id3(&self) -> &[u8] {
        &self.id3
    }

    /// Load the block group holding byte `index * block` of each channel.
    fn load(&mut self, index: u64) -> io::Result<()> {
        if self.cached == Some(index) {
            return Ok(());
        }
        let ch = self.format.channels.max(1) as usize;
        let size = self.block * ch;
        self.cache.resize(size, 0);
        self.r
            .seek(SeekFrom::Start(self.data_start + index * size as u64))?;
        let mut got = 0;
        while got < size {
            match self.r.read(&mut self.cache[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        // A short last block: silence rather than stale bytes.
        self.cache[got..].fill(if self.lsb_first {
            SILENCE.reverse_bits()
        } else {
            SILENCE
        });
        self.cached = Some(index);
        Ok(())
    }
}

impl<R: Read + Seek + Send> DsdSource for DsfReader<R> {
    fn format(&self) -> DsdFormat {
        self.format
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let ch = self.format.channels.max(1) as usize;
        let room = buf.len() / ch;
        let mut n = 0usize;
        while n < room && self.pos < self.len {
            let index = self.pos / self.block as u64;
            let off = (self.pos % self.block as u64) as usize;
            self.load(index)?;
            let take = (self.block - off)
                .min(room - n)
                .min((self.len - self.pos) as usize);
            for c in 0..ch {
                let from = &self.cache[c * self.block + off..][..take];
                for (i, &b) in from.iter().enumerate() {
                    buf[(n + i) * ch + c] = if self.lsb_first { b.reverse_bits() } else { b };
                }
            }
            n += take;
            self.pos += take as u64;
        }
        Ok(n * ch)
    }

    fn seek(&mut self, pos: u64) -> io::Result<()> {
        self.pos = pos.min(self.len);
        Ok(())
    }

    fn len(&self) -> Option<u64> {
        Some(self.len)
    }

    fn tags(&self) -> Vec<(String, String)> {
        super::id3::parse(&self.id3)
            .map(|t| t.tags)
            .unwrap_or_default()
    }
}
