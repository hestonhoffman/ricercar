use std::path::PathBuf;
use std::time::Instant;

use super::fixture::{FRAME, Spec, dst_escape_frame, image, plain_frame};
use super::*;

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
    Spec::new(dst, 2, &[(5, "First"), (4, "Second")])
}

#[test]
fn parses_disc_and_tracks() {
    let p = scratch(&image(&spec(false), plain_frame));
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
    let p = scratch(&image(&spec(false), plain_frame));
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
    let p = scratch(&image(&spec(false), frame));
    let mut s = SacdImage::open(p.path()).unwrap().track_source(1).unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(s.read(&mut buf).unwrap(), 8);
    assert_eq!(buf, [0x80, 0x01, 0x80, 0x01, 0x80, 0x01, 0x80, 0x01]);
}

#[test]
fn seeks_to_the_right_frame() {
    let p = scratch(&image(&spec(false), plain_frame));
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
    let p = scratch(&image(&spec(true), dst_escape_frame));
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
    let spec = Spec::new(true, 1, &[(n * 4, "One")]);
    let p = scratch(&image(&spec, |t| pairs[(t % n) as usize].0.clone()));
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
