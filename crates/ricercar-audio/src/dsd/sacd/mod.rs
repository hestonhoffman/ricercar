//! SACD disc images (ISO): the master TOC and text, the stereo area's
//! track list, and its tracks as DSD streams (plain DSD or DST-coded).
//!
//! Only table-of-contents sectors are read when an image is opened; audio
//! is streamed sector by sector when a track is played. The multichannel
//! area is listed but not decoded.

mod stream;
#[cfg(test)]
mod tests;

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::dsd::source::{DsdFormat, DsdSource};
use crate::error::{AudioError, Result};

pub use stream::RawFrames;

pub(crate) const SECTOR: usize = 2048;
const MASTER_TOC: u32 = 510;
/// Master TOC copies, tried in order.
const MASTER_TOC_COPIES: [u32; 3] = [510, 520, 530];
/// SACD frames per second (and per second of time codes).
pub const FRAMES_PER_SEC: u32 = 75;

/// The two kinds of audio area a disc can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AreaKind {
    Stereo,
    Multichannel,
}

/// How an area stores its audio frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFormat {
    /// DST lossless compression.
    Dst,
    /// Plain DSD, 3 frames in 14 sectors.
    Dsd3In14,
    /// Plain DSD, 3 frames in 16 sectors.
    Dsd3In16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AreaInfo {
    pub kind: AreaKind,
    pub format: DsdFormat,
    pub frame_format: FrameFormat,
    pub track_count: u32,
    /// Total play time, in frames of 1/75 s.
    pub frames: u32,
}

impl AreaInfo {
    pub fn duration(&self) -> Duration {
        frames_to_duration(self.frames)
    }
}

/// Disc-wide information from the master TOC and master text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscInfo {
    /// Album title (or the disc title when the album has none).
    pub title: Option<String>,
    pub artist: Option<String>,
    pub publisher: Option<String>,
    pub copyright: Option<String>,
    pub genre: Option<String>,
    /// `YYYY`, `YYYY-MM` or `YYYY-MM-DD`.
    pub date: Option<String>,
    pub catalog_number: Option<String>,
    /// Position of this disc in its album set (1-based), and the set size.
    pub disc_number: u32,
    pub disc_count: u32,
    /// A CD layer is present as well.
    pub hybrid: bool,
    pub areas: Vec<AreaInfo>,
}

/// A track of the stereo area.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SacdTrack {
    /// 1-based, as given to [`SacdImage::track_source`].
    pub number: u32,
    pub title: Option<String>,
    pub performer: Option<String>,
    pub songwriter: Option<String>,
    pub composer: Option<String>,
    pub arranger: Option<String>,
    pub message: Option<String>,
    pub isrc: Option<String>,
    /// Start in frames of 1/75 s from the start of the area.
    pub start_frame: u32,
    /// Length in frames of 1/75 s.
    pub frames: u32,
    /// First sector and sector count, as listed in the area TOC.
    pub start_sector: u32,
    pub sectors: u32,
}

impl SacdTrack {
    pub fn start(&self) -> Duration {
        frames_to_duration(self.start_frame)
    }

    pub fn duration(&self) -> Duration {
        frames_to_duration(self.frames)
    }
}

fn frames_to_duration(frames: u32) -> Duration {
    Duration::from_nanos(frames as u64 * 1_000_000_000 / FRAMES_PER_SEC as u64)
}

/// The stereo area, as needed to stream its tracks.
#[derive(Debug, Clone)]
pub(crate) struct Area {
    pub info: AreaInfo,
    /// Audio sectors: `start..end`.
    pub start: u32,
    pub end: u32,
    pub tracks: Vec<SacdTrack>,
}

/// An opened SACD image.
#[derive(Debug, Clone)]
pub struct SacdImage {
    path: PathBuf,
    disc: DiscInfo,
    stereo: Option<Area>,
}

impl SacdImage {
    /// Reads the TOC of the image at `path` (a few dozen sectors).
    pub fn open(path: impl AsRef<Path>) -> Result<SacdImage> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let (disc, stereo) = parse(&file)?;
        Ok(SacdImage { path, disc, stereo })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn disc(&self) -> &DiscInfo {
        &self.disc
    }

    /// The stereo area, if the disc has one.
    pub fn stereo_area(&self) -> Option<&AreaInfo> {
        self.stereo.as_ref().map(|a| &a.info)
    }

    /// Tracks of the stereo area (empty when there is none).
    pub fn tracks(&self) -> &[SacdTrack] {
        self.stereo.as_ref().map_or(&[], |a| &a.tracks)
    }

    /// Track `n` (1-based) of the stereo area as a DSD stream.
    pub fn track_source(&self, n: u32) -> Result<Box<dyn DsdSource>> {
        let (area, track) = self.track(n)?;
        let tags = self.tags(track, area.tracks.len());
        let s = stream::TrackStream::open(&self.path, area, track, tags)?;
        Ok(Box::new(s))
    }

    /// The frames of track `n` as stored on the disc (DST-coded or plain),
    /// one item per frame of 1/75 s.
    pub fn track_frames(&self, n: u32) -> Result<RawFrames> {
        let (area, track) = self.track(n)?;
        RawFrames::open(&self.path, area, track)
    }

    fn track(&self, n: u32) -> Result<(&Area, &SacdTrack)> {
        let Some(area) = &self.stereo else {
            let msg = if self.disc.areas.is_empty() {
                "SACD image has no audio area"
            } else {
                "SACD multichannel areas are not supported yet"
            };
            return Err(AudioError::UnsupportedSource(msg.into()));
        };
        let track =
            area.tracks.iter().find(|t| t.number == n).ok_or_else(|| {
                AudioError::UnsupportedSource(format!("SACD image has no track {n}"))
            })?;
        Ok((area, track))
    }

    fn tags(&self, t: &SacdTrack, total: usize) -> Vec<(String, String)> {
        let d = &self.disc;
        let mut tags = Vec::new();
        let mut add = |k: &str, v: Option<&String>| {
            if let Some(v) = v {
                tags.push((k.to_string(), v.clone()));
            }
        };
        add("TITLE", t.title.as_ref());
        add("ARTIST", t.performer.as_ref().or(d.artist.as_ref()));
        add("ALBUM", d.title.as_ref());
        add("ALBUMARTIST", d.artist.as_ref());
        add("TRACKNUMBER", Some(&t.number.to_string()));
        add("TRACKTOTAL", Some(&total.to_string()));
        if d.disc_count > 1 {
            add("DISCNUMBER", Some(&d.disc_number.to_string()));
            add("DISCTOTAL", Some(&d.disc_count.to_string()));
        }
        add("DATE", d.date.as_ref());
        add("GENRE", d.genre.as_ref());
        add("COMPOSER", t.composer.as_ref().or(t.songwriter.as_ref()));
        add("ORGANIZATION", d.publisher.as_ref());
        add("COPYRIGHT", d.copyright.as_ref());
        add("ISRC", t.isrc.as_ref());
        tags
    }
}

fn bad(msg: impl Into<String>) -> AudioError {
    AudioError::Decode(format!("SACD image: {}", msg.into()))
}

pub(crate) fn read_sectors(file: &File, first: u32, count: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; count * SECTOR];
    file.read_exact_at(&mut buf, first as u64 * SECTOR as u64)?;
    Ok(buf)
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// A minutes / seconds / frames time code, in frames.
pub(crate) fn time_code(b: &[u8]) -> u32 {
    (b[0] as u32 * 60 + b[1] as u32) * FRAMES_PER_SEC + b[2] as u32
}

fn parse(file: &File) -> Result<(DiscInfo, Option<Area>)> {
    let mut mtoc = None;
    for lsn in MASTER_TOC_COPIES {
        match read_sectors(file, lsn, 1) {
            Ok(s) if &s[..8] == b"SACDMTOC" => {
                mtoc = Some(s);
                break;
            }
            Ok(_) => {}
            Err(e) if lsn == MASTER_TOC => return Err(e.into()),
            Err(_) => {}
        }
    }
    let m = mtoc.ok_or_else(|| bad("no master TOC (not a SACD image?)"))?;

    let mut disc = DiscInfo {
        disc_count: be16(&m, 16) as u32,
        disc_number: be16(&m, 18) as u32,
        hybrid: m[80] & 0x80 != 0,
        ..DiscInfo::default()
    };
    disc.catalog_number = latin1(&m[88..104])
        .or_else(|| latin1(&m[24..40]))
        .filter(|s| !s.is_empty());
    disc.genre = genre(&m[104..120]).or_else(|| genre(&m[40..56]));
    let year = be16(&m, 120);
    let (month, day) = (m[122], m[123]);
    disc.date = match (year, month, day) {
        (0, _, _) => None,
        (y, 1..=12, 1..=31) => Some(format!("{y:04}-{month:02}-{day:02}")),
        (y, 1..=12, _) => Some(format!("{y:04}-{month:02}")),
        (y, _, _) => Some(format!("{y:04}")),
    };

    // Master text: one sector per language, the first one is used.
    if m[128] > 0
        && let Ok(t) = read_sectors(file, MASTER_TOC + 1, 1)
        && &t[..8] == b"SACDText"
    {
        let charset = m[136 + 2];
        let field = |at: usize| text_at(&t, be16(&t, at) as usize, charset);
        disc.title = field(16).or_else(|| field(32));
        disc.artist = field(18).or_else(|| field(34));
        disc.publisher = field(20).or_else(|| field(36));
        disc.copyright = field(22).or_else(|| field(38));
    }

    let mut stereo = None;
    for (kind, tocs) in [
        (AreaKind::Stereo, [be32(&m, 64), be32(&m, 68)]),
        (AreaKind::Multichannel, [be32(&m, 72), be32(&m, 76)]),
    ] {
        let mut area = None;
        for lsn in tocs.into_iter().filter(|&l| l != 0) {
            match parse_area(file, lsn, kind) {
                Ok(a) => {
                    area = Some(a);
                    break;
                }
                Err(e) => tracing::warn!("{kind:?} area TOC at sector {lsn}: {e}"),
            }
        }
        if let Some(a) = area {
            disc.areas.push(a.info.clone());
            if kind == AreaKind::Stereo {
                stereo = Some(a);
            }
        }
    }
    Ok((disc, stereo))
}

fn parse_area(file: &File, lsn: u32, kind: AreaKind) -> Result<Area> {
    let head = read_sectors(file, lsn, 1)?;
    let id: &[u8] = match kind {
        AreaKind::Stereo => b"TWOCHTOC",
        AreaKind::Multichannel => b"MULCHTOC",
    };
    if &head[..8] != id {
        return Err(bad("bad area TOC signature"));
    }
    let size = (be16(&head, 10) as usize).clamp(1, 64);
    let toc = read_sectors(file, lsn, size)?;

    let rate = match toc[20] {
        4 => 2_822_400,
        c => return Err(bad(format!("unknown sampling frequency code {c}"))),
    };
    let frame_format = match toc[21] & 0x0f {
        0 => FrameFormat::Dst,
        2 => FrameFormat::Dsd3In14,
        3 => FrameFormat::Dsd3In16,
        f => return Err(bad(format!("unknown frame format {f}"))),
    };
    let channels = toc[32] as u16;
    if !(1..=6).contains(&channels) {
        return Err(bad(format!("{channels} channels")));
    }
    let track_count = toc[69] as usize;
    let info = AreaInfo {
        kind,
        format: DsdFormat { rate, channels },
        frame_format,
        track_count: track_count as u32,
        frames: time_code(&toc[64..67]),
    };
    let (start, end) = (be32(&toc, 72), be32(&toc, 76));
    if end < start {
        return Err(bad("empty track area"));
    }
    let charset = if toc[80] > 0 { toc[88 + 2] } else { 0 };

    let find = |sig: &[u8]| {
        toc.chunks(SECTOR)
            .position(|s| &s[..8] == sig)
            .map(|i| i * SECTOR)
    };
    let mut tracks: Vec<SacdTrack> = (0..track_count)
        .map(|i| SacdTrack {
            number: i as u32 + 1,
            ..SacdTrack::default()
        })
        .collect();
    let at = find(b"SACDTRL1").ok_or_else(|| bad("no track sector list"))?;
    for (i, t) in tracks.iter_mut().enumerate() {
        t.start_sector = be32(&toc, at + 8 + 4 * i);
        t.sectors = be32(&toc, at + 8 + 1020 + 4 * i);
    }
    let at = find(b"SACDTRL2").ok_or_else(|| bad("no track time list"))?;
    for (i, t) in tracks.iter_mut().enumerate() {
        t.start_frame = time_code(&toc[at + 8 + 4 * i..]);
        t.frames = time_code(&toc[at + 8 + 1020 + 4 * i..]);
    }
    if let Some(at) = find(b"SACD_IGL") {
        for (i, t) in tracks.iter_mut().enumerate() {
            let p = at + 8 + 12 * i;
            if let Some(isrc) = toc.get(p..p + 12) {
                t.isrc = latin1(isrc).filter(|s| s.len() == 12);
            }
        }
    }
    let text_at_sector = match be16(&toc, 128) as usize * SECTOR {
        0 => find(b"SACDTTxt"),
        p => Some(p).filter(|&p| toc.get(p..p + 8) == Some(b"SACDTTxt")),
    };
    if let Some(at) = text_at_sector {
        track_text(&toc, at, charset, &mut tracks);
    }
    Ok(Area {
        info,
        start,
        end: end + 1,
        tracks,
    })
}

const TEXT_TITLE: u8 = 0x01;
const TEXT_PERFORMER: u8 = 0x02;
const TEXT_SONGWRITER: u8 = 0x03;
const TEXT_COMPOSER: u8 = 0x04;
const TEXT_ARRANGER: u8 = 0x05;
const TEXT_MESSAGE: u8 = 0x06;

/// Track text: per track, an item count, then items of a type byte, a
/// padding byte and a NUL-terminated string, separated by NUL padding.
/// Offsets count from the `SACDTTxt` sector and may run past it.
fn track_text(toc: &[u8], at: usize, charset: u8, tracks: &mut [SacdTrack]) {
    for (i, t) in tracks.iter_mut().enumerate() {
        let pos = be16(toc, at + 8 + 2 * i) as usize;
        if pos == 0 {
            continue;
        }
        let mut p = at + pos;
        let Some(&count) = toc.get(p) else { continue };
        p += 4;
        for k in 0..count {
            let Some(&kind) = toc.get(p) else { break };
            p += 2;
            let value = text_at(toc, p, charset);
            match kind {
                TEXT_TITLE => t.title = value,
                TEXT_PERFORMER => t.performer = value,
                TEXT_SONGWRITER => t.songwriter = value,
                TEXT_COMPOSER => t.composer = value,
                TEXT_ARRANGER => t.arranger = value,
                TEXT_MESSAGE => t.message = value,
                _ => {}
            }
            if k + 1 < count {
                while toc.get(p).is_some_and(|&b| b != 0) {
                    p += 1;
                }
                while toc.get(p) == Some(&0) {
                    p += 1;
                }
            }
        }
    }
}

/// The NUL-terminated string at `pos` (0 means absent).
fn text_at(buf: &[u8], pos: usize, charset: u8) -> Option<String> {
    if pos == 0 || pos >= buf.len() {
        return None;
    }
    let raw = &buf[pos..];
    let raw = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
    decode_text(raw, charset)
}

/// Character sets of the master and area TOCs: 1 ISO 646, 2 ISO 8859-1,
/// 3 Shift-JIS (RIS 506), 4 KS C 5601, 5 GB 2312, 6 Big5, 7 ISO 8859-1
/// with escapes. The double-byte sets are only decoded when they hold
/// ASCII or valid UTF-8; other characters become U+FFFD.
pub(crate) fn decode_text(raw: &[u8], charset: u8) -> Option<String> {
    let s = match charset {
        3..=6 => match std::str::from_utf8(raw) {
            Ok(s) => s.to_string(),
            Err(_) => {
                let mut s = String::new();
                let mut i = 0;
                while i < raw.len() {
                    let b = raw[i];
                    if b < 0x80 {
                        s.push(b as char);
                    } else {
                        s.push(char::REPLACEMENT_CHARACTER);
                        // Shift-JIS half-width katakana are single bytes.
                        if !(charset == 3 && (0xa1..=0xdf).contains(&b)) {
                            i += 1;
                        }
                    }
                    i += 1;
                }
                s
            }
        },
        _ => return latin1(raw),
    };
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn latin1(raw: &[u8]) -> Option<String> {
    let raw = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
    let s: String = raw.iter().map(|&b| b as char).collect();
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

const GENRES: [&str; 30] = [
    "",
    "",
    "Adult Contemporary",
    "Alternative Rock",
    "Children's Music",
    "Classical",
    "Contemporary Christian",
    "Country",
    "Dance",
    "Easy Listening",
    "Erotic",
    "Folk",
    "Gospel",
    "Hip Hop",
    "Jazz",
    "Latin",
    "Musical",
    "New Age",
    "Opera",
    "Operetta",
    "Pop Music",
    "Rap",
    "Reggae",
    "Rock Music",
    "Rhythm & Blues",
    "Sound Effects",
    "Soundtrack",
    "Spoken Word",
    "World Music",
    "Blues",
];

/// The first general-table genre of a 4-entry genre list.
fn genre(list: &[u8]) -> Option<String> {
    list.chunks(4)
        .filter(|g| g[0] == 1)
        .filter_map(|g| GENRES.get(g[3] as usize))
        .find(|g| !g.is_empty())
        .map(|g| g.to_string())
}
