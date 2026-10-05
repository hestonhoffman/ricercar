//! Prints the disc and track list of a SACD image, and writes a track as
//! a DSDIFF file to compare with other tools.
//!
//! ```text
//! cargo run -p ricercar-audio --example sacd_info -- /path/to/disc.iso
//! cargo run -p ricercar-audio --example sacd_info -- /path/to/disc.iso --extract 3 out.dff
//! cargo run -p ricercar-audio --example sacd_info -- /path/to/disc.iso --raw-dst 3 out.dff
//! ```
//!
//! `--extract` writes plain DSD; `--raw-dst` copies the DST frames of a
//! DST-coded area unchanged, for checking another decoder against ours.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::time::Duration;

use ricercar_audio::dsd::sacd::{FrameFormat, SacdImage};
use ricercar_audio::dsd::source::DsdFormat;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("sacd_info: {e}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let usage = "usage: sacd_info IMAGE [--extract N OUT.dff | --raw-dst N OUT.dff]";
    let image = SacdImage::open(args.first().ok_or(usage)?)?;
    match args.get(1).map(String::as_str) {
        None => info(&image)?,
        Some(mode @ ("--extract" | "--raw-dst")) => {
            let n: u32 = args.get(2).ok_or(usage)?.parse()?;
            let out = args.get(3).ok_or(usage)?;
            let area = image.stereo_area().ok_or("no stereo area")?;
            let mut w = BufWriter::new(File::create(out)?);
            if mode == "--extract" {
                let mut src = image.track_source(n)?;
                let dff = Dff::start(&mut w, area.format, false)?;
                let mut buf = vec![0u8; 1 << 16];
                loop {
                    let got = src.read(&mut buf)?;
                    if got == 0 {
                        break;
                    }
                    dff.w.write_all(&buf[..got])?;
                }
                dff.finish()?;
            } else {
                if area.frame_format != FrameFormat::Dst {
                    return Err("the stereo area is not DST-coded".into());
                }
                let mut dff = Dff::start(&mut w, area.format, true)?;
                for frame in image.track_frames(n)? {
                    dff.dst_frame(&frame?)?;
                }
                dff.finish()?;
            }
            w.flush()?;
        }
        Some(_) => return Err(usage.into()),
    }
    Ok(())
}

fn info(image: &SacdImage) -> Result<(), Box<dyn std::error::Error>> {
    let d = image.disc();
    let show = |k: &str, v: &Option<String>| {
        if let Some(v) = v {
            println!("{k:<10} {v}");
        }
    };
    show("title", &d.title);
    show("artist", &d.artist);
    show("publisher", &d.publisher);
    show("copyright", &d.copyright);
    show("genre", &d.genre);
    show("date", &d.date);
    show("catalog", &d.catalog_number);
    println!(
        "disc       {} of {}{}",
        d.disc_number,
        d.disc_count,
        if d.hybrid { ", hybrid" } else { "" }
    );
    for a in &d.areas {
        println!(
            "area       {:?}: {} ch, DSD{}, {:?}, {} tracks, {}",
            a.kind,
            a.format.channels,
            a.format.multiple(),
            a.frame_format,
            a.track_count,
            clock(a.duration())
        );
    }
    for t in image.tracks() {
        println!(
            "{:>3}  {}  {}  {}{}",
            t.number,
            clock(t.start()),
            clock(t.duration()),
            t.title.as_deref().unwrap_or("?"),
            t.performer
                .as_deref()
                .map(|p| format!(" / {p}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn clock(d: Duration) -> String {
    let ms = d.as_millis();
    format!("{:02}:{:02}.{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

/// A DSDIFF writer: chunk sizes are patched in when done.
struct Dff<'a, W: Write + Seek> {
    w: &'a mut W,
    dst: bool,
    frames: u32,
    /// Where the size of the `DSD ` / `DST ` chunk is.
    data_size_at: u64,
}

impl<'a, W: Write + Seek> Dff<'a, W> {
    fn start(w: &'a mut W, f: DsdFormat, dst: bool) -> io::Result<Self> {
        let names: &[&[u8; 4]] = match f.channels {
            2 => &[b"SLFT", b"SRGT"],
            _ => &[b"C000", b"C001", b"C002", b"C003", b"C004", b"C005"],
        };
        let mut prop = b"SND ".to_vec();
        prop.extend(chunk(b"FS  ", &f.rate.to_be_bytes()));
        let mut chnl = f.channels.to_be_bytes().to_vec();
        for n in &names[..f.channels as usize] {
            chnl.extend_from_slice(*n);
        }
        prop.extend(chunk(b"CHNL", &chnl));
        let (id, name): (&[u8], &[u8]) = if dst {
            (b"DST ", b"DST Encoded")
        } else {
            (b"DSD ", b"not compressed")
        };
        let mut cmpr = id.to_vec();
        cmpr.push(name.len() as u8);
        cmpr.extend_from_slice(name);
        prop.extend(chunk(b"CMPR", &cmpr));

        w.write_all(b"FRM8\0\0\0\0\0\0\0\0DSD ")?;
        w.write_all(&chunk(b"FVER", &0x0105_0000u32.to_be_bytes()))?;
        w.write_all(&chunk(b"PROP", &prop))?;
        w.write_all(id)?;
        let data_size_at = w.stream_position()?;
        w.write_all(&[0; 8])?;
        if dst {
            // Frame count patched in when done, 75 frames per second.
            w.write_all(&chunk(b"FRTE", &[0, 0, 0, 0, 0, 75]))?;
        }
        Ok(Dff {
            w,
            dst,
            frames: 0,
            data_size_at,
        })
    }

    fn dst_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        self.w.write_all(&chunk(b"DSTF", frame))?;
        self.frames += 1;
        Ok(())
    }

    fn finish(self) -> io::Result<()> {
        let end = self.w.stream_position()?;
        if end % 2 == 1 {
            self.w.write_all(&[0])?;
        }
        let total = self.w.stream_position()?;
        self.w.seek(SeekFrom::Start(4))?;
        self.w.write_all(&(total - 12).to_be_bytes())?;
        self.w.seek(SeekFrom::Start(self.data_size_at))?;
        self.w
            .write_all(&(end - self.data_size_at - 8).to_be_bytes())?;
        if self.dst {
            self.w.seek(SeekFrom::Start(self.data_size_at + 20))?;
            self.w.write_all(&self.frames.to_be_bytes())?;
        }
        self.w.seek(SeekFrom::Start(total))?;
        Ok(())
    }
}

/// An IFF chunk, padded to an even size.
fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut c = id.to_vec();
    c.extend((data.len() as u64).to_be_bytes());
    c.extend_from_slice(data);
    if data.len() % 2 == 1 {
        c.push(0);
    }
    c
}
