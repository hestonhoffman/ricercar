use std::path::PathBuf;
use std::time::Instant;

use super::*;

const FRAME: usize = 4704;

/// A frame's plain DSD bytes for two channels, interleaved.
fn plain_frame(time: u32) -> Vec<u8> {
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
fn dst_escape_frame(time: u32) -> Vec<u8> {
    let mut f = vec![0u8];
    f.extend(plain_frame(time));
    f
}

struct Spec {
    dst: bool,
    /// Frames before the first track.
    pregap: u32,
    /// Frames and title of each track.
    tracks: Vec<(u32, &'static str)>,
}

fn put(img: &mut [u8], lsn: usize, at: usize, bytes: &[u8]) {
    let p = lsn * SECTOR + at;
    img[p..p + bytes.len()].copy_from_slice(bytes);
}

fn put_time(img: &mut [u8], lsn: usize, at: usize, frames: u32) {
    let (m, s, f) = (frames / 4500, frames / 75 % 60, frames % 75);
    put(img, lsn, at, &[m as u8, s as u8, f as u8]);
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

/// A minimal image: master TOC and text, a stereo area TOC (track lists,
/// ISRCs, track text) and the audio sectors.
fn build(spec: &Spec, frame: impl Fn(u32) -> Vec<u8>) -> Vec<u8> {
    let total = spec.pregap + spec.tracks.iter().map(|t| t.0).sum::<u32>();
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
    put(&mut img, 510, 104, &[1, 0, 0, 14]);
    put(&mut img, 510, 120, &[0x07, 0xd1, 2, 3]);
    put(&mut img, 510, 128, &[1]);
    put(&mut img, 510, 136, b"en\x02");

    // Master text, ISO 8859-1.
    put(&mut img, 511, 0, b"SACDText");
    put(&mut img, 511, 16, &[0, 64, 0, 96, 0, 128]);
    put(&mut img, 511, 64, b"Caf\xe9 Album\0");
    put(&mut img, 511, 96, b"Some Ensemble\0");
    put(&mut img, 511, 128, b"Some Label\0");

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
    for (i, &(len, title)) in spec.tracks.iter().enumerate() {
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
        item.extend(title.as_bytes());
        item.push(0);
        item.resize(item.len().next_multiple_of(4), 0);
        item.extend([2, 0x20]);
        item.extend(b"Player\0\0");
        put(&mut img, a + 5, text, &item);
        text += item.len().next_multiple_of(4);
        first += len;
    }

    for (i, s) in sectors.iter().enumerate() {
        put(&mut img, AUDIO_START + i, 0, s);
    }
    img
}

/// The image in a temporary file, removed when dropped.
fn scratch(img: &[u8]) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, img).unwrap();
    f
}

fn read_all(s: &mut dyn DsdSource, chunk: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        let n = s.read(&mut buf).unwrap();
        if n == 0 {
            return out;
        }
        assert_eq!(n % 2, 0);
        out.extend_from_slice(&buf[..n]);
    }
}

fn expected(times: std::ops::Range<u32>) -> Vec<u8> {
    times.flat_map(plain_frame).collect()
}

fn spec(dst: bool) -> Spec {
    Spec {
        dst,
        pregap: 2,
        tracks: vec![(5, "First"), (4, "Second")],
    }
}

#[test]
fn parses_disc_and_tracks() {
    let p = scratch(&build(&spec(false), plain_frame));
    let img = SacdImage::open(p.path()).unwrap();
    let d = img.disc();
    assert_eq!(d.title.as_deref(), Some("Café Album"));
    assert_eq!(d.artist.as_deref(), Some("Some Ensemble"));
    assert_eq!(d.publisher.as_deref(), Some("Some Label"));
    assert_eq!(d.genre.as_deref(), Some("Jazz"));
    assert_eq!(d.date.as_deref(), Some("2001-02-03"));
    assert_eq!(d.catalog_number.as_deref(), Some("CAT 0001"));
    assert!(d.hybrid);
    assert_eq!(d.areas.len(), 1);
    let area = &d.areas[0];
    assert_eq!(area.kind, AreaKind::Stereo);
    assert_eq!(area.frame_format, FrameFormat::Dsd3In14);
    assert_eq!(
        area.format,
        DsdFormat {
            rate: 2_822_400,
            channels: 2
        }
    );
    assert_eq!(area.frames, 11);

    let t = img.tracks();
    assert_eq!(t.len(), 2);
    assert_eq!(t[0].number, 1);
    assert_eq!(t[0].title.as_deref(), Some("First"));
    assert_eq!(t[1].title.as_deref(), Some("Second"));
    assert_eq!(t[1].performer.as_deref(), Some("Player"));
    assert_eq!(t[1].isrc.as_deref(), Some("ZZX010100002"));
    assert_eq!((t[0].start_frame, t[0].frames), (2, 5));
    assert_eq!((t[1].start_frame, t[1].frames), (7, 4));
    assert_eq!(t[1].duration(), Duration::from_nanos(4_000_000_000 / 75));
    assert!(t[1].start_sector > t[0].start_sector);
}

#[test]
fn reads_tracks_and_tags() {
    let p = scratch(&build(&spec(false), plain_frame));
    let img = SacdImage::open(p.path()).unwrap();
    let mut s = img.track_source(1).unwrap();
    assert_eq!(s.len(), Some(5 * FRAME as u64));
    // Odd buffer: reads stay whole sample pairs.
    assert_eq!(read_all(s.as_mut(), 1001), expected(2..7));
    let mut s = img.track_source(2).unwrap();
    assert_eq!(read_all(s.as_mut(), 65536), expected(7..11));

    let tags = s.tags();
    let tag = |k: &str| tags.iter().find(|t| t.0 == k).map(|t| t.1.as_str());
    assert_eq!(tag("TITLE"), Some("Second"));
    assert_eq!(tag("ARTIST"), Some("Player"));
    assert_eq!(tag("ALBUM"), Some("Café Album"));
    assert_eq!(tag("TRACKNUMBER"), Some("2"));
    assert_eq!(tag("ISRC"), Some("ZZX010100002"));
    assert!(img.track_source(3).is_err());
}

#[test]
fn keeps_channel_and_bit_order() {
    // Left bytes 0x80 (earliest bit set), right bytes 0x01: the disc's
    // byte-interleaved MSB-first layout goes out unchanged.
    let frame = |_| [0x80u8, 0x01].repeat(FRAME);
    let p = scratch(&build(&spec(false), frame));
    let mut s = SacdImage::open(p.path()).unwrap().track_source(1).unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(s.read(&mut buf).unwrap(), 8);
    assert_eq!(buf, [0x80, 0x01, 0x80, 0x01, 0x80, 0x01, 0x80, 0x01]);
}

#[test]
fn seeks_to_the_right_frame() {
    let p = scratch(&build(&spec(false), plain_frame));
    let img = SacdImage::open(p.path()).unwrap();
    let all = expected(2..7);
    let mut s = img.track_source(1).unwrap();
    for pos in [
        2 * FRAME as u64 + 100,
        0,
        4 * FRAME as u64 + FRAME as u64 - 1,
        1,
    ] {
        s.seek(pos).unwrap();
        assert_eq!(
            read_all(s.as_mut(), 3000),
            all[pos as usize * 2..],
            "pos {pos}"
        );
    }
    s.seek(5 * FRAME as u64).unwrap();
    assert_eq!(read_all(s.as_mut(), 3000), Vec::<u8>::new());

    let mut s = img.track_source(2).unwrap();
    s.seek(FRAME as u64 * 3 + 7).unwrap();
    assert_eq!(
        read_all(s.as_mut(), 3000),
        expected(7..11)[(FRAME * 3 + 7) * 2..]
    );
}

#[test]
fn reads_dst_areas() {
    let p = scratch(&build(&spec(true), dst_escape_frame));
    let img = SacdImage::open(p.path()).unwrap();
    assert_eq!(img.disc().areas[0].frame_format, FrameFormat::Dst);
    let mut s = img.track_source(1).unwrap();
    assert_eq!(read_all(s.as_mut(), 4096), expected(2..7));
    s.seek(FRAME as u64 + 5).unwrap();
    assert_eq!(
        read_all(s.as_mut(), 4096),
        expected(2..7)[(FRAME + 5) * 2..]
    );
    let raw: Vec<_> = img.track_frames(2).unwrap().map(|f| f.unwrap()).collect();
    assert_eq!(raw, (7..11).map(dst_escape_frame).collect::<Vec<_>>());
}

#[test]
fn decodes_text_charsets() {
    assert_eq!(decode_text(b"Caf\xe9", 2).as_deref(), Some("Café"));
    assert_eq!(decode_text(b"Plain ", 1).as_deref(), Some("Plain"));
    assert_eq!(
        decode_text("Caf\u{e9}".as_bytes(), 3).as_deref(),
        Some("Café")
    );
    assert_eq!(decode_text(b"A\x82\xa0B", 3).as_deref(), Some("A\u{fffd}B"));
    assert_eq!(decode_text(b"", 2), None);
}

#[test]
fn rejects_other_files() {
    let p = scratch(&vec![0u8; 600 * SECTOR]);
    assert!(SacdImage::open(p.path()).is_err());
    let p = scratch(&[0u8; 100]);
    assert!(SacdImage::open(p.path()).is_err());
}

/// Real DST frames (`frame_NNN.dst` with the expected `frame_NNN.dsd`,
/// stereo DSD64) from the directory in RICERCAR_TEST_DST_FRAMES, packed
/// into a synthetic image.
#[test]
fn decodes_real_dst_frames() {
    let Ok(dir) = std::env::var("RICERCAR_TEST_DST_FRAMES") else {
        eprintln!("RICERCAR_TEST_DST_FRAMES unset, skipped");
        return;
    };
    let mut pairs = Vec::new();
    for i in 1.. {
        let base = PathBuf::from(&dir).join(format!("frame_{i:03}"));
        let (Ok(dst), Ok(dsd)) = (
            std::fs::read(base.with_extension("dst")),
            std::fs::read(base.with_extension("dsd")),
        ) else {
            break;
        };
        pairs.push((dst, dsd));
    }
    assert!(!pairs.is_empty());
    let n = pairs.len() as u32;
    let spec = Spec {
        dst: true,
        pregap: 1,
        tracks: vec![(n * 4, "One")],
    };
    let p = scratch(&build(&spec, |t| pairs[(t % n) as usize].0.clone()));
    let img = SacdImage::open(p.path()).unwrap();
    let want: Vec<u8> = (1..1 + n * 4)
        .flat_map(|t| pairs[(t % n) as usize].1.clone())
        .collect();
    let mut s = img.track_source(1).unwrap();
    assert_eq!(read_all(s.as_mut(), 8192), want);

    // Speed: 30 s of audio through the decoder.
    let mut dec = dst_decoder::decoder::DstDecoder::new(2, 2_822_400).unwrap();
    let mut out = vec![0u8; dec.dsd_frame_bytes()];
    let t = Instant::now();
    for i in 0..30 * 75 {
        dec.decode_frame(&pairs[i % pairs.len()].0, &mut out)
            .unwrap();
    }
    let secs = t.elapsed().as_secs_f64();
    eprintln!(
        "DST: 30 s decoded in {secs:.3} s, {:.1}x real time",
        30.0 / secs
    );
}

/// A real image, from RICERCAR_TEST_SACD_ISO.
#[test]
fn reads_a_real_image() {
    let Ok(path) = std::env::var("RICERCAR_TEST_SACD_ISO") else {
        eprintln!("RICERCAR_TEST_SACD_ISO unset, skipped");
        return;
    };
    let t = Instant::now();
    let img = SacdImage::open(&path).unwrap();
    eprintln!("opened in {:?}", t.elapsed());
    let area = img.stereo_area().expect("stereo area");
    eprintln!(
        "{:?}, {} Hz, {} channels, {} tracks, {:?}",
        area.frame_format,
        area.format.rate,
        area.format.channels,
        img.tracks().len(),
        area.duration()
    );
    assert!(img.tracks().len() >= 2);
    let second = (area.format.rate / 8) as usize;
    let ch = area.format.channels as usize;

    // The first seconds of track 1.
    let mut s = img.track_source(1).unwrap();
    assert_eq!(s.len(), Some(img.tracks()[0].frames as u64 * FRAME as u64));
    let mut buf = vec![0u8; 5 * second * ch];
    let mut got = 0;
    while got < buf.len() {
        let n = s.read(&mut buf[got..]).unwrap();
        assert!(n > 0);
        got += n;
    }
    assert!(buf.iter().any(|&b| b != buf[0]));

    // The end of track 1.
    let len = s.len().unwrap();
    s.seek(len - second as u64 / 2).unwrap();
    assert_eq!(read_all(s.as_mut(), 65536).len(), second / 2 * ch);

    // A seek into track 2 lands where reading through does.
    let mut s = img.track_source(2).unwrap();
    let pos = 10 * second + 123;
    let mut through = vec![0u8; (pos + second) * ch];
    let mut got = 0;
    while got < through.len() {
        got += s.read(&mut through[got..]).unwrap();
    }
    let t = Instant::now();
    s.seek(pos as u64).unwrap();
    let mut after = vec![0u8; second * ch];
    let mut got = 0;
    while got < after.len() {
        got += s.read(&mut after[got..]).unwrap();
    }
    eprintln!("seek and 1 s read in {:?}", t.elapsed());
    assert!(after == through[pos * ch..]);
}
