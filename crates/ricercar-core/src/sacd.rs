//! SACD disc images (ISO) in the library: the tracks of the stereo area.
//!
//! A track's library key is `<image path>#sacd=<n>` and its URI the
//! image's `file://` URI with the same fragment; the engine opens the
//! image and plays the track through the DSD path. Indexing reads the
//! TOC only, never audio.

use std::path::Path;

use ricercar_audio::dsd::sacd::{SacdImage, SacdTrack};

use crate::meta::{TagInfo, file_uri};

pub use ricercar_audio::dsd::sacd::{TRACK_MARK, split_key};

/// Extension of the images looked at by scans (other ISO files are
/// skipped once their TOC is not a SACD one).
pub const EXT: &str = "iso";

/// Library key of track `n` of an image.
pub fn key(image: &Path, n: u32) -> String {
    format!("{}{TRACK_MARK}{n}", image.to_string_lossy())
}

/// The image behind a library path (the path itself for anything else).
pub fn file_of(path: &str) -> &str {
    split_key(path).map_or(path, |(image, _)| image)
}

/// The URI the engine plays for a track key.
pub fn uri(key: &str) -> Option<String> {
    let (image, n) = split_key(key)?;
    Some(format!("{}{TRACK_MARK}{n}", file_uri(Path::new(image))))
}

/// Tags of a track: track text first, then the disc's.
pub fn track_tags(img: &SacdImage, t: &SacdTrack) -> TagInfo {
    let d = img.disc();
    let area = img.stereo_area();
    let year = d.date.as_deref().and_then(|s| {
        s.get(..4)
            .and_then(|y| y.parse::<i32>().ok())
            .filter(|y| (1000..3000).contains(y))
    });
    let album = d.title.clone().or_else(|| {
        img.path()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
    });
    let (rate, channels) = area.map_or((2_822_400, 2), |a| (a.format.rate, a.format.channels));
    TagInfo {
        title: Some(
            t.title
                .clone()
                .unwrap_or_else(|| format!("Track {:02}", t.number)),
        ),
        artist: t.performer.clone().or_else(|| d.artist.clone()),
        album_artist: d.artist.clone(),
        album,
        track: Some(t.number),
        disc: (d.disc_count > 1).then_some(d.disc_number),
        year,
        genre: d.genre.clone(),
        composer: t.composer.clone().or_else(|| t.songwriter.clone()),
        duration_ms: t.duration().as_millis() as u64,
        sample_rate: Some(rate),
        bits: Some(1),
        channels: Some(channels.min(255) as u8),
        bitrate: Some(rate / 1000 * channels as u32),
        codec: Some("SACD".into()),
        ..TagInfo::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_uris() {
        let k = key(Path::new("/m/#1 hits/disc.iso"), 7);
        assert_eq!(k, "/m/#1 hits/disc.iso#sacd=7");
        assert_eq!(split_key(&k), Some(("/m/#1 hits/disc.iso", 7)));
        assert_eq!(file_of(&k), "/m/#1 hits/disc.iso");
        assert_eq!(file_of("/m/a.flac"), "/m/a.flac");
        assert_eq!(uri(&k).unwrap(), "file:///m/%231%20hits/disc.iso#sacd=7");
        assert_eq!(uri("/m/a.flac"), None);
    }
}
