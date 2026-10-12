//! Display order of a playlist's tracks, local or from a plugin. The
//! playlist itself keeps its order: sorting only changes what is shown
//! and played from the page.

use std::cmp::Ordering;

use crate::library::Track;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlaylistSort {
    /// The playlist's own order.
    #[default]
    Playlist,
    /// Last added first. Entries without a date count as older than any
    /// dated one, the later in the playlist the more recent.
    RecentlyAdded,
    Title,
    Artist,
    Album,
    Duration,
}

impl PlaylistSort {
    /// The choice of the sort menu, in its order; out of range: the
    /// playlist's order.
    pub fn from_index(i: i32) -> PlaylistSort {
        match i {
            1 => PlaylistSort::RecentlyAdded,
            2 => PlaylistSort::Title,
            3 => PlaylistSort::Artist,
            4 => PlaylistSort::Album,
            5 => PlaylistSort::Duration,
            _ => PlaylistSort::Playlist,
        }
    }
}

/// Indexes into `tracks` in display order. `added`: when each entry was
/// added (Unix seconds), aligned with `tracks`; missing entries count as
/// undated. Ties keep the playlist's order.
pub fn order(tracks: &[Track], added: &[Option<i64>], sort: PlaylistSort) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..tracks.len()).collect();
    let date = |i: usize| added.get(i).copied().flatten();
    match sort {
        PlaylistSort::Playlist => {}
        PlaylistSort::RecentlyAdded => idx.sort_by(|&a, &b| match (date(a), date(b)) {
            (Some(x), Some(y)) => y.cmp(&x).then(b.cmp(&a)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => b.cmp(&a),
        }),
        PlaylistSort::Title => {
            idx.sort_by_cached_key(|&i| (text(Some(&tracks[i].title)), i));
        }
        PlaylistSort::Artist => idx.sort_by_cached_key(|&i| {
            let t = &tracks[i];
            let artist = t.artist.as_ref().or(t.album_artist.as_ref());
            (text(artist), text(t.album.as_ref()), place(t), i)
        }),
        PlaylistSort::Album => idx.sort_by_cached_key(|&i| {
            let t = &tracks[i];
            (text(t.album.as_ref()), place(t), i)
        }),
        PlaylistSort::Duration => idx.sort_by_key(|&i| (tracks[i].duration_ms, i)),
    }
    idx
}

/// Sort key of a name: case ignored, empty names last.
fn text(s: Option<&String>) -> (bool, String) {
    let s = s.map(|s| s.trim().to_lowercase()).unwrap_or_default();
    (s.is_empty(), s)
}

/// Disc and track number, unnumbered last.
fn place(t: &Track) -> (u32, u32) {
    (t.disc.unwrap_or(1), t.track.unwrap_or(u32::MAX))
}

/// Unix seconds of an `added_at` value from a plugin: a number of
/// seconds, or an RFC 3339 date and time (`2024-05-01T18:30:00Z`,
/// `2024-05-01T20:30:00.5+02:00`). Anything else: `None`.
pub fn parse_added(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
            .filter(|&s| s >= 0),
        serde_json::Value::String(s) => parse_rfc3339(s),
        _ => None,
    }
}

fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Fraction of a second, then the offset.
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        let digits = r.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &r[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let ob = rest.as_bytes();
            if ob.len() != 6 || !matches!(ob[0], b'+' | b'-') || ob[3] != b':' {
                return None;
            }
            let oh: i64 = rest.get(1..3)?.parse().ok()?;
            let om: i64 = rest.get(4..6)?.parse().ok()?;
            let o = oh * 3600 + om * 60;
            if ob[0] == b'-' { -o } else { o }
        }
    };
    let days = days_from_civil(y, mo, d);
    let t = days * 86_400 + h * 3600 + mi * 60 + sec - offset;
    (t >= 0).then_some(t)
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tr(title: &str, artist: &str, album: &str, no: u32, ms: u64) -> Track {
        Track {
            title: title.into(),
            artist: (!artist.is_empty()).then(|| artist.into()),
            album: (!album.is_empty()).then(|| album.into()),
            track: Some(no),
            duration_ms: ms,
            ..Default::default()
        }
    }

    fn list() -> Vec<Track> {
        vec![
            tr("b", "Zeta", "Two", 2, 300),
            tr("A", "alpha", "One", 1, 100),
            tr("c", "", "", 1, 200),
            tr("a", "Alpha", "One", 3, 100),
        ]
    }

    #[test]
    fn playlist_order_is_kept() {
        assert_eq!(order(&list(), &[], PlaylistSort::Playlist), [0, 1, 2, 3]);
    }

    #[test]
    fn recently_added_puts_dated_entries_first_newest_first() {
        let added = [Some(10), None, Some(30), None];
        assert_eq!(
            order(&list(), &added, PlaylistSort::RecentlyAdded),
            [2, 0, 3, 1]
        );
        // No dates at all: the end of the playlist first.
        assert_eq!(
            order(&list(), &[], PlaylistSort::RecentlyAdded),
            [3, 2, 1, 0]
        );
        // Same date: the later entry first.
        let same = [Some(5), Some(5), Some(5), Some(5)];
        assert_eq!(
            order(&list(), &same, PlaylistSort::RecentlyAdded),
            [3, 2, 1, 0]
        );
    }

    #[test]
    fn names_sort_without_case_and_empty_last() {
        assert_eq!(order(&list(), &[], PlaylistSort::Title), [1, 3, 0, 2]);
        assert_eq!(order(&list(), &[], PlaylistSort::Artist), [1, 3, 0, 2]);
        assert_eq!(order(&list(), &[], PlaylistSort::Album), [1, 3, 0, 2]);
        assert_eq!(order(&list(), &[], PlaylistSort::Duration), [1, 3, 2, 0]);
    }

    #[test]
    fn menu_indexes_map_to_sorts() {
        assert_eq!(PlaylistSort::from_index(1), PlaylistSort::RecentlyAdded);
        assert_eq!(PlaylistSort::from_index(5), PlaylistSort::Duration);
        assert_eq!(PlaylistSort::from_index(9), PlaylistSort::Playlist);
        assert_eq!(PlaylistSort::from_index(-1), PlaylistSort::Playlist);
    }

    #[test]
    fn added_dates_parse_from_seconds_or_rfc3339() {
        assert_eq!(parse_added(&json!(1714588200)), Some(1714588200));
        assert_eq!(parse_added(&json!(1714588200.7)), Some(1714588200));
        assert_eq!(
            parse_added(&json!("2024-05-01T18:30:00Z")),
            Some(1714588200)
        );
        assert_eq!(
            parse_added(&json!("2024-05-01T20:30:00.25+02:00")),
            Some(1714588200)
        );
        assert_eq!(
            parse_added(&json!("2024-05-01 13:30:00-05:00")),
            Some(1714588200)
        );
        assert_eq!(parse_added(&json!("1970-01-01T00:00:00Z")), Some(0));
        for bad in [
            json!("2024-05-01"),
            json!("2024-13-01T00:00:00Z"),
            json!("2024-05-01T18:30:00"),
            json!("yesterday"),
            json!(-5),
            json!(true),
            json!(null),
        ] {
            assert_eq!(parse_added(&bad), None, "{bad}");
        }
    }
}
