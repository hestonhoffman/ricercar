//! SQLite music library: tracks, albums, artists, stats, playlists, search.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use rayon::prelude::*;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::meta::{TagInfo, file_uri, read_tags};

pub const SUPPORTED_EXTS: &[&str] = &[
    "flac", "wav", "aiff", "aif", "aifc", "mp3", "ogg", "oga", "m4a", "mp4", "alac", "dsf", "dff",
];

/// Bump with a new step in `schema`: each step upgrades from the version
/// before it and must keep user data (stats, playlists, `added_at`...).
const SCHEMA_VERSION: i32 = 3;

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub path: String,
    pub uri: String,
    pub title: String,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub album_id: String,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub duration_ms: u64,
    pub sample_rate: Option<u32>,
    pub bits: Option<u8>,
    pub channels: Option<u8>,
    pub bitrate: Option<u32>,
    pub codec: Option<String>,
    pub rg_track_gain: Option<f32>,
    pub rg_track_peak: Option<f32>,
    pub rg_album_gain: Option<f32>,
    pub rg_album_peak: Option<f32>,
    pub mb_album_id: Option<String>,
    pub favorite: bool,
    pub play_count: u32,
    /// Cover image URL (plugin tracks in playlists; library tracks use
    /// their file).
    #[serde(default)]
    pub art: Option<String>,
    /// Plugin tracks: their album and artist refs (see `TrackInfo`), so a
    /// track played from a list keeps them in the queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_ref: Option<String>,
}

impl Track {
    /// A `plugin://` entry of a playlist rather than a library file.
    pub fn is_plugin(&self) -> bool {
        crate::plugin::is_plugin_uri(&self.path)
    }

    /// Playlist entry of a plugin track, from its saved metadata.
    pub fn from_info(info: &crate::controller::TrackInfo) -> Track {
        Track {
            path: info.uri.clone(),
            uri: info.uri.clone(),
            title: info.title.clone(),
            artist: info.artist.clone(),
            album_artist: info.album_artist.clone(),
            album: info.album.clone(),
            album_id: String::new(),
            track: info.track_no,
            year: info.year,
            genre: info.genre.clone(),
            duration_ms: info.duration_ms,
            sample_rate: info.sample_rate,
            bits: info.bits,
            codec: info.codec.clone(),
            art: info.cover.clone(),
            album_ref: info.album_ref.clone(),
            artist_ref: info.artist_ref.clone(),
            ..Default::default()
        }
    }

    pub fn is_hires(&self) -> bool {
        is_hires(self.sample_rate, self.bits)
    }
}

pub fn is_hires(rate: Option<u32>, bits: Option<u8>) -> bool {
    rate.is_some_and(|r| r > 48_000) || bits.is_some_and(|b| b > 16)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Album {
    pub id: String,
    pub title: String,
    /// Album artist, else the sole track artist, else "Various Artists".
    pub artist: String,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub track_count: u32,
    pub disc_count: u32,
    pub duration_ms: u64,
    pub added_at: i64,
    pub max_rate: Option<u32>,
    pub max_bits: Option<u8>,
    pub codec: Option<String>,
    /// A track of the album, to resolve the cover from.
    pub cover_path: String,
    pub dir: String,
    pub favorite: bool,
}

impl Album {
    pub fn is_hires(&self) -> bool {
        is_hires(self.max_rate, self.max_bits)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Artist {
    pub name: String,
    pub album_count: u32,
    pub track_count: u32,
    pub cover_path: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Genre {
    pub name: String,
    pub album_count: u32,
    pub track_count: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub track_count: u32,
    pub duration_ms: u64,
    pub updated_at: i64,
    pub cover_path: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub artists: Vec<Artist>,
    pub composers: Vec<Artist>,
    pub albums: Vec<Album>,
    pub tracks: Vec<Track>,
}

impl SearchResults {
    pub fn is_empty(&self) -> bool {
        self.artists.is_empty()
            && self.composers.is_empty()
            && self.albums.is_empty()
            && self.tracks.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlbumSort {
    #[default]
    Title,
    Artist,
    YearDesc,
    RecentlyAdded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackSort {
    #[default]
    Artist,
    Title,
    Album,
    Duration,
    RecentlyAdded,
    MostPlayed,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LibraryStats {
    pub tracks: u32,
    pub albums: u32,
    pub artists: u32,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub hires_tracks: u32,
}

/// How much of the library comes at one sample rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateCount {
    pub rate: u32,
    pub albums: u32,
    pub tracks: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub total: usize,
}

/// Live scan progress, readable from any thread.
#[derive(Default)]
pub struct ScanProgress {
    pub running: AtomicBool,
    pub done: AtomicUsize,
    pub total: AtomicUsize,
    /// Completed scans since startup.
    pub finished: AtomicUsize,
}

pub struct Library {
    writer: Mutex<Connection>,
    /// Separate WAL reader so the UI never waits behind a scan transaction.
    reader: Option<Mutex<Connection>>,
    rev: AtomicU64,
    pub progress: ScanProgress,
}

fn schema(conn: &Connection) -> rusqlite::Result<()> {
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 2 {
        // v1 only held an index of files (no user data): rebuild it.
        conn.execute_batch(
            "DROP TABLE IF EXISTS tracks_fts;
             DROP TABLE IF EXISTS tracks;",
        )?;
    }
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS tracks (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            dir TEXT NOT NULL DEFAULT '',
            title TEXT NOT NULL DEFAULT '',
            artist TEXT, album_artist TEXT, album TEXT,
            album_id TEXT NOT NULL DEFAULT '',
            track INTEGER, disc INTEGER, year INTEGER, genre TEXT, composer TEXT,
            duration_ms INTEGER NOT NULL DEFAULT 0,
            sample_rate INTEGER, bits INTEGER, channels INTEGER, bitrate INTEGER, codec TEXT,
            rg_track_gain REAL, rg_track_peak REAL, rg_album_gain REAL, rg_album_peak REAL,
            mb_album_id TEXT, mb_track_id TEXT,
            size INTEGER NOT NULL DEFAULT 0,
            mtime INTEGER NOT NULL DEFAULT 0,
            added_at INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS idx_tracks_album ON tracks(album_id);
         CREATE INDEX IF NOT EXISTS idx_tracks_artist ON tracks(artist COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS idx_tracks_aa ON tracks(album_artist COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS idx_tracks_added ON tracks(added_at);
         CREATE INDEX IF NOT EXISTS idx_tracks_name ON tracks(COALESCE(album_artist, artist) COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS idx_tracks_composer ON tracks(composer COLLATE NOCASE);

         CREATE VIRTUAL TABLE IF NOT EXISTS tracks_fts USING fts5(
            title, artist, album, album_artist, genre, composer,
            content='tracks', content_rowid='id',
            tokenize='unicode61 remove_diacritics 2'
         );
         CREATE TRIGGER IF NOT EXISTS tracks_ai AFTER INSERT ON tracks BEGIN
            INSERT INTO tracks_fts(rowid, title, artist, album, album_artist, genre, composer)
            VALUES (new.id, new.title, new.artist, new.album, new.album_artist, new.genre, new.composer);
         END;
         CREATE TRIGGER IF NOT EXISTS tracks_ad AFTER DELETE ON tracks BEGIN
            INSERT INTO tracks_fts(tracks_fts, rowid, title, artist, album, album_artist, genre, composer)
            VALUES ('delete', old.id, old.title, old.artist, old.album, old.album_artist, old.genre, old.composer);
         END;
         CREATE TRIGGER IF NOT EXISTS tracks_au AFTER UPDATE ON tracks BEGIN
            INSERT INTO tracks_fts(tracks_fts, rowid, title, artist, album, album_artist, genre, composer)
            VALUES ('delete', old.id, old.title, old.artist, old.album, old.album_artist, old.genre, old.composer);
            INSERT INTO tracks_fts(rowid, title, artist, album, album_artist, genre, composer)
            VALUES (new.id, new.title, new.artist, new.album, new.album_artist, new.genre, new.composer);
         END;

         CREATE TABLE IF NOT EXISTS stats (
            path TEXT PRIMARY KEY,
            play_count INTEGER NOT NULL DEFAULT 0,
            last_played INTEGER,
            favorite INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS history (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL,
            played_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_history_time ON history(played_at);
         CREATE TABLE IF NOT EXISTS album_favs (
            album_id TEXT PRIMARY KEY,
            added_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS playlists (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS playlist_items (
            playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
            pos INTEGER NOT NULL,
            path TEXT NOT NULL,
            PRIMARY KEY (playlist_id, pos)
         );",
    )?;
    // Metadata of plugin:// playlist entries (no row in `tracks`).
    let has_info: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('playlist_items') WHERE name = 'info'")?
        .exists([])?;
    if !has_info {
        conn.execute_batch("ALTER TABLE playlist_items ADD COLUMN info TEXT")?;
    }
    // When each entry was added (Unix seconds); NULL for entries older
    // than the column. Older versions leave it NULL too.
    let has_added: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('playlist_items') WHERE name = 'added_at'")?
        .exists([])?;
    if !has_added {
        conn.execute_batch("ALTER TABLE playlist_items ADD COLUMN added_at INTEGER")?;
    }
    // Metadata of plugin:// plays, likewise (the Home page's history).
    let history_info: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('history') WHERE name = 'info'")?
        .exists([])?;
    if !history_info {
        conn.execute_batch("ALTER TABLE history ADD COLUMN info TEXT")?;
    }
    // v3: tracks cut from one audio file by a cue sheet (see `cue`).
    if version < 3 {
        conn.execute_batch(
            "ALTER TABLE tracks ADD COLUMN cue_start INTEGER;
             ALTER TABLE tracks ADD COLUMN cue_end INTEGER;
             ALTER TABLE tracks ADD COLUMN cue_sheet TEXT;",
        )?;
    }
    // Later steps go here, as `if version < N { ... }`.
    if version < SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Stable album identity: (album, album artist) when an album artist is
/// tagged, else (album, directory) so untagged compilations stay together.
pub fn album_id_for(tags: &TagInfo, path: &Path) -> String {
    let dir = path
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let album = norm(tags.album.as_deref().unwrap_or(""));
    let key = match tags.album_artist.as_deref() {
        Some(aa) if !album.is_empty() => format!("{album}\u{1f}{}", norm(aa)),
        _ => format!("{album}\u{1f}dir:{dir}"),
    };
    format!("{:016x}", fnv(&key))
}

const TRACK_COLS: &str =
    "t.path, t.title, t.artist, t.album_artist, t.album, t.album_id, t.track, t.disc,
    t.year, t.genre, t.composer, t.duration_ms, t.sample_rate, t.bits, t.channels, t.bitrate,
    t.codec, t.rg_track_gain, t.rg_track_peak, t.rg_album_gain, t.rg_album_peak, t.mb_album_id,
    COALESCE(s.favorite, 0), COALESCE(s.play_count, 0), t.cue_start, t.cue_end";
/// Number of columns in `TRACK_COLS`.
const TRACK_NCOLS: usize = 26;

/// The URI the engine plays: a cue track is its span of the audio file,
/// a SACD track its number in the image.
fn track_uri(path: &str, cue_start: Option<i64>, cue_end: Option<i64>) -> String {
    if let Some(uri) = crate::sacd::uri(path) {
        return uri;
    }
    match cue_start {
        Some(start) => format!(
            "{}{}",
            file_uri(Path::new(crate::cue::file_of(path))),
            crate::cue::fragment(start.max(0) as u64, cue_end.map(|e| e.max(0) as u64))
        ),
        None => file_uri(Path::new(path)),
    }
}

fn row_to_track(r: &rusqlite::Row<'_>) -> rusqlite::Result<Track> {
    let path: String = r.get(0)?;
    Ok(Track {
        uri: track_uri(&path, r.get(24)?, r.get(25)?),
        path,
        title: r.get(1)?,
        artist: r.get(2)?,
        album_artist: r.get(3)?,
        album: r.get(4)?,
        album_id: r.get(5)?,
        track: r.get(6)?,
        disc: r.get(7)?,
        year: r.get(8)?,
        genre: r.get(9)?,
        composer: r.get(10)?,
        duration_ms: r.get::<_, i64>(11)? as u64,
        sample_rate: r.get(12)?,
        bits: r.get(13)?,
        channels: r.get(14)?,
        bitrate: r.get(15)?,
        codec: r.get(16)?,
        rg_track_gain: r.get(17)?,
        rg_track_peak: r.get(18)?,
        rg_album_gain: r.get(19)?,
        rg_album_peak: r.get(20)?,
        mb_album_id: r.get(21)?,
        favorite: r.get::<_, i64>(22)? != 0,
        play_count: r.get(23)?,
        art: None,
        album_ref: None,
        artist_ref: None,
    })
}

const ALBUM_SELECT: &str = "SELECT t.album_id, MAX(t.album), MAX(t.album_artist), MAX(t.artist),
    COUNT(DISTINCT t.artist), MIN(t.year), MAX(t.genre), COUNT(*),
    COUNT(DISTINCT COALESCE(t.disc, 1)), SUM(t.duration_ms), MIN(t.added_at),
    MAX(t.sample_rate), MAX(t.bits), MAX(t.codec), MIN(t.path), MAX(t.dir),
    EXISTS(SELECT 1 FROM album_favs f WHERE f.album_id = t.album_id)
    FROM tracks t";

fn row_to_album(r: &rusqlite::Row<'_>) -> rusqlite::Result<Album> {
    let album: Option<String> = r.get(1)?;
    let album_artist: Option<String> = r.get(2)?;
    let artist: Option<String> = r.get(3)?;
    let distinct_artists: u32 = r.get(4)?;
    let dir: String = r.get(15)?;
    let title = album.filter(|a| !a.is_empty()).unwrap_or_else(|| {
        Path::new(&dir)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Unknown album".into())
    });
    let artist = album_artist.unwrap_or_else(|| {
        if distinct_artists > 1 {
            "Various Artists".into()
        } else {
            artist.unwrap_or_else(|| "Unknown artist".into())
        }
    });
    Ok(Album {
        id: r.get(0)?,
        title,
        artist,
        year: r.get(5)?,
        genre: r.get(6)?,
        track_count: r.get(7)?,
        disc_count: r.get(8)?,
        duration_ms: r.get::<_, i64>(9)? as u64,
        added_at: r.get(10)?,
        max_rate: r.get(11)?,
        max_bits: r.get(12)?,
        codec: r.get(13)?,
        cover_path: r.get(14)?,
        dir,
        favorite: r.get(16)?,
    })
}

const PLAYLIST_SELECT: &str = "SELECT p.id, p.name,
        SUM(CASE WHEN t.path IS NOT NULL OR i.info IS NOT NULL THEN 1 ELSE 0 END),
        COALESCE(SUM(COALESCE(t.duration_ms, json_extract(i.info, '$.duration_ms'))), 0),
        p.updated_at,
        (SELECT i.path FROM playlist_items i WHERE i.playlist_id = p.id ORDER BY i.pos LIMIT 1)
     FROM playlists p
     LEFT JOIN playlist_items i ON i.playlist_id = p.id
     LEFT JOIN tracks t ON t.path = i.path";

fn row_to_playlist(r: &rusqlite::Row<'_>) -> rusqlite::Result<Playlist> {
    Ok(Playlist {
        id: r.get(0)?,
        name: r.get(1)?,
        track_count: r.get(2)?,
        duration_ms: r.get::<_, i64>(3)? as u64,
        updated_at: r.get(4)?,
        cover_path: r.get(5)?,
    })
}

/// Artist rows: `COALESCE(album_artist, artist)` grouped case-insensitively
/// (the expression matches `idx_tracks_name`).
const ARTIST_SELECT: &str = "SELECT COALESCE(album_artist, artist) AS name,
    COUNT(DISTINCT album_id), COUNT(*), MIN(path) FROM tracks";

/// Composers read as artists: the name is the COMPOSER tag.
const COMPOSER_SELECT: &str = "SELECT composer AS name,
    COUNT(DISTINCT album_id), COUNT(*), MIN(path) FROM tracks";

fn row_to_artist(r: &rusqlite::Row<'_>) -> rusqlite::Result<Artist> {
    Ok(Artist {
        name: r.get(0)?,
        album_count: r.get(1)?,
        track_count: r.get(2)?,
        cover_path: r.get(3)?,
    })
}

/// Turn free text into an FTS5 prefix query: `miles kind` → `"miles"* "kind"*`.
fn fts_query(q: &str) -> Option<String> {
    let terms: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"*", t.replace('"', "")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

struct Scanned {
    path: PathBuf,
    size: i64,
    mtime: i64,
}

/// An audio file cut into tracks by a cue sheet.
struct CueJob {
    audio: PathBuf,
    size: i64,
    /// Latest of the audio file's and the sheet's.
    mtime: i64,
    sheet_path: PathBuf,
    sheet: std::sync::Arc<crate::cue::CueSheet>,
    /// Index of the audio file in the sheet's `FILE` entries.
    file: usize,
}

impl CueJob {
    /// (key, size, track index) of each track; the file size is counted
    /// once, on its first track.
    fn keys(&self) -> Vec<(String, i64, usize)> {
        self.sheet
            .tracks_of(self.file)
            .enumerate()
            .map(|(n, (i, t))| {
                let size = if n == 0 { self.size } else { 0 };
                (crate::cue::key(&self.audio, t.number), size, i)
            })
            .collect()
    }
}

/// A SACD image whose TOC changed (or is new): indexed as its tracks.
struct SacdJob {
    size: i64,
    mtime: i64,
    image: ricercar_audio::dsd::sacd::SacdImage,
}

impl SacdJob {
    /// (key, size, track) of each track of the stereo area; the image size
    /// is counted once, on its first track.
    fn keys(&self) -> Vec<(String, i64, &ricercar_audio::dsd::sacd::SacdTrack)> {
        self.image
            .tracks()
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let size = if i == 0 { self.size } else { 0 };
                (crate::sacd::key(self.image.path(), t.number), size, t)
            })
            .collect()
    }
}

fn mtime_of(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Audio files described by the cue sheets found in a scan, each by the
/// first sheet (in path order) that cuts it into two tracks or more.
fn cue_jobs(sheets: &mut [(PathBuf, i64)], audio: &HashMap<PathBuf, (i64, i64)>) -> Vec<CueJob> {
    sheets.sort();
    let mut jobs: Vec<CueJob> = Vec::new();
    for (sheet_path, sheet_mtime) in sheets.iter() {
        let Some(sheet) = crate::cue::read(sheet_path) else {
            continue;
        };
        let sheet = std::sync::Arc::new(sheet);
        for (file, name) in sheet.files.iter().enumerate() {
            if sheet.tracks_of(file).count() < 2 {
                continue;
            }
            let Some(path) = crate::cue::resolve_file(sheet_path, name, SUPPORTED_EXTS) else {
                continue;
            };
            let Some(&(size, mtime)) = audio.get(&path) else {
                continue;
            };
            if jobs.iter().any(|j| j.audio == path) {
                continue;
            }
            jobs.push(CueJob {
                audio: path,
                size,
                mtime: mtime.max(*sheet_mtime),
                sheet_path: sheet_path.clone(),
                sheet: sheet.clone(),
                file,
            });
        }
    }
    jobs
}

impl Library {
    pub fn open(path: &Path) -> rusqlite::Result<Library> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let writer = Connection::open(path)?;
        writer.pragma_update(None, "journal_mode", "WAL")?;
        writer.pragma_update(None, "synchronous", "NORMAL")?;
        writer.busy_timeout(std::time::Duration::from_secs(5))?;
        schema(&writer)?;
        let reader = Connection::open(path)?;
        reader.busy_timeout(std::time::Duration::from_secs(5))?;
        reader.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Library {
            writer: Mutex::new(writer),
            reader: Some(Mutex::new(reader)),
            rev: AtomicU64::new(1),
            progress: ScanProgress::default(),
        })
    }

    pub fn in_memory() -> rusqlite::Result<Library> {
        let conn = Connection::open_in_memory()?;
        schema(&conn)?;
        Ok(Library {
            writer: Mutex::new(conn),
            reader: None,
            rev: AtomicU64::new(1),
            progress: ScanProgress::default(),
        })
    }

    fn w(&self) -> MutexGuard<'_, Connection> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn r(&self) -> MutexGuard<'_, Connection> {
        match &self.reader {
            Some(r) => r.lock().unwrap_or_else(|e| e.into_inner()),
            None => self.w(),
        }
    }

    /// Bumped on every change; views compare it to know when to refresh.
    pub fn revision(&self) -> u64 {
        self.rev.load(Ordering::Relaxed)
    }

    fn touch(&self) {
        self.rev.fetch_add(1, Ordering::Relaxed);
    }

    fn query<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Vec<T> {
        let conn = self.r();
        let res = conn.prepare_cached(sql).and_then(|mut st| {
            st.query_map(params, f)?
                .collect::<rusqlite::Result<Vec<T>>>()
        });
        res.unwrap_or_else(|e| {
            tracing::warn!("library query failed: {e}");
            #[cfg(test)]
            eprintln!("query failed: {e}\n{sql}");
            Vec::new()
        })
    }

    // ------------------------------------------------------------ scanning

    /// Incremental scan of several roots: only new or modified files are
    /// re-tagged (in parallel); vanished files are pruned.
    pub fn scan_roots(&self, roots: &[PathBuf]) -> ScanReport {
        if self.progress.running.swap(true, Ordering::SeqCst) {
            return ScanReport::default();
        }
        self.progress.done.store(0, Ordering::Relaxed);
        self.progress.total.store(0, Ordering::Relaxed);
        let report = self.scan_inner(roots);
        self.progress.running.store(false, Ordering::SeqCst);
        self.progress.finished.fetch_add(1, Ordering::SeqCst);
        if report.added + report.updated + report.removed > 0 {
            self.touch();
        }
        report
    }

    fn scan_inner(&self, roots: &[PathBuf]) -> ScanReport {
        let known: HashMap<String, (i64, i64)> = self
            .query("SELECT path, size, mtime FROM tracks", [], |r| {
                Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?)))
            })
            .into_iter()
            .collect();
        let mut seen: HashSet<String> = HashSet::new();
        let mut found: Vec<Scanned> = Vec::new();
        let mut sheets: Vec<(PathBuf, i64)> = Vec::new();
        let mut images: Vec<Scanned> = Vec::new();
        let mut report = ScanReport::default();
        let mut live_roots: Vec<PathBuf> = Vec::new();
        // Directories the walk could not read: what is under them is kept.
        let mut unreadable: Vec<PathBuf> = Vec::new();

        for root in roots {
            if !root.is_dir() {
                // Unmounted disk / offline NAS: never prune what we can't see.
                tracing::warn!("library root {} is not available", root.display());
                continue;
            }
            live_roots.push(root.clone());
            for entry in WalkDir::new(root).follow_links(true) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("library scan: {e}");
                        unreadable.push(e.path().unwrap_or(root).to_path_buf());
                        continue;
                    }
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase());
                let Some(ext) = ext else { continue };
                let is_sheet = ext == "cue";
                let is_image = ext == crate::sacd::EXT;
                if !is_sheet && !is_image && !SUPPORTED_EXTS.contains(&ext.as_str()) {
                    continue;
                }
                let Ok(md) = entry.metadata() else { continue };
                if is_sheet {
                    sheets.push((path.to_path_buf(), mtime_of(&md)));
                    continue;
                }
                let scanned = Scanned {
                    path: path.to_path_buf(),
                    size: md.len() as i64,
                    mtime: mtime_of(&md),
                };
                if is_image {
                    images.push(scanned);
                } else {
                    found.push(scanned);
                }
            }
        }

        let by_path: HashMap<PathBuf, (i64, i64)> = found
            .iter()
            .map(|s| (s.path.clone(), (s.size, s.mtime)))
            .collect();
        let jobs = cue_jobs(&mut sheets, &by_path);
        let cut: HashSet<&Path> = jobs.iter().map(|j| j.audio.as_path()).collect();
        let mut todo: Vec<Scanned> = Vec::new();
        for s in found {
            // An audio file cut by a cue sheet is listed as its tracks only.
            if cut.contains(s.path.as_path()) {
                continue;
            }
            let key = s.path.to_string_lossy().into_owned();
            if known.get(&key) != Some(&(s.size, s.mtime)) {
                todo.push(s);
            }
            seen.insert(key);
        }
        let mut cue_todo: Vec<&CueJob> = Vec::new();
        for job in &jobs {
            let mut changed = false;
            for (key, size, _) in job.keys() {
                changed |= known.get(&key) != Some(&(size, job.mtime));
                seen.insert(key);
            }
            if changed {
                cue_todo.push(job);
            }
        }
        // SACD images: an unchanged image keeps its tracks without being
        // opened; a new or modified one has its TOC read (never its audio).
        // Other ISO files are not SACD images and are skipped.
        let mut indexed: HashMap<&str, Vec<&String>> = HashMap::new();
        for k in known.keys() {
            if let Some((image, _)) = crate::sacd::split_key(k) {
                indexed.entry(image).or_default().push(k);
            }
        }
        let mut changed_images: Vec<&Scanned> = Vec::new();
        for img in &images {
            let path = img.path.to_string_lossy();
            let first = crate::sacd::key(&img.path, 1);
            match indexed.get(path.as_ref()) {
                Some(keys) if known.get(&first) == Some(&(img.size, img.mtime)) => {
                    seen.extend(keys.iter().map(|k| (*k).clone()));
                }
                _ => changed_images.push(img),
            }
        }
        let sacd_todo: Vec<SacdJob> = changed_images
            .par_iter()
            .filter_map(|s| {
                let image = ricercar_audio::dsd::sacd::SacdImage::open(&s.path).ok()?;
                (!image.tracks().is_empty()).then_some(SacdJob {
                    size: s.size,
                    mtime: s.mtime,
                    image,
                })
            })
            .collect();
        for job in &sacd_todo {
            seen.extend(job.keys().into_iter().map(|k| k.0));
        }
        report.total = seen.len();
        self.progress.total.store(
            todo.len() + cue_todo.len() + sacd_todo.len(),
            Ordering::Relaxed,
        );

        for chunk in todo.chunks(256) {
            let tagged: Vec<(&Scanned, TagInfo)> =
                chunk.par_iter().map(|s| (s, read_tags(&s.path))).collect();
            let mut conn = self.w();
            let Ok(tx) = conn.transaction() else { break };
            for (s, tags) in &tagged {
                match upsert_tags(&tx, &s.path, tags, s.size, s.mtime, None) {
                    Ok(true) => report.added += 1,
                    Ok(false) => report.updated += 1,
                    Err(e) => tracing::warn!("index {}: {e}", s.path.display()),
                }
            }
            let _ = tx.commit();
            drop(conn);
            self.progress.done.fetch_add(chunk.len(), Ordering::Relaxed);
            // Let views pick up big libraries progressively.
            self.touch();
        }

        for chunk in cue_todo.chunks(64) {
            let tagged: Vec<(&CueJob, TagInfo)> = chunk
                .par_iter()
                .map(|j| (*j, read_tags(&j.audio)))
                .collect();
            let mut conn = self.w();
            let Ok(tx) = conn.transaction() else { break };
            for (job, file_tags) in &tagged {
                let spans = job.sheet.spans();
                let sheet = job.sheet_path.to_string_lossy();
                for (key, size, i) in job.keys() {
                    let span = spans[i];
                    let tags = crate::cue::track_tags(&job.sheet, i, span, file_tags);
                    let cue = Some((span.start, span.end, sheet.as_ref()));
                    match upsert_tags(&tx, Path::new(&key), &tags, size, job.mtime, cue) {
                        Ok(true) => report.added += 1,
                        Ok(false) => report.updated += 1,
                        Err(e) => tracing::warn!("index {key}: {e}"),
                    }
                }
            }
            let _ = tx.commit();
            drop(conn);
            self.progress.done.fetch_add(chunk.len(), Ordering::Relaxed);
            self.touch();
        }

        if !sacd_todo.is_empty() {
            let mut conn = self.w();
            if let Ok(tx) = conn.transaction() {
                for job in &sacd_todo {
                    for (key, size, t) in job.keys() {
                        let tags = crate::sacd::track_tags(&job.image, t);
                        match upsert_tags(&tx, Path::new(&key), &tags, size, job.mtime, None) {
                            Ok(true) => report.added += 1,
                            Ok(false) => report.updated += 1,
                            Err(e) => tracing::warn!("index {key}: {e}"),
                        }
                    }
                }
                let _ = tx.commit();
            }
            drop(conn);
            self.progress
                .done
                .fetch_add(sacd_todo.len(), Ordering::Relaxed);
            self.touch();
        }

        let gone: Vec<&String> = known
            .keys()
            .filter(|p| {
                let path = Path::new(p.as_str());
                !seen.contains(*p)
                    && live_roots.iter().any(|r| path.starts_with(r))
                    && !unreadable.iter().any(|d| path.starts_with(d))
            })
            .collect();
        if !gone.is_empty() {
            let mut conn = self.w();
            if let Ok(tx) = conn.transaction() {
                for p in &gone {
                    let _ = tx.execute("DELETE FROM tracks WHERE path = ?1", [p]);
                }
                let _ = tx.commit();
            }
            report.removed = gone.len();
        }
        report
    }

    /// Index already tagged files (path, tags, size, mtime) in one
    /// transaction; returns how many were new. Used to build synthetic
    /// libraries for tests and benchmarks.
    pub fn upsert_many(&self, items: &[(PathBuf, TagInfo, i64, i64)]) -> usize {
        let mut added = 0;
        let mut conn = self.w();
        if let Ok(tx) = conn.transaction() {
            for (path, tags, size, mtime) in items {
                if upsert_tags(&tx, path, tags, *size, *mtime, None).unwrap_or(false) {
                    added += 1;
                }
            }
            let _ = tx.commit();
        }
        drop(conn);
        self.touch();
        added
    }

    /// Insert or refresh one file (watcher path). Returns true when new.
    pub fn upsert(&self, path: &Path) -> bool {
        let tags = read_tags(path);
        let md = std::fs::metadata(path).ok();
        let size = md.as_ref().map(|m| m.len() as i64).unwrap_or(0);
        let mtime = md.as_ref().map(mtime_of).unwrap_or(0);
        let res = upsert_tags(&self.w(), path, &tags, size, mtime, None);
        self.touch();
        res.unwrap_or(false)
    }

    pub fn remove(&self, path: &str) {
        let _ = self
            .w()
            .execute("DELETE FROM tracks WHERE path = ?1", [path]);
        self.touch();
    }

    /// Drop every track outside the given roots (after roots were removed
    /// from the settings).
    pub fn retain_roots(&self, roots: &[PathBuf]) -> usize {
        let paths: Vec<String> = self.query("SELECT path FROM tracks", [], |r| r.get(0));
        let mut n = 0;
        let mut conn = self.w();
        if let Ok(tx) = conn.transaction() {
            for p in paths
                .iter()
                .filter(|p| !roots.iter().any(|r| Path::new(p.as_str()).starts_with(r)))
            {
                n += tx
                    .execute("DELETE FROM tracks WHERE path = ?1", [p])
                    .unwrap_or(0);
            }
            let _ = tx.commit();
        }
        drop(conn);
        if n > 0 {
            self.touch();
        }
        n
    }

    // ------------------------------------------------------------ lookups

    pub fn count(&self) -> u32 {
        self.r()
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn has_path(&self, path: &str) -> bool {
        self.r()
            .query_row("SELECT 1 FROM tracks WHERE path = ?1", [path], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    pub fn track(&self, path: &str) -> Option<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t LEFT JOIN stats s ON s.path = t.path WHERE t.path = ?1"
            ),
            [path],
            row_to_track,
        )
        .into_iter()
        .next()
    }

    pub fn stats(&self) -> LibraryStats {
        self.r()
            .query_row(
                "SELECT COUNT(*), COUNT(DISTINCT album_id),
                    COUNT(DISTINCT COALESCE(album_artist, artist)),
                    COALESCE(SUM(duration_ms), 0), COALESCE(SUM(size), 0),
                    SUM(CASE WHEN sample_rate > 48000 OR bits > 16 THEN 1 ELSE 0 END)
                 FROM tracks",
                [],
                |r| {
                    Ok(LibraryStats {
                        tracks: r.get(0)?,
                        albums: r.get(1)?,
                        artists: r.get(2)?,
                        duration_ms: r.get::<_, i64>(3)? as u64,
                        size_bytes: r.get::<_, i64>(4)? as u64,
                        hires_tracks: r.get::<_, Option<u32>>(5)?.unwrap_or(0),
                    })
                },
            )
            .unwrap_or_default()
    }

    /// Albums and tracks per sample rate, lowest rate first (tracks with an
    /// unknown rate are left out). An album with several rates counts once
    /// per rate.
    pub fn rate_histogram(&self) -> Vec<RateCount> {
        self.query(
            "SELECT sample_rate, COUNT(DISTINCT album_id), COUNT(*) FROM tracks
             WHERE sample_rate > 0 GROUP BY sample_rate ORDER BY sample_rate",
            [],
            |r| {
                Ok(RateCount {
                    rate: r.get(0)?,
                    albums: r.get(1)?,
                    tracks: r.get(2)?,
                })
            },
        )
    }

    // ------------------------------------------------------------ albums

    pub fn albums(&self, sort: AlbumSort) -> Vec<Album> {
        let order = match sort {
            AlbumSort::Title => "MAX(t.album) COLLATE NOCASE",
            AlbumSort::Artist => {
                "COALESCE(MAX(t.album_artist), MAX(t.artist)) COLLATE NOCASE, MIN(t.year), MAX(t.album) COLLATE NOCASE"
            }
            AlbumSort::YearDesc => "MIN(t.year) DESC, MAX(t.album) COLLATE NOCASE",
            AlbumSort::RecentlyAdded => "MIN(t.added_at) DESC, MAX(t.album) COLLATE NOCASE",
        };
        self.query(
            &format!("{ALBUM_SELECT} GROUP BY t.album_id ORDER BY {order}"),
            [],
            row_to_album,
        )
    }

    pub fn album(&self, id: &str) -> Option<Album> {
        self.query(
            &format!("{ALBUM_SELECT} WHERE t.album_id = ?1 GROUP BY t.album_id"),
            [id],
            row_to_album,
        )
        .into_iter()
        .next()
    }

    fn albums_by_ids(&self, ids: &[String]) -> Vec<Album> {
        if ids.is_empty() {
            return Vec::new();
        }
        let marks = vec!["?"; ids.len()].join(",");
        let mut found: HashMap<String, Album> = self
            .query(
                &format!("{ALBUM_SELECT} WHERE t.album_id IN ({marks}) GROUP BY t.album_id"),
                params_from_iter(ids.iter()),
                row_to_album,
            )
            .into_iter()
            .map(|a| (a.id.clone(), a))
            .collect();
        ids.iter().filter_map(|id| found.remove(id)).collect()
    }

    pub fn album_tracks(&self, album_id: &str) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t LEFT JOIN stats s ON s.path = t.path
                 WHERE t.album_id = ?1
                 ORDER BY COALESCE(t.disc, 1), COALESCE(t.track, 0), t.path"
            ),
            [album_id],
            row_to_track,
        )
    }

    pub fn recently_added_albums(&self, limit: usize) -> Vec<Album> {
        self.query(
            &format!(
                "{ALBUM_SELECT} GROUP BY t.album_id ORDER BY MIN(t.added_at) DESC, MAX(t.album) LIMIT ?1"
            ),
            [limit as i64],
            row_to_album,
        )
    }

    pub fn recently_played_albums(&self, limit: usize) -> Vec<Album> {
        let ids: Vec<String> = self.query(
            "SELECT t.album_id FROM history h JOIN tracks t ON t.path = h.path
             GROUP BY t.album_id ORDER BY MAX(h.played_at) DESC LIMIT ?1",
            [limit as i64],
            |r| r.get(0),
        );
        self.albums_by_ids(&ids)
    }

    pub fn most_played_albums(&self, limit: usize) -> Vec<Album> {
        let ids: Vec<String> = self.query(
            "SELECT t.album_id FROM stats s JOIN tracks t ON t.path = s.path
             WHERE s.play_count > 0
             GROUP BY t.album_id ORDER BY SUM(s.play_count) DESC LIMIT ?1",
            [limit as i64],
            |r| r.get(0),
        );
        self.albums_by_ids(&ids)
    }

    pub fn favorite_albums(&self) -> Vec<Album> {
        let ids: Vec<String> = self.query(
            "SELECT album_id FROM album_favs ORDER BY added_at DESC",
            [],
            |r| r.get(0),
        );
        self.albums_by_ids(&ids)
    }

    pub fn set_album_favorite(&self, album_id: &str, fav: bool) {
        let conn = self.w();
        let _ = if fav {
            conn.execute(
                "INSERT OR IGNORE INTO album_favs (album_id, added_at) VALUES (?1, ?2)",
                params![album_id, now_unix()],
            )
        } else {
            conn.execute("DELETE FROM album_favs WHERE album_id = ?1", [album_id])
        };
        drop(conn);
        self.touch();
    }

    // ------------------------------------------------------------ artists & genres

    pub fn artists(&self) -> Vec<Artist> {
        self.query(
            &format!(
                "{ARTIST_SELECT} WHERE name IS NOT NULL AND name != ''
                 GROUP BY name COLLATE NOCASE ORDER BY name COLLATE NOCASE"
            ),
            [],
            row_to_artist,
        )
    }

    /// The artists with these names (any case), in no particular order.
    fn artists_named(&self, names: &[String]) -> Vec<Artist> {
        if names.is_empty() {
            return Vec::new();
        }
        let marks = vec!["?"; names.len()].join(",");
        self.query(
            &format!(
                "{ARTIST_SELECT} WHERE COALESCE(album_artist, artist) COLLATE NOCASE IN ({marks})
                 GROUP BY name COLLATE NOCASE"
            ),
            params_from_iter(names.iter()),
            row_to_artist,
        )
    }

    /// (own albums, albums the artist appears on).
    pub fn artist_albums(&self, name: &str) -> (Vec<Album>, Vec<Album>) {
        let own = self.query(
            &format!(
                "{ALBUM_SELECT} WHERE t.album_id IN (
                    SELECT album_id FROM tracks WHERE COALESCE(album_artist, artist) = ?1 COLLATE NOCASE)
                 GROUP BY t.album_id ORDER BY MIN(t.year) DESC, MAX(t.album)"
            ),
            [name],
            row_to_album,
        );
        let own_ids: HashSet<&str> = own.iter().map(|a| a.id.as_str()).collect();
        let appears = self
            .query(
                &format!(
                    "{ALBUM_SELECT} WHERE t.album_id IN (
                        SELECT album_id FROM tracks WHERE artist = ?1 COLLATE NOCASE
                           OR composer = ?1 COLLATE NOCASE)
                     GROUP BY t.album_id ORDER BY MIN(t.year) DESC, MAX(t.album)"
                ),
                [name],
                row_to_album,
            )
            .into_iter()
            .filter(|a| !own_ids.contains(a.id.as_str()))
            .collect();
        (own, appears)
    }

    /// Most played tracks by an artist (artist page "popular" list).
    pub fn artist_top_tracks(&self, name: &str, limit: usize) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t LEFT JOIN stats s ON s.path = t.path
                 WHERE t.artist = ?1 COLLATE NOCASE OR t.album_artist = ?1 COLLATE NOCASE
                 ORDER BY COALESCE(s.play_count, 0) DESC, COALESCE(s.favorite, 0) DESC, t.year DESC
                 LIMIT ?2"
            ),
            params![name, limit as i64],
            row_to_track,
        )
    }

    pub fn genres(&self) -> Vec<Genre> {
        self.query(
            "SELECT genre, COUNT(DISTINCT album_id), COUNT(*) FROM tracks
             WHERE genre IS NOT NULL AND genre != ''
             GROUP BY genre COLLATE NOCASE ORDER BY COUNT(*) DESC",
            [],
            |r| {
                Ok(Genre {
                    name: r.get(0)?,
                    album_count: r.get(1)?,
                    track_count: r.get(2)?,
                })
            },
        )
    }

    pub fn genre_albums(&self, genre: &str) -> Vec<Album> {
        self.query(
            &format!(
                "{ALBUM_SELECT} WHERE t.album_id IN (SELECT album_id FROM tracks WHERE genre = ?1 COLLATE NOCASE)
                 GROUP BY t.album_id ORDER BY COALESCE(MAX(t.album_artist), MAX(t.artist)) COLLATE NOCASE"
            ),
            [genre],
            row_to_album,
        )
    }

    // ------------------------------------------------------------ composers

    /// Composers (the COMPOSER tag), one per name in any case, with their
    /// album and track counts and a track to take a cover from.
    pub fn composers(&self) -> Vec<Artist> {
        self.query(
            &format!(
                "{COMPOSER_SELECT} WHERE name IS NOT NULL AND name != ''
                 GROUP BY name COLLATE NOCASE ORDER BY name COLLATE NOCASE"
            ),
            [],
            row_to_artist,
        )
    }

    /// The composers with these names (any case), in no particular order.
    fn composers_named(&self, names: &[String]) -> Vec<Artist> {
        if names.is_empty() {
            return Vec::new();
        }
        let marks = vec!["?"; names.len()].join(",");
        self.query(
            &format!(
                "{COMPOSER_SELECT} WHERE composer COLLATE NOCASE IN ({marks})
                 GROUP BY name COLLATE NOCASE"
            ),
            params_from_iter(names.iter()),
            row_to_artist,
        )
    }

    /// The albums holding works by a composer, newest first.
    pub fn composer_albums(&self, name: &str) -> Vec<Album> {
        self.query(
            &format!(
                "{ALBUM_SELECT} WHERE t.album_id IN (
                    SELECT album_id FROM tracks WHERE composer = ?1 COLLATE NOCASE)
                 GROUP BY t.album_id ORDER BY MIN(t.year) DESC, MAX(t.album), t.album_id"
            ),
            [name],
            row_to_album,
        )
    }

    /// A composer's tracks, album by album in the order of
    /// `composer_albums`, then by disc and track number.
    pub fn composer_tracks(&self, name: &str) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t LEFT JOIN stats s ON s.path = t.path
                 JOIN (SELECT album_id, MIN(year) AS year, MAX(album) AS album FROM tracks
                       WHERE album_id IN (SELECT album_id FROM tracks WHERE composer = ?1 COLLATE NOCASE)
                       GROUP BY album_id) a ON a.album_id = t.album_id
                 WHERE t.composer = ?1 COLLATE NOCASE
                 ORDER BY a.year DESC, a.album, t.album_id, COALESCE(t.disc,1), COALESCE(t.track,0), t.id"
            ),
            [name],
            row_to_track,
        )
    }

    // ------------------------------------------------------------ tracks

    pub fn tracks(&self, sort: TrackSort) -> Vec<Track> {
        self.tracks_range(sort, 0, usize::MAX)
    }

    /// A slice of `tracks(sort)`: pages of one sort always line up (ties are
    /// broken by row id), so a list can be loaded in several queries.
    pub fn tracks_range(&self, sort: TrackSort, offset: usize, limit: usize) -> Vec<Track> {
        let order = match sort {
            TrackSort::Artist => {
                "COALESCE(t.album_artist, t.artist) COLLATE NOCASE, t.year, t.album_id, COALESCE(t.disc,1), COALESCE(t.track,0)"
            }
            TrackSort::Title => "t.title COLLATE NOCASE",
            TrackSort::Album => "t.album COLLATE NOCASE, COALESCE(t.disc,1), COALESCE(t.track,0)",
            TrackSort::Duration => "t.duration_ms DESC",
            TrackSort::RecentlyAdded => "t.added_at DESC, t.path",
            TrackSort::MostPlayed => "COALESCE(s.play_count,0) DESC, t.title COLLATE NOCASE",
        };
        let limit = i64::try_from(limit).unwrap_or(-1);
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t LEFT JOIN stats s ON s.path = t.path
                 ORDER BY {order}, t.id LIMIT ?1 OFFSET ?2"
            ),
            params![limit, offset as i64],
            row_to_track,
        )
    }

    pub fn favorite_tracks(&self) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t JOIN stats s ON s.path = t.path
                 WHERE s.favorite = 1 ORDER BY COALESCE(t.album_artist, t.artist) COLLATE NOCASE, t.album, t.track"
            ),
            [],
            row_to_track,
        )
    }

    pub fn most_played_tracks(&self, limit: usize) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t JOIN stats s ON s.path = t.path
                 WHERE s.play_count > 0 ORDER BY s.play_count DESC, s.last_played DESC LIMIT ?1"
            ),
            [limit as i64],
            row_to_track,
        )
    }

    pub fn recently_played_tracks(&self, limit: usize) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t JOIN stats s ON s.path = t.path
                 WHERE s.last_played IS NOT NULL ORDER BY s.last_played DESC LIMIT ?1"
            ),
            [limit as i64],
            row_to_track,
        )
    }

    /// The last tracks played, most recent first, each once: library files
    /// and plugin tracks (from the metadata saved with the play).
    pub fn recent_plays(&self, limit: usize) -> Vec<Track> {
        let rows: Vec<(String, Option<String>)> = self.query(
            "SELECT h.path, h.info FROM history h
             JOIN (SELECT path, MAX(id) AS last FROM history GROUP BY path) l ON h.id = l.last
             ORDER BY h.id DESC LIMIT ?1",
            [(limit * 2) as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        );
        rows.into_iter()
            .filter_map(|(path, info)| match info {
                Some(json) if crate::plugin::is_plugin_uri(&path) => {
                    serde_json::from_str::<crate::controller::TrackInfo>(&json)
                        .ok()
                        .map(|i| Track::from_info(&i))
                }
                _ => self.track(&path),
            })
            .take(limit)
            .collect()
    }

    /// Random tracks (for a "shuffle all" / radio-like mix).
    pub fn random_tracks(&self, limit: usize) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t LEFT JOIN stats s ON s.path = t.path
                 ORDER BY RANDOM() LIMIT ?1"
            ),
            [limit as i64],
            row_to_track,
        )
    }

    pub fn set_favorite(&self, path: &str, fav: bool) {
        let _ = self.w().execute(
            "INSERT INTO stats (path, favorite) VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET favorite = ?2",
            params![path, fav as i64],
        );
        self.touch();
    }

    pub fn is_favorite(&self, path: &str) -> bool {
        self.r()
            .query_row("SELECT favorite FROM stats WHERE path = ?1", [path], |r| {
                r.get::<_, i64>(0)
            })
            .map(|v| v != 0)
            .unwrap_or(false)
    }

    pub fn record_play(&self, path: &str) {
        self.record_play_with(path, None);
    }

    /// Count a play: a library file by path, a plugin track by URI with its
    /// metadata (it has no row in `tracks`). Other streams are not counted.
    pub fn record_played(&self, info: &crate::controller::TrackInfo) {
        if crate::plugin::is_plugin_uri(&info.uri) {
            self.record_play_with(&info.uri, serde_json::to_string(info).ok());
        } else if let Some(p) = &info.path {
            self.record_play(p);
        }
    }

    fn record_play_with(&self, path: &str, info: Option<String>) {
        let now = now_unix();
        let conn = self.w();
        let _ = conn.execute(
            "INSERT INTO stats (path, play_count, last_played) VALUES (?1, 1, ?2)
             ON CONFLICT(path) DO UPDATE SET play_count = play_count + 1, last_played = ?2",
            params![path, now],
        );
        let _ = conn.execute(
            "INSERT INTO history (path, played_at, info) VALUES (?1, ?2, ?3)",
            params![path, now, info],
        );
        drop(conn);
        self.touch();
    }

    // ------------------------------------------------------------ search

    pub fn search(&self, q: &str) -> SearchResults {
        let Some(fq) = fts_query(q) else {
            return SearchResults::default();
        };
        // Rank inside the index first and join only the rows kept: short
        // prefixes match a large share of a big library.
        let tracks = self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM (
                    SELECT rowid, bm25(tracks_fts, 10.0, 5.0, 4.0, 4.0, 1.0, 2.0) AS score
                    FROM tracks_fts WHERE tracks_fts MATCH ?1 ORDER BY score LIMIT 200) m
                 JOIN tracks t ON t.id = m.rowid
                 LEFT JOIN stats s ON s.path = t.path
                 ORDER BY m.score"
            ),
            [&fq],
            row_to_track,
        );
        // An album ranks by its best track; the best 2000 tracks are enough
        // to find the best 40 albums unless albums are huge.
        let album_ids: Vec<String> = self.query(
            "SELECT t.album_id FROM (
                SELECT rowid, bm25(tracks_fts, 1.0, 5.0, 10.0, 8.0, 1.0, 2.0) AS score
                FROM tracks_fts WHERE tracks_fts MATCH ?1 ORDER BY score LIMIT 2000) m
             JOIN tracks t ON t.id = m.rowid
             GROUP BY t.album_id ORDER BY MIN(m.score) LIMIT 40",
            [format!("{{album album_artist artist}} : ({fq})")],
            |r| r.get(0),
        );
        let artist_names: Vec<String> = self.query(
            "SELECT COALESCE(t.album_artist, t.artist) AS n FROM tracks_fts f JOIN tracks t ON t.id = f.rowid
             WHERE tracks_fts MATCH ?1 AND n IS NOT NULL
             GROUP BY n COLLATE NOCASE ORDER BY COUNT(*) DESC LIMIT 12",
            [format!("{{artist album_artist}} : ({fq})")],
            |r| r.get(0),
        );
        let mut artists = self.artists_named(&artist_names);
        artists.sort_by_key(|a| {
            artist_names
                .iter()
                .position(|n| n.eq_ignore_ascii_case(&a.name))
                .unwrap_or(usize::MAX)
        });
        let composer_names: Vec<String> = self.query(
            "SELECT t.composer AS n FROM tracks_fts f JOIN tracks t ON t.id = f.rowid
             WHERE tracks_fts MATCH ?1 AND n IS NOT NULL
             GROUP BY n COLLATE NOCASE ORDER BY COUNT(*) DESC LIMIT 12",
            [format!("{{composer}} : ({fq})")],
            |r| r.get(0),
        );
        let mut composers = self.composers_named(&composer_names);
        composers.sort_by_key(|a| {
            composer_names
                .iter()
                .position(|n| n.eq_ignore_ascii_case(&a.name))
                .unwrap_or(usize::MAX)
        });
        SearchResults {
            artists,
            composers,
            albums: self.albums_by_ids(&album_ids),
            tracks,
        }
    }

    // ------------------------------------------------------------ playlists

    pub fn playlists(&self) -> Vec<Playlist> {
        self.query(
            &format!("{PLAYLIST_SELECT} GROUP BY p.id ORDER BY p.name COLLATE NOCASE"),
            [],
            row_to_playlist,
        )
    }

    pub fn playlist(&self, id: i64) -> Option<Playlist> {
        self.query(
            &format!("{PLAYLIST_SELECT} WHERE p.id = ?1 GROUP BY p.id"),
            [id],
            row_to_playlist,
        )
        .into_iter()
        .next()
    }

    pub fn create_playlist(&self, name: &str) -> i64 {
        let conn = self.w();
        let now = now_unix();
        let _ = conn.execute(
            "INSERT INTO playlists (name, created_at, updated_at) VALUES (?1, ?2, ?2)",
            params![name, now],
        );
        let id = conn.last_insert_rowid();
        drop(conn);
        self.touch();
        id
    }

    pub fn rename_playlist(&self, id: i64, name: &str) {
        let _ = self.w().execute(
            "UPDATE playlists SET name = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, name, now_unix()],
        );
        self.touch();
    }

    pub fn delete_playlist(&self, id: i64) {
        let conn = self.w();
        let _ = conn.execute("DELETE FROM playlist_items WHERE playlist_id = ?1", [id]);
        let _ = conn.execute("DELETE FROM playlists WHERE id = ?1", [id]);
        drop(conn);
        self.touch();
    }

    /// Library tracks and plugin entries of a playlist, in order; files
    /// that left the library are skipped.
    pub fn playlist_tracks(&self, id: i64) -> Vec<Track> {
        self.playlist_entries(id)
            .into_iter()
            .map(|(t, _)| t)
            .collect()
    }

    /// `playlist_tracks` with when each entry was added (Unix seconds;
    /// `None` for entries older than that record).
    pub fn playlist_entries(&self, id: i64) -> Vec<(Track, Option<i64>)> {
        let rows: Vec<Option<(Track, Option<i64>)>> = self.query(
            &format!(
                "SELECT {TRACK_COLS}, i.info, i.added_at FROM playlist_items i
                 LEFT JOIN tracks t ON t.path = i.path
                 LEFT JOIN stats s ON s.path = i.path
                 WHERE i.playlist_id = ?1 ORDER BY i.pos"
            ),
            [id],
            |r| {
                let added: Option<i64> = r.get(TRACK_NCOLS + 1)?;
                if r.get::<_, Option<String>>(0)?.is_some() {
                    return row_to_track(r).map(|t| Some((t, added)));
                }
                let info: Option<String> = r.get(TRACK_NCOLS)?;
                Ok(info
                    .and_then(|j| serde_json::from_str::<crate::controller::TrackInfo>(&j).ok())
                    .filter(|i| crate::plugin::is_plugin_uri(&i.uri))
                    .map(|i| (Track::from_info(&i), added)))
            },
        );
        rows.into_iter().flatten().collect()
    }

    /// Every stored entry, in order.
    fn playlist_items(&self, id: i64) -> Vec<PlaylistItem> {
        self.query(
            "SELECT path, info, added_at FROM playlist_items WHERE playlist_id = ?1 ORDER BY pos",
            [id],
            |r| {
                Ok(PlaylistItem {
                    path: r.get(0)?,
                    info: r.get(1)?,
                    added_at: r.get(2)?,
                })
            },
        )
    }

    /// Whether each stored entry shows in `playlist_tracks`, in order.
    fn playlist_visibility(&self, id: i64) -> Vec<bool> {
        self.query(
            "SELECT t.path IS NOT NULL, i.info FROM playlist_items i
             LEFT JOIN tracks t ON t.path = i.path
             WHERE i.playlist_id = ?1 ORDER BY i.pos",
            [id],
            |r| {
                let in_library: bool = r.get(0)?;
                let info: Option<String> = r.get(1)?;
                Ok(in_library
                    || info
                        .and_then(|j| serde_json::from_str::<crate::controller::TrackInfo>(&j).ok())
                        .is_some_and(|i| crate::plugin::is_plugin_uri(&i.uri)))
            },
        )
    }

    fn set_playlist_items(&self, id: i64, items: &[PlaylistItem]) {
        let mut conn = self.w();
        let res = conn.transaction().and_then(|tx| {
            tx.execute("DELETE FROM playlist_items WHERE playlist_id = ?1", [id])?;
            for (pos, it) in items.iter().enumerate() {
                tx.execute(
                    "INSERT INTO playlist_items (playlist_id, pos, path, info, added_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, pos as i64, it.path, it.info, it.added_at],
                )?;
            }
            tx.execute(
                "UPDATE playlists SET updated_at = ?2 WHERE id = ?1",
                params![id, now_unix()],
            )?;
            tx.commit()
        });
        drop(conn);
        if let Err(e) = res {
            tracing::warn!("playlist {id} not saved: {e}");
            return;
        }
        self.touch();
    }

    #[cfg(test)]
    fn playlist_paths(&self, id: i64) -> Vec<String> {
        self.playlist_items(id)
            .into_iter()
            .map(|it| it.path)
            .collect()
    }

    /// Replace the whole item list (simplest correct way to reorder/remove);
    /// entries already there keep their saved metadata and the time they
    /// were added, new ones are added now.
    pub fn set_playlist_paths(&self, id: i64, paths: &[String]) {
        let mut known: HashMap<String, PlaylistItem> = HashMap::new();
        for it in self.playlist_items(id) {
            known.entry(it.path.clone()).or_insert(it);
        }
        let now = now_unix();
        let items: Vec<PlaylistItem> = paths
            .iter()
            .map(|p| {
                known.get(p).cloned().unwrap_or_else(|| PlaylistItem {
                    path: p.clone(),
                    info: None,
                    added_at: Some(now),
                })
            })
            .collect();
        self.set_playlist_items(id, &items);
    }

    /// Append queue entries: library files by path, plugin tracks by URI
    /// with their metadata (shown even when the plugin is gone).
    pub fn add_infos_to_playlist(&self, id: i64, infos: &[crate::controller::TrackInfo]) {
        let mut all = self.playlist_items(id);
        let added_at = Some(now_unix());
        for i in infos {
            if crate::plugin::is_plugin_uri(&i.uri) {
                all.push(PlaylistItem {
                    path: i.uri.clone(),
                    info: serde_json::to_string(i).ok(),
                    added_at,
                });
            } else if let Some(p) = &i.path {
                all.push(PlaylistItem {
                    path: p.clone(),
                    info: None,
                    added_at,
                });
            }
        }
        self.set_playlist_items(id, &all);
    }

    pub fn add_to_playlist(&self, id: i64, paths: &[String]) {
        let mut all = self.playlist_items(id);
        let added_at = Some(now_unix());
        all.extend(paths.iter().map(|p| PlaylistItem {
            path: p.clone(),
            info: None,
            added_at,
        }));
        self.set_playlist_items(id, &all);
    }

    /// Remove the items at the given positions (as returned by `playlist_tracks`
    /// order, which skips files no longer in the library).
    pub fn remove_from_playlist(&self, id: i64, positions: &[usize]) {
        // Stored index of each visible entry.
        let stored: Vec<usize> = self
            .playlist_visibility(id)
            .into_iter()
            .enumerate()
            .filter_map(|(i, v)| v.then_some(i))
            .collect();
        let drop: HashSet<usize> = positions
            .iter()
            .filter_map(|&i| stored.get(i).copied())
            .collect();
        if drop.is_empty() {
            return;
        }
        let remaining: Vec<PlaylistItem> = self
            .playlist_items(id)
            .into_iter()
            .enumerate()
            .filter_map(|(i, item)| (!drop.contains(&i)).then_some(item))
            .collect();
        self.set_playlist_items(id, &remaining);
    }

    pub fn move_in_playlist(&self, id: i64, from: usize, to: usize) {
        let mut items = self.playlist_items(id);
        if from < items.len() && to < items.len() {
            let it = items.remove(from);
            items.insert(to, it);
            self.set_playlist_items(id, &items);
        }
    }

    pub fn export_m3u(&self, id: i64, dest: &Path) -> std::io::Result<()> {
        let mut out = String::from("#EXTM3U\n");
        // plugin:// entries mean nothing to other players.
        for t in self
            .playlist_tracks(id)
            .into_iter()
            .filter(|t| !t.is_plugin())
        {
            // Other players cannot play part of a file: a cue or SACD
            // track is a comment they skip, which `import_m3u` reads back.
            if crate::cue::split_key(&t.path).is_some() {
                out.push_str(&format!("{CUE_M3U}{}\n", t.path));
                continue;
            }
            if crate::sacd::split_key(&t.path).is_some() {
                out.push_str(&format!("{SACD_M3U}{}\n", t.path));
                continue;
            }
            out.push_str(&format!(
                "#EXTINF:{},{} - {}\n{}\n",
                t.duration_ms / 1000,
                t.artist.as_deref().unwrap_or(""),
                t.title,
                t.path
            ));
        }
        std::fs::write(dest, out)
    }

    /// Import an M3U/M3U8 file; entries not in the library are skipped.
    pub fn import_m3u(&self, src: &Path) -> std::io::Result<i64> {
        let text = std::fs::read_to_string(src)?;
        let base = src.parent().unwrap_or(Path::new("/"));
        let paths: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| {
                !l.is_empty()
                    && (!l.starts_with('#') || l.starts_with(CUE_M3U) || l.starts_with(SACD_M3U))
            })
            .map(|l| {
                if let Some(key) = l.strip_prefix(CUE_M3U).or_else(|| l.strip_prefix(SACD_M3U)) {
                    return key.to_string();
                }
                let p = crate::meta::uri_to_path(l).unwrap_or_else(|| PathBuf::from(l));
                let p = if p.is_absolute() { p } else { base.join(p) };
                p.to_string_lossy().into_owned()
            })
            .filter(|p| self.has_path(p))
            .collect();
        let name = src
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Imported".into());
        let id = self.create_playlist(&name);
        self.set_playlist_paths(id, &paths);
        Ok(id)
    }
}

/// A stored playlist entry: a library path or a plugin URI.
#[derive(Debug, Clone)]
struct PlaylistItem {
    path: String,
    /// Saved metadata of a plugin entry.
    info: Option<String>,
    /// Unix seconds; `None` for entries older than the column.
    added_at: Option<i64>,
}

/// M3U comment line holding a cue track's library key.
const CUE_M3U: &str = "#RICERCAR-CUE:";
/// Likewise for a track of a SACD image.
const SACD_M3U: &str = "#RICERCAR-SACD:";

/// Span of a cue track in its audio file (frames) and the sheet's path.
type CueCols<'a> = (u64, Option<u64>, &'a str);

/// Returns Ok(true) when the row is new. `path` is the library key (that
/// of a cue track for `cue`).
fn upsert_tags(
    conn: &Connection,
    path: &Path,
    tags: &TagInfo,
    size: i64,
    mtime: i64,
    cue: Option<CueCols<'_>>,
) -> rusqlite::Result<bool> {
    let key = path.to_string_lossy();
    let existed = conn
        .query_row("SELECT 1 FROM tracks WHERE path = ?1", [&key], |_| Ok(()))
        .optional()?
        .is_some();
    let dir = path
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    conn.execute(
        "INSERT INTO tracks (path, dir, title, artist, album_artist, album, album_id, track, disc,
            year, genre, composer, duration_ms, sample_rate, bits, channels, bitrate, codec,
            rg_track_gain, rg_track_peak, rg_album_gain, rg_album_peak, mb_album_id, mb_track_id,
            size, mtime, added_at, cue_start, cue_end, cue_sheet)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30)
         ON CONFLICT(path) DO UPDATE SET dir=?2, title=?3, artist=?4, album_artist=?5, album=?6,
            album_id=?7, track=?8, disc=?9, year=?10, genre=?11, composer=?12, duration_ms=?13,
            sample_rate=?14, bits=?15, channels=?16, bitrate=?17, codec=?18, rg_track_gain=?19,
            rg_track_peak=?20, rg_album_gain=?21, rg_album_peak=?22, mb_album_id=?23,
            mb_track_id=?24, size=?25, mtime=?26, cue_start=?28, cue_end=?29, cue_sheet=?30",
        params![
            key,
            dir,
            tags.title.clone().unwrap_or_default(),
            tags.artist,
            tags.album_artist,
            tags.album,
            album_id_for(tags, path),
            tags.track,
            tags.disc,
            tags.year,
            tags.genre,
            tags.composer,
            tags.duration_ms as i64,
            tags.sample_rate,
            tags.bits,
            tags.channels,
            tags.bitrate,
            tags.codec,
            tags.rg_track_gain,
            tags.rg_track_peak,
            tags.rg_album_gain,
            tags.rg_album_peak,
            tags.mb_album_id,
            tags.mb_track_id,
            size,
            mtime,
            now_unix(),
            cue.map(|c| c.0 as i64),
            cue.and_then(|c| c.1).map(|e| e as i64),
            cue.map(|c| c.2),
        ],
    )?;
    Ok(!existed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(title: &str, artist: &str, album: &str, aa: Option<&str>, n: u32) -> TagInfo {
        TagInfo {
            title: Some(title.into()),
            artist: Some(artist.into()),
            album: Some(album.into()),
            album_artist: aa.map(Into::into),
            track: Some(n),
            duration_ms: 180_000,
            sample_rate: Some(96_000),
            bits: Some(24),
            genre: Some("Jazz".into()),
            ..Default::default()
        }
    }

    fn lib_with(rows: &[(&str, TagInfo)]) -> Library {
        let lib = Library::in_memory().unwrap();
        for (p, t) in rows {
            upsert_tags(&lib.w(), Path::new(p), t, 1, 1, None).unwrap();
        }
        lib
    }

    #[test]
    fn albums_group_by_album_artist_or_dir() {
        let lib = lib_with(&[
            (
                "/m/kob/1.flac",
                tags(
                    "So What",
                    "Miles Davis",
                    "Kind of Blue",
                    Some("Miles Davis"),
                    1,
                ),
            ),
            (
                "/m/kob/2.flac",
                tags(
                    "Freddie",
                    "Miles Davis",
                    "Kind of Blue",
                    Some("Miles Davis"),
                    2,
                ),
            ),
            ("/m/comp/1.flac", tags("A", "X", "Hits", None, 1)),
            ("/m/comp/2.flac", tags("B", "Y", "Hits", None, 2)),
            ("/m/other/1.flac", tags("C", "Z", "Hits", None, 1)),
        ]);
        let albums = lib.albums(AlbumSort::Title);
        assert_eq!(albums.len(), 3);
        let comp = albums.iter().find(|a| a.dir == "/m/comp").unwrap();
        assert_eq!(comp.artist, "Various Artists");
        assert_eq!(comp.track_count, 2);
        let kob = albums.iter().find(|a| a.title == "Kind of Blue").unwrap();
        assert!(kob.is_hires());
        let tracks = lib.album_tracks(&kob.id);
        assert_eq!(tracks[0].title, "So What");
        assert_eq!(lib.stats().albums, 3);
        assert_eq!(
            lib.rate_histogram(),
            [RateCount {
                rate: 96_000,
                albums: 3,
                tracks: 5
            }]
        );
    }

    #[test]
    fn composers_list_their_albums_and_tracks() {
        let work = |title: &str, album: &str, year: i32, n: u32, composer: Option<&str>| TagInfo {
            year: Some(year),
            composer: composer.map(Into::into),
            ..tags(title, "Glenn Gould", album, Some("Glenn Gould"), n)
        };
        let lib = lib_with(&[
            (
                "/m/gb/1.flac",
                work(
                    "Aria",
                    "Goldberg Variations",
                    1981,
                    1,
                    Some("Johann Sebastian Bach"),
                ),
            ),
            (
                "/m/gb/2.flac",
                work(
                    "Variatio 1",
                    "Goldberg Variations",
                    1981,
                    2,
                    Some("Johann Sebastian Bach"),
                ),
            ),
            (
                "/m/mix/2.flac",
                work(
                    "Sonata K. 545",
                    "Recital",
                    1955,
                    2,
                    Some("Wolfgang Amadeus Mozart"),
                ),
            ),
            (
                "/m/mix/1.flac",
                work(
                    "Partita No. 2",
                    "Recital",
                    1955,
                    1,
                    Some("johann sebastian bach"),
                ),
            ),
            (
                "/m/mix/3.flac",
                work("Improvisation", "Recital", 1955, 3, None),
            ),
        ]);
        let composers = lib.composers();
        let names: Vec<_> = composers.iter().map(|c| c.name.to_lowercase()).collect();
        assert_eq!(names, ["johann sebastian bach", "wolfgang amadeus mozart"]);
        // One entry per name in any case.
        assert_eq!((composers[0].album_count, composers[0].track_count), (2, 3));
        assert_eq!((composers[1].album_count, composers[1].track_count), (1, 1));

        let albums = lib.composer_albums("JOHANN SEBASTIAN BACH");
        let titles: Vec<_> = albums.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["Goldberg Variations", "Recital"]);
        // The whole album is listed, works by others included.
        assert_eq!(albums[1].track_count, 3);
        // Tracks follow the albums, then their numbers.
        let tracks = lib.composer_tracks("Johann Sebastian Bach");
        let titles: Vec<_> = tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["Aria", "Variatio 1", "Partita No. 2"]);
        assert!(lib.composer_tracks("Nobody").is_empty());

        let r = lib.search("mozart");
        assert_eq!(r.composers.len(), 1);
        assert_eq!(r.composers[0].name, "Wolfgang Amadeus Mozart");
        // Artists are not composers.
        assert!(lib.search("gould").composers.is_empty());
    }

    #[test]
    fn fts_search_prefix_and_diacritics() {
        let lib = lib_with(&[
            (
                "/m/a/1.flac",
                tags("Jóga", "Björk", "Homogenic", Some("Björk"), 1),
            ),
            (
                "/m/b/1.flac",
                tags(
                    "So What",
                    "Miles Davis",
                    "Kind of Blue",
                    Some("Miles Davis"),
                    1,
                ),
            ),
        ]);
        let r = lib.search("bjork");
        assert_eq!(r.tracks.len(), 1);
        assert_eq!(r.artists[0].name, "Björk");
        let r = lib.search("mil kind");
        assert_eq!(r.albums.len(), 1);
        assert_eq!(r.albums[0].title, "Kind of Blue");
        assert!(lib.search("  ").is_empty());
        // update keeps FTS in sync
        upsert_tags(
            &lib.w(),
            Path::new("/m/b/1.flac"),
            &tags(
                "Blue in Green",
                "Miles Davis",
                "Kind of Blue",
                Some("Miles Davis"),
                3,
            ),
            2,
            2,
            None,
        )
        .unwrap();
        assert!(lib.search("what").tracks.is_empty());
        assert_eq!(lib.search("green").tracks.len(), 1);
    }

    #[test]
    fn stats_favorites_and_history() {
        let lib = lib_with(&[
            ("/m/a/1.flac", tags("One", "A", "Al", Some("A"), 1)),
            ("/m/a/2.flac", tags("Two", "A", "Al", Some("A"), 2)),
        ]);
        let rev = lib.revision();
        lib.record_play("/m/a/2.flac");
        lib.record_play("/m/a/2.flac");
        lib.set_favorite("/m/a/1.flac", true);
        assert!(lib.revision() > rev);
        assert_eq!(lib.most_played_tracks(5)[0].play_count, 2);
        assert_eq!(lib.favorite_tracks()[0].title, "One");
        assert!(lib.track("/m/a/1.flac").unwrap().favorite);
        assert_eq!(lib.recently_played_albums(5).len(), 1);
        let id = lib.albums(AlbumSort::Title)[0].id.clone();
        lib.set_album_favorite(&id, true);
        assert!(lib.album(&id).unwrap().favorite);
        assert_eq!(lib.favorite_albums().len(), 1);

        // Plugin plays join the history with their metadata; each track
        // shows once, most recent first; other streams are not counted.
        let plugin = crate::controller::TrackInfo {
            uri: crate::plugin::plugin_uri("demo", "track/9"),
            title: "Far".into(),
            ..Default::default()
        };
        lib.record_played(&plugin);
        lib.record_played(&crate::controller::TrackInfo {
            uri: "https://radio.example/stream".into(),
            title: "Radio".into(),
            ..Default::default()
        });
        lib.record_play("/m/a/1.flac");
        let recent: Vec<(String, bool)> = lib
            .recent_plays(10)
            .into_iter()
            .map(|t| (t.title.clone(), t.is_plugin()))
            .collect();
        assert_eq!(
            recent,
            [
                ("One".to_string(), false),
                ("Far".to_string(), true),
                ("Two".to_string(), false)
            ]
        );
    }

    #[test]
    fn playlists_crud_and_m3u() {
        let dir = tempfile::tempdir().unwrap();
        let lib = lib_with(&[
            ("/m/a/1.flac", tags("One", "A", "Al", Some("A"), 1)),
            ("/m/a/2.flac", tags("Two", "A", "Al", Some("A"), 2)),
            ("/m/a/3.flac", tags("Three", "A", "Al", Some("A"), 3)),
        ]);
        let id = lib.create_playlist("Mix");
        lib.add_to_playlist(
            id,
            &[
                "/m/a/3.flac".into(),
                "/m/a/1.flac".into(),
                "/m/a/2.flac".into(),
            ],
        );
        lib.move_in_playlist(id, 0, 2);
        let titles: Vec<String> = lib
            .playlist_tracks(id)
            .into_iter()
            .map(|t| t.title)
            .collect();
        assert_eq!(titles, ["One", "Two", "Three"]);
        lib.remove_from_playlist(id, &[1]);
        assert_eq!(lib.playlist(id).unwrap().track_count, 2);

        let m3u = dir.path().join("mix.m3u8");
        lib.export_m3u(id, &m3u).unwrap();
        let id2 = lib.import_m3u(&m3u).unwrap();
        assert_eq!(lib.playlist_tracks(id2).len(), 2);
        // Plugin tracks live in playlists by URI, with their metadata.
        let plugin = crate::controller::TrackInfo {
            uri: crate::plugin::plugin_uri("demo", "track/9"),
            title: "Far".into(),
            duration_ms: 60_000,
            cover: Some("https://x/c.jpg".into()),
            ..Default::default()
        };
        let local = crate::controller::TrackInfo::from(&lib.track("/m/a/3.flac").unwrap());
        lib.add_infos_to_playlist(id2, &[plugin.clone(), local]);
        let t = lib.playlist_tracks(id2);
        assert_eq!(t.len(), 4);
        assert!(t[2].is_plugin() && t[2].title == "Far");
        assert_eq!(
            crate::controller::TrackInfo::from(&t[2]).cover,
            plugin.cover
        );
        assert_eq!(crate::controller::TrackInfo::from(&t[2]).path, None);
        let p = lib.playlist(id2).unwrap();
        assert_eq!((p.track_count, p.duration_ms), (4, 3 * 180_000 + 60_000));
        lib.move_in_playlist(id2, 2, 0);
        assert_eq!(lib.playlist_tracks(id2)[0].title, "Far");
        lib.export_m3u(id2, &m3u).unwrap();
        assert!(!std::fs::read_to_string(&m3u).unwrap().contains("plugin://"));
        lib.remove_from_playlist(id2, &[0]);
        assert_eq!(lib.playlist_tracks(id2).len(), 3);
        lib.rename_playlist(id2, "Copy");
        lib.delete_playlist(id);
        assert_eq!(lib.playlists().len(), 1);
        assert_eq!(lib.playlists()[0].name, "Copy");
    }

    #[test]
    fn remove_from_playlist_uses_the_position() {
        let lib = lib_with(&[
            ("/m/a/1.flac", tags("One", "A", "Al", Some("A"), 1)),
            ("/m/a/2.flac", tags("Two", "A", "Al", Some("A"), 2)),
        ]);
        let id = lib.create_playlist("Dup");
        lib.add_to_playlist(
            id,
            &[
                "/m/gone.flac".into(),
                "/m/a/1.flac".into(),
                "/m/a/2.flac".into(),
                "/m/a/1.flac".into(),
            ],
        );
        // Visible positions skip the missing file.
        lib.remove_from_playlist(id, &[2]);
        assert_eq!(
            lib.playlist_paths(id),
            ["/m/gone.flac", "/m/a/1.flac", "/m/a/2.flac"]
        );
        lib.remove_from_playlist(id, &[0]);
        assert_eq!(lib.playlist_paths(id), ["/m/gone.flac", "/m/a/2.flac"]);
    }

    #[test]
    fn playlist_entries_keep_when_they_were_added() {
        let lib = lib_with(&[
            ("/m/a/1.flac", tags("One", "A", "Al", Some("A"), 1)),
            ("/m/a/2.flac", tags("Two", "A", "Al", Some("A"), 2)),
        ]);
        let id = lib.create_playlist("Mix");
        lib.add_to_playlist(id, &["/m/a/1.flac".into(), "/m/a/2.flac".into()]);
        // An entry from before the column: no date.
        lib.w()
            .execute(
                "UPDATE playlist_items SET added_at = NULL WHERE path = '/m/a/1.flac'",
                [],
            )
            .unwrap();
        lib.w()
            .execute(
                "UPDATE playlist_items SET added_at = 100 WHERE path = '/m/a/2.flac'",
                [],
            )
            .unwrap();
        let dates = |lib: &Library| -> Vec<(String, Option<i64>)> {
            lib.playlist_entries(id)
                .into_iter()
                .map(|(t, a)| (t.title, a))
                .collect()
        };
        assert_eq!(
            dates(&lib),
            [("One".into(), None), ("Two".into(), Some(100))]
        );
        // Moving, removing and adding rewrite the list; dates stay.
        lib.move_in_playlist(id, 1, 0);
        lib.add_to_playlist(id, &["/m/a/1.flac".into()]);
        let d = dates(&lib);
        assert_eq!(d[0], ("Two".into(), Some(100)));
        assert_eq!(d[1], ("One".into(), None));
        assert!(d[2].1.is_some_and(|t| t >= now_unix() - 60));
        lib.remove_from_playlist(id, &[0]);
        assert_eq!(dates(&lib)[0], ("One".into(), None));
    }

    #[test]
    fn roots_are_path_prefixes_not_string_prefixes() {
        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("music");
        let hires = dir.path().join("music-hires");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::create_dir_all(&hires).unwrap();
        std::fs::write(music.join("a.flac"), b"x").unwrap();
        std::fs::write(hires.join("b.flac"), b"x").unwrap();
        let lib = Library::open(&dir.path().join("lib.db")).unwrap();
        lib.scan_roots(&[music.clone(), hires.clone()]);
        assert_eq!(lib.count(), 2);
        // Scanning one root leaves the other alone.
        assert_eq!(lib.scan_roots(std::slice::from_ref(&music)).removed, 0);
        assert_eq!(lib.count(), 2);
        assert_eq!(lib.retain_roots(std::slice::from_ref(&music)), 1);
        assert!(lib.has_path(&music.join("a.flac").to_string_lossy()));
    }

    #[test]
    fn unreadable_directories_are_not_pruned() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("m");
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("a.flac"), b"x").unwrap();
        let lib = Library::open(&dir.path().join("lib.db")).unwrap();
        lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!(lib.count(), 1);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(&locked).is_ok();
        let r = lib.scan_roots(std::slice::from_ref(&root));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !readable {
            assert_eq!(r.removed, 0);
            assert_eq!(lib.count(), 1);
        }
    }

    #[test]
    fn reopening_keeps_user_data() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lib.db");
        {
            let lib = Library::open(&db).unwrap();
            lib.upsert_many(&[(
                "/m/a/1.flac".into(),
                tags("One", "A", "Al", Some("A"), 1),
                1,
                1,
            )]);
            lib.record_play("/m/a/1.flac");
        }
        let lib = Library::open(&db).unwrap();
        assert_eq!(lib.count(), 1);
        assert_eq!(lib.most_played_tracks(5)[0].play_count, 1);
    }

    #[test]
    fn dsf_and_dff_are_indexed_with_tags_and_covers() {
        use ricercar_audio::dsd::{DsdFormat, fixture, id3};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let al = root.join("Sonatas");
        std::fs::create_dir_all(&al).unwrap();
        let f64 = DsdFormat {
            rate: 2_822_400,
            channels: 2,
        };
        let f128 = DsdFormat {
            rate: 5_644_800,
            channels: 2,
        };
        let data = fixture::sine(f64, 440.0, 0.4, 0.5);
        let tags = |t: &str, n: &str| {
            id3::build(
                &[
                    ("title", t),
                    ("artist", "Ensemble Lumen"),
                    ("album", "Sonatas"),
                    ("tracknumber", n),
                    ("date", "2018-05-01"),
                    ("genre", "Classical"),
                ],
                Some((b"\x89PNG fake cover", "image/png")),
            )
        };
        std::fs::write(
            al.join("01.dsf"),
            fixture::dsf(f64, &data, &tags("Allegro", "1/3")),
        )
        .unwrap();
        std::fs::write(
            al.join("02.dff"),
            fixture::dff(f128, &data, &tags("Adagio", "2/3"), ""),
        )
        .unwrap();
        std::fs::write(
            al.join("03.dff"),
            fixture::dff_dst(f64, &data, &tags("Presto", "3/3"), fixture::dst_plain_frame),
        )
        .unwrap();
        let lib = Library::open(&dir.path().join("db/lib.db")).unwrap();
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!(r.added, 3);
        let tracks = lib.tracks(TrackSort::Title);
        let by = |t: &str| tracks.iter().find(|x| x.title == t).unwrap().clone();
        let a = by("Allegro");
        assert_eq!(a.artist.as_deref(), Some("Ensemble Lumen"));
        assert_eq!(a.album.as_deref(), Some("Sonatas"));
        assert_eq!((a.track, a.year), (Some(1), Some(2018)));
        assert_eq!(a.genre.as_deref(), Some("Classical"));
        assert_eq!((a.sample_rate, a.bits), (Some(2_822_400), Some(1)));
        assert_eq!(a.codec.as_deref(), Some("DSF"));
        assert_eq!(a.duration_ms, 500);
        assert!(a.is_hires());
        let b = by("Adagio");
        assert_eq!(
            (b.codec.as_deref(), b.sample_rate),
            (Some("DFF"), Some(5_644_800))
        );
        assert_eq!(b.duration_ms, 250);
        let c = by("Presto");
        assert_eq!(c.codec.as_deref(), Some("DST"));
        // Whole DST frames: 0.5 s rounded up to 1/75 s.
        assert_eq!(c.duration_ms, 506);
        let al = &lib.albums(AlbumSort::Title)[0];
        assert_eq!((al.title.as_str(), al.track_count), ("Sonatas", 3));
        let cover = crate::meta::cover_bytes(Path::new(&a.path)).unwrap();
        assert_eq!(
            cover,
            (b"\x89PNG fake cover".to_vec(), "image/png".to_string())
        );
    }

    #[test]
    fn scan_is_incremental_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("al")).unwrap();
        std::fs::write(root.join("al/a.flac"), b"not really flac").unwrap();
        std::fs::write(root.join("al/b.mp3"), b"nope").unwrap();
        std::fs::write(root.join("al/cover.jpg"), b"").unwrap();
        let lib = Library::open(&dir.path().join("db/lib.db")).unwrap();
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!((r.added, r.total), (2, 2));
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!((r.added, r.updated, r.removed), (0, 0, 0));
        std::fs::remove_file(root.join("al/b.mp3")).unwrap();
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!(r.removed, 1);
        // a missing root never prunes
        let r = lib.scan_roots(&[root.join("gone")]);
        assert_eq!(r.removed, 0);
        assert_eq!(lib.count(), 1);
        // untagged file falls back to file name / dir name
        let a = &lib.albums(AlbumSort::Title)[0];
        assert_eq!(a.title, "al");
        assert_eq!(lib.retain_roots(&[]), 1);
    }
    /// A 16-bit mono WAV of silence with RIFF INFO tags.
    fn write_wav(path: &Path, secs: u32, info: &[(&[u8; 4], &str)]) {
        let rate = 44_100u32;
        let data_len = rate * secs * 2;
        let mut list = b"INFO".to_vec();
        for (id, v) in info {
            let mut v = v.as_bytes().to_vec();
            v.push(0);
            if v.len() % 2 == 1 {
                v.push(0);
            }
            list.extend(*id);
            list.extend((v.len() as u32).to_le_bytes());
            list.extend(v);
        }
        let mut f = b"RIFF".to_vec();
        f.extend((4 + 24 + 8 + list.len() as u32 + 8 + data_len).to_le_bytes());
        f.extend(b"WAVEfmt ");
        f.extend(16u32.to_le_bytes());
        f.extend(1u16.to_le_bytes());
        f.extend(1u16.to_le_bytes());
        f.extend(rate.to_le_bytes());
        f.extend((rate * 2).to_le_bytes());
        f.extend(2u16.to_le_bytes());
        f.extend(16u16.to_le_bytes());
        f.extend(b"LIST");
        f.extend((list.len() as u32).to_le_bytes());
        f.extend(list);
        f.extend(b"data");
        f.extend(data_len.to_le_bytes());
        f.resize(f.len() + data_len as usize, 0);
        std::fs::write(path, f).unwrap();
    }

    /// Give a file a new mtime, a few seconds later than before.
    fn bump_mtime(path: &Path, secs: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        let t = f.metadata().unwrap().modified().unwrap();
        f.set_modified(t + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    const CUE: &str = "REM DATE 1999\r\nPERFORMER \"Disc Artist\"\r\nTITLE \"Disc Title\"\r\n\
        FILE \"album.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n    TITLE \"One\"\r\n\
        PERFORMER \"Guest\"\r\n    INDEX 01 00:00:00\r\n  TRACK 02 AUDIO\r\n    TITLE \"Two\"\r\n\
        INDEX 00 00:01:50\r\n    INDEX 01 00:02:00\r\n  TRACK 03 AUDIO\r\n\
        INDEX 01 00:04:15\r\n";

    fn cue_rig() -> (tempfile::TempDir, PathBuf, Library) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("music");
        std::fs::create_dir_all(root.join("al")).unwrap();
        // The sheet names the FLAC it was ripped to; the file is a WAV.
        write_wav(
            &root.join("al/album.wav"),
            6,
            &[
                (b"IART", "File Artist"),
                (b"IPRD", "File Album"),
                (b"IGNR", "FileGenre"),
            ],
        );
        std::fs::write(root.join("al/album.cue"), CUE).unwrap();
        let lib = Library::open(&dir.path().join("db/lib.db")).unwrap();
        (dir, root, lib)
    }

    #[test]
    fn cue_sheet_cuts_its_audio_file_into_tracks() {
        let (_dir, root, lib) = cue_rig();
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!((r.added, r.total), (3, 3));
        let audio = root.join("al/album.wav");
        assert!(
            !lib.has_path(&audio.to_string_lossy()),
            "the file itself is hidden"
        );
        let albums = lib.albums(AlbumSort::Title);
        assert_eq!(albums.len(), 1);
        let al = &albums[0];
        assert_eq!(
            (al.title.as_str(), al.artist.as_str()),
            ("Disc Title", "Disc Artist")
        );
        assert_eq!(al.year, Some(1999));
        assert_eq!(
            al.genre.as_deref(),
            Some("FileGenre"),
            "gaps filled from the file"
        );
        assert_eq!(al.duration_ms, 6000);
        let t = lib.album_tracks(&al.id);
        let show: Vec<_> = t
            .iter()
            .map(|t| {
                (
                    t.track,
                    t.title.as_str(),
                    t.artist.as_deref(),
                    t.duration_ms,
                )
            })
            .collect();
        assert_eq!(
            show,
            [
                (Some(1), "One", Some("Guest"), 2000),
                (Some(2), "Two", Some("Disc Artist"), 2200),
                (Some(3), "Track 03", Some("Disc Artist"), 1800),
            ]
        );
        let base = file_uri(&audio);
        assert_eq!(t[0].path, format!("{}#cue=1", audio.display()));
        assert_eq!(t[0].uri, format!("{base}#t=0,2"));
        assert_eq!(t[1].uri, format!("{base}#t=2,4.2"));
        assert_eq!(t[2].uri, format!("{base}#t=4.2"));
        assert_eq!(lib.search("two").tracks.len(), 1);
        // Size counted once.
        assert_eq!(
            lib.stats().size_bytes,
            std::fs::metadata(&audio).unwrap().len()
        );
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!((r.added, r.updated, r.removed), (0, 0, 0));
    }

    #[test]
    fn editing_or_removing_the_cue_sheet_updates_its_tracks() {
        let (_dir, root, lib) = cue_rig();
        lib.scan_roots(std::slice::from_ref(&root));
        let two = format!("{}#cue=2", root.join("al/album.wav").display());
        lib.record_play(&two);
        lib.set_favorite(&two, true);
        let pid = lib.create_playlist("p");
        lib.add_to_playlist(pid, std::slice::from_ref(&two));

        // Track 3 dropped, track 2 renamed.
        let sheet = root.join("al/album.cue");
        let edited = CUE.replace("\"Two\"", "\"Deux\"");
        let edited = &edited[..edited.find("  TRACK 03").unwrap()];
        std::fs::write(&sheet, edited).unwrap();
        bump_mtime(&sheet, 5);
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!((r.updated, r.removed, r.total), (2, 1, 2));
        let t = lib.track(&two).unwrap();
        assert_eq!(t.title, "Deux");
        assert_eq!((t.play_count, t.favorite), (1, true), "stats kept");
        assert_eq!(t.duration_ms, 4000, "now the last track of the file");
        assert!(t.uri.ends_with("#t=2"));
        assert_eq!(lib.playlist_tracks(pid).len(), 1);

        // Without its sheet the file is an ordinary track again.
        std::fs::remove_file(&sheet).unwrap();
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!((r.added, r.removed, r.total), (1, 2, 1));
        let audio = root.join("al/album.wav").to_string_lossy().into_owned();
        let t = lib.track(&audio).unwrap();
        assert_eq!(t.album.as_deref(), Some("File Album"));
        assert_eq!(t.uri, file_uri(Path::new(&audio)));
    }

    #[test]
    fn cue_tracks_in_m3u_round_trip() {
        let (dir, root, lib) = cue_rig();
        lib.scan_roots(std::slice::from_ref(&root));
        let key = format!("{}#cue=2", root.join("al/album.wav").display());
        let pid = lib.create_playlist("cue");
        lib.add_to_playlist(pid, std::slice::from_ref(&key));
        let m3u = dir.path().join("cue.m3u8");
        lib.export_m3u(pid, &m3u).unwrap();
        let text = std::fs::read_to_string(&m3u).unwrap();
        assert_eq!(text, format!("#EXTM3U\n#RICERCAR-CUE:{key}\n"));
        let back = lib.import_m3u(&m3u).unwrap();
        let t = lib.playlist_tracks(back);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].title, "Two");
    }

    fn write_sacd(path: &Path, tracks: &[(u32, &str)]) {
        use ricercar_audio::dsd::sacd::fixture;
        let spec = fixture::Spec::new(false, 1, tracks);
        std::fs::write(path, fixture::image(&spec, fixture::plain_frame)).unwrap();
    }

    fn sacd_rig() -> (tempfile::TempDir, PathBuf, Library) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("music");
        std::fs::create_dir_all(root.join("al")).unwrap();
        write_sacd(&root.join("al/disc.iso"), &[(75, "One"), (38, "Two")]);
        std::fs::write(root.join("al/cover.jpg"), b"jpeg").unwrap();
        // An ISO that is not a SACD image is skipped without a word.
        std::fs::write(root.join("al/data.iso"), vec![0u8; 600 * 2048]).unwrap();
        let lib = Library::open(&dir.path().join("db/lib.db")).unwrap();
        (dir, root, lib)
    }

    #[test]
    fn sacd_image_is_indexed_as_its_tracks() {
        let (_dir, root, lib) = sacd_rig();
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!((r.added, r.total), (2, 2));
        let image = root.join("al/disc.iso");
        assert!(!lib.has_path(&image.to_string_lossy()));
        let albums = lib.albums(AlbumSort::Title);
        assert_eq!(albums.len(), 1);
        let al = &albums[0];
        assert_eq!(
            (al.title.as_str(), al.artist.as_str()),
            ("Café Album", "Some Ensemble")
        );
        assert_eq!((al.year, al.genre.as_deref()), (Some(2001), Some("Jazz")));
        assert_eq!(al.duration_ms, 1000 + 506);
        assert_eq!(al.max_rate, Some(2_822_400));
        let t = lib.album_tracks(&al.id);
        let show: Vec<_> = t
            .iter()
            .map(|t| {
                (
                    t.track,
                    t.title.as_str(),
                    t.artist.as_deref(),
                    t.duration_ms,
                )
            })
            .collect();
        assert_eq!(
            show,
            [
                (Some(1), "One", Some("Player"), 1000),
                (Some(2), "Two", Some("Player"), 506),
            ]
        );
        assert_eq!(t[1].path, format!("{}#sacd=2", image.display()));
        assert_eq!(t[1].uri, format!("{}#sacd=2", file_uri(&image)));
        assert_eq!(
            (t[0].codec.as_deref(), t[0].sample_rate, t[0].channels),
            (Some("SACD"), Some(2_822_400), Some(2))
        );
        assert_eq!(lib.search("two").tracks.len(), 1);
        assert_eq!(
            lib.stats().size_bytes,
            std::fs::metadata(&image).unwrap().len(),
            "size counted once"
        );
        // Covers come from the folder; the image itself has none.
        let p = Path::new(&t[0].path);
        assert_eq!(crate::meta::on_disk(p), image);
        assert_eq!(
            crate::meta::folder_cover(p),
            Some(root.join("al/cover.jpg"))
        );
        assert_eq!(crate::meta::embedded_cover(p), None);
        assert_eq!(crate::meta::local_lyrics(p), None);
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!((r.added, r.updated, r.removed), (0, 0, 0));
    }

    #[test]
    fn changing_or_removing_a_sacd_image_updates_its_tracks() {
        let (_dir, root, lib) = sacd_rig();
        lib.scan_roots(std::slice::from_ref(&root));
        let image = root.join("al/disc.iso");
        let two = crate::sacd::key(&image, 2);
        lib.record_play(&two);
        lib.set_favorite(&two, true);
        let pid = lib.create_playlist("p");
        lib.add_to_playlist(pid, std::slice::from_ref(&two));

        // A new image in place of the old one: three tracks.
        write_sacd(&image, &[(75, "Uno"), (75, "Dos"), (75, "Tres")]);
        bump_mtime(&image, 5);
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!((r.added, r.updated, r.removed, r.total), (1, 2, 0, 3));
        let t = lib.track(&two).unwrap();
        assert_eq!((t.title.as_str(), t.duration_ms), ("Dos", 1000));
        assert_eq!((t.play_count, t.favorite), (1, true), "stats kept");
        assert_eq!(lib.playlist_tracks(pid).len(), 1);

        std::fs::remove_file(&image).unwrap();
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!((r.removed, r.total), (3, 0));
        assert_eq!(lib.count(), 0);
    }

    #[test]
    fn sacd_tracks_in_m3u_round_trip() {
        let (dir, root, lib) = sacd_rig();
        lib.scan_roots(std::slice::from_ref(&root));
        let key = crate::sacd::key(&root.join("al/disc.iso"), 2);
        let pid = lib.create_playlist("sacd");
        lib.add_to_playlist(pid, std::slice::from_ref(&key));
        let m3u = dir.path().join("sacd.m3u8");
        lib.export_m3u(pid, &m3u).unwrap();
        let text = std::fs::read_to_string(&m3u).unwrap();
        assert_eq!(text, format!("#EXTM3U\n#RICERCAR-SACD:{key}\n"));
        let back = lib.import_m3u(&m3u).unwrap();
        let t = lib.playlist_tracks(back);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].title, "Two");
    }

    #[test]
    fn upgrade_from_v2_keeps_user_data() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lib.db");
        {
            let lib = Library::open(&db).unwrap();
            lib.upsert_many(&[(
                "/m/a/1.flac".into(),
                tags("One", "A", "Al", Some("A"), 1),
                1,
                1,
            )]);
            lib.record_play("/m/a/1.flac");
            lib.set_favorite("/m/a/1.flac", true);
            let pid = lib.create_playlist("p");
            lib.add_to_playlist(pid, &["/m/a/1.flac".to_string()]);
        }
        {
            // Back to what version 2 looked like.
            let c = Connection::open(&db).unwrap();
            c.execute_batch(
                "ALTER TABLE tracks DROP COLUMN cue_start;
                 ALTER TABLE tracks DROP COLUMN cue_end;
                 ALTER TABLE tracks DROP COLUMN cue_sheet;
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let lib = Library::open(&db).unwrap();
        let t = lib.track("/m/a/1.flac").unwrap();
        assert_eq!((t.play_count, t.favorite), (1, true));
        assert_eq!(lib.playlists()[0].track_count, 1);
        assert_eq!(lib.recent_plays(5).len(), 1);
        let v: i32 = lib
            .r()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }
}
