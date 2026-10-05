//! Tracks of a SACD image through the engine and the file sink: the same
//! output as the same DSD bytes in a DSF file, DoP bit for bit, seeks
//! inside a track and consecutive tracks back to back. The images are
//! synthetic (`dsd::sacd::fixture`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ricercar_audio::dsd::sacd::fixture::{self, FRAME, Spec};
use ricercar_audio::dsd::{DsdFormat, DsdMode};
use ricercar_audio::player::{EngineEvent, PlayerHandle, TransportStatus};
use ricercar_audio::{PcmFormat, TrackSource};

const DSD64: DsdFormat = DsdFormat {
    rate: 2_822_400,
    channels: 2,
};

/// Bytes of one frame of 1/75 s, both channels.
const FRAME_BYTES: usize = FRAME * 2;

/// A synthetic disc: a 2-frame pregap, then tracks of 30 and 25 frames of
/// a sigma-delta sine.
struct Disc {
    path: PathBuf,
    /// Interleaved DSD bytes of the whole area.
    data: Vec<u8>,
    spec: Spec,
}

impl Disc {
    fn new(tag: &str) -> Disc {
        let spec = Spec::new(false, 2, &[(30, "First"), (25, "Second")]);
        let secs = spec.frames() as f64 / 75.0 + 0.01;
        let mut data = ricercar_audio::dsd::fixture::sine(DSD64, 1000.0, 0.25, secs);
        data.truncate(spec.frames() as usize * FRAME_BYTES);
        let img = fixture::image(&spec, |t| {
            data[t as usize * FRAME_BYTES..(t as usize + 1) * FRAME_BYTES].to_vec()
        });
        let path = tmp(&format!("{tag}.iso"));
        std::fs::write(&path, img).unwrap();
        Disc { path, data, spec }
    }

    fn uri(&self, n: u32) -> String {
        format!("file://{}#sacd={n}", self.path.display())
    }

    /// The DSD bytes of tracks `range` (1-based, inclusive).
    fn bytes(&self, first: u32, last: u32) -> &[u8] {
        let start: u32 = self.spec.pregap
            + self.spec.tracks[..first as usize - 1]
                .iter()
                .map(|t| t.0)
                .sum::<u32>();
        let len: u32 = self.spec.tracks[first as usize - 1..last as usize]
            .iter()
            .map(|t| t.0)
            .sum();
        &self.data[start as usize * FRAME_BYTES..(start + len) as usize * FRAME_BYTES]
    }
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ricercar-sacd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    let _ = std::fs::remove_file(&p);
    p
}

fn file_uri(p: &Path) -> String {
    format!("file://{}", p.display())
}

fn file_player(tag: &str, formats: &str) -> (PlayerHandle, PathBuf) {
    let out = tmp(&format!("{tag}.raw"));
    let spec = format!("file:{}?formats={formats}", out.display());
    (ricercar_audio::player::spawn_player(&spec), out)
}

fn wait_for_start(sub: &ricercar_audio::Subscriber) -> Option<PcmFormat> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match sub.0.recv_timeout(Duration::from_millis(100)) {
            Ok(EngineEvent::TrackStarted { format, .. }) => return format,
            Ok(EngineEvent::Error { message, .. }) => panic!("engine error: {message}"),
            _ => {}
        }
    }
    panic!("track did not start");
}

fn wait_stopped(sub: &ricercar_audio::Subscriber) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match sub.0.recv_timeout(Duration::from_millis(100)) {
            Ok(EngineEvent::Status {
                status: TransportStatus::Stopped,
            }) => return,
            Ok(EngineEvent::Error { message, .. }) => panic!("engine error: {message}"),
            _ => {}
        }
    }
    panic!("playback did not finish");
}

/// What the file sink holds after playing `uris` back to back.
fn play(tag: &str, mode: DsdMode, uris: &[String]) -> Vec<u8> {
    let (h, out) = file_player(tag, "S24_3LE");
    let sub = h.subscribe();
    h.set_dsd_mode(mode);
    h.load(uris[0].clone());
    for u in &uris[1..] {
        h.enqueue_next(u.clone());
    }
    wait_stopped(&sub);
    std::fs::read(out).unwrap()
}

/// DoP frames for `data` (S24_3LE), built independently of the player.
fn dop_reference(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut marker = false;
    for frame in data.chunks(4) {
        let m = if marker { 0xFA } else { 0x05 };
        out.extend_from_slice(&[frame[2], frame[0], m]);
        out.extend_from_slice(&[frame[3], frame[1], m]);
        marker = !marker;
    }
    out
}

#[test]
fn pcm_track_matches_the_same_bytes_in_a_dsf() {
    let disc = Disc::new("pcm");
    let dsf = tmp("pcm-ref.dsf");
    std::fs::write(
        &dsf,
        ricercar_audio::dsd::fixture::dsf(DSD64, disc.bytes(2, 2), &[]),
    )
    .unwrap();
    let got = play("pcm-sacd", DsdMode::Pcm, &[disc.uri(2)]);
    let want = play("pcm-dsf", DsdMode::Pcm, &[file_uri(&dsf)]);
    // 25 frames of 1/75 s at 88.2 kHz, stereo, 3 bytes.
    assert_eq!(got.len(), 25 * 1176 * 2 * 3);
    assert!(got == want, "converted PCM differs from the DSF's");
}

#[test]
fn dop_track_is_bit_exact_at_unity_gain() {
    let disc = Disc::new("dop");
    let (h, out) = file_player("dop-sacd", "S24_3LE");
    let sub = h.subscribe();
    h.set_dsd_mode(DsdMode::Dop);
    h.set_volume(30);
    h.set_gain(-3.0);
    h.load(disc.uri(1));
    assert_eq!(
        wait_for_start(&sub),
        Some(PcmFormat {
            sample_rate: 176_400,
            channels: 2,
            bits: 24
        })
    );
    {
        let st = h.state.lock().unwrap();
        assert_eq!(st.dur_ms, Some(400));
        let d = st.chain.dsd.unwrap();
        assert_eq!((d.source, d.mode), (DSD64, DsdMode::Dop));
        assert!(d.gain_locked() && st.chain.bit_perfect);
    }
    wait_stopped(&sub);
    let got = std::fs::read(out).unwrap();
    assert!(got == dop_reference(disc.bytes(1, 1)), "DoP stream differs");
}

#[test]
fn seeks_inside_a_track() {
    let disc = Disc::new("seek");
    let bytes = disc.bytes(2, 2);
    let mut src = TrackSource::open_with(&disc.uri(2), DsdMode::Dop).unwrap();
    assert_eq!(src.duration_ms, Some(333));
    let at = src.seek_ms(200).unwrap();
    assert!((at.as_secs_f64() - 0.2).abs() < 1e-4, "{at:?}");
    src.pump().unwrap();
    let mut q: VecDeque<i32> = std::mem::take(&mut src.pending);
    let off = 70_560 * 2; // 0.2 s, bytes of both channels
    let want = ((bytes[off] as i32) << 8) | bytes[off + 2] as i32;
    assert_eq!(q.pop_front(), Some(want));
    let want = ((bytes[off + 1] as i32) << 8) | bytes[off + 3] as i32;
    assert_eq!(q.pop_front(), Some(want));

    // Through the engine: position relative to the track, and what plays
    // from there is the rest of the track.
    let (h, out) = file_player("seek-engine", "S24_3LE");
    let sub = h.subscribe();
    h.set_dsd_mode(DsdMode::Dop);
    h.load(disc.uri(2));
    h.pause();
    wait_for_start(&sub);
    h.seek_ms(250);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "no position");
        if let Ok(EngineEvent::Position { pos_ms, .. }) =
            sub.0.recv_timeout(Duration::from_millis(100))
            && pos_ms > 0
        {
            assert!((245..=255).contains(&pos_ms), "{pos_ms}");
            break;
        }
    }
    h.resume();
    wait_stopped(&sub);
    let got = std::fs::read(out).unwrap();
    let rest = dop_reference(&bytes[88_200 * 2..]);
    assert!(got.len() >= rest.len());
    let tail = &got[got.len() - rest.len()..];
    // The marker phase is whatever it was; the payloads must match.
    let payload = |v: &[u8]| -> Vec<[u8; 2]> { v.chunks(3).map(|c| [c[0], c[1]]).collect() };
    assert!(
        payload(tail) == payload(&rest),
        "audio after the seek differs"
    );
}

#[test]
fn consecutive_tracks_play_back_to_back() {
    let disc = Disc::new("gapless");
    let uris = [disc.uri(1), disc.uri(2)];
    let both = disc.bytes(1, 2);

    let got = play("gapless-dop", DsdMode::Dop, &uris);
    assert!(got == dop_reference(both), "DoP across the track change");

    let dsf = tmp("gapless-ref.dsf");
    std::fs::write(&dsf, ricercar_audio::dsd::fixture::dsf(DSD64, both, &[])).unwrap();
    let got = play("gapless-pcm", DsdMode::Pcm, &uris);
    let want = play("gapless-pcm-ref", DsdMode::Pcm, &[file_uri(&dsf)]);
    assert_eq!(got.len(), 55 * 1176 * 2 * 3);
    assert!(got == want, "PCM across the track change");
}

#[test]
fn missing_tracks_and_images_are_errors() {
    let disc = Disc::new("missing");
    assert!(TrackSource::open(&disc.uri(3)).is_err());
    assert!(TrackSource::open("file:///nonexistent/disc.iso#sacd=1").is_err());
    let src = TrackSource::open(&disc.uri(1)).unwrap();
    // 30 frames converted to 88.2 kHz.
    assert_eq!(src.frames(), Some(30 * 1176));
    assert_eq!(src.dsd().map(|d| d.0), Some(DSD64));
}
