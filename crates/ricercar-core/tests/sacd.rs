//! A real SACD image, from RICERCAR_TEST_SACD_ISO (skipped when unset):
//! indexed as its tracks, then 3 s of track 2 played to the file sink as
//! converted PCM. The image is only read, through a symlink in a scratch
//! library.

use std::time::{Duration, Instant};

use ricercar_audio::dsd::sacd::SacdImage;
use ricercar_audio::player::EngineEvent;
use ricercar_core::library::{AlbumSort, Library};

#[test]
fn real_image_is_indexed_and_plays() {
    let Ok(iso) = std::env::var("RICERCAR_TEST_SACD_ISO") else {
        eprintln!("RICERCAR_TEST_SACD_ISO unset, skipped");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("music");
    std::fs::create_dir_all(&root).unwrap();
    let image = root.join("disc.iso");
    std::os::unix::fs::symlink(&iso, &image).unwrap();
    let lib = Library::open(&dir.path().join("lib.db")).unwrap();

    let t = Instant::now();
    let r = lib.scan_roots(std::slice::from_ref(&root));
    eprintln!("indexed {} tracks in {:?}", r.added, t.elapsed());
    let toc = SacdImage::open(&iso).unwrap();
    assert!(toc.tracks().len() >= 2);
    assert_eq!(r.added, toc.tracks().len());
    let albums = lib.albums(AlbumSort::Title);
    assert_eq!(albums.len(), 1, "one album");
    let tracks = lib.album_tracks(&albums[0].id);
    assert_eq!(tracks.len(), toc.tracks().len());
    for (t, want) in tracks.iter().zip(toc.tracks()) {
        assert_eq!(t.track, Some(want.number));
        assert!(!t.title.is_empty());
        if let Some(title) = &want.title {
            assert_eq!(&t.title, title);
        }
        assert_eq!(t.duration_ms, want.duration().as_millis() as u64);
    }
    let t = Instant::now();
    let again = lib.scan_roots(std::slice::from_ref(&root));
    eprintln!("rescan in {:?}", t.elapsed());
    assert_eq!((again.added, again.updated, again.removed), (0, 0, 0));

    // 3 s of track 2, converted to PCM.
    let out = dir.path().join("out.raw");
    let h =
        ricercar_audio::player::spawn_player(&format!("file:{}?formats=S24_3LE", out.display()));
    let sub = h.subscribe();
    let t = Instant::now();
    h.load(tracks[1].uri.clone());
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut started = None;
    loop {
        assert!(Instant::now() < deadline, "did not play 3 s");
        match sub.0.recv_timeout(Duration::from_millis(100)) {
            Ok(EngineEvent::TrackStarted { format, .. }) => {
                assert_eq!(format.map(|f| f.sample_rate), Some(88_200));
                started = Some(t.elapsed());
            }
            Ok(EngineEvent::Position { pos_ms, .. }) if pos_ms >= 3000 => break,
            Ok(EngineEvent::Error { message, .. }) => panic!("engine error: {message}"),
            _ => {}
        }
    }
    let played = t.elapsed();
    assert_eq!(
        h.state.lock().unwrap().dur_ms,
        Some(tracks[1].duration_ms),
        "duration of the track"
    );
    h.stop();
    eprintln!(
        "started in {:?}, 3 s converted in {played:?}",
        started.unwrap()
    );
    let pcm = std::fs::read(&out).unwrap();
    assert!(pcm.len() >= 3 * 88_200 * 2 * 3, "{} bytes", pcm.len());
    assert!(pcm.iter().any(|&b| b != 0), "silence");
}
