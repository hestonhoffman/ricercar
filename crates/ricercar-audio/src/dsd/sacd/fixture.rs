//! Synthetic SACD images for tests and the demo library: a master TOC and
//! text, a stereo area TOC (track lists, ISRCs, track text) and the audio
//! sectors, packed the way discs pack them. Not used for playback.

use super::SECTOR;

/// Bytes per channel in a frame of 1/75 s of DSD64.
pub const FRAME: usize = 4704;

/// What goes on the disc.
pub struct Spec {
    /// DST-coded area (frames as given) rather than plain DSD.
    pub dst: bool,
    /// Frames before the first track.
    pub pregap: u32,
    /// Frames and title of each track.
    pub tracks: Vec<(u32, String)>,
    pub title: String,
    pub artist: String,
    pub publisher: String,
    /// Performer of every track.
    pub performer: String,
    /// Index in the general genre table (14 is Jazz).
    pub genre: u8,
    pub date: (u16, u8, u8),
}

impl Spec {
    pub fn new(dst: bool, pregap: u32, tracks: &[(u32, &str)]) -> Spec {
        Spec {
            dst,
            pregap,
            tracks: tracks.iter().map(|&(n, t)| (n, t.to_string())).collect(),
            title: "Café Album".into(),
            artist: "Some Ensemble".into(),
            publisher: "Some Label".into(),
            performer: "Player".into(),
            genre: 14,
            date: (2001, 2, 3),
        }
    }

    /// Frames on the disc, pregap included.
    pub fn frames(&self) -> u32 {
        self.pregap + self.tracks.iter().map(|t| t.0).sum::<u32>()
    }
}

/// A frame's plain DSD bytes for two channels, interleaved: a pattern
/// that differs from frame to frame.
pub fn plain_frame(time: u32) -> Vec<u8> {
    let mut f = Vec::with_capacity(FRAME * 2);
    for i in 0..FRAME {
        for ch in 0..2u32 {
            f.push(((time * 31 + i as u32 * 7 + ch * 101) % 251) as u8);
        }
    }
    f
}

/// DST frame that carries its DSD bytes uncoded: a zero flag bit, a
/// dummy bit, six zero stuffing bits, then the bytes.
pub fn dst_escape_frame(time: u32) -> Vec<u8> {
    let mut f = vec![0u8];
    f.extend(plain_frame(time));
    f
}

fn put(img: &mut [u8], lsn: usize, at: usize, bytes: &[u8]) {
    let p = lsn * SECTOR + at;
    img[p..p + bytes.len()].copy_from_slice(bytes);
}

fn put_time(img: &mut [u8], lsn: usize, at: usize, frames: u32) {
    let (m, s, f) = (frames / 4500, frames / 75 % 60, frames % 75);
    put(img, lsn, at, &[m as u8, s as u8, f as u8]);
}

/// Text as ISO 8859-1, NUL-terminated.
fn latin1(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect();
    v.push(0);
    v
}

/// Packs frames into audio sectors the way discs do: a small
/// supplementary packet first, then audio packets, a frame info per frame
/// starting in the sector. Returns the sectors and the sector where each
/// frame starts.
fn pack(frames: &[Vec<u8>], dst: bool) -> (Vec<Vec<u8>>, Vec<usize>) {
    let info = if dst { 4 } else { 3 };
    let mut sectors = Vec::new();
    let mut starts = Vec::new();
    let (mut i, mut o) = (0, 0);
    while i < frames.len() {
        let mut packets: Vec<(bool, u8, Vec<u8>)> = vec![(false, 3, vec![0xee; 10])];
        let mut times = Vec::new();
        while packets.len() < 7 && i < frames.len() {
            let start = o == 0;
            let header = 1 + 2 * (packets.len() + 1) + info * (times.len() + start as usize);
            let used = header + packets.iter().map(|p| p.2.len()).sum::<usize>();
            if used >= SECTOR {
                break;
            }
            let take = (SECTOR - used).min(frames[i].len() - o).min(2047);
            if start {
                times.push(i as u32);
                starts.push(sectors.len());
            }
            packets.push((start, 2, frames[i][o..o + take].to_vec()));
            o += take;
            if o == frames[i].len() {
                i += 1;
                o = 0;
            }
        }
        let mut s = vec![0u8; SECTOR];
        s[0] = (packets.len() as u8) << 5 | (times.len() as u8) << 2 | dst as u8;
        let mut p = 1;
        for (start, kind, data) in &packets {
            let v = (*start as u16) << 15 | (*kind as u16) << 11 | data.len() as u16;
            s[p..p + 2].copy_from_slice(&v.to_be_bytes());
            p += 2;
        }
        for t in &times {
            let (m, sec, f) = (t / 4500, t / 75 % 60, t % 75);
            s[p..p + 3].copy_from_slice(&[m as u8, sec as u8, f as u8]);
            p += info;
        }
        for (_, _, data) in &packets {
            s[p..p + data.len()].copy_from_slice(data);
            p += data.len();
        }
        sectors.push(s);
    }
    (sectors, starts)
}

const AREA_TOC: usize = 544;
const AUDIO_START: usize = 560;

/// The image of `spec`, with `frame(n)` as the content of area frame `n`
/// (interleaved stereo DSD64 bytes, or a DST frame when `spec.dst`).
pub fn image(spec: &Spec, frame: impl Fn(u32) -> Vec<u8>) -> Vec<u8> {
    let total = spec.frames();
    let frames: Vec<Vec<u8>> = (0..total).map(frame).collect();
    let (sectors, starts) = pack(&frames, spec.dst);
    let end = AUDIO_START + sectors.len();
    let mut img = vec![0u8; end * SECTOR];

    // Master TOC.
    put(&mut img, 510, 0, b"SACDMTOC\x01\x14");
    put(&mut img, 510, 16, &[0, 1, 0, 1]);
    put(&mut img, 510, 64, &(AREA_TOC as u32).to_be_bytes());
    put(&mut img, 510, 80, &[0x80]);
    put(&mut img, 510, 84, &8u16.to_be_bytes());
    put(&mut img, 510, 88, b"CAT 0001        ");
    put(&mut img, 510, 104, &[1, 0, 0, spec.genre]);
    let (y, m, d) = spec.date;
    put(&mut img, 510, 120, &y.to_be_bytes());
    put(&mut img, 510, 122, &[m, d]);
    put(&mut img, 510, 128, &[1]);
    put(&mut img, 510, 136, b"en\x02");

    // Master text, ISO 8859-1: title, artist and publisher.
    put(&mut img, 511, 0, b"SACDText");
    for (i, text) in [&spec.title, &spec.artist, &spec.publisher]
        .into_iter()
        .enumerate()
    {
        let at = 64 + 384 * i;
        put(&mut img, 511, 16 + 2 * i, &(at as u16).to_be_bytes());
        let mut t = latin1(text);
        t.truncate(383);
        put(&mut img, 511, at, &t);
    }

    // Area TOC.
    let a = AREA_TOC;
    put(&mut img, a, 0, b"TWOCHTOC\x01\x14\x00\x08");
    put(&mut img, a, 20, &[4, if spec.dst { 0 } else { 2 }]);
    put(&mut img, a, 32, &[2]);
    put_time(&mut img, a, 64, total);
    put(&mut img, a, 69, &[spec.tracks.len() as u8]);
    put(&mut img, a, 72, &(AUDIO_START as u32).to_be_bytes());
    put(&mut img, a, 76, &(end as u32 - 1).to_be_bytes());
    put(&mut img, a, 80, &[1]);
    put(&mut img, a, 88, b"en\x02");
    put(&mut img, a, 128, &5u16.to_be_bytes());
    put(&mut img, a + 1, 0, b"SACDTRL1");
    put(&mut img, a + 2, 0, b"SACDTRL2");
    put(&mut img, a + 3, 0, b"SACD_IGL");
    put(&mut img, a + 5, 0, b"SACDTTxt");
    let mut first = spec.pregap;
    let mut text = 0x200;
    for (i, (len, title)) in spec.tracks.iter().enumerate() {
        let len = *len;
        let lsn = AUDIO_START + starts[first as usize];
        let next = starts
            .get((first + len) as usize)
            .map_or(end, |s| AUDIO_START + s);
        put(&mut img, a + 1, 8 + 4 * i, &(lsn as u32).to_be_bytes());
        put(
            &mut img,
            a + 1,
            1028 + 4 * i,
            &((next - lsn) as u32).to_be_bytes(),
        );
        put_time(&mut img, a + 2, 8 + 4 * i, first);
        put_time(&mut img, a + 2, 1028 + 4 * i, len);
        put(
            &mut img,
            a + 3,
            8 + 12 * i,
            format!("ZZX0101{:05}", i + 1).as_bytes(),
        );

        put(&mut img, a + 5, 8 + 2 * i, &(text as u16).to_be_bytes());
        let mut item = vec![2, 0, 0, 0, 1, 0x20];
        item.extend(latin1(title));
        item.resize(item.len().next_multiple_of(4), 0);
        item.extend([2, 0x20]);
        item.extend(latin1(&spec.performer));
        item.resize(item.len().next_multiple_of(4), 0);
        put(&mut img, a + 5, text, &item);
        text += item.len().next_multiple_of(4);
        first += len;
    }

    for (i, s) in sectors.iter().enumerate() {
        put(&mut img, AUDIO_START + i, 0, s);
    }
    img
}
