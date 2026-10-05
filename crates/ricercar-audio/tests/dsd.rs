//! DSD files end to end: readers, DoP / native / PCM output through the
//! file sink, the gain lock and the shared-device guard. Fixtures are
//! generated here (`dsd::fixture`), never real hardware.

use std::collections::VecDeque;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ricercar_audio::dsd::dff::{DffReader, FrameDecoder};
use ricercar_audio::dsd::dsf::DsfReader;
use ricercar_audio::dsd::play::DsdPlayback;
use ricercar_audio::dsd::{DsdFormat, DsdMode, DsdSource, SILENCE, fixture, id3};
use ricercar_audio::player::{EngineEvent, PlayerHandle, TransportStatus};
use ricercar_audio::sink::device_info;
use ricercar_audio::{AudioSink, Container, DeviceInfo, PcmFormat, TrackOpts};

const DSD64: DsdFormat = DsdFormat {
    rate: 2_822_400,
    channels: 2,
};

/// A known, non-symmetric pattern: byte i of channel c.
fn pattern(f: DsdFormat, len: usize) -> Vec<u8> {
    let ch = f.channels as usize;
    (0..len * ch)
        .map(|i| {
            let (n, c) = (i / ch, i % ch);
            (n as u32 * 37 + c as u32 * 101 + (n as u32 >> 8)) as u8
        })
        .collect()
}

fn read_all(src: &mut dyn DsdSource, chunk: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        let n = src.read(&mut buf).unwrap();
        if n == 0 {
            return out;
        }
        assert_eq!(n % src.format().channels as usize, 0);
        out.extend_from_slice(&buf[..n]);
    }
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ricercar-dsd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    let _ = std::fs::remove_file(&p);
    p
}

fn uri(p: &Path) -> String {
    format!("file://{}", p.display())
}

#[test]
fn dsf_reads_back_msb_first_and_interleaved() {
    let data = pattern(DSD64, 10_000); // two blocks and a bit
    let tag = id3::build(
        &[("title", "Étude"), ("artist", "Ensemble"), ("date", "2019")],
        None,
    );
    let file = fixture::dsf(DSD64, &data, &tag);
    let mut r = DsfReader::new(Cursor::new(file.clone())).unwrap();
    assert_eq!(r.format(), DSD64);
    assert_eq!(r.len(), Some(10_000));
    for chunk in [2, 7 * 2, 4096 * 2, 65_536] {
        r.seek(0).unwrap();
        assert_eq!(read_all(&mut r, chunk), data, "chunk {chunk}");
    }
    // The file stores the bits LSB first, and per-channel blocks.
    assert_eq!(file[92], data[0].reverse_bits());
    assert_eq!(file[92 + 4096], data[1].reverse_bits());
    r.seek(4095).unwrap();
    let mut b = [0u8; 4];
    assert_eq!(r.read(&mut b).unwrap(), 4);
    assert_eq!(b, data[4095 * 2..4097 * 2]);
    let tags = r.tags();
    assert!(tags.contains(&("title".into(), "Étude".into())));
    assert!(tags.contains(&("date".into(), "2019".into())));
    // Odd buffer sizes still return whole frames.
    r.seek(0).unwrap();
    let mut odd = [0u8; 5];
    assert_eq!(r.read(&mut odd).unwrap(), 4);
}

#[test]
fn dff_reads_back_with_diin_and_id3() {
    let data = pattern(DSD64, 3_001);
    let tag = id3::build(&[("album", "Nocturnes")], None);
    let file = fixture::dff(DSD64, &data, &tag, "From DIIN");
    let mut r = DffReader::new(Cursor::new(file)).unwrap();
    assert_eq!(r.format(), DSD64);
    assert_eq!(r.len(), Some(3_001));
    assert!(!r.is_dst());
    assert_eq!(read_all(&mut r, 1000), data);
    r.seek(3_000).unwrap();
    assert_eq!(read_all(&mut r, 64), data[6_000..]);
    let tags = r.tags();
    assert!(tags.contains(&("album".into(), "Nocturnes".into())));
    assert!(tags.contains(&("title".into(), "From DIIN".into())));
}

#[test]
fn more_than_two_channels_are_refused_clearly() {
    let six = DsdFormat {
        rate: 2_822_400,
        channels: 6,
    };
    let data = pattern(six, 100);
    let err = DffReader::new(Cursor::new(fixture::dff(six, &data, &[], "")))
        .err()
        .unwrap();
    assert!(err.to_string().contains("6 channels"), "{err}");
    let err = DsfReader::new(Cursor::new(fixture::dsf(six, &data, &[])))
        .err()
        .unwrap();
    assert!(err.to_string().contains("only mono and stereo"), "{err}");
    // The library can still read what the file is.
    assert!(DsfReader::new_unchecked(Cursor::new(fixture::dsf(six, &data, &[]))).is_ok());
}

/// DST with the real decoder. The frames use DST's escape for plain DSD
/// (valid DST, not arithmetic-coded): the decoder must give them back as is.
#[test]
fn dst_frames_decode_with_the_dst_decoder() {
    let frame = 4704; // bytes per channel in 1/75 s of DSD64
    let data = pattern(DSD64, frame * 3 + 100);
    let file = fixture::dff_dst(DSD64, &data, &[], fixture::dst_plain_frame);
    let mut r = DffReader::new(Cursor::new(file)).unwrap();
    assert!(r.is_dst());
    assert_eq!(r.len(), Some(frame as u64 * 4));
    let got = read_all(&mut r, 3000);
    assert_eq!(got[..data.len()], data[..]);
    assert!(got[data.len()..].iter().all(|&b| b == SILENCE));
    // Seek into the third frame.
    r.seek(frame as u64 * 2 + 5).unwrap();
    let mut b = [0u8; 2];
    r.read(&mut b).unwrap();
    assert_eq!(b, data[(frame * 2 + 5) * 2..][..2]);
}

/// The DST chunk plumbing with a stand-in decoder: each `DSTF` payload is
/// handed over whole, in order, once per frame.
#[test]
fn dst_chunk_plumbing_with_a_mock_decoder() {
    struct Mock(Arc<Mutex<Vec<Vec<u8>>>>);
    impl FrameDecoder for Mock {
        fn decode(&mut self, frame: &[u8], out: &mut [u8]) -> std::io::Result<()> {
            self.0.lock().unwrap().push(frame.to_vec());
            out.fill(frame[0]);
            Ok(())
        }
    }
    let frame = 4704;
    let data = vec![0u8; frame * 2 * 3];
    let n = std::sync::atomic::AtomicU8::new(0);
    // "Compressed" frames: three bytes naming the frame.
    let file = fixture::dff_dst(DSD64, &data, &[], |_| {
        let i = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        vec![i, i, i]
    });
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut r = DffReader::with_decoder(Cursor::new(file), Box::new(Mock(seen.clone()))).unwrap();
    let got = read_all(&mut r, 1000);
    assert_eq!(got.len(), frame * 2 * 3);
    assert_eq!(got[0], 1);
    assert_eq!(got[frame * 2], 2);
    assert_eq!(got[frame * 4 + 7], 3);
    assert_eq!(
        *seen.lock().unwrap(),
        [vec![1, 1, 1], vec![2, 2, 2], vec![3, 3, 3]]
    );
}

#[test]
fn damaged_files_never_panic() {
    let data = pattern(DSD64, 5000);
    let tag = id3::build(&[("title", "x")], Some((b"img", "image/png")));
    for file in [
        fixture::dsf(DSD64, &data, &tag),
        fixture::dff(DSD64, &data, &tag, "t"),
        fixture::dff_dst(DSD64, &data, &[], fixture::dst_plain_frame),
    ] {
        for cut in (0..file.len()).step_by(97).chain([file.len() - 1]) {
            let bytes = file[..cut].to_vec();
            let open: std::io::Result<Box<dyn DsdSource>> = if &file[..4] == b"DSD " {
                DsfReader::new(Cursor::new(bytes)).map(|r| Box::new(r) as _)
            } else {
                DffReader::new(Cursor::new(bytes)).map(|r| Box::new(r) as _)
            };
            if let Ok(mut r) = open {
                let mut buf = vec![0u8; 4096];
                while let Ok(n) = r.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                }
                let _ = r.tags();
            }
        }
    }
}

// ---------------------------------------------------------------- PCM

/// Amplitude of the `freq` component (single-bin DFT, Hann window).
fn tone_level(x: &[f64], rate: f64, freq: f64) -> f64 {
    let n = x.len() as f64;
    let (mut re, mut im) = (0.0, 0.0);
    for (i, v) in x.iter().enumerate() {
        let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n).cos();
        let a = 2.0 * std::f64::consts::PI * freq * i as f64 / rate;
        re += v * w * a.cos();
        im -= v * w * a.sin();
    }
    // Hann window: coherent gain 0.5.
    2.0 * (re * re + im * im).sqrt() / (n * 0.5)
}

#[test]
fn pcm_conversion_of_a_sigma_delta_sine() {
    let amp = 0.5; // SACD 0 dB: expected at −6 dBFS
    let data = fixture::sine(DSD64, 1000.0, amp, 0.5);
    let file = fixture::dsf(DSD64, &data, &[]);
    let src = DsfReader::new(Cursor::new(file)).unwrap();
    let mut p = DsdPlayback::new(Box::new(src), DsdMode::Pcm, None).unwrap();
    let fmt = p.pcm_format();
    assert_eq!(
        fmt,
        PcmFormat {
            sample_rate: 88_200,
            channels: 2,
            bits: 24
        }
    );
    let mut q = VecDeque::new();
    while p.pump(&mut q).unwrap() {}
    let left: Vec<f64> = q
        .iter()
        .step_by(2)
        .skip(2000) // past the filters' start
        .map(|&v| v as f64 / (1 << 23) as f64)
        .collect();
    let rate = 88_200.0;
    let one_k = tone_level(&left, rate, 1000.0);
    let db = 20.0 * (one_k / amp).log10();
    assert!(db.abs() < 1.0, "1 kHz at {db:.2} dB from the source level");
    for f in [200.0, 3000.0, 7000.0, 15000.0, 19000.0] {
        let other = tone_level(&left, rate, f);
        assert!(
            other < one_k * 1e-3,
            "{f} Hz at {:.1} dB below the tone",
            20.0 * (other / one_k).log10()
        );
    }
    // Both channels carry the same tone.
    let right: Vec<i32> = q.iter().skip(1).step_by(2).copied().collect();
    let left_i: Vec<i32> = q.iter().step_by(2).copied().collect();
    assert_eq!(left_i, right);
}

// ---------------------------------------------------------------- player

fn write_fixture(name: &str, file: &[u8]) -> PathBuf {
    let p = tmp(name);
    std::fs::write(&p, file).unwrap();
    p
}

fn wait_stopped(sub: &ricercar_audio::Subscriber, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
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

fn file_player(tag: &str, formats: &str) -> (PlayerHandle, PathBuf) {
    let out = tmp(&format!("{tag}.raw"));
    let spec = format!("file:{}?formats={formats}", out.display());
    (ricercar_audio::player::spawn_player(&spec), out)
}

/// DoP frames for `data`, built independently of the player.
fn dop_reference(data: &[u8], ch: usize, bytes_per_sample: usize, first: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut marker = first;
    for frame in data.chunks(2 * ch) {
        for c in 0..ch {
            let m = if marker { 0xFA } else { 0x05 };
            let (hi, lo) = (frame[c], frame[ch + c]);
            match bytes_per_sample {
                3 => out.extend_from_slice(&[lo, hi, m]),
                _ => out.extend_from_slice(&[0, lo, hi, m]),
            }
        }
        marker = !marker;
    }
    out
}

#[test]
fn dop_reaches_the_file_sink_bit_exact() {
    let data = pattern(DSD64, 30_000);
    let path = write_fixture("dop.dsf", &fixture::dsf(DSD64, &data, &[]));
    for (formats, bytes, label) in [("S24_3LE", 3, "S24_3LE"), ("S32_LE", 4, "S32_LE")] {
        let (h, out) = file_player(&format!("dop-{formats}"), formats);
        let sub = h.subscribe();
        h.set_dsd_mode(DsdMode::Dop);
        // Volume, mute and gain must not touch DoP.
        h.set_volume(40);
        h.set_gain(-3.0);
        h.load_with(
            uri(&path),
            TrackOpts {
                gain_db: Some(-6.0),
                peak: Some(0.5),
            },
        );
        let started = wait_for_start(&sub);
        assert_eq!(
            started,
            Some(PcmFormat {
                sample_rate: 176_400,
                channels: 2,
                bits: 24
            })
        );
        {
            let st = h.state.lock().unwrap();
            assert!(st.chain.bit_perfect, "DoP is bit-perfect at any volume");
            assert_eq!(st.chain.container, Some(label));
            let d = st.chain.dsd.unwrap();
            assert_eq!((d.source, d.mode), (DSD64, DsdMode::Dop));
            assert!(d.gain_locked());
        }
        wait_stopped(&sub, 20);
        let got = std::fs::read(&out).unwrap();
        let want = dop_reference(&data, 2, bytes, false);
        assert_eq!(got.len(), want.len(), "{formats}");
        assert!(got == want, "{formats}: DoP stream differs");
    }
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

#[test]
fn gapless_dop_keeps_the_markers_alternating() {
    // An odd number of DoP frames in the first track.
    let a = pattern(DSD64, 2 * 4001);
    let b: Vec<u8> = pattern(DSD64, 5000).iter().map(|x| !x).collect();
    let pa = write_fixture("gap-a.dff", &fixture::dff(DSD64, &a, &[], ""));
    let pb = write_fixture("gap-b.dsf", &fixture::dsf(DSD64, &b, &[]));
    let (h, out) = file_player("dop-gapless", "S24_3LE");
    let sub = h.subscribe();
    h.set_dsd_mode(DsdMode::Dop);
    h.load(uri(&pa));
    h.enqueue_next(uri(&pb));
    wait_stopped(&sub, 20);
    let got = std::fs::read(&out).unwrap();
    let mut want = dop_reference(&a, 2, 3, false);
    want.extend(dop_reference(&b, 2, 3, true));
    assert!(got == want, "gapless DoP differs");
}

#[test]
fn native_dsd_reaches_the_file_sink_in_each_format() {
    let data = pattern(DSD64, 12_000);
    let path = write_fixture("native.dsf", &fixture::dsf(DSD64, &data, &[]));
    let words: Vec<[u8; 8]> = data
        .chunks(8)
        .map(|f| {
            // L: f[0], f[2], f[4], f[6]; R: f[1], f[3], f[5], f[7]
            [f[0], f[2], f[4], f[6], f[1], f[3], f[5], f[7]]
        })
        .collect();
    for (formats, rate) in [
        ("DSD_U8", 352_800),
        ("DSD_U16_LE", 176_400),
        ("DSD_U16_BE", 176_400),
        ("DSD_U32_LE", 88_200),
        ("DSD_U32_BE", 88_200),
    ] {
        let (h, out) = file_player(&format!("native-{formats}"), formats);
        let sub = h.subscribe();
        h.set_dsd_mode(DsdMode::Native);
        h.set_mute(true);
        h.load(uri(&path));
        wait_for_start(&sub);
        {
            let st = h.state.lock().unwrap();
            assert_eq!(st.chain.container, Some(formats));
            assert!(st.chain.bit_perfect);
            // The engine counts 32-bit words: 88.2 kHz whatever the format.
            assert_eq!(st.chain.format.unwrap().sample_rate, 88_200);
        }
        wait_stopped(&sub, 20);
        let got = std::fs::read(&out).unwrap();
        let mut want = Vec::new();
        for w in &words {
            let (l, r) = (&w[..4], &w[4..]);
            match formats {
                "DSD_U8" => (0..4).for_each(|k| want.extend_from_slice(&[l[k], r[k]])),
                "DSD_U16_BE" => (0..2).for_each(|k| {
                    want.extend_from_slice(&[l[2 * k], l[2 * k + 1], r[2 * k], r[2 * k + 1]])
                }),
                "DSD_U16_LE" => (0..2).for_each(|k| {
                    want.extend_from_slice(&[l[2 * k + 1], l[2 * k], r[2 * k + 1], r[2 * k]])
                }),
                "DSD_U32_BE" => {
                    want.extend_from_slice(l);
                    want.extend_from_slice(r);
                }
                _ => {
                    want.extend(l.iter().rev());
                    want.extend(r.iter().rev());
                }
            }
        }
        assert!(got == want, "{formats} differs");
        let _ = rate;
    }
}

#[test]
fn native_without_dsd_formats_is_refused() {
    let data = pattern(DSD64, 4096);
    let path = write_fixture("refuse.dsf", &fixture::dsf(DSD64, &data, &[]));
    let (h, _out) = file_player("native-refused", "S24_3LE");
    let sub = h.subscribe();
    h.set_dsd_mode(DsdMode::Native);
    h.load(uri(&path));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no refusal");
        if let Ok(EngineEvent::Error { message, .. }) =
            sub.0.recv_timeout(Duration::from_millis(100))
        {
            assert!(message.contains("native DSD"), "{message}");
            break;
        }
    }
}

#[test]
fn pcm_mode_is_not_bit_perfect_and_follows_the_volume() {
    let data = fixture::sine(DSD64, 1000.0, 0.5, 0.2);
    let path = write_fixture("pcm.dsf", &fixture::dsf(DSD64, &data, &[]));
    let (h, out) = file_player("pcm", "S24_3LE");
    let sub = h.subscribe();
    h.load(uri(&path));
    assert_eq!(wait_for_start(&sub).unwrap().sample_rate, 88_200);
    {
        let st = h.state.lock().unwrap();
        assert!(!st.chain.bit_perfect, "conversion is never bit-perfect");
        assert_eq!(st.chain.dsd.unwrap().mode, DsdMode::Pcm);
        assert!(!st.chain.dsd.unwrap().gain_locked());
    }
    wait_stopped(&sub, 20);
    let got = std::fs::read(&out).unwrap();
    // 0.2 s at 88.2 kHz, stereo, 3 bytes.
    assert_eq!(got.len(), 17_640 * 2 * 3);
}

/// Converted PCM falls back to 44.1 kHz on a DAC that refuses 88.2 kHz.
#[test]
fn pcm_mode_steps_down_when_88k_is_refused() {
    struct Picky {
        dev: DeviceInfo,
        fmt: Option<PcmFormat>,
    }
    impl AudioSink for Picky {
        fn device(&self) -> &DeviceInfo {
            &self.dev
        }
        fn opened_format(&self) -> Option<PcmFormat> {
            self.fmt
        }
        fn opened_container(&self) -> Option<Container> {
            self.fmt.map(|_| Container::S24_3)
        }
        fn open(&mut self, fmt: PcmFormat) -> ricercar_audio::Result<()> {
            if fmt.sample_rate != 44_100 {
                return Err(ricercar_audio::AudioError::FormatRefused {
                    device: self.dev.name.clone(),
                    detail: format!("{} Hz", fmt.sample_rate),
                });
            }
            self.fmt = Some(fmt);
            Ok(())
        }
        fn write_i32(&mut self, _: &[i32]) -> ricercar_audio::Result<()> {
            Ok(())
        }
        fn drain(&mut self) -> ricercar_audio::Result<()> {
            Ok(())
        }
        fn close(&mut self) {
            self.fmt = None;
        }
    }
    let data = pattern(DSD64, 20_000);
    let path = write_fixture("picky.dsf", &fixture::dsf(DSD64, &data, &[]));
    let h = ricercar_audio::spawn_player_with_sink(Box::new(Picky {
        dev: device_info("hw:9,0"),
        fmt: None,
    }));
    let sub = h.subscribe();
    h.load(uri(&path));
    // The first format offered was refused, quietly.
    wait_stopped(&sub, 20);
    assert_eq!(
        h.state.lock().unwrap().chain.format.map(|f| f.sample_rate),
        Some(44_100)
    );
}

/// Records what a shared device (PipeWire) would receive.
struct Shared {
    dev: DeviceInfo,
    fmt: Option<PcmFormat>,
    opened: Arc<Mutex<Vec<(PcmFormat, bool)>>>,
}

impl AudioSink for Shared {
    fn device(&self) -> &DeviceInfo {
        &self.dev
    }
    fn opened_format(&self) -> Option<PcmFormat> {
        self.fmt
    }
    fn opened_container(&self) -> Option<Container> {
        self.fmt.map(|_| Container::S32)
    }
    fn open(&mut self, fmt: PcmFormat) -> ricercar_audio::Result<()> {
        self.opened.lock().unwrap().push((fmt, false));
        self.fmt = Some(fmt);
        Ok(())
    }
    fn open_dsd(&mut self, fmt: PcmFormat) -> ricercar_audio::Result<()> {
        self.opened.lock().unwrap().push((fmt, true));
        self.fmt = Some(fmt);
        Ok(())
    }
    fn write_i32(&mut self, _: &[i32]) -> ricercar_audio::Result<()> {
        Ok(())
    }
    fn drain(&mut self) -> ricercar_audio::Result<()> {
        Ok(())
    }
    fn close(&mut self) {
        self.fmt = None;
    }
}

#[test]
fn dop_and_native_never_reach_a_shared_device() {
    let data = pattern(DSD64, 8192);
    let path = write_fixture("shared.dsf", &fixture::dsf(DSD64, &data, &[]));
    for name in ["pipewire", "default", "plughw:1,0"] {
        for mode in [DsdMode::Dop, DsdMode::Native] {
            let opened = Arc::new(Mutex::new(Vec::new()));
            let h = ricercar_audio::spawn_player_with_sink(Box::new(Shared {
                dev: device_info(name),
                fmt: None,
                opened: opened.clone(),
            }));
            let sub = h.subscribe();
            h.set_dsd_mode(mode);
            h.load(uri(&path));
            wait_stopped(&sub, 20);
            let o = opened.lock().unwrap();
            assert!(!o.is_empty());
            for (fmt, dsd) in o.iter() {
                assert!(!dsd, "{name}: native DSD opened");
                assert_eq!(
                    fmt.sample_rate, 88_200,
                    "{name} {mode:?}: not converted PCM"
                );
            }
            let st = h.state.lock().unwrap();
            assert_eq!(st.chain.dsd.map(|d| d.mode), Some(DsdMode::Pcm), "{name}");
            assert!(!st.chain.bit_perfect);
        }
    }
}

#[test]
fn switching_between_dsd_and_pcm_reopens_the_device() {
    let data = pattern(DSD64, 4096 * 4);
    let pa = write_fixture("switch-a.dsf", &fixture::dsf(DSD64, &data, &[]));
    let pb = write_fixture("switch-b.dsf", &fixture::dsf(DSD64, &data, &[]));
    let opened = Arc::new(Mutex::new(Vec::new()));
    let h = ricercar_audio::spawn_player_with_sink(Box::new(Shared {
        dev: device_info("hw:7,0"),
        fmt: None,
        opened: opened.clone(),
    }));
    let sub = h.subscribe();
    h.set_dsd_mode(DsdMode::Native);
    h.load(uri(&pa));
    wait_for_start(&sub);
    // The next one is played as DoP: same device, other wire.
    h.set_dsd_mode(DsdMode::Dop);
    h.enqueue_next(uri(&pb));
    wait_stopped(&sub, 20);
    let o = opened.lock().unwrap();
    let kinds: Vec<bool> = o.iter().map(|(_, dsd)| *dsd).collect();
    assert_eq!(kinds, [true, false], "{o:?}");
    assert_eq!(o[1].0.sample_rate, 176_400);
}

#[test]
fn seek_lands_on_a_word_and_reports_it() {
    let data = pattern(DSD64, 352_800); // one second
    let path = write_fixture("seek.dsf", &fixture::dsf(DSD64, &data, &[]));
    for mode in [DsdMode::Pcm, DsdMode::Dop, DsdMode::Native] {
        let src = ricercar_audio::dsd::open_path(&path).unwrap();
        let mut p = DsdPlayback::new(src, mode, None).unwrap();
        assert_eq!(p.duration_ms(), Some(1000));
        let at = p.seek_ms(500).unwrap();
        assert!((at.as_secs_f64() - 0.5).abs() < 1e-4, "{mode:?}: {at:?}");
        let mut q = VecDeque::new();
        p.pump(&mut q).unwrap();
        if mode == DsdMode::Dop {
            let byte = 176_400; // half a second, bytes per channel
            assert_eq!(
                q[0],
                ((data[byte * 2] as i32) << 8) | data[byte * 2 + 2] as i32
            );
        }
    }
    // Through the engine: position and duration from the length.
    let (h, _out) = file_player("seek-engine", "S24_3LE");
    let sub = h.subscribe();
    h.set_dsd_mode(DsdMode::Dop);
    h.load(uri(&path));
    h.pause();
    wait_for_start(&sub);
    assert_eq!(h.state.lock().unwrap().dur_ms, Some(1000));
    h.seek_ms(750);
    let pos = wait_pos(&sub);
    assert!((740..=760).contains(&pos), "{pos}");
}

fn wait_pos(sub: &ricercar_audio::Subscriber) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(EngineEvent::Position { pos_ms, .. }) =
            sub.0.recv_timeout(Duration::from_millis(100))
            && pos_ms > 0
        {
            return pos_ms;
        }
    }
    panic!("no position");
}
