//! The stage: the playing song filling a display, for leaving up while listening.
//!
//! The window itself becomes the stage: frameless over one display's work area (the taskbar
//! stays, and nothing is exclusive full screen), and back where it was on leaving. Its room is lit
//! by the song: the artwork (and, when fanart.tv has one, a photo of the artist) made into a soft
//! field of colour on a worker, a palette for the lights, and the engine's four meter bands turned
//! into slow, musical light (see [`Light`]). Lyrics are the window's own (`lyric_view`), shown
//! word by word.

use crate::{
    App, StageLight, StageLook, Theme, Ui, image_loaded, images, logos, win, with, workers,
};
use image::RgbaImage;
use slint::{
    Color, ComponentHandle, Image, Model, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode,
};
use std::time::{Duration, Instant};

/// The artwork's size on the stage, logical pixels (it's shown at up to about this, alone on a
/// large display).
const STAGE_ART: f32 = 800.0;
/// The atmosphere is made this small: it's a blur, and the GPU's smooth scaling does the rest.
const BACK_W: u32 = 480;
const BACK_H: u32 = 270;
/// How many songs' looks are kept, for going back and forth in a queue.
const LOOKS_KEPT: usize = 6;
/// Tungsten: the room's own warm light, which every palette leans a little towards.
const TUNGSTEN: [f32; 3] = [233.0 / 255.0, 161.0 / 255.0, 95.0 / 255.0];

#[derive(Default)]
pub struct Stage {
    /// The window as it was before it became the stage; `Some` while on stage.
    windowed: Option<win::Windowed>,
    /// The song the look is for (or being made for).
    look_for: String,
    /// The large artwork asked for.
    art_url: Option<String>,
    /// Looks made lately, newest last: (video id, look, made with the artist's photo).
    looks: Vec<(String, Look, bool)>,
    light: Light,
    last_step: Option<Instant>,
    /// Draws the stage once per refresh of its display while the song plays (see `stage_frame`).
    frame: Timer,
    /// Its display's refresh rate.
    hz: u32,
    last_frame: Option<Instant>,
    /// The light as the meter last measured it, and as shown (catching up a frame at a time).
    measured: StageLight,
    shown: StageLight,
    /// The meter's bars as shown.
    bars: [f32; 4],
    reduced: bool,
    /// Waits for moves and display changes to settle before fitting the window again.
    refit: Timer,
}

impl Stage {
    pub fn open(&self) -> bool {
        self.windowed.is_some()
    }
}

#[derive(Clone)]
struct Look {
    back: Image,
    key: Color,
    fill: Color,
    accent: Color,
    deep: Color,
    layout: i32,
}

/// What a worker makes from the artwork: the atmosphere and the palette.
struct Made {
    back: SharedPixelBuffer<Rgba8Pixel>,
    /// Key, fill, accent and deep, as 0..1 RGB.
    colors: [[f32; 3]; 4],
    layout: i32,
}

impl App {
    pub(crate) fn stage_toggle(&mut self) {
        if self.stage.open() {
            self.stage_close();
        } else {
            self.stage_enter();
        }
    }

    fn stage_enter(&mut self) {
        let Some(h) = self.hwnd else {
            return;
        };
        let Some(d) = self.stage_target(h) else {
            return;
        };
        let Some(w) = win::enter_stage(h, d.work) else {
            return;
        };
        self.stage.windowed = Some(w);
        self.stage.reduced = win::reduced_motion();
        self.stage.light = Light::default();
        self.stage.last_step = None;
        self.stage.hz = d.hz;
        log::info!("stage on {} at {} Hz", d.name, d.hz);
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_stage_open(true);
        g.set_stage_displays(win::displays().len() > 1);
        g.set_reduced_motion(self.stage.reduced);
        if g.get_stage_glow().size().width == 0 {
            g.set_stage_glow(Image::from_rgba8(glow_sprite()));
            g.set_stage_grain(Image::from_rgba8(grain_tile()));
        }
        if g.get_stage_look().key == Color::default() {
            g.set_stage_look(default_look(&ui));
        }
        self.stage.measured = self.stage.light.still();
        self.stage.shown = self.stage.measured.clone();
        g.set_stage_light(self.stage.shown.clone());
        // Lyrics are wanted from here on, as when the panel shows them.
        self.lyrics_opened = true;
        self.lyrics_for_current();
        self.stage_song();
        self.tick();
    }

    pub(crate) fn stage_close(&mut self) {
        let Some(w) = self.stage.windowed.take() else {
            return;
        };
        self.stage.refit.stop();
        self.stage_hold();
        if let Some(h) = self.hwnd {
            win::leave_stage(h, &w);
        }
        log::info!("stage off");
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_stage_open(false);
        // The artwork is the big one; the rest is small and makes a return instant.
        g.set_stage_art(Image::default());
        self.tick();
    }

    /// The display the stage goes on: the one the window is on.
    fn stage_target(&self, h: windows::Win32::Foundation::HWND) -> Option<win::Display> {
        win::display_of_window(h)
    }

    pub(crate) fn stage_next_display(&mut self) {
        let (Some(h), true) = (self.hwnd, self.stage.open()) else {
            return;
        };
        let all = win::displays();
        let here = win::display_of_window(h)
            .map(|d| d.name)
            .unwrap_or_default();
        let at = all.iter().position(|d| d.name == here).unwrap_or(0);
        if let Some(d) = all.get((at + 1) % all.len().max(1)) {
            win::fit_stage(h, d.work);
            // Moving between displays of different scale resizes the window once more.
            self.stage_displays_changed();
        }
    }

    /// The window moved (dragged to another display with Win+Shift+Arrow, say) or the displays
    /// changed: once things settle, the stage fills its display's work area again.
    pub(crate) fn stage_displays_changed(&mut self) {
        if !self.stage.open() {
            return;
        }
        self.stage
            .refit
            .start(TimerMode::SingleShot, Duration::from_millis(150), || {
                with(|a| a.stage_fit());
            });
    }

    fn stage_fit(&mut self) {
        let (Some(h), true) = (self.hwnd, self.stage.open()) else {
            return;
        };
        let Some(d) = win::display_of_window(h) else {
            return;
        };
        if win::window_rect(h) != Some(d.work) {
            win::fit_stage(h, d.work);
        }
        if self.stage.hz != d.hz {
            self.stage.hz = d.hz;
            if self.stage.frame.running() {
                self.stage.frame.stop();
                self.stage_frames();
            }
        }
        self.ui()
            .global::<Ui>()
            .set_stage_displays(win::displays().len() > 1);
        // A display of another scale wants the artwork at another size.
        self.stage_song();
    }

    /// A new song (or the stage just opened): its artwork, and its look once made.
    pub(crate) fn stage_song(&mut self) {
        if !self.stage.open() {
            return;
        }
        let Some(t) = self
            .player
            .current()
            .and_then(|id| self.known.get(id))
            .cloned()
        else {
            return;
        };
        let px = self.px(STAGE_ART);
        let url = images::url_for(&t.thumbs, px);
        // Remembered, so it's known when it arrives even if the scale has changed since.
        self.stage.art_url = url.clone();
        let art = url
            .as_deref()
            .and_then(|u| self.images.get_urgent(u, px, image_loaded));
        let ui = self.ui();
        let g = ui.global::<Ui>();
        // The bar's smaller copy stands in until the large one arrives.
        g.set_stage_art(art.unwrap_or_else(|| g.get_np().art));

        if self.stage.look_for == t.video_id {
            return;
        }
        self.stage.look_for = t.video_id.clone();
        if let Some(i) = self.stage.looks.iter().position(|l| l.0 == t.video_id) {
            let (id, look, full) = self.stage.looks.remove(i);
            self.stage_show(&look);
            self.stage.looks.push((id, look, full));
            if full {
                return;
            }
        }
        let id = t.video_id.clone();
        let cover = url;
        let artist = t.artists.first().cloned().or(t.channel.clone());
        let c = cover.clone();
        let first = id.clone();
        // The artwork alone first (quick); then, when there's a photo of the artist, again with it.
        workers::spawn_art(
            move || {
                let made = make(c.as_deref(), None);
                let _ = slint::invoke_from_event_loop(move || {
                    with(|a| a.stage_made(&first, made, false));
                });
            },
            true,
        );
        let Some(artist) = artist else {
            return;
        };
        workers::run_background(
            move || logos::backdrop(&artist.name, artist.id.as_deref()),
            move |found| {
                let Some(photo) = found else {
                    return;
                };
                if with(|a| a.stage.look_for == id) != Some(true) {
                    return;
                }
                workers::spawn_art(
                    move || {
                        let made = make(cover.as_deref(), Some(&photo));
                        let _ = slint::invoke_from_event_loop(move || {
                            with(|a| a.stage_made(&id, made, true));
                        });
                    },
                    false,
                );
            },
        );
    }

    fn stage_made(&mut self, id: &str, made: Made, full: bool) {
        let c = |k: usize| {
            let [r, g, b] = made.colors[k].map(|x| (x.clamp(0.0, 1.0) * 255.0).round() as u8);
            Color::from_rgb_u8(r, g, b)
        };
        let look = Look {
            back: Image::from_rgba8(made.back),
            key: c(0),
            fill: c(1),
            accent: c(2),
            deep: c(3),
            layout: made.layout,
        };
        self.stage.looks.retain(|l| l.0 != id);
        self.stage.looks.push((id.to_string(), look.clone(), full));
        if self.stage.looks.len() > LOOKS_KEPT {
            self.stage.looks.remove(0);
        }
        if self.stage.open() && self.stage.look_for == id {
            self.stage_show(&look);
        }
    }

    /// Fades the look in over the one showing (the other slot).
    fn stage_show(&mut self, look: &Look) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut s = g.get_stage_look();
        s.slot = 1 - s.slot;
        if s.slot == 0 {
            s.back_a = look.back.clone();
        } else {
            s.back_b = look.back.clone();
        }
        s.key = look.key;
        s.fill = look.fill;
        s.accent = look.accent;
        s.deep = look.deep;
        s.layout = look.layout;
        g.set_stage_look(s);
    }

    /// An image arrived: the stage's artwork, if it's the one.
    pub(crate) fn stage_image(&mut self, url: &str, img: &Image) {
        if !self.stage.open() {
            return;
        }
        if self.stage.art_url.as_deref() == Some(url) {
            self.ui().global::<Ui>().set_stage_art(img.clone());
        }
    }

    /// The meter's levels as light (from the meter's ~30 Hz timer while playing). The music is
    /// measured at that pace; the stage is drawn at its display's (`stage_frame`).
    pub(crate) fn stage_meter(&mut self, raw: [f32; 4]) {
        if !self.stage.open() {
            return;
        }
        let now = Instant::now();
        let dt = self
            .stage
            .last_step
            .map_or(0.033, |t| (now - t).as_secs_f32().min(0.1));
        self.stage.last_step = Some(now);
        self.stage.measured = self.stage.light.step(raw, dt, self.stage.reduced);
        if !self.stage.frame.running() {
            self.stage_frames();
        }
    }

    /// The stage is being drawn every refresh (the meter's bars are then shown from here).
    pub(crate) fn stage_drawing(&self) -> bool {
        self.stage.frame.running()
    }

    fn stage_frames(&mut self) {
        self.stage.last_frame = None;
        let every = Duration::from_secs_f64(1.0 / self.stage.hz.clamp(30, 500) as f64);
        self.stage.frame.start(TimerMode::Repeated, every, || {
            with(|a| a.stage_frame());
        });
    }

    /// One frame: the words fill to where the song is heard right now, and the light and bars
    /// glide to what the meter last measured.
    fn stage_frame(&mut self) {
        if !self.stage.open() {
            self.stage.frame.stop();
            return;
        }
        let now = Instant::now();
        let dt = self
            .stage
            .last_frame
            .map_or(0.0, |t| (now - t).as_secs_f32().min(0.1));
        self.stage.last_frame = Some(now);
        // Catches up in ~25 ms: smooth between measurements, never a beat behind.
        let k = 1.0 - (-dt / 0.025).exp();
        let (to, s) = (&self.stage.measured, &mut self.stage.shown);
        for (v, t) in [
            (&mut s.bloom, to.bloom),
            (&mut s.body, to.body),
            (&mut s.air, to.air),
            (&mut s.pulse, to.pulse),
            (&mut s.energy, to.energy),
        ] {
            *v += (t - *v) * k;
        }
        if !self.stage.reduced {
            s.phase += dt * (0.35 + 1.1 * to.energy);
        }
        let light = s.clone();
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_stage_light(light);
        if let Some(p) = self.player.engine.position() {
            g.set_stage_ms((p.ms + crate::lyric_view::LEAD_MS).min(i32::MAX as u64) as i32);
        }
        for (i, b) in self.stage.bars.iter_mut().enumerate() {
            *b += (self.meter[i].1 - *b) * k;
            self.levels.set_row_data(i, *b);
        }
    }

    /// Playback stopped moving (paused, between songs): the light holds where it is, and the
    /// next step doesn't count the wait as time passed.
    pub(crate) fn stage_hold(&mut self) {
        self.stage.last_step = None;
        self.stage.frame.stop();
    }
}

/// The look before any song's is made: the room's own tungsten and oxide.
fn default_look(ui: &crate::AppWindow) -> StageLook {
    let t = ui.global::<Theme>();
    StageLook {
        back_a: Image::default(),
        back_b: Image::default(),
        slot: 0,
        key: t.get_amber(),
        fill: t.get_oxide(),
        accent: t.get_amber(),
        deep: t.get_carbon(),
        layout: 0,
    }
}

// ---- light ------------------------------------------------------------------------------------

/// The music as light. Each band is measured against its own recent peak (so quiet songs light
/// up too), then eased: the bass swells quickly and fades slowly (`bloom`), the midrange drifts
/// (`body`), the treble flickers a little faster (`air`). A beat is a jump in bass and midrange
/// well above their recent movement (`pulse`, at most ~5 a second, decaying in ~200 ms).
/// `energy` follows how loud and busy the song has been over ~15 s, and sets the pace: the lights
/// drift slowly for ambient music and more briskly, with stronger beats, for lively music.
#[derive(Default)]
struct Light {
    peak: [f32; 4],
    bloom: f32,
    body: f32,
    air: f32,
    pulse: f32,
    energy: f32,
    phase: f32,
    prev: [f32; 2],
    flux_avg: f32,
    since_beat: u32,
    started: bool,
}

impl Light {
    fn step(&mut self, raw: [f32; 4], dt: f32, still: bool) -> StageLight {
        if !self.started {
            self.started = true;
            self.energy = 0.35;
        }
        let mut n = [0.0f32; 4];
        for i in 0..4 {
            // Peaks forget over ~20 s, so the scale follows the song.
            self.peak[i] = raw[i].max(self.peak[i] * 0.9985).max(1e-4);
            n[i] = (raw[i] / self.peak[i]).clamp(0.0, 1.0);
        }
        let ease = |v: &mut f32, to: f32, up: f32, down: f32| {
            *v += (to - *v) * if to > *v { up } else { down };
        };
        let lively = self.energy;
        ease(&mut self.bloom, n[0].powf(1.3), 0.08 + 0.12 * lively, 0.035);
        ease(&mut self.body, (n[1] + n[2]) * 0.5, 0.06, 0.025);
        ease(&mut self.air, n[3].powf(1.2), 0.25, 0.07);

        let (low, mid) = (n[0], (n[1] + n[2]) * 0.5);
        let flux = (low - self.prev[0]).max(0.0) * 0.65 + (mid - self.prev[1]).max(0.0) * 0.35;
        self.prev = [low, mid];
        self.flux_avg += (flux - self.flux_avg) * 0.03;
        self.since_beat = self.since_beat.saturating_add(1);
        self.pulse *= 0.86;
        if flux > self.flux_avg * 2.4 + 0.06 && self.since_beat > 6 {
            self.pulse = self.pulse.max(0.5 + 0.5 * self.energy);
            self.since_beat = 0;
        }
        let busy = (0.6 * (low + mid) * 0.5 + 2.5 * flux).min(1.0);
        self.energy += (busy - self.energy) * 0.0025;
        if !still {
            self.phase += dt * (0.35 + 1.1 * self.energy);
        }
        StageLight {
            bloom: self.bloom,
            body: self.body,
            air: self.air,
            // A pulse is motion of a kind; with animations off the light only breathes.
            pulse: if still { 0.0 } else { self.pulse },
            energy: self.energy,
            phase: self.phase,
        }
    }

    /// As it is, without a new reading (the stage opening while paused).
    fn still(&self) -> StageLight {
        StageLight {
            bloom: self.bloom,
            body: self.body,
            air: self.air,
            pulse: 0.0,
            energy: self.energy,
            phase: self.phase,
        }
    }
}

// ---- the look -------------------------------------------------------------------------------

/// The atmosphere and palette from the cover at `cover` (and the artist's photo at `photo`).
/// Without a cover (none, or it didn't load) the room is made from Ember's own colours.
fn make(cover: Option<&str>, photo: Option<&str>) -> Made {
    let cover = cover.and_then(images::decoded);
    let photo = photo.and_then(images::decoded);
    let (colors, layout) = match &cover {
        Some(c) => palette(c),
        None => (
            [TUNGSTEN, [0.46, 0.16, 0.12], TUNGSTEN, [0.09, 0.07, 0.06]],
            0,
        ),
    };
    let back = atmosphere(cover.as_ref(), photo.as_ref(), &colors, layout);
    Made {
        back: SharedPixelBuffer::clone_from_slice(back.as_raw(), back.width(), back.height()),
        colors,
        layout,
    }
}

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Hue (degrees), saturation and value.
fn hsv(c: [f32; 3]) -> (f32, f32, f32) {
    let max = c[0].max(c[1]).max(c[2]);
    let min = c[0].min(c[1]).min(c[2]);
    let d = max - min;
    let h = if d < 1e-5 {
        0.0
    } else if max == c[0] {
        60.0 * ((c[1] - c[2]) / d).rem_euclid(6.0)
    } else if max == c[1] {
        60.0 * ((c[2] - c[0]) / d + 2.0)
    } else {
        60.0 * ((c[0] - c[1]) / d + 4.0)
    };
    (h, if max > 0.0 { d / max } else { 0.0 }, max)
}

fn from_hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match (h.rem_euclid(360.0) / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [r + m, g + m, b + m]
}

fn mix(a: [f32; 3], b: [f32; 3], f: f32) -> [f32; 3] {
    [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * f)
}

fn hue_apart(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

/// The cover's lights: `key` (its strongest colour, as a light), `fill` (another hue from it),
/// `accent` (its most vivid colour, lifted to glow) and `deep` (its shadow). Also which way the
/// room is lit: from the side the artwork itself is brightest on.
fn palette(img: &RgbaImage) -> ([[f32; 3]; 4], i32) {
    let small = image::imageops::resize(img, 40, 40, image::imageops::FilterType::Triangle);
    let px: Vec<[f32; 3]> = small
        .pixels()
        .filter(|p| p.0[3] > 127)
        .map(|p| [0, 1, 2].map(|i| p.0[i] as f32 / 255.0))
        .collect();
    if px.is_empty() {
        return (
            [TUNGSTEN, [0.46, 0.16, 0.12], TUNGSTEN, [0.09, 0.07, 0.06]],
            0,
        );
    }
    // k-means from evenly spread brightnesses: the same cover always gives the same palette.
    const K: usize = 6;
    let mut sorted = px.clone();
    sorted.sort_by(|a, b| lum(*a).total_cmp(&lum(*b)));
    let mut centres: Vec<[f32; 3]> = (0..K)
        .map(|k| sorted[(k * 2 + 1) * sorted.len() / (2 * K)])
        .collect();
    let mut counts = [0usize; K];
    for _ in 0..8 {
        let mut sums = [[0.0f32; 3]; K];
        counts = [0; K];
        for p in &px {
            let k = (0..K)
                .min_by(|&a, &b| {
                    let d = |c: [f32; 3]| (0..3).map(|i| (c[i] - p[i]).powi(2)).sum::<f32>();
                    d(centres[a]).total_cmp(&d(centres[b]))
                })
                .unwrap_or(0);
            counts[k] += 1;
            for i in 0..3 {
                sums[k][i] += p[i];
            }
        }
        for k in 0..K {
            if counts[k] > 0 {
                centres[k] = sums[k].map(|s| s / counts[k] as f32);
            }
        }
    }
    let total = px.len() as f32;
    let score = |k: usize| {
        let (_, s, v) = hsv(centres[k]);
        let dark = if v < 0.1 { 0.05 } else { 1.0 };
        (counts[k] as f32 / total).sqrt() * (0.2 + s) * (0.25 + v) * dark
    };
    let mut order: Vec<usize> = (0..K).filter(|&k| counts[k] > 0).collect();
    order.sort_by(|&a, &b| score(b).total_cmp(&score(a)));
    let key = centres[order[0]];
    let (kh, ks, _) = hsv(key);
    let fill = order
        .iter()
        .skip(1)
        .map(|&k| centres[k])
        .find(|&c| {
            let (h, s, v) = hsv(c);
            s > 0.12 && v > 0.12 && hue_apart(h, kh) > 35.0
        })
        // One colour only: its neighbour on the warm side.
        .unwrap_or_else(|| from_hsv(kh - 28.0, ks.max(0.4), 0.8));
    let vivid = order
        .iter()
        .map(|&k| centres[k])
        .max_by(|&a, &b| {
            let w = |c: [f32; 3]| {
                let (_, s, v) = hsv(c);
                s * v.powf(1.5)
            };
            w(a).total_cmp(&w(b))
        })
        .unwrap_or(key);

    // As lights: full brightness, saturation kept between washed out and neon. A grey cover's
    // lights are tungsten. Every colour leans a little warm, like glass in this room.
    let light = |c: [f32; 3]| {
        let (h, s, _) = hsv(c);
        let lit = from_hsv(h, s.clamp(0.3, 0.82), 1.0);
        let lit = if s < 0.1 {
            mix(lit, TUNGSTEN, 0.7)
        } else {
            lit
        };
        mix(lit, TUNGSTEN, 0.12)
    };
    let accent = {
        let (h, s, v) = hsv(vivid);
        let a = from_hsv(h, s.clamp(0.25, 0.62), v.max(0.94));
        if s < 0.1 {
            mix(a, TUNGSTEN, 0.6)
        } else {
            mix(a, TUNGSTEN, 0.1)
        }
    };
    let deep = {
        let (h, s, _) = hsv(key);
        from_hsv(h, s.clamp(0.2, 0.7), 0.16)
    };

    // The brightest quarter of the artwork: the main light comes from there.
    let quarter = |x0: u32, y0: u32| {
        let mut sum = 0.0;
        for y in y0..y0 + 20 {
            for x in x0..x0 + 20 {
                let p = small.get_pixel(x, y).0;
                sum += lum([p[0] as f32, p[1] as f32, p[2] as f32]);
            }
        }
        sum
    };
    // Layouts: 0 lower left, 1 upper right, 2 left, 3 lower right.
    let quarters = [
        (quarter(0, 20), 0),
        (quarter(20, 0), 1),
        (quarter(0, 0), 2),
        (quarter(20, 20), 3),
    ];
    let layout = quarters
        .iter()
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map_or(0, |q| q.1);
    ([light(key), light(fill), accent, deep], layout)
}

/// The cover as a dark room's worth of soft colour, 16:9, with the artist's photo faintly in it
/// when there is one (tinted by the key light, as if lit by it). Dark enough that white words
/// always read over it, vignetted, and dithered so its gradients don't band.
fn atmosphere(
    cover: Option<&RgbaImage>,
    photo: Option<&RgbaImage>,
    colors: &[[f32; 3]; 4],
    layout: i32,
) -> RgbaImage {
    use image::imageops::{FilterType, blur, crop_imm, resize};
    let n = (BACK_W * BACK_H) as usize;
    let mut out: Vec<[f32; 3]> = match cover {
        Some(c) => {
            let (w, h) = c.dimensions();
            // The middle band of a square cover; a wide one (a video's) as it is.
            let band = (w * 9 / 16).clamp(1, h);
            let crop = crop_imm(c, 0, (h - band) / 2, w, band).to_image();
            let tiny = blur(&resize(&crop, 64, 36, FilterType::Triangle), 2.5);
            let soft = blur(&resize(&tiny, BACK_W, BACK_H, FilterType::Triangle), 6.0);
            soft.pixels()
                .map(|p| [0, 1, 2].map(|i| p.0[i] as f32 / 255.0))
                .collect()
        }
        None => {
            // A glow of the key colour from its corner, over the deep one.
            let (kx, ky) =
                [(0.1, 0.92), (0.92, 0.08), (0.04, 0.42), (0.9, 0.88)][layout as usize % 4];
            (0..n)
                .map(|i| {
                    let x = (i as u32 % BACK_W) as f32 / BACK_W as f32;
                    let y = (i as u32 / BACK_W) as f32 / BACK_H as f32;
                    let d = ((x - kx).powi(2) + (y - ky).powi(2) * 0.6).sqrt();
                    mix(colors[3], colors[0], (1.0 - d * 1.3).clamp(0.0, 1.0) * 0.5)
                })
                .collect()
        }
    };
    // A little richer, then down to a dim room: its average brightness about 15%, nothing
    // brighter than 50%.
    let mean = out.iter().map(|&c| lum(c)).sum::<f32>() / n as f32;
    let gain = (0.125 / mean.max(0.02)).clamp(0.25, 1.6);
    for c in &mut out {
        let l = lum(*c);
        *c = [0, 1, 2].map(|i| ((l + (c[i] - l) * 1.3) * gain).clamp(0.0, 0.5));
    }
    if let Some(p) = photo {
        // Cut to 16:9 keeping the upper part (where faces are), slightly softened.
        let (w, h) = p.dimensions();
        let (cw, ch) = if w * 9 > h * 16 {
            (h * 16 / 9, h)
        } else {
            (w, w * 9 / 16)
        };
        let crop = crop_imm(p, (w - cw) / 2, (h - ch) / 4, cw.max(1), ch.max(1)).to_image();
        let photo = blur(&resize(&crop, BACK_W, BACK_H, FilterType::Triangle), 1.2);
        let tint = colors[0];
        for (c, q) in out.iter_mut().zip(photo.pixels()) {
            let l = lum([0, 1, 2].map(|i| q.0[i] as f32 / 255.0)).powf(1.3);
            for i in 0..3 {
                let lit = l * (0.32 * tint[i] + 0.12);
                c[i] = (c[i] * 0.72 + lit).min(0.5);
            }
        }
    }
    let mut seed = 0x2545_F491u32;
    let mut img = RgbaImage::new(BACK_W, BACK_H);
    for (i, p) in img.pixels_mut().enumerate() {
        let x = (i as u32 % BACK_W) as f32 / BACK_W as f32 - 0.5;
        let y = (i as u32 / BACK_W) as f32 / BACK_H as f32 - 0.5;
        let v = 1.0 - 0.55 * (x * x + y * y * 1.4);
        for (d, c) in p.0.iter_mut().zip(out[i]) {
            *d = (c * v * 255.0 + noise(&mut seed)).clamp(0.0, 255.0) as u8;
        }
        p.0[3] = 255;
    }
    img
}

/// -1..1, from a small LCG (dithering only needs it to look random).
fn noise(seed: &mut u32) -> f32 {
    *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    (*seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0
}

/// A soft white disc fading to nothing at its edge: each light is this, tinted and stretched.
fn glow_sprite() -> SharedPixelBuffer<Rgba8Pixel> {
    const N: u32 = 256;
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(N, N);
    let mut seed = 7u32;
    for (i, p) in buf.make_mut_slice().iter_mut().enumerate() {
        let x = (i as u32 % N) as f32 / (N / 2) as f32 - 1.0;
        let y = (i as u32 / N) as f32 / (N / 2) as f32 - 1.0;
        let r2 = x * x + y * y;
        let a = (-r2 * 3.2).exp() * (1.0 - r2).max(0.0).powf(1.5);
        *p = Rgba8Pixel {
            r: 255,
            g: 255,
            b: 255,
            a: (a * 255.0 + noise(&mut seed) * 0.6).clamp(0.0, 255.0) as u8,
        };
    }
    buf
}

/// Film grain, tiled over the stage: faint specks of light and dark.
fn grain_tile() -> SharedPixelBuffer<Rgba8Pixel> {
    const N: u32 = 160;
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(N, N);
    let mut seed = 99u32;
    for p in buf.make_mut_slice() {
        let v = noise(&mut seed);
        let c = if v > 0.0 { 255 } else { 0 };
        *p = Rgba8Pixel {
            r: c,
            g: c,
            b: c,
            a: (v.abs() * 9.0) as u8,
        };
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            let [r, g, b] = f(x, y);
            image::Rgba([r, g, b, 255])
        })
    }

    #[test]
    fn palette_lights_from_the_bright_side() {
        // Dark blue, with a bright orange top right.
        let img = solid(120, 120, |x, y| {
            if x >= 60 && y < 60 {
                [240, 140, 40]
            } else {
                [20, 30, 90]
            }
        });
        let ([key, fill, accent, deep], layout) = palette(&img);
        assert_eq!(layout, 1);
        // Lights are bright, the shadow dark, and the two lights different hues.
        assert!(hsv(key).2 > 0.9 && hsv(fill).2 > 0.9);
        assert!(hsv(deep).2 < 0.2);
        assert!(hue_apart(hsv(key).0, hsv(fill).0) > 30.0);
        assert!(lum(accent) > 0.5);
    }

    #[test]
    fn grey_covers_light_warm() {
        let img = solid(64, 64, |x, _| {
            let v = (x * 4) as u8;
            [v, v, v]
        });
        let ([key, ..], _) = palette(&img);
        // Tungsten: red over green over blue.
        assert!(key[0] > key[1] && key[1] > key[2]);
    }

    #[test]
    fn atmosphere_stays_dark() {
        let img = solid(300, 300, |_, _| [255, 255, 255]);
        let (colors, layout) = palette(&img);
        let a = atmosphere(Some(&img), None, &colors, layout);
        assert_eq!(a.dimensions(), (BACK_W, BACK_H));
        assert!(
            a.pixels()
                .all(|p| p.0[0] <= 132 && p.0[1] <= 132 && p.0[2] <= 132)
        );
        let none = atmosphere(None, None, &colors, layout);
        assert_eq!(none.dimensions(), (BACK_W, BACK_H));
    }

    #[test]
    fn light_follows_the_bass_and_finds_beats() {
        let mut l = Light::default();
        let mut s = l.step([0.0; 4], 0.033, false);
        for _ in 0..30 {
            s = l.step([0.01, 0.01, 0.01, 0.01], 0.033, false);
        }
        // A kick: the bass jumps.
        let mut beat = false;
        for k in 0..60 {
            let bass = if k % 15 == 0 { 0.2 } else { 0.01 };
            s = l.step([bass, 0.01, 0.01, 0.01], 0.033, false);
            beat |= s.pulse > 0.4;
        }
        assert!(beat);
        assert!(s.phase > 0.0);
        // With animations off, the lights hold still and don't pulse.
        let mut still = Light::default();
        let s = still.step([0.5; 4], 0.033, true);
        assert_eq!(s.phase, 0.0);
        assert_eq!(s.pulse, 0.0);
    }
}
