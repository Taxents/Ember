//! What the app remembers between runs: `settings.json` and `queue.json` in the config folder.

use music_core::audio::EqSettings;
use music_core::paths::{config_dir, write_atomic};
use music_core::player::Repeat;
use music_core::yt::model::{Chip, Track};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Slider position, 0..1 (the gain is its square).
    pub volume: f32,
    pub eq: EqSettings,
    /// "Flat", "Bass", "Vocal", "Treble", or empty for custom.
    pub eq_preset: String,
    /// Logical pixels.
    pub window: (f32, f32),
    pub sidebar_collapsed: bool,
    pub panel_open: bool,
    pub panel_tab: i32,
    /// Playlists hidden from the app (YouTube playlists that aren't music), by id.
    pub hidden_playlists: Vec<String>,
    /// The sidebar's playlists in the order you dragged them into, by id (ones not in it, new
    /// since, go first).
    pub playlist_order: Vec<String>,
    /// Your name in Listen Along.
    pub together_name: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            volume: 1.0,
            eq: EqSettings::default(),
            eq_preset: "Flat".into(),
            window: (1280.0, 800.0),
            sidebar_collapsed: false,
            panel_open: false,
            panel_tab: 1,
            hidden_playlists: Vec::new(),
            playlist_order: Vec::new(),
            together_name: std::env::var("USERNAME").unwrap_or_default(),
        }
    }
}

/// The queue as it was, in play order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedQueue {
    pub tracks: Vec<Track>,
    pub at: usize,
    pub position_ms: u64,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// The queue is a radio: the song it grows from, its choices and the one picked.
    pub autoplay: Option<(String, Vec<Chip>, usize)>,
}

fn load<T: for<'a> Deserialize<'a> + Default>(name: &str) -> T {
    std::fs::read(config_dir().join(name))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save<T: Serialize>(name: &str, v: &T) {
    let json = serde_json::to_vec(v).expect("json");
    if let Err(e) = write_atomic(&config_dir().join(name), &json) {
        log::warn!("saving {name}: {e}");
    }
}

impl Settings {
    pub fn load() -> Self {
        load("settings.json")
    }

    pub fn save(&self) {
        save("settings.json", self)
    }
}

impl SavedQueue {
    pub fn load() -> Self {
        load("queue.json")
    }

    pub fn save(&self) {
        save("queue.json", self)
    }
}

/// Play counts looked up earlier (video id → count, unix time), kept a week: they change
/// slowly, and each lookup costs a request.
pub fn load_plays() -> std::collections::HashMap<String, (u64, u64)> {
    let path = music_core::paths::cache_dir().join("plays.json");
    let mut m: std::collections::HashMap<String, (u64, u64)> = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let week_ago = music_core::paths::unix_now().saturating_sub(7 * 24 * 3600);
    m.retain(|_, (_, t)| *t >= week_ago);
    m
}

pub fn save_plays(m: &std::collections::HashMap<String, (u64, u64)>) {
    let json = serde_json::to_vec(m).expect("json");
    let _ =
        music_core::paths::write_atomic(&music_core::paths::cache_dir().join("plays.json"), &json);
}
