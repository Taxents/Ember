//! 10-band graphic EQ: RBJ biquads (low shelf, 8 peaks, high shelf) in transposed direct form II.
//!
//! Coefficients are designed in f64 and run in f32. A flat EQ is skipped entirely, so playback
//! stays bit-perfect. Changes crossfade between the old and new filters over one buffer, so
//! moving a slider never clicks.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const BANDS: [f64; 10] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];
pub const MAX_DB: f32 = 12.0;
const Q: f64 = 1.41;
const CH: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct EqSettings {
    pub enabled: bool,
    /// Per band, dB.
    pub gains: [f32; 10],
    /// dB. `None` = automatic: minus the loudest point of the whole curve, so boosts can't
    /// clip (neighbouring bands overlap, so their boosts add up).
    pub preamp: Option<f32>,
}

impl EqSettings {
    pub fn is_flat(&self) -> bool {
        !self.enabled || (self.gains.iter().all(|g| *g == 0.0) && self.preamp.unwrap_or(0.0) == 0.0)
    }

    pub fn preamp_db(&self) -> f32 {
        self.preamp
            .unwrap_or_else(|| -peak_db(&self.gains).max(0.0) as f32)
            .clamp(-24.0, MAX_DB)
    }

    pub fn preset(name: &str) -> Option<[f32; 10]> {
        Some(match name {
            "flat" => [0.0; 10],
            "bass" => [6.0, 5.0, 4.0, 2.0, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0],
            "vocal" => [-2.0, -2.0, -1.0, 0.0, 2.0, 3.0, 3.0, 2.0, 0.0, -1.0],
            "treble" => [0.0, 0.0, 0.0, 0.0, 0.0, 0.5, 2.0, 4.0, 5.0, 6.0],
            _ => return None,
        })
    }
}

/// Where the response is checked: log-spaced over the audible range.
const CURVE_POINTS: usize = 256;
/// The rate the curve is checked at (below 20 kHz the device rate barely matters).
const CURVE_RATE: f64 = 48000.0;

/// The filter kind of band `i`: low shelf, peaks, high shelf.
fn kind(i: usize) -> usize {
    match i {
        0 => 0,
        9 => 2,
        _ => 1,
    }
}

/// The gain of all bands together at `freq`, dB.
fn response_db(gains: &[f32; 10], freq: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / CURVE_RATE;
    gains
        .iter()
        .enumerate()
        .filter(|(_, g)| **g != 0.0)
        .map(|(i, g)| {
            let [b0, b1, b2, a1, a2] = coeffs(kind(i), BANDS[i], *g as f64, CURVE_RATE);
            let mag2 = |c0: f64, c1: f64, c2: f64| {
                let re = c0 + c1 * w.cos() + c2 * (2.0 * w).cos();
                let im = c1 * w.sin() + c2 * (2.0 * w).sin();
                re * re + im * im
            };
            10.0 * (mag2(b0, b1, b2) / mag2(1.0, a1, a2)).log10()
        })
        .sum()
}

/// The loudest point of the curve, dB.
fn peak_db(gains: &[f32; 10]) -> f64 {
    (0..CURVE_POINTS)
        .map(|i| 20.0 * 1000f64.powf(i as f64 / (CURVE_POINTS - 1) as f64))
        .map(|f| response_db(gains, f))
        .fold(f64::MIN, f64::max)
}

/// RBJ biquad coefficients, normalised: `[b0, b1, b2, a1, a2]`. `kind`: 0 low shelf, 1 peak,
/// 2 high shelf.
fn coeffs(kind: usize, freq: f64, db: f64, rate: f64) -> [f64; 5] {
    let a = 10f64.powf(db / 40.0);
    let w0 = 2.0 * std::f64::consts::PI * (freq / rate).min(0.49);
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * Q);
    let (b0, b1, b2, a0, a1, a2) = match kind {
        0 => {
            let k = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) - (a - 1.0) * cos + k),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - k),
                (a + 1.0) + (a - 1.0) * cos + k,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - k,
            )
        }
        2 => {
            let k = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) + (a - 1.0) * cos + k),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - k),
                (a + 1.0) - (a - 1.0) * cos + k,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - k,
            )
        }
        _ => (
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        ),
    };
    [b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0]
}

/// Lock-free hand-over of settings from the UI to the audio callback.
#[derive(Default)]
pub struct SharedEq {
    gains: [AtomicU32; 10],
    preamp: AtomicU32,
    enabled: AtomicU32,
    version: AtomicU64,
}

impl SharedEq {
    pub fn set(&self, s: &EqSettings) {
        for (a, g) in self.gains.iter().zip(s.gains) {
            a.store(g.clamp(-MAX_DB, MAX_DB).to_bits(), Ordering::Relaxed);
        }
        self.preamp
            .store(s.preamp_db().to_bits(), Ordering::Relaxed);
        self.enabled.store(!s.is_flat() as u32, Ordering::Relaxed);
        self.version.fetch_add(1, Ordering::Release);
    }

    fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    fn snapshot(&self) -> (bool, f32, [f32; 10]) {
        let mut g = [0.0; 10];
        for (o, a) in g.iter_mut().zip(&self.gains) {
            *o = f32::from_bits(a.load(Ordering::Relaxed));
        }
        let pre = f32::from_bits(self.preamp.load(Ordering::Relaxed));
        (self.enabled.load(Ordering::Relaxed) != 0, pre, g)
    }
}

#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    s1: [f32; CH],
    s2: [f32; CH],
}

impl Biquad {
    fn design(kind: usize, freq: f64, db: f64, rate: f64) -> Self {
        let [b0, b1, b2, a1, a2] = coeffs(kind, freq, db, rate).map(|c| c as f32);
        Biquad {
            b0,
            b1,
            b2,
            a1,
            a2,
            ..Default::default()
        }
    }

    /// New coefficients, keeping the running state.
    fn retune(&mut self, other: &Biquad) {
        (self.b0, self.b1, self.b2, self.a1, self.a2) =
            (other.b0, other.b1, other.b2, other.a1, other.a2);
    }

    #[inline]
    fn run(&mut self, x: f32, c: usize) -> f32 {
        let y = self.b0 * x + self.s1[c];
        self.s1[c] = self.b1 * x - self.a1 * y + self.s2[c];
        self.s2[c] = self.b2 * x - self.a2 * y;
        y
    }
}

#[derive(Clone, Copy)]
struct Bank {
    filters: [Biquad; 10],
    /// Which bands do anything (0 dB peaks are skipped).
    active: [bool; 10],
    pre: f32,
}

impl Bank {
    fn run(&mut self, buf: &mut [f32]) {
        for f in buf.as_chunks_mut::<CH>().0 {
            for (c, s) in f.iter_mut().enumerate() {
                let mut x = *s * self.pre;
                for (b, on) in self.filters.iter_mut().zip(self.active) {
                    if on {
                        x = b.run(x, c);
                    }
                }
                *s = x;
            }
        }
    }
}

/// The audio-thread side. Allocation-free.
pub struct Eq {
    rate: f64,
    seen: u64,
    on: bool,
    bank: Bank,
    scratch: Vec<f32>,
}

impl Eq {
    pub fn new(rate: u32, max_frames: usize) -> Self {
        Eq {
            rate: rate as f64,
            seen: u64::MAX,
            on: false,
            bank: Bank {
                filters: [Biquad::default(); 10],
                active: [false; 10],
                pre: 1.0,
            },
            scratch: vec![0.0; max_frames * CH],
        }
    }

    /// Filters interleaved stereo in place. Picks up new settings first.
    pub fn process(&mut self, shared: &SharedEq, buf: &mut [f32]) {
        let v = shared.version();
        if v != self.seen {
            self.seen = v;
            let (on, pre_db, gains) = shared.snapshot();
            let mut next = self.bank;
            for (i, (f, g)) in next.filters.iter_mut().zip(gains).enumerate() {
                f.retune(&Biquad::design(kind(i), BANDS[i], g as f64, self.rate));
                next.active[i] = g != 0.0;
            }
            next.pre = 10f32.powf(pre_db / 20.0);
            if !on {
                next.pre = 1.0;
                next.active = [false; 10];
            }
            // Crossfade from the old filter to the new one across this buffer.
            if (self.on || on) && buf.len() <= self.scratch.len() {
                let old = &mut self.scratch[..buf.len()];
                old.copy_from_slice(buf);
                let mut prev = self.bank;
                if self.on {
                    prev.run(old);
                }
                next.run(buf);
                let n = (buf.len() / CH).max(1) as f32;
                for (i, (o, s)) in old
                    .as_chunks::<CH>()
                    .0
                    .iter()
                    .zip(buf.as_chunks_mut::<CH>().0)
                    .enumerate()
                {
                    let t = i as f32 / n;
                    for c in 0..CH {
                        s[c] = o[c] * (1.0 - t) + s[c] * t;
                    }
                }
                self.bank = next;
                self.on = on;
                return;
            }
            self.bank = next;
            self.on = on;
        }
        if self.on {
            self.bank.run(buf);
        }
    }

    /// Clears filter memory (after a seek or track change that flushes audio).
    pub fn reset(&mut self) {
        for f in &mut self.bank.filters {
            f.s1 = [0.0; CH];
            f.s2 = [0.0; CH];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_is_bit_exact() {
        let shared = SharedEq::default();
        shared.set(&EqSettings {
            enabled: true,
            gains: [0.0; 10],
            preamp: None,
        });
        let mut eq = Eq::new(48000, 4096);
        let input: Vec<f32> = (0..2048).map(|i| ((i as f32) * 0.37).sin() * 1.3).collect();
        let mut buf = input.clone();
        eq.process(&shared, &mut buf);
        eq.process(&shared, &mut buf);
        assert!(
            input
                .iter()
                .zip(&buf)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
    }

    #[test]
    fn auto_preamp_covers_overlapping_boosts() {
        let mut one = [0.0; 10];
        one[5] = 6.0;
        let single = EqSettings {
            enabled: true,
            gains: one,
            preamp: None,
        };
        assert!(
            (single.preamp_db() + 6.0).abs() < 0.2,
            "{}",
            single.preamp_db()
        );
        // Neighbouring boosts add up past the largest one.
        let bass = EqSettings {
            gains: EqSettings::preset("bass").unwrap(),
            ..single
        };
        assert!(bass.preamp_db() < -6.5, "{}", bass.preamp_db());
        assert!(peak_db(&bass.gains) + (bass.preamp_db() as f64) < 0.01);
        // Cuts need next to none (only the shelves' small overshoot).
        let cut = EqSettings {
            gains: [-6.0; 10],
            ..single
        };
        assert!(cut.preamp_db() > -0.5, "{}", cut.preamp_db());
    }

    #[test]
    fn a_full_scale_tone_at_the_loudest_point_does_not_clip() {
        let gains = EqSettings::preset("bass").unwrap();
        let loudest = (0..CURVE_POINTS)
            .map(|i| 20.0 * 1000f64.powf(i as f64 / (CURVE_POINTS - 1) as f64))
            .max_by(|a, b| response_db(&gains, *a).total_cmp(&response_db(&gains, *b)))
            .unwrap();
        let shared = SharedEq::default();
        shared.set(&EqSettings {
            enabled: true,
            gains,
            preamp: None,
        });
        let mut eq = Eq::new(48000, 48000);
        let mut buf: Vec<f32> = (0..48000)
            .flat_map(|i| {
                let s = (2.0 * std::f64::consts::PI * loudest * i as f64 / 48000.0).sin() as f32;
                [s, s]
            })
            .collect();
        eq.process(&shared, &mut buf[..2]);
        eq.process(&shared, &mut buf[2..]);
        let peak = buf[48000..].iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak <= 1.01, "peak {peak} at {loudest:.0} Hz");
    }

    #[test]
    fn boost_raises_band_level() {
        let shared = SharedEq::default();
        let mut g = [0.0; 10];
        g[5] = 6.0;
        shared.set(&EqSettings {
            enabled: true,
            gains: g,
            preamp: Some(0.0),
        });
        let mut eq = Eq::new(48000, 48000);
        let tone: Vec<f32> = (0..48000)
            .flat_map(|i| {
                let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin() * 0.1;
                [s, s]
            })
            .collect();
        let mut buf = tone.clone();
        eq.process(&shared, &mut buf[..2]);
        eq.process(&shared, &mut buf[2..]);
        let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
        let db = 20.0 * (rms(&buf[48000..]) / rms(&tone[48000..])).log10();
        assert!((db - 6.0).abs() < 0.3, "1 kHz gain {db} dB");
    }
}
