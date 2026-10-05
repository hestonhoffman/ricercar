//! Just enough ID3v2 (2.2, 2.3, 2.4) for DSF and DFF files: text frames
//! and the cover picture. Compressed or encrypted frames are skipped.

/// Tags as `(key, value)`, keys lowercase (`title`, `artist`, `album`,
/// `albumartist`, `tracknumber`, `discnumber`, `date`, `genre`,
/// `composer`), and the cover picture as `(bytes, mime)`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Id3 {
    pub tags: Vec<(String, String)>,
    pub picture: Option<(Vec<u8>, String)>,
}

fn key_of(id: &[u8]) -> Option<&'static str> {
    Some(match id {
        b"TIT2" | b"TT2" => "title",
        b"TPE1" | b"TP1" => "artist",
        b"TPE2" | b"TP2" => "albumartist",
        b"TALB" | b"TAL" => "album",
        b"TRCK" | b"TRK" => "tracknumber",
        b"TPOS" | b"TPA" => "discnumber",
        b"TDRC" | b"TYER" | b"TYE" | b"TDOR" | b"TORY" => "date",
        b"TCON" | b"TCO" => "genre",
        b"TCOM" | b"TCM" => "composer",
        _ => return None,
    })
}

fn synchsafe(b: &[u8]) -> usize {
    b.iter()
        .fold(0usize, |a, &x| (a << 7) | (x & 0x7F) as usize)
}

fn be(b: &[u8]) -> usize {
    b.iter().fold(0usize, |a, &x| (a << 8) | x as usize)
}

/// Undo unsynchronisation: `FF 00` → `FF`.
fn resync(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        out.push(data[i]);
        if data[i] == 0xFF && data.get(i + 1) == Some(&0) {
            i += 1;
        }
        i += 1;
    }
    out
}

fn utf16(b: &[u8], big: bool) -> String {
    let units: Vec<u16> = b
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| {
            if big {
                u16::from_be_bytes(c)
            } else {
                u16::from_le_bytes(c)
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// Text in one of the four ID3 encodings.
fn decode(enc: u8, b: &[u8]) -> String {
    match enc {
        0 => b.iter().map(|&c| c as char).collect(),
        1 => match b {
            [0xFF, 0xFE, rest @ ..] => utf16(rest, false),
            [0xFE, 0xFF, rest @ ..] => utf16(rest, true),
            _ => utf16(b, false),
        },
        2 => utf16(b, true),
        _ => String::from_utf8_lossy(b).into_owned(),
    }
}

/// Splits `b` at the first terminator of the encoding (one zero byte, or
/// two aligned ones in UTF-16): (text, rest).
fn split_text(enc: u8, b: &[u8]) -> (&[u8], &[u8]) {
    if matches!(enc, 1 | 2) {
        let mut i = 0;
        while i + 1 < b.len() {
            if b[i] == 0 && b[i + 1] == 0 {
                return (&b[..i], &b[i + 2..]);
            }
            i += 2;
        }
        (b, &[])
    } else {
        match b.iter().position(|&c| c == 0) {
            Some(i) => (&b[..i], &b[i + 1..]),
            None => (b, &[]),
        }
    }
}

fn text_frame(data: &[u8]) -> Option<String> {
    let (&enc, mut rest) = data.split_first()?;
    let mut parts = Vec::new();
    while !rest.is_empty() {
        let (t, r) = split_text(enc, rest);
        let s = decode(enc, t).trim().to_string();
        if !s.is_empty() {
            parts.push(s);
        }
        rest = r;
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// "(17)Rock" → "Rock"; a bare "(17)" is dropped.
fn genre(v: String) -> Option<String> {
    let mut s = v.as_str();
    while let Some(rest) = s.strip_prefix('(') {
        match rest.split_once(')') {
            Some((n, tail)) if n.chars().all(|c| c.is_ascii_digit()) => s = tail,
            _ => break,
        }
    }
    let s = s.trim();
    (!s.is_empty() && !s.chars().all(|c| c.is_ascii_digit())).then(|| s.to_string())
}

/// (picture type, bytes, mime) of an `APIC` (or v2.2 `PIC`) frame.
fn picture(data: &[u8], v22: bool) -> Option<(u8, Vec<u8>, String)> {
    let (&enc, rest) = data.split_first()?;
    let (mime, rest) = if v22 {
        let fmt = rest.get(..3)?;
        let mime = match &fmt.to_ascii_uppercase()[..] {
            b"PNG" => "image/png",
            _ => "image/jpeg",
        };
        (mime.to_string(), &rest[3..])
    } else {
        let (m, r) = split_text(0, rest);
        (String::from_utf8_lossy(m).to_ascii_lowercase(), r)
    };
    let (&kind, rest) = rest.split_first()?;
    let (_, body) = split_text(enc, rest);
    if body.is_empty() {
        return None;
    }
    let mime = match mime.as_str() {
        "" | "image/jpg" => "image/jpeg".to_string(),
        m if !m.contains('/') => format!("image/{m}"),
        _ => mime,
    };
    Some((kind, body.to_vec(), mime))
}

/// Parse an ID3v2 tag starting at `bytes[0]`. `None` when it is not one.
pub fn parse(bytes: &[u8]) -> Option<Id3> {
    if bytes.len() < 10 || &bytes[..3] != b"ID3" {
        return None;
    }
    let major = bytes[3];
    if !(2..=4).contains(&major) {
        return None;
    }
    let flags = bytes[5];
    let size = synchsafe(&bytes[6..10]);
    let body = bytes.get(10..10 + size).unwrap_or(&bytes[10..]);
    let body = if flags & 0x80 != 0 && major < 4 {
        resync(body)
    } else {
        body.to_vec()
    };
    let mut at = 0usize;
    if flags & 0x40 != 0 && major > 2 {
        at = match major {
            3 => 4 + be(body.get(..4)?),
            _ => synchsafe(body.get(..4)?),
        };
    }

    let mut out = Id3::default();
    let mut pics: Vec<(u8, Vec<u8>, String)> = Vec::new();
    let (id_len, head) = if major == 2 { (3, 6) } else { (4, 10) };
    while at + head <= body.len() {
        let h = &body[at..at + head];
        if h[0] == 0 {
            break; // padding
        }
        let id = &h[..id_len];
        let size = match major {
            2 => be(&h[3..6]),
            3 => be(&h[4..8]),
            _ => synchsafe(&h[4..8]),
        };
        let fflags = if major == 2 { 0 } else { be(&h[8..10]) };
        at += head;
        let Some(raw) = body.get(at..at + size) else {
            break;
        };
        at += size;
        let mut data = raw.to_vec();
        match major {
            3 if fflags & 0x00C0 != 0 => continue,
            4 => {
                if fflags & 0x000C != 0 {
                    continue;
                }
                if fflags & 0x0002 != 0 {
                    data = resync(&data);
                }
                if fflags & 0x0001 != 0 {
                    data = data.get(4..).unwrap_or_default().to_vec();
                }
            }
            _ => {}
        }
        if id == b"APIC" || id == b"PIC" {
            if let Some(p) = picture(&data, major == 2) {
                pics.push(p);
            }
            continue;
        }
        let Some(key) = key_of(id) else { continue };
        let Some(mut value) = text_frame(&data) else {
            continue;
        };
        if key == "genre" {
            match genre(value) {
                Some(g) => value = g,
                None => continue,
            }
        }
        if !out.tags.iter().any(|(k, _)| k == key) {
            out.tags.push((key.to_string(), value));
        }
    }
    // Front cover first, then anything but "other", then the first one.
    let pick = pics
        .iter()
        .position(|p| p.0 == 3)
        .or_else(|| pics.iter().position(|p| p.0 != 0))
        .or((!pics.is_empty()).then_some(0));
    out.picture = pick.map(|i| {
        let (_, b, m) = pics.swap_remove(i);
        (b, m)
    });
    Some(out)
}

/// Builds an ID3v2.3 tag (test fixtures and the demo generator).
#[doc(hidden)]
pub fn build(tags: &[(&str, &str)], picture: Option<(&[u8], &str)>) -> Vec<u8> {
    let mut frames = Vec::new();
    let mut frame = |id: &[u8], data: &[u8]| {
        frames.extend_from_slice(id);
        frames.extend_from_slice(&(data.len() as u32).to_be_bytes());
        frames.extend_from_slice(&[0, 0]);
        frames.extend_from_slice(data);
    };
    for (key, value) in tags {
        let id: &[u8] = match *key {
            "title" => b"TIT2",
            "artist" => b"TPE1",
            "albumartist" => b"TPE2",
            "album" => b"TALB",
            "tracknumber" => b"TRCK",
            "discnumber" => b"TPOS",
            "date" => b"TYER",
            "genre" => b"TCON",
            "composer" => b"TCOM",
            _ => continue,
        };
        // UTF-16 with a BOM, as most taggers write v2.3.
        let mut data = vec![1u8, 0xFF, 0xFE];
        for u in value.encode_utf16() {
            data.extend_from_slice(&u.to_le_bytes());
        }
        frame(id, &data);
    }
    if let Some((bytes, mime)) = picture {
        let mut data = vec![0u8];
        data.extend_from_slice(mime.as_bytes());
        data.extend_from_slice(&[0, 3, 0]);
        data.extend_from_slice(bytes);
        frame(b"APIC", &data);
    }
    let size = frames.len();
    let mut out = b"ID3\x03\x00\x00".to_vec();
    for shift in [21, 14, 7, 0] {
        out.push(((size >> shift) & 0x7F) as u8);
    }
    out.extend_from_slice(&frames);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_v23() {
        let tag = build(
            &[
                ("title", "Sonate für Klavier"),
                ("artist", "Ensemble"),
                ("tracknumber", "3/12"),
                ("genre", "(32)Classical"),
            ],
            Some((b"\x89PNGdata", "image/png")),
        );
        let id3 = parse(&tag).unwrap();
        assert_eq!(
            id3.tags,
            [
                ("title".to_string(), "Sonate für Klavier".to_string()),
                ("artist".into(), "Ensemble".into()),
                ("tracknumber".into(), "3/12".into()),
                ("genre".into(), "Classical".into()),
            ]
        );
        assert_eq!(
            id3.picture,
            Some((b"\x89PNGdata".to_vec(), "image/png".into()))
        );
    }

    #[test]
    fn v24_utf8_and_v22() {
        // v2.4, UTF-8 text, two values.
        let frame = b"\x03Bach\x00Busoni";
        let mut tag = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
        let mut body = b"TPE1".to_vec();
        body.extend_from_slice(&[0, 0, 0, frame.len() as u8, 0, 0]);
        body.extend_from_slice(frame);
        body.extend_from_slice(&[0; 8]);
        tag[9] = body.len() as u8;
        tag.extend_from_slice(&body);
        let id3 = parse(&tag).unwrap();
        assert_eq!(id3.tags, [("artist".to_string(), "Bach; Busoni".into())]);

        let mut v22 = b"ID3\x02\x00\x00\x00\x00\x00\x00".to_vec();
        let body = b"TT2\x00\x00\x06\x00Title";
        v22[9] = body.len() as u8;
        v22.extend_from_slice(body);
        assert_eq!(parse(&v22).unwrap().tags[0].1, "Title");
        assert!(parse(b"not a tag at all").is_none());
    }

    #[test]
    fn truncated_tags_do_not_panic() {
        let tag = build(&[("title", "x"), ("album", "y")], None);
        for n in 0..tag.len() {
            let _ = parse(&tag[..n]);
        }
    }
}
