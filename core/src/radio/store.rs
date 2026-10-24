//! What Radio remembers between runs (`radio.json` in the config folder): favourite stations,
//! recently played ones, recent searches and places visited. Kept locally, never on YouTube.
//!
//! A file that can't be read is set aside as `radio.json.bad-<unix time>` rather than written
//! over, and [`Store::damaged`] says so, for the app to tell the user.

use super::{PlaceRef, Station};
use crate::paths::{config_dir, write_atomic};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FILE: &str = "radio.json";
const RECENT: usize = 30;
const SEARCHES: usize = 12;
const VISITED: usize = 12;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RadioData {
    /// In the order they were added.
    pub favourites: Vec<Station>,
    /// Most recent first.
    pub recent: Vec<Station>,
    /// Station searches, most recent first.
    pub searches: Vec<String>,
    /// Places and countries browsed, most recent first.
    pub visited: Vec<PlaceRef>,
}

pub struct Store {
    path: PathBuf,
    pub data: RadioData,
    /// The file couldn't be read when loading (it was set aside).
    pub damaged: bool,
}

/// Moves an unreadable file out of the way, to `<name>.bad-<unix time>` next to it.
fn set_aside(path: &Path) -> Option<PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.push(format!(".bad-{}", crate::paths::unix_now()));
    let to = path.with_file_name(name);
    match std::fs::rename(path, &to) {
        Ok(()) => Some(to),
        Err(e) => {
            log::warn!("setting {} aside: {e}", path.display());
            None
        }
    }
}

impl Store {
    pub fn load() -> Self {
        Self::load_at(config_dir().join(FILE))
    }

    pub fn load_at(path: PathBuf) -> Self {
        let mut damaged = false;
        let mut data: RadioData = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                log::warn!("{FILE} can't be read ({e}); setting it aside");
                set_aside(&path);
                damaged = true;
                RadioData::default()
            }),
            Err(_) => RadioData::default(),
        };
        let ok = |s: &Station| super::valid_id(&s.id) && !s.title.is_empty();
        data.favourites.retain(ok);
        data.recent.retain(ok);
        data.visited.retain(|p| super::valid_id(&p.id));
        Store {
            path,
            data,
            damaged,
        }
    }

    fn save(&self) {
        let json = serde_json::to_vec_pretty(&self.data).expect("json");
        if let Err(e) = write_atomic(&self.path, &json) {
            log::warn!("saving {}: {e}", FILE);
        }
    }

    pub fn is_favourite(&self, id: &str) -> bool {
        self.data.favourites.iter().any(|s| s.id == id)
    }

    /// Adds or removes a favourite. Returns whether it is one now.
    pub fn set_favourite(&mut self, st: &Station, on: bool) -> bool {
        let at = self.data.favourites.iter().position(|s| s.id == st.id);
        match (at, on) {
            (None, true) => self.data.favourites.push(st.clone()),
            (Some(i), false) => {
                self.data.favourites.remove(i);
            }
            _ => return on,
        }
        self.save();
        on
    }

    /// A station started playing: it moves to the front of the recents.
    pub fn played(&mut self, st: &Station) {
        if self.data.recent.first() == Some(st) {
            return;
        }
        self.data.recent.retain(|s| s.id != st.id);
        self.data.recent.insert(0, st.clone());
        self.data.recent.truncate(RECENT);
        self.save();
    }

    pub fn forget_played(&mut self, id: &str) {
        let n = self.data.recent.len();
        self.data.recent.retain(|s| s.id != id);
        if self.data.recent.len() != n {
            self.save();
        }
    }

    pub fn searched(&mut self, q: &str) {
        let q = q.trim();
        if q.is_empty() || self.data.searches.first().is_some_and(|s| s == q) {
            return;
        }
        self.data.searches.retain(|s| !s.eq_ignore_ascii_case(q));
        self.data.searches.insert(0, q.to_string());
        self.data.searches.truncate(SEARCHES);
        self.save();
    }

    pub fn forget_search(&mut self, q: &str) {
        let n = self.data.searches.len();
        self.data.searches.retain(|s| s != q);
        if self.data.searches.len() != n {
            self.save();
        }
    }

    pub fn visited(&mut self, p: &PlaceRef) {
        if self.data.visited.first().is_some_and(|v| v.id == p.id) {
            return;
        }
        self.data.visited.retain(|v| v.id != p.id);
        self.data.visited.insert(0, p.clone());
        self.data.visited.truncate(VISITED);
        self.save();
    }

    pub fn forget_visited(&mut self, id: &str) {
        let n = self.data.visited.len();
        self.data.visited.retain(|v| v.id != id);
        if self.data.visited.len() != n {
            self.save();
        }
    }

    /// What's known of a station, from favourites or recents.
    pub fn station(&self, id: &str) -> Option<&Station> {
        self.data
            .favourites
            .iter()
            .chain(&self.data.recent)
            .find(|s| s.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(id: &str, title: &str) -> Station {
        Station {
            id: id.into(),
            title: title.into(),
            place: "London".into(),
            country: "United Kingdom".into(),
            place_id: Some("0eZoYyEW".into()),
        }
    }

    #[test]
    fn remembers_across_loads() {
        let dir = std::env::temp_dir().join(format!("ember-radio-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(FILE);
        let _ = std::fs::remove_file(&path);
        let (kiss, jazz) = (st("hcx4T119", "Kiss FM"), st("J4zz", "Jazz FM"));

        let mut s = Store::load_at(path.clone());
        assert!(s.set_favourite(&kiss, true));
        assert!(s.set_favourite(&kiss, true), "twice is still once");
        s.played(&kiss);
        s.played(&jazz);
        s.played(&kiss);
        s.searched("jazz");
        s.searched("bbc");
        s.searched("Jazz");
        s.visited(&PlaceRef {
            id: "0eZoYyEW".into(),
            title: "London".into(),
            ..Default::default()
        });

        let mut s = Store::load_at(path.clone());
        assert!(s.is_favourite("hcx4T119") && !s.is_favourite("J4zz"));
        assert_eq!(s.data.favourites.len(), 1);
        let recent: Vec<&str> = s.data.recent.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(recent, ["Kiss FM", "Jazz FM"]);
        assert_eq!(s.data.searches, ["Jazz", "bbc"]);
        assert_eq!(s.data.visited[0].title, "London");
        assert_eq!(s.station("J4zz").unwrap().place, "London");

        assert!(!s.set_favourite(&kiss, false));
        s.forget_played("J4zz");
        s.forget_search("bbc");
        s.forget_visited("0eZoYyEW");
        let s = Store::load_at(path.clone());
        assert!(s.data.favourites.is_empty());
        assert_eq!(s.data.recent.len(), 1);
        assert_eq!(s.data.searches, ["Jazz"]);
        assert!(s.data.visited.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sets_an_unreadable_file_aside() {
        let dir = std::env::temp_dir().join(format!("ember-radio-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        let junk = b"{\"favourites\": [ oops";
        std::fs::write(&path, junk).unwrap();
        let mut s = Store::load_at(path.clone());
        assert!(s.damaged);
        assert!(s.data.favourites.is_empty());
        assert!(!path.exists());
        let aside: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(aside.len(), 1);
        let name = aside[0].file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("radio.json.bad-"), "{name}");
        assert_eq!(std::fs::read(&aside[0]).unwrap(), junk);
        // Saving afterwards leaves it alone, and the new file reads fine.
        s.searched("jazz");
        assert_eq!(std::fs::read(&aside[0]).unwrap(), junk);
        assert!(!Store::load_at(path).damaged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drops_damaged_entries() {
        let dir = std::env::temp_dir().join(format!("ember-radio-bad-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(FILE);
        std::fs::write(
            &path,
            br#"{"favourites":[{"id":"../x","title":"Bad"},{"id":"ok1","title":"Good"}],"recent":[{"id":"ok2","title":""}]}"#,
        )
        .unwrap();
        let s = Store::load_at(path);
        assert_eq!(s.data.favourites.len(), 1);
        assert!(s.data.recent.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
