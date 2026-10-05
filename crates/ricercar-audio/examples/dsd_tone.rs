//! Writes a DSD64 sine as a DSF or DFF file (demo library, manual tests).
//!
//! cargo run -p ricercar-audio --example dsd_tone -- <out.dsf|out.dff> <Hz> <seconds> [key=value…]
//!
//! Keys: title, artist, albumartist, album, tracknumber, date, genre.

use ricercar_audio::dsd::{DsdFormat, fixture, id3};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, freq, secs, tags @ ..] = args.as_slice() else {
        eprintln!("usage: dsd_tone <out.dsf|out.dff> <Hz> <seconds> [key=value…]");
        std::process::exit(2);
    };
    let freq: f64 = freq.parse().expect("frequency");
    let secs: f64 = secs.parse().expect("seconds");
    let tags: Vec<(&str, &str)> = tags.iter().filter_map(|t| t.split_once('=')).collect();
    let f = DsdFormat {
        rate: 2_822_400,
        channels: 2,
    };
    // −6 dB SACD level, the usual reference.
    let data = fixture::sine(f, freq, 0.25, secs);
    let tag = id3::build(&tags, None);
    let bytes = if out.to_ascii_lowercase().ends_with(".dff") {
        fixture::dff(f, &data, &tag, "")
    } else {
        fixture::dsf(f, &data, &tag)
    };
    std::fs::write(out, bytes).expect("write");
}
