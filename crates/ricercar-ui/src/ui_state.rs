//! Interface state kept between runs (`$XDG_DATA_HOME/ricercar/ui-state.json`):
//! window geometry, last page, sorts and filters, queue drawer, recent
//! searches. Distinct from the config file: nothing here is a setting.

use std::path::{Path, PathBuf};

use ricercar_core::writer::{Durability, LatestWriter};
use serde::{Deserialize, Serialize};

use crate::Page;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    /// Logical size.
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
    /// Physical position; X11 only (Wayland never reports it).
    pub x: Option<i32>,
    pub y: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub window: Option<WindowState>,
    pub page: String,
    pub page_arg: String,
    pub album_sort: i32,
    pub track_sort: i32,
    pub albums_hires_only: bool,
    pub queue_open: bool,
    /// Most recent first.
    pub recent_searches: Vec<String>,
    /// Last known capabilities of each hw: device.
    pub dac_caps: crate::dac::CapsCache,
    /// Readable names of PipeWire outputs seen before (node name, name),
    /// most recent first: a device that is not connected keeps its name.
    pub device_names: Vec<(String, String)>,
}

/// How many PipeWire output names are remembered.
pub const DEVICE_NAMES_MAX: usize = 50;

impl Default for UiState {
    fn default() -> Self {
        UiState {
            window: None,
            page: page_name(Page::Home).into(),
            page_arg: String::new(),
            album_sort: 0,
            track_sort: 0,
            albums_hires_only: false,
            queue_open: false,
            recent_searches: Vec::new(),
            dac_caps: Default::default(),
            device_names: Vec::new(),
        }
    }
}

pub fn default_path() -> PathBuf {
    ricercar_core::config::data_dir().join("ui-state.json")
}

impl UiState {
    /// Missing or unreadable file: defaults.
    pub fn load(path: &Path) -> UiState {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Queue a write on `io` and return at once. ui-state.json is a cache:
    /// no fsync while running (a slow sync froze the window), only the last
    /// save at exit is synced ([`save_durable`](Self::save_durable)).
    pub fn submit(&self, path: &Path, io: &LatestWriter) {
        match serde_json::to_vec_pretty(self) {
            Ok(json) => io.submit(path, json, Durability::Cache),
            Err(e) => tracing::warn!("save ui-state.json: {e}"),
        }
    }

    /// Write now, synced, after what `io` is writing; drops a queued older
    /// version.
    pub fn save_durable(&self, path: &Path, io: &LatestWriter) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        io.write_now(path, &json, Durability::Durable)
    }

    pub fn page(&self) -> Page {
        page_from_name(&self.page).unwrap_or(Page::Home)
    }

    /// The readable name remembered for a PipeWire node.
    pub fn device_name(&self, node: &str) -> Option<&str> {
        self.device_names
            .iter()
            .find(|(n, _)| n == node)
            .map(|(_, name)| name.as_str())
    }

    /// Remember a node's readable name. `recent` moves it to the front (the
    /// user picked it); otherwise a known node keeps its place. Returns
    /// whether anything changed.
    pub fn remember_device(&mut self, node: &str, name: &str, recent: bool) -> bool {
        if node.is_empty() || name.is_empty() {
            return false;
        }
        let pos = self.device_names.iter().position(|(n, _)| n == node);
        match pos {
            Some(i) if self.device_names[i].1 == name && (!recent || i == 0) => return false,
            Some(i) if !recent => {
                self.device_names[i].1 = name.to_string();
                return true;
            }
            Some(i) => {
                self.device_names.remove(i);
            }
            None => {}
        }
        self.device_names
            .insert(0, (node.to_string(), name.to_string()));
        self.device_names.truncate(DEVICE_NAMES_MAX);
        true
    }
}

const PAGES: [(Page, &str); 17] = [
    (Page::Home, "home"),
    (Page::Albums, "albums"),
    (Page::Album, "album"),
    (Page::Artists, "artists"),
    (Page::Artist, "artist"),
    (Page::Tracks, "tracks"),
    (Page::Genres, "genres"),
    (Page::Genre, "genre"),
    (Page::Composers, "composers"),
    (Page::Composer, "composer"),
    (Page::Favorites, "favorites"),
    (Page::Playlist, "playlist"),
    (Page::Radio, "radio"),
    (Page::Search, "search"),
    (Page::Settings, "settings"),
    (Page::Browse, "browse"),
    (Page::Plugins, "plugins"),
];

pub fn page_name(p: Page) -> &'static str {
    PAGES
        .iter()
        .find(|(q, _)| *q == p)
        .map(|(_, n)| *n)
        .unwrap_or("home")
}

pub fn page_from_name(n: &str) -> Option<Page> {
    PAGES.iter().find(|(_, m)| *m == n).map(|(p, _)| *p)
}

/// The page to open at startup. The search page depends on text that is not
/// kept, a plugin page on a plugin that is still starting, and an album or
/// playlist may have gone since: fall back to Home.
pub fn start_page(
    st: &UiState,
    album_exists: impl Fn(&str) -> bool,
    playlist_exists: impl Fn(i64) -> bool,
) -> (Page, String) {
    let page = st.page();
    let arg = st.page_arg.clone();
    let ok = match page {
        Page::Search | Page::Browse => false,
        Page::Album => album_exists(&arg),
        Page::Playlist => arg.parse().is_ok_and(&playlist_exists),
        Page::Artist | Page::Genre | Page::Composer => !arg.is_empty(),
        _ => true,
    };
    if ok {
        (page, arg)
    } else {
        (Page::Home, String::new())
    }
}

/// A saved size is only reused when it is plausible (not a collapsed or
/// absurd window from a broken session).
pub fn usable_size(w: &WindowState) -> Option<(f32, f32)> {
    let ok = |v: f32| v.is_finite() && (320.0..=16384.0).contains(&v);
    (ok(w.width) && ok(w.height)).then_some((w.width, w.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ui-state.json");
        assert_eq!(UiState::load(&p), UiState::default());
        let st = UiState {
            window: Some(WindowState {
                width: 1600.0,
                height: 1000.0,
                maximized: false,
                x: None,
                y: None,
            }),
            page: "artists".into(),
            album_sort: 1,
            track_sort: 2,
            albums_hires_only: true,
            queue_open: true,
            recent_searches: vec!["bach".into()],
            ..Default::default()
        };
        let io = LatestWriter::new("test-io");
        st.submit(&p, &io);
        io.flush();
        let back = UiState::load(&p);
        assert_eq!(back, st);
        assert_eq!(back.page(), Page::Artists);
        // Unknown or partial files fall back field by field.
        std::fs::write(&p, r#"{"page":"nowhere","track_sort":3}"#).unwrap();
        let st = UiState::load(&p);
        assert_eq!((st.page(), st.track_sort, st.window), (Page::Home, 3, None));
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(UiState::load(&p), UiState::default());
    }

    #[test]
    fn device_names_are_capped_and_optional() {
        let mut st = UiState::default();
        assert!(st.remember_device("bluez_output.00_11_22_33_44_55.1", "Headset", false));
        assert!(!st.remember_device("bluez_output.00_11_22_33_44_55.1", "Headset", false));
        assert_eq!(
            st.device_name("bluez_output.00_11_22_33_44_55.1"),
            Some("Headset")
        );
        assert_eq!(st.device_name("tunnel.office"), None);
        assert!(!st.remember_device("", "x", false));
        // A renamed device keeps its place unless picked.
        assert!(st.remember_device("tunnel.office", "Office", false));
        assert!(st.remember_device("bluez_output.00_11_22_33_44_55.1", "Phones", false));
        assert_eq!(st.device_names[1].1, "Phones");
        assert!(st.remember_device("bluez_output.00_11_22_33_44_55.1", "Phones", true));
        assert_eq!(st.device_names[0].0, "bluez_output.00_11_22_33_44_55.1");
        assert!(!st.remember_device("bluez_output.00_11_22_33_44_55.1", "Phones", true));
        for i in 0..60 {
            st.remember_device(&format!("tunnel.n{i}"), &format!("N{i}"), false);
        }
        assert_eq!(st.device_names.len(), DEVICE_NAMES_MAX);
        assert_eq!(st.device_names[0].1, "N59");
        assert_eq!(st.device_name("tunnel.office"), None);
        // Saved and read back; files from before load with no names.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ui-state.json");
        st.save_durable(&p, &LatestWriter::new("test-io")).unwrap();
        assert_eq!(UiState::load(&p).device_names, st.device_names);
        std::fs::write(&p, r#"{"page":"albums","dac_caps":{}}"#).unwrap();
        let old = UiState::load(&p);
        assert_eq!((old.page(), old.device_names.len()), (Page::Albums, 0));
    }

    #[test]
    fn saves_while_running_are_not_synced() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        let io = LatestWriter::with_sink("test-io", move |_, bytes, durability| {
            s.lock().unwrap().push((bytes.to_vec(), durability));
            Ok(())
        });
        let p = Path::new("ui-state.json");
        let mut st = UiState::default();
        st.submit(p, &io);
        io.flush();
        st.queue_open = true;
        st.save_durable(p, &io).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].1, Durability::Cache);
        assert_eq!(seen.last().unwrap().1, Durability::Durable);
        let last: UiState = serde_json::from_slice(&seen.last().unwrap().0).unwrap();
        assert!(last.queue_open);
    }

    #[test]
    fn page_names_cover_every_page() {
        for (p, n) in PAGES {
            assert_eq!(page_from_name(n), Some(p));
            assert_eq!(page_name(p), n);
        }
    }

    #[test]
    fn start_page_falls_back_home() {
        let st = |page: &str, arg: &str| UiState {
            page: page.into(),
            page_arg: arg.into(),
            ..Default::default()
        };
        let yes = |_: &str| true;
        let no = |_: &str| false;
        let pl = |id: i64| id == 7;
        assert_eq!(start_page(&st("artists", ""), no, pl).0, Page::Artists);
        assert_eq!(start_page(&st("album", "abc"), yes, pl).0, Page::Album);
        assert_eq!(start_page(&st("album", "abc"), no, pl).0, Page::Home);
        assert_eq!(start_page(&st("playlist", "7"), no, pl).0, Page::Playlist);
        assert_eq!(start_page(&st("playlist", "8"), no, pl).0, Page::Home);
        assert_eq!(start_page(&st("search", ""), yes, pl).0, Page::Home);
        assert_eq!(start_page(&st("artist", ""), yes, pl).0, Page::Home);
        assert_eq!(start_page(&st("composers", ""), no, pl).0, Page::Composers);
        assert_eq!(
            start_page(&st("composer", "Bach"), no, pl).0,
            Page::Composer
        );
        assert_eq!(start_page(&st("composer", ""), yes, pl).0, Page::Home);
    }

    #[test]
    fn sizes_are_sanity_checked() {
        let w = |width, height| WindowState {
            width,
            height,
            maximized: false,
            x: None,
            y: None,
        };
        assert_eq!(usable_size(&w(1600.0, 1000.0)), Some((1600.0, 1000.0)));
        assert_eq!(usable_size(&w(0.0, 1000.0)), None);
        assert_eq!(usable_size(&w(f32::NAN, 900.0)), None);
    }
}
