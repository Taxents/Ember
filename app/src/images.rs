//! Artwork: fetched at the size it's shown, cached on disk, decoded on a worker, and kept in a
//! small in-memory cache that's emptied when the window is minimized.

use music_core::paths::cache_dir;
use music_core::yt::model::{Thumb, best_thumb, sized};
use sha1::{Digest, Sha1};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::collections::HashMap;
use std::path::PathBuf;

const MEMORY_ITEMS: usize = 400;
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
        self.fetch(url.to_string(), url, Some(px), loaded, false)
    }

    /// [`Images::get`], loaded ahead of everything else waiting.
    pub fn get_urgent(&mut self, url: &str, px: u32, loaded: fn(String, Image)) -> Option<Image> {
        self.fetch(url.to_string(), url, Some(px), loaded, true)
    }

    /// A soft, darkened wide crop of `url` for a banner. Its key (what `loaded` gets) is
    /// [`blurred_key`].
    pub fn get_blurred(&mut self, url: &str, loaded: fn(String, Image)) -> Option<Image> {
        self.fetch(blurred_key(url), url, None, loaded, false)
    }

    /// A wide picture (an artist's) for a banner, crisp and darkened. Its key is [`wide_key`].
    pub fn get_wide(&mut self, url: &str, loaded: fn(String, Image)) -> Option<Image> {
        self.fetch(wide_key(url), url, Some(0), loaded, false)
    }

    /// The image if it's in memory, without loading it.
    pub fn peek(&self, url: &str) -> Option<Image> {
        self.mem.get(url).map(|(img, _)| img.clone())
    }

    fn fetch(
        &mut self,
        key: String,
        url: &str,
        // `Some(0)` is a wide banner, `None` a blurred one.
        px: Option<u32>,
        loaded: fn(String, Image),
        urgent: bool,
    ) -> Option<Image> {
        self.tick += 1;
        if let Some((img, t)) = self.mem.get_mut(&key) {
            *t = self.tick;
            return Some(img.clone());
        }
        if self.pending.insert(key.clone()) {
            let url = url.to_string();
            crate::workers::spawn_art(
                move || {
                    let buf = match px {
                        Some(0) => load_wide(&url),
                        Some(px) => load(&url, px),
                        None => load_blurred(&url),
                    };
                    let _ = slint::invoke_from_event_loop(move || match buf {
                        Some(b) => loaded(key, Image::from_rgba8(b)),
                        None => loaded(key, Image::default()),
                    });
                },
                urgent,
            );
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

pub fn blurred_key(url: &str) -> String {
    format!("blur:{url}")
}

pub fn wide_key(url: &str) -> String {
    format!("wide:{url}")
}

/// The image's bytes, from the disk cache or the network (then cached).
fn bytes(url: &str) -> Option<Vec<u8>> {
    let path = disk_path(url);
    if let Ok(b) = std::fs::read(&path) {
        return Some(b);
    }
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
    Some(b)
}

fn load(url: &str, px: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let mut img = image::load_from_memory(&bytes(url)?).ok()?.to_rgba8();
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

/// The cover's middle band in a 3:1 banner shape, blurred and darkened, darker still on the left
/// (under the title). It's made small: the blur is
/// cheap at this size and the GPU's smooth scaling softens it further.
fn load_blurred(url: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = image::load_from_memory(&bytes(url)?).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let band = (w / 3).clamp(1, h);
    let crop = image::imageops::crop_imm(&img, 0, (h - band) / 2, w, band).to_image();
    let small = image::imageops::resize(&crop, 192, 64, image::imageops::FilterType::Triangle);
    let mut soft = image::imageops::blur(&small, 5.0);
    let sw = soft.width();
    let smooth = |t: f32| t * t * (3.0 - 2.0 * t);
    for (x, _, p) in soft.enumerate_pixels_mut() {
        let fx = x as f32 / (sw - 1) as f32;
        let left = 1.0 - 0.55 * (1.0 - smooth((fx / 0.55).min(1.0)));
        for c in &mut p.0[..3] {
            *c = (*c as f32 * 0.42 * left) as u8;
        }
        p.0[3] = 255;
    }
    Some(SharedPixelBuffer::clone_from_slice(
        soft.as_raw(),
        soft.width(),
        soft.height(),
    ))
}

/// A wide picture cut to the banner's 3:1 (keeping the top, where faces are), at most 1600
/// pixels across, and darkened (more on the left, under the title).
fn load_wide(url: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let mut img = image::load_from_memory(&bytes(url)?).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let band = (w / 3).clamp(1, h);
    img = image::imageops::crop_imm(&img, 0, (h - band) / 6, w, band).to_image();
    if img.width() > 1600 {
        let h = (img.height() as u64 * 1600 / img.width() as u64) as u32;
        img = image::imageops::resize(&img, 1600, h.max(1), image::imageops::FilterType::Triangle);
    }
    let (w, h) = img.dimensions();
    let smooth = |t: f32| t * t * (3.0 - 2.0 * t);
    for (x, _, p) in img.enumerate_pixels_mut() {
        let fx = x as f32 / (w.max(2) - 1) as f32;
        let left = 1.0 - 0.5 * (1.0 - smooth((fx / 0.6).min(1.0)));
        for c in &mut p.0[..3] {
            *c = (*c as f32 * 0.82 * left) as u8;
        }
    }
    Some(SharedPixelBuffer::clone_from_slice(img.as_raw(), w, h))
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
