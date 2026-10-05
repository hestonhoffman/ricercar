//! Writes a synthetic SACD image (stereo area, plain DSD64) of sine tones,
//! one track per tone (demo library, manual tests).
//!
//! cargo run -p ricercar-audio --example sacd_image -- <out.iso> <album> <artist> <Hz:seconds:title>…

use ricercar_audio::dsd::DsdFormat;
use ricercar_audio::dsd::fixture::sine;
use ricercar_audio::dsd::sacd::fixture::{FRAME, Spec, image};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, album, artist, tracks @ ..] = args.as_slice() else {
        eprintln!("usage: sacd_image <out.iso> <album> <artist> <Hz:seconds:title>…");
        std::process::exit(2);
    };
    let f = DsdFormat {
        rate: 2_822_400,
        channels: 2,
    };
    let mut data = Vec::new();
    let mut list = Vec::new();
    for t in tracks {
        let mut parts = t.splitn(3, ':');
        let (Some(freq), Some(secs), Some(title)) = (parts.next(), parts.next(), parts.next())
        else {
            eprintln!("track: Hz:seconds:title");
            std::process::exit(2);
        };
        let freq: f64 = freq.parse().expect("frequency");
        let secs: f64 = secs.parse().expect("seconds");
        let frames = (secs * 75.0).round() as u32;
        // −6 dB SACD level, the usual reference.
        let mut tone = sine(f, freq, 0.25, frames as f64 / 75.0 + 0.01);
        tone.truncate(frames as usize * FRAME * 2);
        data.extend(tone);
        list.push((frames, title.to_string()));
    }
    let mut spec = Spec::new(false, 0, &[]);
    spec.tracks = list;
    spec.title = album.clone();
    spec.artist = artist.clone();
    spec.performer = artist.clone();
    spec.publisher = String::new();
    spec.genre = 5;
    spec.date = (2024, 0, 0);
    let bytes = image(&spec, |t| {
        let at = t as usize * FRAME * 2;
        data[at..at + FRAME * 2].to_vec()
    });
    std::fs::write(out, bytes).expect("write");
}
