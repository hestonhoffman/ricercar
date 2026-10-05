//! CUE sheets: an album ripped as one audio file, cut into tracks.
//!
//! Times are in CD frames (1/75 s). A track runs from its `INDEX 01` to the
//! next track's `INDEX 01` in the same file (the pregap, `INDEX 00`, plays
//! at the end of the track before it), and the last track of a file runs to
//! the end of the file.

use std::path::{Path, PathBuf};

use crate::meta::TagInfo;

/// CD frames per second.
pub const FPS: u64 = 75;

/// Suffix of the library key of a cue track: `<audio path>#cue=<track>`.
const KEY_MARK: &str = "#cue=";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CueSheet {
    pub title: Option<String>,
    pub performer: Option<String>,
    pub songwriter: Option<String>,
    pub genre: Option<String>,
    pub date: Option<String>,
    pub comment: Option<String>,
    pub disc: Option<u32>,
    pub rg_album_gain: Option<f32>,
    pub rg_album_peak: Option<f32>,
    /// `FILE` names, as written.
    pub files: Vec<String>,
    pub tracks: Vec<CueTrack>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CueTrack {
    pub number: u32,
    /// Index in `files` of the file holding `INDEX 01`.
    pub file: usize,
    pub title: Option<String>,
    pub performer: Option<String>,
    pub songwriter: Option<String>,
    pub rg_gain: Option<f32>,
    pub rg_peak: Option<f32>,
    /// `INDEX 01`, in frames.
    pub start: u64,
}

/// A track's place in its file, in frames; `end` is `None` up to the end
/// of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u64,
    pub end: Option<u64>,
}

impl CueSheet {
    /// Span of each track, in order.
    pub fn spans(&self) -> Vec<Span> {
        self.tracks
            .iter()
            .enumerate()
            .map(|(i, t)| Span {
                start: t.start,
                end: self
                    .tracks
                    .get(i + 1)
                    .filter(|n| n.file == t.file && n.start > t.start)
                    .map(|n| n.start),
            })
            .collect()
    }

    /// Tracks of one file.
    pub fn tracks_of(&self, file: usize) -> impl Iterator<Item = (usize, &CueTrack)> {
        self.tracks
            .iter()
            .enumerate()
            .filter(move |(_, t)| t.file == file)
    }
}

/// Text of a cue sheet: UTF-8 (with or without BOM), else Windows-1252,
/// which also covers Latin-1.
pub fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| cp1252(b)).collect(),
    }
}

/// Windows-1252 differs from Latin-1 in 0x80..0xA0 only.
fn cp1252(b: u8) -> char {
    const HIGH: [u16; 32] = [
        0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160,
        0x2039, 0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013,
        0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
    ];
    match b {
        0x80..=0x9F => char::from_u32(HIGH[(b - 0x80) as usize] as u32).unwrap_or('\u{FFFD}'),
        _ => b as char,
    }
}

/// Read and parse a `.cue` file.
pub fn read(path: &Path) -> Option<CueSheet> {
    let bytes = std::fs::read(path).ok()?;
    let sheet = parse(&decode(&bytes));
    (!sheet.tracks.is_empty()).then_some(sheet)
}

/// First word of a line and the rest, trimmed.
fn split_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim()),
        None => (s, ""),
    }
}

/// A value, quoted or not: `"A B"` → `A B`, `A B` → `A B`.
fn value(s: &str) -> Option<String> {
    let s = s.trim();
    let v = match s.strip_prefix('"') {
        Some(rest) => rest.rsplit_once('"').map_or(rest, |(v, _)| v),
        None => s,
    };
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// `FILE "name" WAVE` → `name` (the type is the last word, the name may
/// hold spaces even unquoted).
fn file_name(s: &str) -> Option<String> {
    let s = s.trim();
    if s.starts_with('"') {
        return value(s);
    }
    let name = match s.rsplit_once(char::is_whitespace) {
        Some((n, kind)) if kind.chars().all(|c| c.is_ascii_alphanumeric()) => n,
        _ => s,
    };
    value(name)
}

/// `mm:ss:ff` → frames.
fn msf(s: &str) -> Option<u64> {
    let mut it = s.trim().split(':').map(|p| p.trim().parse::<u64>().ok());
    let (m, sec, f) = (it.next()??, it.next()??, it.next()??);
    (it.next().is_none() && sec < 60 && f < FPS).then_some((m * 60 + sec) * FPS + f)
}

fn gain(s: &str) -> Option<f32> {
    let v = value(s)?;
    let n = v
        .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c.is_whitespace())
        .trim();
    n.parse::<f32>().ok().filter(|g| g.is_finite())
}

/// Parse cue sheet text. Unknown or broken lines are skipped; tracks
/// without `INDEX 01`, data tracks and repeated track numbers are left out.
pub fn parse(text: &str) -> CueSheet {
    let mut sheet = CueSheet::default();
    let mut current: Option<CueTrack> = None;
    let mut audio = false;
    let mut has_start = false;
    let finish = |sheet: &mut CueSheet, t: Option<CueTrack>, audio: bool, has_start: bool| {
        if let Some(t) = t
            && audio
            && has_start
            && !sheet.tracks.iter().any(|o| o.number == t.number)
        {
            sheet.tracks.push(t);
        }
    };
    for line in text.lines() {
        let (cmd, rest) = split_word(line);
        match cmd.to_ascii_uppercase().as_str() {
            "FILE" => {
                if let Some(name) = file_name(rest) {
                    sheet.files.push(name);
                }
            }
            "TRACK" => {
                finish(&mut sheet, current.take(), audio, has_start);
                let (num, kind) = split_word(rest);
                audio = kind.eq_ignore_ascii_case("AUDIO");
                has_start = false;
                current = num
                    .parse::<u32>()
                    .ok()
                    .filter(|_| !sheet.files.is_empty())
                    .map(|n| CueTrack {
                        number: n,
                        file: sheet.files.len() - 1,
                        ..Default::default()
                    });
            }
            "INDEX" => {
                let (num, at) = split_word(rest);
                if let (Some(t), Ok(1), Some(at)) = (current.as_mut(), num.parse::<u32>(), msf(at))
                {
                    t.start = at;
                    t.file = sheet.files.len().saturating_sub(1);
                    has_start = true;
                }
            }
            "TITLE" | "PERFORMER" | "SONGWRITER" => {
                let v = value(rest);
                let slot = match (current.as_mut(), cmd.to_ascii_uppercase().as_str()) {
                    (Some(t), "TITLE") => &mut t.title,
                    (Some(t), "PERFORMER") => &mut t.performer,
                    (Some(t), _) => &mut t.songwriter,
                    (None, "TITLE") => &mut sheet.title,
                    (None, "PERFORMER") => &mut sheet.performer,
                    (None, _) => &mut sheet.songwriter,
                };
                *slot = v;
            }
            "REM" => {
                let (key, v) = split_word(rest);
                match (current.as_mut(), key.to_ascii_uppercase().as_str()) {
                    (None, "GENRE") => sheet.genre = value(v),
                    (None, "DATE") => sheet.date = value(v),
                    (None, "COMMENT") => sheet.comment = value(v),
                    (None, "DISCNUMBER") => {
                        sheet.disc = value(v).and_then(|d| d.parse().ok());
                    }
                    (_, "REPLAYGAIN_ALBUM_GAIN") => sheet.rg_album_gain = gain(v),
                    (_, "REPLAYGAIN_ALBUM_PEAK") => sheet.rg_album_peak = gain(v),
                    (Some(t), "REPLAYGAIN_TRACK_GAIN") => t.rg_gain = gain(v),
                    (Some(t), "REPLAYGAIN_TRACK_PEAK") => t.rg_peak = gain(v),
                    _ => {}
                }
            }
            // FLAGS, ISRC, CATALOG, PREGAP, POSTGAP, CDTEXTFILE, junk.
            _ => {}
        }
    }
    finish(&mut sheet, current.take(), audio, has_start);
    sheet
}

/// The audio file a `FILE` entry names, next to the sheet. Rippers often
/// name the WAV they read and the file is compressed afterwards, so the
/// same name with another audio extension is accepted too.
pub fn resolve_file(sheet_path: &Path, name: &str, exts: &[&str]) -> Option<PathBuf> {
    let dir = sheet_path.parent()?;
    // Only the file name: a sheet never reaches outside its folder.
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let exact = dir.join(name);
    if exact.is_file() {
        return Some(exact);
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    let file_name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
    if let Some(p) = entries
        .iter()
        .find(|p| file_name(p).is_some_and(|n| n.eq_ignore_ascii_case(name)))
    {
        return Some(p.clone());
    }
    let stem = Path::new(name)
        .file_stem()?
        .to_string_lossy()
        .to_lowercase();
    let mut same_stem: Vec<&PathBuf> = entries
        .iter()
        .filter(|p| {
            p.file_stem()
                .is_some_and(|s| s.to_string_lossy().to_lowercase() == stem)
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| exts.contains(&e.to_ascii_lowercase().as_str()))
        })
        .collect();
    same_stem.sort();
    same_stem.first().map(|p| (*p).clone())
}

/// Library key of track `number` of an audio file.
pub fn key(audio: &Path, number: u32) -> String {
    format!("{}{KEY_MARK}{number}", audio.to_string_lossy())
}

/// Split a library key into the audio file and the cue track number;
/// `None` for ordinary tracks.
pub fn split_key(path: &str) -> Option<(&str, u32)> {
    let (audio, n) = path.rsplit_once(KEY_MARK)?;
    let n = n
        .parse()
        .ok()
        .filter(|_| n.bytes().all(|b| b.is_ascii_digit()))?;
    Some((audio, n))
}

/// The file on disk behind a library path (the audio file of a cue track).
pub fn file_of(path: &str) -> &str {
    split_key(path).map_or(path, |(audio, _)| audio)
}

/// Seconds of a frame count, written exactly enough that rounding to the
/// sample at any rate gives the frame back.
fn secs(frames: u64) -> String {
    let whole = frames / FPS;
    let nanos = ((frames % FPS) * 1_000_000_000 + FPS / 2) / FPS;
    if nanos == 0 {
        return whole.to_string();
    }
    let frac = format!("{nanos:09}");
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

/// URI fragment that makes the engine play a span (`#t=start[,end]` in
/// seconds, as in W3C media fragments).
pub fn fragment(start: u64, end: Option<u64>) -> String {
    match end {
        Some(e) => format!("#t={},{}", secs(start), secs(e)),
        None => format!("#t={}", secs(start)),
    }
}

/// Tags of track `idx` of a sheet: the sheet first, the audio file's own
/// tags for what it leaves out.
pub fn track_tags(sheet: &CueSheet, idx: usize, span: Span, file: &TagInfo) -> TagInfo {
    let t = &sheet.tracks[idx];
    let ms = |frames: u64| frames * 1000 / FPS;
    let year = sheet.date.as_deref().and_then(|d| {
        let y: String = d
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        y.parse::<i32>().ok().filter(|y| (1000..3000).contains(y))
    });
    TagInfo {
        title: Some(
            t.title
                .clone()
                .unwrap_or_else(|| format!("Track {:02}", t.number)),
        ),
        artist: t
            .performer
            .clone()
            .or_else(|| sheet.performer.clone())
            .or_else(|| file.artist.clone()),
        album_artist: sheet
            .performer
            .clone()
            .or_else(|| file.album_artist.clone()),
        album: sheet.title.clone().or_else(|| file.album.clone()),
        track: Some(t.number),
        disc: sheet.disc.or(file.disc),
        year: year.or(file.year),
        genre: sheet.genre.clone().or_else(|| file.genre.clone()),
        composer: t
            .songwriter
            .clone()
            .or_else(|| sheet.songwriter.clone())
            .or_else(|| file.composer.clone()),
        duration_ms: match span.end {
            Some(e) => ms(e - span.start),
            None => file.duration_ms.saturating_sub(ms(span.start)),
        },
        rg_track_gain: t.rg_gain,
        rg_track_peak: t.rg_peak,
        // The file's own gain covers the whole disc.
        rg_album_gain: sheet
            .rg_album_gain
            .or(file.rg_album_gain)
            .or(file.rg_track_gain),
        rg_album_peak: sheet
            .rg_album_peak
            .or(file.rg_album_peak)
            .or(file.rg_track_peak),
        mb_track_id: None,
        ..file.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHEET: &str = "REM GENRE \"Jazz\"\r\nREM DATE 1959\r\nREM COMMENT \"ExactAudioCopy v1.6\"\r\n\
PERFORMER \"Miles Davis\"\r\nTITLE \"Kind of Blue\"\r\nFILE \"Kind of Blue.wav\" WAVE\r\n\
  TRACK 01 AUDIO\r\n    TITLE \"So What\"\r\n    PERFORMER \"Miles Davis\"\r\n    FLAGS DCP\r\n\
    ISRC USSM15900113\r\n    INDEX 01 00:00:00\r\n\
  TRACK 02 AUDIO\r\n    TITLE \"Freddie Freeloader\"\r\n    SONGWRITER \"M. Davis\"\r\n\
    INDEX 00 09:20:50\r\n    INDEX 01 09:22:00\r\n\
  TRACK 03 AUDIO\r\n    TITLE \"Blue in Green\"\r\n    INDEX 01 19:08:37\r\n";

    #[test]
    fn parses_a_single_file_album() {
        let s = parse(SHEET);
        assert_eq!(s.title.as_deref(), Some("Kind of Blue"));
        assert_eq!(s.performer.as_deref(), Some("Miles Davis"));
        assert_eq!(s.genre.as_deref(), Some("Jazz"));
        assert_eq!(s.date.as_deref(), Some("1959"));
        assert_eq!(s.comment.as_deref(), Some("ExactAudioCopy v1.6"));
        assert_eq!(s.files, ["Kind of Blue.wav"]);
        let titles: Vec<_> = s.tracks.iter().map(|t| t.title.clone().unwrap()).collect();
        assert_eq!(titles, ["So What", "Freddie Freeloader", "Blue in Green"]);
        assert_eq!(s.tracks[1].songwriter.as_deref(), Some("M. Davis"));
        assert_eq!(s.tracks[1].performer, None);
        // The pregap (INDEX 00) stays with the track before.
        assert_eq!(
            s.spans(),
            [
                Span {
                    start: 0,
                    end: Some((9 * 60 + 22) * 75)
                },
                Span {
                    start: (9 * 60 + 22) * 75,
                    end: Some((19 * 60 + 8) * 75 + 37)
                },
                Span {
                    start: (19 * 60 + 8) * 75 + 37,
                    end: None
                },
            ]
        );
    }

    #[test]
    fn several_files() {
        // One file per track, the gap of track 2 at the end of file 1.
        let s = parse(
            "FILE \"01.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\n\
             INDEX 00 03:00:00\nFILE \"02.flac\" WAVE\nINDEX 01 00:00:00\n\
             FILE \"03 and 04.flac\" WAVE\nTRACK 03 AUDIO\nINDEX 01 00:00:00\n\
             TRACK 04 AUDIO\nINDEX 01 01:00:00\n",
        );
        assert_eq!(s.files, ["01.flac", "02.flac", "03 and 04.flac"]);
        let files: Vec<usize> = s.tracks.iter().map(|t| t.file).collect();
        assert_eq!(files, [0, 1, 2, 2]);
        let ends: Vec<Option<u64>> = s.spans().iter().map(|s| s.end).collect();
        assert_eq!(ends, [None, None, Some(4500), None]);
        assert_eq!(s.tracks_of(2).count(), 2);
    }

    #[test]
    fn junk_unquoted_and_odd_lines() {
        let s = parse(
            "\u{FEFF}garbage line\n\nTITLE Unquoted Album Name\nPERFORMER  \"\"\n\
             FILE album.flac WAVE\n  TRACK 01 AUDIO\n  TITLE \"Has \"quotes\" inside\"\n\
             INDEX 01 00:00:00\n  TRACK 02 MODE1/2352\n  INDEX 01 01:00:00\n\
             TRACK xx AUDIO\nINDEX 01 02:00:00\nTRACK 03 AUDIO\nINDEX 01 99:99:99\n\
             TRACK 04 AUDIO\nINDEX 01 03:00:00\nTRACK 04 AUDIO\nINDEX 01 04:00:00\n\
             REM REPLAYGAIN_ALBUM_GAIN -7.50 dB\n",
        );
        assert_eq!(s.title.as_deref(), Some("Unquoted Album Name"));
        assert_eq!(s.performer, None);
        assert_eq!(s.files, ["album.flac"]);
        let nums: Vec<u32> = s.tracks.iter().map(|t| t.number).collect();
        assert_eq!(
            nums,
            [1, 4],
            "data, broken and repeated tracks are left out"
        );
        assert_eq!(s.tracks[0].title.as_deref(), Some("Has \"quotes\" inside"));
        assert_eq!(s.rg_album_gain, Some(-7.5));
    }

    #[test]
    fn encodings() {
        let utf8 = "TITLE \"Début\"\nFILE \"a.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n";
        let mut bom = b"\xEF\xBB\xBF".to_vec();
        bom.extend(utf8.as_bytes());
        assert_eq!(parse(&decode(&bom)).title.as_deref(), Some("Début"));
        assert_eq!(
            parse(&decode(utf8.as_bytes())).title.as_deref(),
            Some("Début")
        );
        // Windows-1252: é is 0xE9, the curly quote 0x92, the euro sign 0x80.
        let cp = b"TITLE \"D\xE9but \x80 l\x92\xE9t\xE9\"\n";
        assert_eq!(parse(&decode(cp)).title.as_deref(), Some("Début € l’été"));
    }

    #[test]
    fn keys_and_fragments() {
        let k = key(Path::new("/m/#1 hits/a.flac"), 7);
        assert_eq!(k, "/m/#1 hits/a.flac#cue=7");
        assert_eq!(split_key(&k), Some(("/m/#1 hits/a.flac", 7)));
        assert_eq!(split_key("/m/#1 hits/a.flac"), None);
        assert_eq!(split_key("/m/a.flac#cue=x"), None);
        assert_eq!(file_of(&k), "/m/#1 hits/a.flac");
        assert_eq!(fragment(0, Some(75)), "#t=0,1");
        assert_eq!(fragment(926, None), "#t=12.346666667");
        assert_eq!(fragment(1, Some(150)), "#t=0.013333333,2");
    }

    #[test]
    fn resolves_renamed_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Album.FLAC"), b"x").unwrap();
        let sheet = dir.path().join("album.cue");
        let exts = ["flac", "wav"];
        let want = Some(dir.path().join("Album.FLAC"));
        assert_eq!(resolve_file(&sheet, "Album.FLAC", &exts), want);
        assert_eq!(resolve_file(&sheet, "album.flac", &exts), want);
        assert_eq!(resolve_file(&sheet, "Album.wav", &exts), want);
        assert_eq!(resolve_file(&sheet, "C:\\rips\\Album.wav", &exts), want);
        assert_eq!(resolve_file(&sheet, "Other.wav", &exts), None);
    }
}
