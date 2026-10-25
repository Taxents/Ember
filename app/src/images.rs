//! Artwork: fetched at the size it's shown, cached on disk, decoded on a worker, and kept in a
//! small in-memory cache that's emptied when the window is minimized.
//!
//! Only `https://` images from YouTube's, Google's and fanart.tv's image hosts are fetched: URLs
//! can come from a Listen Along guest, and a request to any other host would tell it our address.

use music_core::paths::{cache_dir, write_atomic};
use music_core::yt::model::{Thumb, best_thumb, sized};
use sha1::{Digest, Sha1};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const MEMORY_ITEMS: usize = 200;
/// What the decoded images in memory may add up to (banners are large).
const MEMORY_BYTES: usize = 96 << 20;
const DISK_BYTES: u64 = 300 * 1024 * 1024;
/// Image hosts fetched from (each also covers its subdomains). gstatic.com serves the fixed
/// covers (Liked Music, Episodes for later).
/// music.youtube.com is allowed only under `MIX_ART`, which draws the generated covers (Replay,
/// Discover Mix, Supermix).
const HOSTS: [&str; 5] = [
    "ytimg.com",
    "googleusercontent.com",
    "ggpht.com",
    "gstatic.com",
    "fanart.tv",
];
const MIX_ART: &str = "music.youtube.com/image/";

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

/// The URL to fetch `url` from: `https://` on one of `HOSTS`. Scheme-relative ones (`//host/…`,
/// as YouTube sometimes sends) get `https:`.
pub fn fetchable(url: &str) -> Option<String> {
    let url = match url.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    };
    allowed(&url).then_some(url)
}

/// `url` is one images are fetched from: `https://` on one of `HOSTS`.
pub fn allowed(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    if rest.starts_with(MIX_ART) {
        return true;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(h, _)| h)
        .to_ascii_lowercase();
    HOSTS.iter().any(|h| {
        host == *h
            || host
                .strip_suffix(h)
                .is_some_and(|sub| sub.ends_with('.') && sub.len() > 1)
    })
}

/// A cache that drops the least recently used entries once it holds more than `cap` of them, or
/// they weigh more than `budget` together.
struct Lru<V> {
    cap: usize,
    budget: usize,
    weight: fn(&V) -> usize,
    total: usize,
    map: HashMap<String, (V, u64)>,
    /// Last use → key; the first entry is the one to drop.
    order: BTreeMap<u64, String>,
    tick: u64,
}

impl<V: Clone> Lru<V> {
    fn new(cap: usize, budget: usize, weight: fn(&V) -> usize) -> Self {
        Lru {
            cap,
            budget,
            weight,
            total: 0,
            map: HashMap::new(),
            order: BTreeMap::new(),
            tick: 0,
        }
    }

    /// The value, now the most recently used.
    fn get(&mut self, key: &str) -> Option<V> {
        let (v, t) = self.map.get_mut(key)?;
        self.order.remove(t);
        self.tick += 1;
        *t = self.tick;
        self.order.insert(self.tick, key.to_string());
        Some(v.clone())
    }

    /// The value, without counting as a use.
    fn peek(&self, key: &str) -> Option<V> {
        self.map.get(key).map(|(v, _)| v.clone())
    }

    fn insert(&mut self, key: String, v: V) {
        self.tick += 1;
        self.total += (self.weight)(&v);
        if let Some((old, t)) = self.map.insert(key.clone(), (v, self.tick)) {
            self.order.remove(&t);
            self.total -= (self.weight)(&old);
        }
        self.order.insert(self.tick, key);
        // The newest entry always stays, however heavy.
        while self.map.len() > self.cap || (self.total > self.budget && self.map.len() > 1) {
            let Some((_, k)) = self.order.pop_first() else {
                break;
            };
            if let Some((old, _)) = self.map.remove(&k) {
                self.total -= (self.weight)(&old);
            }
        }
    }

    fn clear(&mut self) {
        self.map = HashMap::new();
        self.order.clear();
        self.total = 0;
    }
}

pub struct Images {
    mem: Lru<Image>,
    pending: std::collections::HashSet<String>,
}

impl Images {
    pub fn new() -> Self {
        Images {
            mem: Lru::new(MEMORY_ITEMS, MEMORY_BYTES, |i: &Image| {
                let s = i.size();
                s.width as usize * s.height as usize * 4
            }),
            pending: Default::default(),
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
        self.mem.peek(url)
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
        if let Some(img) = self.mem.get(&key) {
            return Some(img);
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
        self.mem.insert(url.to_string(), img);
    }

    /// Forgets every decoded image (on minimize). Disk copies stay.
    pub fn clear(&mut self) {
        self.mem.clear();
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

/// The image decoded, from the disk cache or the network (then cached). A cached file that
/// doesn't decode (cut short, say) is deleted and fetched again, once.
pub(crate) fn decoded(url: &str) -> Option<image::RgbaImage> {
    let path = disk_path(url);
    if let Ok(b) = std::fs::read(&path) {
        // Read now: pruning goes by when a file was last used, not written.
        touch(&path);
        match image::load_from_memory(&b) {
            Ok(img) => return Some(img.to_rgba8()),
            Err(e) => {
                log::info!(
                    "image {}: cached copy unreadable ({e})",
                    music_core::net::short(url)
                );
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let b = download(url, &path)?;
    match image::load_from_memory(&b) {
        Ok(img) => Some(img.to_rgba8()),
        Err(e) => {
            log::info!("image {}: {e}", music_core::net::short(url));
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

fn touch(path: &Path) {
    if let Ok(f) = std::fs::File::options().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

/// Fetches the image and keeps it at `path`.
fn download(url: &str, path: &Path) -> Option<Vec<u8>> {
    let Some(url) = fetchable(url) else {
        log::info!("image {}: host not allowed", music_core::net::short(url));
        return None;
    };
    let mut r = music_core::net::agent().get(&url).call().ok()?;
    if r.status().as_u16() != 200 {
        log::warn!(
            "image {}: HTTP {}",
            music_core::net::short(&url),
            r.status()
        );
        return None;
    }
    let b = r
        .body_mut()
        .with_config()
        .limit(8 * 1024 * 1024)
        .read_to_vec()
        .ok()?;
    let _ = std::fs::create_dir_all(path.parent()?);
    let _ = write_atomic(path, &b);
    Some(b)
}

fn load(url: &str, px: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let mut img = decoded(url)?;
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
    let img = decoded(url)?;
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
    let mut img = decoded(url)?;
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

/// Keeps the disk cache under its cap by deleting the least recently used files. Call once at
/// start; from then on it runs again every hour on its own thread.
pub fn prune_disk() {
    static HOURLY: std::sync::Once = std::sync::Once::new();
    HOURLY.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("image-prune".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                    prune();
                }
            });
    });
    prune();
}

fn prune() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_hosts() {
        assert!(allowed("https://i.ytimg.com/vi/x/hqdefault.jpg"));
        assert!(allowed("https://lh3.googleusercontent.com/abc=w120-h120"));
        assert!(allowed("https://music.youtube.com/image/mixart?r=abc"));
        assert!(!allowed("https://music.youtube.com/watch?v=abc"));
        assert!(!allowed(
            "https://music.youtube.com.evil.example/image/mixart"
        ));
        assert!(allowed("https://yt3.ggpht.com/abc"));
        assert!(allowed(
            "https://assets.fanart.tv/fanart/music/x/hdmusiclogo/y.png"
        ));
        assert!(allowed("https://I.YTIMG.COM:443/vi/x.jpg"));
        assert!(allowed(
            "https://www.gstatic.com/youtube/media/ytm/images/pbg/liked-music-@576.png"
        ));
        assert_eq!(
            fetchable("//yt3.ggpht.com/abc").as_deref(),
            Some("https://yt3.ggpht.com/abc")
        );
        assert_eq!(fetchable("//evil.example/abc"), None);
        assert!(!allowed("http://i.ytimg.com/vi/x.jpg"));
        assert!(!allowed("https://evil.example/x.jpg"));
        assert!(!allowed("https://ytimg.com.evil.example/x.jpg"));
        assert!(!allowed("https://evilytimg.com/x.jpg"));
        assert!(!allowed("https://i.ytimg.com@evil.example/x.jpg"));
        assert!(!allowed("https://evil.example/?i.ytimg.com"));
        assert!(!allowed("file:///C:/x.jpg"));
    }

    #[test]
    fn least_recently_used_goes() {
        let mut c = Lru::new(2, usize::MAX, |_: &i32| 0);
        c.insert("a".into(), 1);
        c.insert("b".into(), 2);
        assert_eq!(c.get("a"), Some(1));
        c.insert("c".into(), 3);
        assert_eq!(c.peek("b"), None, "b was used least recently");
        assert_eq!(c.peek("a"), Some(1));
        assert_eq!(c.peek("c"), Some(3));
        // Replacing a key keeps one entry for it.
        c.insert("c".into(), 4);
        c.insert("d".into(), 5);
        assert_eq!((c.map.len(), c.order.len()), (2, 2));
        assert_eq!(c.peek("a"), None);
        assert_eq!(c.peek("c"), Some(4));
        // A peek doesn't count as a use.
        assert_eq!(c.peek("c"), Some(4));
        c.insert("e".into(), 6);
        assert_eq!(c.peek("c"), None);
        assert_eq!(c.peek("d"), Some(5));
    }

    #[test]
    fn heavy_entries_push_old_ones_out() {
        let mut c = Lru::new(10, 100, |v: &usize| *v);
        c.insert("a".into(), 40);
        c.insert("b".into(), 40);
        c.insert("c".into(), 40);
        assert_eq!(c.peek("a"), None, "a went to keep the total under 100");
        assert_eq!((c.total, c.map.len()), (80, 2));
        // Replacing an entry counts its new weight, not both.
        c.insert("c".into(), 10);
        assert_eq!(c.total, 50);
        // One entry heavier than the budget still stays (it's being shown).
        c.insert("d".into(), 500);
        assert_eq!((c.peek("d"), c.map.len(), c.total), (Some(500), 1, 500));
        c.clear();
        assert_eq!(c.total, 0);
    }
}
