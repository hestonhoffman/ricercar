//! Test and demo material: a sigma-delta modulator, and DSF / DFF writers
//! (DFF with plain DSD or with DST frames). Not used for playback.

use super::source::DsdFormat;

/// Second-order sigma-delta modulator (error feedback, noise shaped by
/// (1 − z⁻¹)²). Stable for inputs within about ±0.6.
#[derive(Default)]
pub struct Modulator {
    e1: f64,
    e2: f64,
}

impl Modulator {
    /// One DSD bit for input `x` (1.0 = full swing).
    pub fn bit(&mut self, x: f64) -> bool {
        let v = x - (2.0 * self.e1 - self.e2);
        let y = if v >= 0.0 { 1.0 } else { -1.0 };
        self.e2 = self.e1;
        self.e1 = y - v;
        y > 0.0
    }
}

/// A sine of `amp` (1.0 = full DSD swing) at `freq` Hz, `secs` long, the
/// same on every channel: interleaved bytes, MSB first.
pub fn sine(f: DsdFormat, freq: f64, amp: f64, secs: f64) -> Vec<u8> {
    let ch = f.channels.max(1) as usize;
    let bytes = (secs * f.rate as f64 / 8.0) as usize;
    let mut m = Modulator::default();
    let w = 2.0 * std::f64::consts::PI * freq / f.rate as f64;
    let mut out = Vec::with_capacity(bytes * ch);
    let mut n = 0u64;
    for _ in 0..bytes {
        let mut b = 0u8;
        for _ in 0..8 {
            b = (b << 1) | m.bit(amp * (w * n as f64).sin()) as u8;
            n += 1;
        }
        out.extend(std::iter::repeat_n(b, ch));
    }
    out
}

/// Interleaved MSB-first bytes → DSF (LSB first, 4096-byte blocks), with
/// `id3` appended as its metadata.
pub fn dsf(f: DsdFormat, data: &[u8], id3: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 4096;
    let ch = f.channels.max(1) as usize;
    let len = data.len() / ch;
    let blocks = len.div_ceil(BLOCK).max(1);
    let data_size = 12 + (blocks * BLOCK * ch) as u64;
    let total = 28 + 52 + data_size + id3.len() as u64;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(b"DSD ");
    out.extend_from_slice(&28u64.to_le_bytes());
    out.extend_from_slice(&total.to_le_bytes());
    let meta = if id3.is_empty() {
        0
    } else {
        28 + 52 + data_size
    };
    out.extend_from_slice(&meta.to_le_bytes());
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&52u64.to_le_bytes());
    for v in [1u32, 0, if ch == 1 { 1 } else { 2 }, ch as u32, f.rate, 1] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&(len as u64 * 8).to_le_bytes());
    out.extend_from_slice(&(BLOCK as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    for b in 0..blocks {
        for c in 0..ch {
            for i in 0..BLOCK {
                let at = b * BLOCK + i;
                let v = if at < len { data[at * ch + c] } else { 0 };
                out.push(v.reverse_bits());
            }
        }
    }
    out.extend_from_slice(id3);
    out
}

fn ck(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u64).to_be_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}

fn prop(f: DsdFormat, dst: bool) -> Vec<u8> {
    let mut p = b"SND ".to_vec();
    ck(&mut p, b"FS  ", &f.rate.to_be_bytes());
    let mut chnl = f.channels.to_be_bytes().to_vec();
    for c in 0..f.channels {
        chnl.extend_from_slice(match (f.channels, c) {
            (1, _) => b"C   ",
            (_, 0) => b"SLFT",
            (_, 1) => b"SRGT",
            _ => b"LFE ",
        });
    }
    ck(&mut p, b"CHNL", &chnl);
    let name: &[u8] = if dst {
        b"DST Encoded"
    } else {
        b"not compressed"
    };
    let mut cmpr = if dst { b"DST " } else { b"DSD " }.to_vec();
    cmpr.push(name.len() as u8);
    cmpr.extend_from_slice(name);
    ck(&mut p, b"CMPR", &cmpr);
    p
}

fn form(f: DsdFormat, dst: bool, audio: (&[u8; 4], Vec<u8>), id3: &[u8], title: &str) -> Vec<u8> {
    let mut body = b"DSD ".to_vec();
    ck(&mut body, b"FVER", &0x0105_0000u32.to_be_bytes());
    ck(&mut body, b"PROP", &prop(f, dst));
    ck(&mut body, audio.0, &audio.1);
    if !title.is_empty() {
        let mut diin = Vec::new();
        let mut t = (title.len() as u32).to_be_bytes().to_vec();
        t.extend_from_slice(title.as_bytes());
        ck(&mut diin, b"DITI", &t);
        ck(&mut body, b"DIIN", &diin);
    }
    if !id3.is_empty() {
        ck(&mut body, b"ID3 ", id3);
    }
    let mut out = Vec::new();
    ck(&mut out, b"FRM8", &body);
    out
}

/// Interleaved MSB-first bytes → DFF with an uncompressed `DSD ` chunk.
/// `title` goes in a DIIN chunk when not empty.
pub fn dff(f: DsdFormat, data: &[u8], id3: &[u8], title: &str) -> Vec<u8> {
    form(f, false, (b"DSD ", data.to_vec()), id3, title)
}

/// DST frame in the format's escape for plain DSD (first bit 0, stuffing
/// zero, then the frame's bytes as they are): valid DST that any decoder
/// must read back exactly.
pub fn dst_plain_frame(frame: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8];
    v.extend_from_slice(frame);
    v
}

/// Interleaved MSB-first bytes → DFF with a `DST ` chunk of 1/75 s frames
/// made by `encode` (the last frame padded with silence).
pub fn dff_dst(
    f: DsdFormat,
    data: &[u8],
    id3: &[u8],
    encode: impl Fn(&[u8]) -> Vec<u8>,
) -> Vec<u8> {
    let ch = f.channels.max(1) as usize;
    let frame = (f.rate / 75 / 8) as usize * ch;
    let frames: Vec<&[u8]> = data.chunks(frame).collect();
    let mut dst = Vec::new();
    let mut frte = (frames.len() as u32).to_be_bytes().to_vec();
    frte.extend_from_slice(&75u16.to_be_bytes());
    ck(&mut dst, b"FRTE", &frte);
    for fr in frames {
        let mut full = fr.to_vec();
        full.resize(frame, super::SILENCE);
        ck(&mut dst, b"DSTF", &encode(&full));
    }
    form(f, true, (b"DST ", dst), id3, "")
}
