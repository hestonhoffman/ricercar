//! DFF (DSDIFF 1.5): big-endian chunks in a `FRM8` form. `PROP` holds the
//! rate (`FS  `), channels (`CHNL`) and compression (`CMPR`). Audio is a
//! `DSD ` chunk of interleaved bytes, MSB first (already the order the
//! player wants), or a `DST ` chunk of DST-compressed frames of 1/75 s,
//! decoded with the `dst-decoder` crate. Tags: `DIIN` (title, artist) and
//! the common, unofficial `ID3 ` chunk.

use std::io::{self, Read, Seek, SeekFrom};

use super::source::{DsdFormat, DsdSource};
use super::{bad, check_format};

/// Largest ID3 or DIIN chunk read.
const MAX_TAG: u64 = 16 << 20;
/// DST frames per second.
const DST_FRAME_RATE: u32 = 75;

/// Decodes one DST frame into interleaved DSD bytes. A trait so tests can
/// stand in for the real decoder.
pub trait FrameDecoder: Send {
    fn decode(&mut self, frame: &[u8], out: &mut [u8]) -> io::Result<()>;
}

/// The `dst-decoder` crate.
struct Dst(dst_decoder::decoder::DstDecoder);

impl FrameDecoder for Dst {
    fn decode(&mut self, frame: &[u8], out: &mut [u8]) -> io::Result<()> {
        self.0
            .decode_frame(frame, out)
            .map(|_| ())
            .map_err(|e| bad(format!("DST frame: {e}")))
    }
}

pub fn dst_decoder(format: DsdFormat) -> io::Result<Box<dyn FrameDecoder>> {
    dst_decoder::decoder::DstDecoder::new(format.channels as usize, format.rate as usize)
        .map(|d| Box::new(Dst(d)) as Box<dyn FrameDecoder>)
        .map_err(|e| bad(format!("DST: {e}")))
}

enum Data {
    Raw {
        start: u64,
    },
    Dst {
        /// (offset, size) of each frame in the file.
        frames: Vec<(u64, u32)>,
        decoder: Option<Box<dyn FrameDecoder>>,
        /// Bytes per channel of one decoded frame.
        frame_len: usize,
        current: Option<usize>,
        buf: Vec<u8>,
        packed: Vec<u8>,
    },
}

pub struct DffReader<R> {
    r: R,
    format: DsdFormat,
    len: u64,
    data: Data,
    pos: u64,
    title: Option<String>,
    artist: Option<String>,
    id3: Vec<u8>,
}

fn u16be(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn u32be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn u64be(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap_or([0; 8]))
}

/// Chunk header: (id, size).
fn chunk<R: Read>(r: &mut R) -> io::Result<([u8; 4], u64)> {
    let mut h = [0u8; 12];
    r.read_exact(&mut h)?;
    Ok(([h[0], h[1], h[2], h[3]], u64be(&h[4..])))
}

fn read_body<R: Read>(r: &mut R, size: u64) -> io::Result<Vec<u8>> {
    let mut v = Vec::new();
    r.take(size.min(MAX_TAG)).read_to_end(&mut v)?;
    Ok(v)
}

/// DIIN text: a 32-bit count, then the text.
fn diin_text(b: &[u8]) -> Option<String> {
    let n = u32be(b.get(..4)?) as usize;
    let t = b.get(4..4 + n).unwrap_or(&b[4..]);
    let s = String::from_utf8_lossy(t)
        .trim_end_matches('\0')
        .trim()
        .to_string();
    (!s.is_empty()).then_some(s)
}

impl<R: Read + Seek + Send> DffReader<R> {
    /// Parse the chunks; refuses formats the player cannot play.
    pub fn new(r: R) -> io::Result<Self> {
        let mut me = Self::new_unchecked(r)?;
        check_format(me.format)?;
        if let Data::Dst { decoder, .. } = &mut me.data {
            *decoder = Some(dst_decoder(me.format)?);
        }
        Ok(me)
    }

    /// Same, with the DST frames handed to `decoder` (tests).
    pub fn with_decoder(r: R, decoder: Box<dyn FrameDecoder>) -> io::Result<Self> {
        let mut me = Self::new_unchecked(r)?;
        check_format(me.format)?;
        if let Data::Dst { decoder: d, .. } = &mut me.data {
            *d = Some(decoder);
        }
        Ok(me)
    }

    /// Parse the chunks whatever the channel count (library scans); DST
    /// audio cannot be read from it.
    pub fn new_unchecked(mut r: R) -> io::Result<Self> {
        let (id, form_size) = chunk(&mut r)?;
        let mut kind = [0u8; 4];
        r.read_exact(&mut kind)?;
        if &id != b"FRM8" || &kind != b"DSD " {
            return Err(bad("not a DSDIFF file"));
        }
        let end = 12 + form_size;
        let mut at = 16u64;
        let (mut rate, mut channels, mut cmpr) = (0u32, 0u16, *b"DSD ");
        let mut data = None;
        let (mut title, mut artist, mut id3) = (None, None, Vec::new());
        while at + 12 <= end {
            r.seek(SeekFrom::Start(at))?;
            let Ok((id, size)) = chunk(&mut r) else { break };
            let body = at + 12;
            match &id {
                b"PROP" => {
                    let mut sub = body + 4;
                    while sub + 12 <= body + size {
                        r.seek(SeekFrom::Start(sub))?;
                        let (sid, ssize) = chunk(&mut r)?;
                        let b = read_body(&mut r, ssize.min(64))?;
                        match &sid {
                            b"FS  " if b.len() >= 4 => rate = u32be(&b),
                            b"CHNL" if b.len() >= 2 => channels = u16be(&b),
                            b"CMPR" if b.len() >= 4 => cmpr.copy_from_slice(&b[..4]),
                            _ => {}
                        }
                        sub += 12 + ssize + (ssize & 1);
                    }
                }
                b"DSD " => data = Some((Data::Raw { start: body }, size)),
                b"DST " => {
                    let mut frames = Vec::new();
                    let mut sub = body;
                    while sub + 12 <= body + size {
                        r.seek(SeekFrom::Start(sub))?;
                        let (sid, ssize) = chunk(&mut r)?;
                        if &sid == b"DSTF" {
                            frames.push((sub + 12, ssize.min(u32::MAX as u64) as u32));
                        }
                        sub += 12 + ssize + (ssize & 1);
                    }
                    data = Some((
                        Data::Dst {
                            frames,
                            decoder: None,
                            frame_len: 0,
                            current: None,
                            buf: Vec::new(),
                            packed: Vec::new(),
                        },
                        0,
                    ));
                }
                b"DIIN" => {
                    let mut sub = body;
                    while sub + 12 <= body + size {
                        r.seek(SeekFrom::Start(sub))?;
                        let (sid, ssize) = chunk(&mut r)?;
                        match &sid {
                            b"DITI" => title = diin_text(&read_body(&mut r, ssize)?),
                            b"DIAR" => artist = diin_text(&read_body(&mut r, ssize)?),
                            _ => {}
                        }
                        sub += 12 + ssize + (ssize & 1);
                    }
                }
                b"ID3 " => id3 = read_body(&mut r, size)?,
                _ => {}
            }
            at = body + size + (size & 1);
        }
        let format = DsdFormat { rate, channels };
        let ch = channels.max(1) as u64;
        let (mut data, size) = data.ok_or_else(|| bad("DSDIFF: no sound data"))?;
        let len = match &mut data {
            Data::Raw { .. } => {
                if &cmpr != b"DSD " {
                    return Err(bad("DSDIFF: unknown compression"));
                }
                size / ch
            }
            Data::Dst {
                frames, frame_len, ..
            } => {
                if &cmpr != b"DST " {
                    return Err(bad("DSDIFF: DST data without DST compression"));
                }
                *frame_len = (rate / DST_FRAME_RATE / 8) as usize;
                frames.len() as u64 * *frame_len as u64
            }
        };
        Ok(DffReader {
            r,
            format,
            len,
            data,
            pos: 0,
            title,
            artist,
            id3,
        })
    }

    /// DST-compressed audio.
    pub fn is_dst(&self) -> bool {
        matches!(self.data, Data::Dst { .. })
    }

    /// The raw `ID3 ` chunk (empty without one).
    pub fn id3(&self) -> &[u8] {
        &self.id3
    }
}

impl<R: Read + Seek + Send> DsdSource for DffReader<R> {
    fn format(&self) -> DsdFormat {
        self.format
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let ch = self.format.channels.max(1) as usize;
        let want = ((buf.len() / ch) as u64).min(self.len - self.pos) as usize;
        if want == 0 {
            return Ok(0);
        }
        match &mut self.data {
            Data::Raw { start } => {
                self.r
                    .seek(SeekFrom::Start(*start + self.pos * ch as u64))?;
                let mut got = 0;
                let out = &mut buf[..want * ch];
                while got < out.len() {
                    match self.r.read(&mut out[got..]) {
                        Ok(0) => break,
                        Ok(n) => got += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
                let got = got / ch;
                if got < want {
                    // The file is shorter than its chunk says.
                    self.len = self.pos + got as u64;
                }
                self.pos += got as u64;
                Ok(got * ch)
            }
            Data::Dst {
                frames,
                decoder,
                frame_len,
                current,
                buf: frame,
                packed,
            } => {
                let decoder = decoder.as_mut().ok_or_else(|| bad("DST: no decoder"))?;
                let mut n = 0;
                while n < want {
                    let index = (self.pos / *frame_len as u64) as usize;
                    let off = (self.pos % *frame_len as u64) as usize;
                    if *current != Some(index) {
                        let (at, size) = frames[index];
                        packed.resize(size as usize, 0);
                        self.r.seek(SeekFrom::Start(at))?;
                        match self.r.read_exact(packed) {
                            Ok(()) => {}
                            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                                // Truncated file: it ends here.
                                self.len = self.pos;
                                break;
                            }
                            Err(e) => return Err(e),
                        }
                        frame.resize(*frame_len * ch, 0);
                        if let Err(e) = decoder.decode(packed, frame) {
                            // One bad frame: 1/75 s of silence, not the end.
                            tracing::warn!("{e}");
                            frame.fill(super::SILENCE);
                        }
                        *current = Some(index);
                    }
                    let take = (*frame_len - off).min(want - n);
                    buf[n * ch..(n + take) * ch]
                        .copy_from_slice(&frame[off * ch..(off + take) * ch]);
                    n += take;
                    self.pos += take as u64;
                }
                Ok(n * ch)
            }
        }
    }

    fn seek(&mut self, pos: u64) -> io::Result<()> {
        self.pos = pos.min(self.len);
        Ok(())
    }

    fn len(&self) -> Option<u64> {
        Some(self.len)
    }

    fn tags(&self) -> Vec<(String, String)> {
        let mut tags = super::id3::parse(&self.id3)
            .map(|t| t.tags)
            .unwrap_or_default();
        for (key, value) in [("title", &self.title), ("artist", &self.artist)] {
            if let Some(v) = value
                && !tags.iter().any(|(k, _)| k == key)
            {
                tags.push((key.to_string(), v.clone()));
            }
        }
        tags
    }
}
