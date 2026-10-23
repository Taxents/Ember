//! Artwork: fetched at the size it's shown, cached on disk, decoded on a worker, and kept in a
//! small in-memory cache that's emptied when the window is minimized.

use music_core::paths::cache_dir;
use music_core::yt::model::{Thumb, best_thumb, sized};
use sha1::{Digest, Sha1};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::collections::HashMap;
use std::path::PathBuf;

const MEMORY_ITEMS: usize = 200;
const DISK_BYTES: u64 = 300 * 1024 * 1024;

/// The URL to fetch for showing `thumbs` at `px` physical pixels square.
pub fn url_for(thumbs: &[Thumb], px: u32) -> Option<String> {
    let t = best_thumb(thumbs, px)?;
    Some(
        if t.url.contains("googleusercontent.com") || t.url.contains("ggpht.com") {
            sized(&t.url, px, px)
        } else {
            t.url.clone()
        },
    )
}

pub struct Images {
    mem: HashMap<String, (Image, u64)>,
    pending: std::collections::HashSet<String>,
    tick: u64,
}

impl Images {
    pub fn new() -> Self {
        Images {
            mem: HashMap::new(),
            pending: Default::default(),
            tick: 0,
        }
    }

    /// The image if it's in memory. Otherwise starts loading it (once) and returns `None`;
    /// `loaded` runs on the UI thread with the URL when it arrives.
    pub fn get(&mut self, url: &str, px: u32, loaded: fn(String, Image)) -> Option<Image> {
        self.tick += 1;
        if let Some((img, t)) = self.mem.get_mut(url) {
            *t = self.tick;
            return Some(img.clone());
        }
        if self.pending.insert(url.to_string()) {
            let url = url.to_string();
            crate::workers::spawn(move || {
                let buf = load(&url, px);
                let _ = slint::invoke_from_event_loop(move || match buf {
                    Some(b) => loaded(url, Image::from_rgba8(b)),
                    None => loaded(url, Image::default()),
                });
            });
        }
        None
    }

    /// Stores a loaded image (called from the `loaded` callback).
    pub fn insert(&mut self, url: &str, img: Image) {
        self.pending.remove(url);
        if img.size().width == 0 {
            return;
        }
        self.tick += 1;
        self.mem.insert(url.to_string(), (img, self.tick));
        if self.mem.len() > MEMORY_ITEMS {
            let mut by_age: Vec<(u64, String)> =
                self.mem.iter().map(|(k, (_, t))| (*t, k.clone())).collect();
            by_age.sort();
            for (_, k) in by_age.into_iter().take(self.mem.len() - MEMORY_ITEMS) {
                self.mem.remove(&k);
            }
        }
    }

    /// Forgets every decoded image (on minimize). Disk copies stay.
    pub fn clear(&mut self) {
        self.mem = HashMap::new();
        self.pending.clear();
    }
}

fn disk_path(url: &str) -> PathBuf {
    let hex: String = Sha1::digest(url.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    cache_dir().join("img").join(hex)
}

fn load(url: &str, px: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let path = disk_path(url);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => {
            let mut r = music_core::net::agent().get(url).call().ok()?;
            if r.status().as_u16() != 200 {
                log::warn!("image {}: HTTP {}", music_core::net::short(url), r.status());
                return None;
            }
            let b = r
                .body_mut()
                .with_config()
                .limit(8 * 1024 * 1024)
                .read_to_vec()
                .ok()?;
            let _ = std::fs::create_dir_all(path.parent()?);
            let _ = std::fs::write(&path, &b);
            b
        }
    };
    let mut img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    // Video thumbnails come larger than asked for; keep at most twice the shown size.
    if img.width() > px * 2 {
        let h = (img.height() as u64 * px as u64 * 2 / img.width() as u64) as u32;
        img = image::imageops::thumbnail(&img, px * 2, h.max(1));
    }
    Some(SharedPixelBuffer::clone_from_slice(
        img.as_raw(),
        img.width(),
        img.height(),
    ))
}

/// Keeps the disk cache under its cap by deleting the least recently written files.
pub fn prune_disk() {
    let dir = cache_dir().join("img");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= DISK_BYTES {
        return;
    }
    files.sort();
    for (_, len, p) in files {
        if total <= DISK_BYTES * 9 / 10 {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            total -= len;
        }
    }
}
