//! Audio output: cpal (WASAPI shared mode) at the device's mix rate.
//!
//! The render callback is wait-free: it never blocks, allocates or logs. It pops stereo frames
//! from the ring, runs the EQ (skipped when flat) and volume (skipped at 100%), and writes them
//! to the device. With a flat EQ, full volume and a matching rate, samples pass through
//! untouched: bit-perfect. On the way it measures four bands (bass to treble) for the visualizer,
//! reading the samples without changing them.
//!
//! Pausing never cuts the wave off: the callback plays on for [`FADE_MS`] while fading out,
//! and fades in again on resume. A flush (seek, skip) can't fade what it drops, so the last
//! sample heard decays to zero over the same time while the new audio fades in.
//!
//! Synced starts (Listen Along): the engine can mark a ring frame as a gate with the instant it
//! must be heard at. Audio before the gate plays as usual; at the gate the callback writes
//! silence until the moment comes (from the device's own playback timestamps), then carries on.
//! It never drops audio: a start that comes too late simply starts late.

use super::clock;
use super::eq::{Eq, SharedEq};
use crate::{Error, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const CH: usize = 2;
/// Frames processed per inner step of the callback.
const STEP: usize = 4096;
/// `gate_frame`: no gate.
pub const NO_GATE: u64 = u64::MAX;
/// `gate_when`: held until told when.
pub const HOLD: u64 = u64::MAX;
/// Length of the pause, resume and flush fades.
pub const FADE_MS: u32 = 8;

/// State shared between the engine and the callback.
#[derive(Default)]
pub struct Shared {
    /// Frames taken from the ring so far (including ones dropped by a flush). Drives position.
    pub played: AtomicU64,
    pub paused: AtomicBool,
    /// f32 bits, linear 0..=1.
    pub volume: AtomicU32,
    pub eq: SharedEq,
    /// Set while audio is expected, so a short buffer counts as an underrun.
    pub expecting: AtomicBool,
    pub underruns: AtomicU64,
    /// Ask the callback to clear filter state (after a flush).
    pub reset_eq: AtomicBool,
    /// Counts flushes, so the callback smooths over what was dropped.
    pub flushes: AtomicU64,
    /// f32 bits: the last buffer's loudness (RMS) in four bands, bass first, before volume.
    pub levels: [AtomicU32; 4],
    /// The ring frame that waits for `gate_when` before it is heard ([`NO_GATE`]: none).
    pub gate_frame: AtomicU64,
    /// When the gate frame must be heard, in [`clock`] ns ([`HOLD`]: not yet known).
    pub gate_when: AtomicU64,
    /// How late the last gate opened, ns (0: on time).
    pub gate_late: AtomicU64,
    /// Where ring frames are heard: frame `anchor_frame` at `anchor_ns`, and the ones after it
    /// one device frame apart. Written by the callback; `anchor_seq` is odd while it writes.
    anchor_seq: AtomicU64,
    anchor_frame: AtomicU64,
    anchor_ns: AtomicU64,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        let s = Shared::default();
        s.volume.store(1f32.to_bits(), Ordering::Relaxed);
        s.gate_frame.store(NO_GATE, Ordering::Relaxed);
        Arc::new(s)
    }

    /// Makes ring frame `frame` wait until `when_ns` ([`HOLD`]: until [`Shared::open_gate`]).
    pub fn set_gate(&self, frame: u64, when_ns: u64) {
        self.gate_frame.store(NO_GATE, Ordering::SeqCst);
        self.gate_when.store(when_ns, Ordering::SeqCst);
        self.gate_late.store(0, Ordering::SeqCst);
        self.gate_frame.store(frame, Ordering::SeqCst);
    }

    /// Says when a held gate opens.
    pub fn open_gate(&self, when_ns: u64) {
        self.gate_when.store(when_ns, Ordering::SeqCst);
    }

    pub fn clear_gate(&self) {
        self.gate_frame.store(NO_GATE, Ordering::SeqCst);
    }

    /// The ring frame waiting at a gate, if any.
    pub fn gate(&self) -> Option<u64> {
        Some(self.gate_frame.load(Ordering::SeqCst)).filter(|&f| f != NO_GATE)
    }

    /// (ring frame, the clock ns it is heard at), as of the last callback.
    pub fn anchor(&self) -> Option<(u64, u64)> {
        loop {
            let s1 = self.anchor_seq.load(Ordering::SeqCst);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let f = self.anchor_frame.load(Ordering::SeqCst);
            let t = self.anchor_ns.load(Ordering::SeqCst);
            if self.anchor_seq.load(Ordering::SeqCst) == s1 {
                return (s1 != 0).then_some((f, t));
            }
        }
    }

    pub(crate) fn set_anchor(&self, frame: u64, ns: u64) {
        self.anchor_seq.fetch_add(1, Ordering::SeqCst);
        self.anchor_frame.store(frame, Ordering::SeqCst);
        self.anchor_ns.store(ns, Ordering::SeqCst);
        self.anchor_seq.fetch_add(1, Ordering::SeqCst);
    }
}

pub type SharedConsumer = Arc<Mutex<Consumer<f32>>>;

pub struct Output {
    /// `None`: a null output that plays nothing (tests drive the ring themselves).
    stream: Option<cpal::Stream>,
    pub rate: u32,
    pub channels: usize,
    running: bool,
}

/// The default device's mix format: (rate, channels).
pub fn device_format() -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    let host = cpal::default_host();
    let dev = host
        .default_output_device()
        .ok_or_else(|| Error::Device("no output device".into()))?;
    let cfg = dev
        .default_output_config()
        .map_err(|e| Error::Device(e.to_string()))?;
    Ok((dev, cfg))
}

impl Output {
    /// Builds a stopped stream on `dev`. `on_error` is told about device loss/changes: errors
    /// after which the stream must be rebuilt (see [`needs_rebuild`]).
    pub fn open(
        dev: &cpal::Device,
        cfg: &cpal::SupportedStreamConfig,
        shared: Arc<Shared>,
        ring: SharedConsumer,
        on_error: impl FnMut(cpal::Error) + Send + 'static,
    ) -> Result<Self> {
        let rate = cfg.sample_rate();
        let channels = cfg.channels() as usize;
        let config: cpal::StreamConfig = (*cfg).into();
        let stream = match cfg.sample_format() {
            SampleFormat::F32 => build::<f32>(dev, config, rate, shared, ring, on_error),
            SampleFormat::I16 => build::<i16>(dev, config, rate, shared, ring, on_error),
            SampleFormat::I32 => build::<i32>(dev, config, rate, shared, ring, on_error),
            f => return Err(Error::Device(format!("unsupported sample format {f}"))),
        }?;
        Ok(Output {
            stream: Some(stream),
            rate,
            channels,
            running: false,
        })
    }

    /// An output without a device: nothing reads the ring.
    pub fn null(rate: u32) -> Self {
        Output {
            stream: None,
            rate,
            channels: CH,
            running: false,
        }
    }

    pub fn start(&mut self) {
        if !self.running {
            if let Some(Err(e)) = self.stream.as_ref().map(|s| s.play()) {
                log::error!("output start failed: {e}");
            }
            self.running = true;
        }
    }

    /// Stops the device stream entirely (0% CPU while idle or paused).
    pub fn stop(&mut self) {
        if self.running {
            if let Some(s) = &self.stream {
                let _ = s.pause();
            }
            self.running = false;
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }
}

fn build<T>(
    dev: &cpal::Device,
    config: cpal::StreamConfig,
    rate: u32,
    shared: Arc<Shared>,
    ring: SharedConsumer,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let dev_ch = config.channels as usize;
    let mut r = Render::new(rate);
    let mut on_error = on_error;
    let on_error = move |e: cpal::Error| {
        if needs_rebuild(e.kind()) {
            on_error(e);
        } else {
            log::debug!("output: {e}");
        }
    };
    dev.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            no_denormals();
            // When this buffer's first frame will be heard, on the device's own clock.
            let ts = info.timestamp();
            let delay = ts.playback.saturating_duration_since(ts.callback);
            r.fill(data, dev_ch, &shared, &ring, delay)
        },
        on_error,
        None,
    )
    .map_err(|e| Error::Device(e.to_string()))
}

/// Flushes denormals to zero on this thread (SSE FTZ and DAZ). An EQ filter ringing down into
/// silence otherwise spends many times the usual cost on every sample.
#[inline]
pub(crate) fn no_denormals() {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: only this thread's SSE control register changes, and only the FTZ (bit 15) and
    // DAZ (bit 6) flags, which no Rust code relies on.
    unsafe {
        let mut csr = 0u32;
        std::arch::asm!("stmxcsr [{}]", in(reg) &mut csr, options(nostack, preserves_flags));
        csr |= 0x8040;
        std::arch::asm!(
            "ldmxcsr [{}]",
            in(reg) &csr,
            options(nostack, preserves_flags, readonly)
        );
    }
}

/// Whether a stream error means the stream is dead or on the wrong device. Glitches, a refused
/// real-time priority and a route the backend already followed by itself don't.
pub fn needs_rebuild(kind: cpal::ErrorKind) -> bool {
    use cpal::ErrorKind as K;
    !matches!(kind, K::Xrun | K::RealtimeDenied | K::DeviceChanged)
}

/// The callback's private state.
pub struct Render {
    eq: Eq,
    scratch: Vec<f32>,
    /// The gain last applied; `None` until the first buffer, which starts at the set volume
    /// (a fresh stream, e.g. after a device change, must not ramp down from full volume).
    gain: Option<f32>,
    meter: Meter,
    rate: u32,
    /// Pause and resume fade: frames into the fade in (`fade_len`: full level).
    fade: usize,
    fade_len: usize,
    /// The last frame written, and the frames left of its decay after a flush.
    last: [f32; CH],
    tail: usize,
    /// [`Shared::flushes`] as last seen.
    flushes: u64,
}

/// Splits mono into four bands with one-pole low-passes at 150 Hz, 600 Hz and 2.5 kHz (each
/// band is the difference between neighbours) and keeps each band's RMS.
struct Meter {
    k: [f32; 3],
    lp: [f32; 3],
}

impl Meter {
    fn new(rate: u32) -> Self {
        let k = [150.0f32, 600.0, 2500.0]
            .map(|f| 1.0 - (-2.0 * std::f32::consts::PI * f / rate.max(1) as f32).exp());
        Meter { k, lp: [0.0; 3] }
    }

    fn measure(&mut self, buf: &[f32], out: &[AtomicU32; 4]) {
        let frames = buf.len() / CH;
        if frames == 0 {
            return;
        }
        let mut sum = [0f32; 4];
        for f in buf.as_chunks::<CH>().0 {
            let x = (f[0] + f[1]) * 0.5;
            for i in 0..3 {
                self.lp[i] += self.k[i] * (x - self.lp[i]);
            }
            let bands = [
                self.lp[0],
                self.lp[1] - self.lp[0],
                self.lp[2] - self.lp[1],
                x - self.lp[2],
            ];
            for i in 0..4 {
                sum[i] += bands[i] * bands[i];
            }
        }
        for i in 0..4 {
            out[i].store((sum[i] / frames as f32).sqrt().to_bits(), Ordering::Relaxed);
        }
    }
}

impl Render {
    pub fn new(rate: u32) -> Self {
        let fade_len = (rate * FADE_MS / 1000).max(1) as usize;
        Render {
            eq: Eq::new(rate, STEP),
            scratch: vec![0.0; STEP * CH],
            gain: None,
            meter: Meter::new(rate),
            rate: rate.max(1),
            fade: fade_len,
            fade_len,
            last: [0.0; CH],
            tail: 0,
            flushes: 0,
        }
    }

    /// Fills `data`, whose first frame is heard `delay` from now.
    pub fn fill<T: SizedSample + FromSample<f32>>(
        &mut self,
        data: &mut [T],
        dev_ch: usize,
        shared: &Shared,
        ring: &SharedConsumer,
        delay: std::time::Duration,
    ) {
        let heard = clock::now_ns() + delay.as_nanos() as u64;
        let rate = self.rate as u64;
        let ns_per = |frames: usize| frames as u64 * 1_000_000_000 / rate;
        if shared.reset_eq.swap(false, Ordering::Relaxed) {
            self.eq.reset();
        }
        let paused = shared.paused.load(Ordering::Relaxed);
        let flushes = shared.flushes.load(Ordering::Relaxed);
        if flushes != self.flushes {
            self.flushes = flushes;
            self.declick();
        }
        // Pausing plays on while it fades out, then stops taking audio.
        let fading_out = paused && self.fade > 0;
        // The engine holds the lock only for a moment while flushing; output silence then.
        let mut cons = if paused && !fading_out {
            None
        } else {
            ring.try_lock().ok()
        };
        if cons.is_none() && !paused {
            self.declick();
        }
        let total = data.len() / dev_ch;
        let mut done = 0;
        while done < total {
            let mut n = (total - done).min(STEP);
            let mut take = true;
            if let Some(gate) = shared.gate() {
                let played = shared.played.load(Ordering::Relaxed);
                if played < gate {
                    n = n.min((gate - played) as usize);
                } else {
                    let when = shared.gate_when.load(Ordering::SeqCst);
                    let at = heard + ns_per(done);
                    if when == HOLD {
                        take = false;
                    } else if when > at {
                        // Silence up to the frame that is heard at `when`.
                        let wait = ((when - at) as u128 * rate as u128).div_ceil(1_000_000_000);
                        n = n.min(wait as usize);
                        take = false;
                    } else {
                        // The moment came (or passed: then start now, never skip audio).
                        shared.gate_late.store(at - when, Ordering::Relaxed);
                        let _ = shared.gate_frame.compare_exchange(
                            gate,
                            NO_GATE,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                        continue;
                    }
                }
            }
            if paused {
                // Only the fade-out is still taken.
                take &= self.fade > 0;
                if take {
                    n = n.min(self.fade);
                }
            }
            let out = &mut data[done * dev_ch..(done + n) * dev_ch];
            let src = if take { cons.as_deref_mut() } else { None };
            self.segment(out, dev_ch, shared, src, take && !paused, paused);
            done += n;
        }
        // From the next frame on, frames are heard one after another from the end of this
        // buffer (until a gate, a pause or an underrun puts silence between).
        shared.set_anchor(shared.played.load(Ordering::Relaxed), heard + ns_per(total));
    }

    /// What was heard stops here: the last frame decays away and what comes next fades in.
    fn declick(&mut self) {
        self.tail = self.fade_len;
        self.fade = 0;
    }

    /// Writes `out` from the ring (`None`: silence). `expect`: a short read is an underrun.
    /// `paused`: fading out.
    fn segment<T: SizedSample + FromSample<f32>>(
        &mut self,
        out: &mut [T],
        dev_ch: usize,
        shared: &Shared,
        cons: Option<&mut Consumer<f32>>,
        expect: bool,
        paused: bool,
    ) {
        let frames = out.len() / dev_ch;
        let got = match cons {
            Some(c) => {
                let n = (c.slots() / CH).min(frames);
                let (a, _) = c.pop_partial_slice(&mut self.scratch[..n * CH]);
                a.len() / CH
            }
            None => 0,
        };
        shared.played.fetch_add(got as u64, Ordering::Relaxed);
        if got < frames && expect && shared.expecting.load(Ordering::Relaxed) {
            shared.underruns.fetch_add(1, Ordering::Relaxed);
        }
        let buf = &mut self.scratch[..got * CH];
        self.eq.process(&shared.eq, buf);
        self.meter.measure(buf, &shared.levels);
        let target = f32::from_bits(shared.volume.load(Ordering::Relaxed));
        ramp_gain(buf, self.gain.get_or_insert(target), target);
        self.apply_fade(got, paused);
        let len = if self.tail > 0 {
            self.add_tail(got, frames)
        } else {
            got
        };
        let buf = &self.scratch[..len * CH];
        self.last = match buf.as_chunks::<CH>().0.last() {
            Some(f) if len == frames => *f,
            // Silence follows: nothing to decay from.
            _ => [0.0; CH],
        };
        write_out(out, buf, dev_ch);
    }

    /// Fades the first `frames` of the scratch buffer out (`paused`) or in. Untouched at full
    /// level.
    fn apply_fade(&mut self, frames: usize, paused: bool) {
        if !paused && self.fade == self.fade_len {
            return;
        }
        for f in self.scratch[..frames * CH].as_chunks_mut::<CH>().0 {
            if paused {
                self.fade = self.fade.saturating_sub(1);
            } else if self.fade < self.fade_len {
                self.fade += 1;
            }
            let g = self.fade as f32 / self.fade_len as f32;
            f[0] *= g;
            f[1] *= g;
        }
    }

    /// Adds the decaying last frame over `frames`, silence after the `got` frames read;
    /// returns how many frames the scratch buffer now holds.
    fn add_tail(&mut self, got: usize, frames: usize) -> usize {
        self.scratch[got * CH..frames * CH].fill(0.0);
        for f in self.scratch[..frames * CH].as_chunks_mut::<CH>().0 {
            if self.tail == 0 {
                break;
            }
            let k = self.tail as f32 / self.fade_len as f32;
            f[0] += self.last[0] * k;
            f[1] += self.last[1] * k;
            self.tail -= 1;
        }
        frames
    }
}

/// Applies volume with a short linear ramp to the new target. Untouched at exactly 1.0.
fn ramp_gain(buf: &mut [f32], gain: &mut f32, target: f32) {
    if *gain == target {
        if target != 1.0 {
            buf.iter_mut().for_each(|s| *s *= target);
        }
        return;
    }
    let frames = buf.len() / CH;
    let step = (target - *gain) / frames.max(1) as f32;
    for f in buf.as_chunks_mut::<CH>().0 {
        *gain += step;
        f[0] *= *gain;
        f[1] *= *gain;
    }
    *gain = target;
}

/// Stereo → device layout; silence after the available frames.
fn write_out<T: SizedSample + FromSample<f32>>(out: &mut [T], stereo: &[f32], dev_ch: usize) {
    let silence = T::from_sample(0.0f32);
    let got = stereo.len() / CH;
    for (i, f) in out.chunks_exact_mut(dev_ch).enumerate() {
        if i < got {
            let (l, r) = (stereo[i * CH], stereo[i * CH + 1]);
            match dev_ch {
                1 => f[0] = T::from_sample((l + r) * 0.5),
                _ => {
                    f[0] = T::from_sample(l);
                    f[1] = T::from_sample(r);
                    f[2..].iter_mut().for_each(|s| *s = silence);
                }
            }
        } else {
            f.iter_mut().for_each(|s| *s = silence);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::eq::EqSettings;
    use std::time::Duration;

    #[test]
    fn flat_path_is_bit_perfect() {
        let shared = Shared::new();
        shared.eq.set(&EqSettings {
            enabled: true,
            gains: [0.0; 10],
            preamp: None,
        });
        let (mut p, c) = rtrb::RingBuffer::<f32>::new(1 << 16);
        let input: Vec<f32> = (0..20000)
            .map(|i| ((i as f32) * 0.013).sin() * 1.2)
            .collect();
        let _ = p.push_partial_slice(&input);
        let ring = Arc::new(Mutex::new(c));
        let mut r = Render::new(48000);
        let mut out = vec![0f32; 10000];
        r.fill(&mut out, 2, &shared, &ring, Duration::ZERO);
        let mut out2 = vec![0f32; 10000];
        r.fill(&mut out2, 2, &shared, &ring, Duration::ZERO);
        out.extend(out2);
        assert!(
            input
                .iter()
                .zip(&out)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
        assert_eq!(shared.played.load(Ordering::Relaxed), 10000);
    }

    /// A ring of `n` frames of 1.0, with a fresh renderer at 48 kHz.
    fn ones(n: usize) -> (Arc<Shared>, SharedConsumer, Render) {
        let shared = Shared::new();
        let (mut p, c) = rtrb::RingBuffer::<f32>::new(1 << 16);
        let _ = p.push_partial_slice(&vec![1.0; n * 2]);
        (shared, Arc::new(Mutex::new(c)), Render::new(48000))
    }

    #[test]
    fn gate_waits_with_silence_until_its_moment() {
        let (shared, ring, mut r) = ones(4000);
        // Frame 10 must be heard 10 ms (480 frames) after this buffer's first frame.
        shared.set_gate(10, clock::now_ns() + 10_000_000);
        let mut out = vec![0f32; 2000 * 2];
        r.fill(&mut out, 2, &shared, &ring, Duration::ZERO);
        let first_after = out[20..].iter().position(|&s| s == 1.0).unwrap() / 2 + 10;
        assert!(out[..20].iter().all(|&s| s == 1.0));
        // The callback starts a moment after `when` was taken: up to a few frames early.
        assert!((470..=481).contains(&first_after), "{first_after}");
        assert!(out[first_after * 2..].iter().all(|&s| s == 1.0));
        assert_eq!(shared.gate(), None);
        // No audio was dropped: 10 before the gate, the rest after.
        let heard = out.iter().filter(|&&s| s == 1.0).count() / 2;
        assert_eq!(shared.played.load(Ordering::Relaxed), heard as u64);
    }

    #[test]
    fn held_gate_plays_nothing_past_it() {
        let (shared, ring, mut r) = ones(4000);
        shared.set_gate(100, HOLD);
        let mut out = vec![0f32; 1000 * 2];
        r.fill(&mut out, 2, &shared, &ring, Duration::ZERO);
        assert_eq!(shared.played.load(Ordering::Relaxed), 100);
        assert!(out[200..].iter().all(|&s| s == 0.0));
        assert_eq!(shared.underruns.load(Ordering::Relaxed), 0);
        // Late: starts at once, nothing skipped.
        shared.open_gate(1);
        r.fill(&mut out, 2, &shared, &ring, Duration::ZERO);
        assert!(out.iter().all(|&s| s == 1.0));
        assert_eq!(shared.played.load(Ordering::Relaxed), 1100);
        assert!(shared.gate_late.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn anchor_says_when_the_next_frame_is_heard() {
        let (shared, ring, mut r) = ones(4000);
        let before = clock::now_ns();
        let mut out = vec![0f32; 480 * 2];
        r.fill(&mut out, 2, &shared, &ring, Duration::from_millis(20));
        let (f, ns) = shared.anchor().unwrap();
        assert_eq!(f, 480);
        // 20 ms latency + the 10 ms buffer.
        let d = ns - before;
        assert!((30_000_000..31_000_000).contains(&d), "{d}");
    }

    #[test]
    fn fresh_stream_starts_at_the_set_volume() {
        let shared = Shared::new();
        shared.volume.store(0.25f32.to_bits(), Ordering::Relaxed);
        let (mut p, c) = rtrb::RingBuffer::<f32>::new(1 << 12);
        let _ = p.push_partial_slice(&[1.0; 512]);
        let ring = Arc::new(Mutex::new(c));
        let mut r = Render::new(48000);
        let mut out = vec![0f32; 512];
        r.fill(&mut out, 2, &shared, &ring, Duration::ZERO);
        assert!(out.iter().all(|&s| s == 0.25), "{:?}", &out[..4]);
    }

    /// The largest jump between neighbouring samples of the left channel.
    fn largest_step(out: &[f32]) -> f32 {
        out.iter()
            .step_by(2)
            .zip(out.iter().step_by(2).skip(1))
            .map(|(a, b)| (b - a).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn pause_and_resume_fade_instead_of_clicking() {
        let (shared, ring, mut r) = ones(20000);
        let mut out = vec![0f32; 960 * 2];
        r.fill(&mut out, 2, &shared, &ring, Duration::ZERO);
        shared.paused.store(true, Ordering::Relaxed);
        let mut paused = vec![0f32; 960 * 2];
        r.fill(&mut paused, 2, &shared, &ring, Duration::ZERO);
        // 8 ms more were played, fading out; then silence.
        assert_eq!(shared.played.load(Ordering::Relaxed), 960 + 384);
        assert!(paused[384 * 2..].iter().all(|&s| s == 0.0));
        shared.paused.store(false, Ordering::Relaxed);
        let mut resumed = vec![0f32; 960 * 2];
        r.fill(&mut resumed, 2, &shared, &ring, Duration::ZERO);
        assert!(resumed[384 * 2..].iter().all(|&s| s == 1.0));
        let all = [out, paused, resumed].concat();
        assert!(largest_step(&all) < 0.01, "{}", largest_step(&all));
    }

    #[test]
    fn a_flush_decays_the_old_audio_and_fades_in_the_new() {
        let shared = Shared::new();
        let (mut p, c) = rtrb::RingBuffer::<f32>::new(1 << 16);
        let ring = Arc::new(Mutex::new(c));
        let _ = p.push_partial_slice(&[0.8; 4000]);
        let mut r = Render::new(48000);
        let mut out = vec![0f32; 960 * 2];
        r.fill(&mut out, 2, &shared, &ring, Duration::ZERO);
        // What the engine does on a seek: drop what is queued, then queue the new place.
        shared.flushes.fetch_add(1, Ordering::Relaxed);
        {
            let mut c = ring.lock().unwrap();
            let n = c.slots();
            c.read_chunk(n).unwrap().commit_all();
        }
        let _ = p.push_partial_slice(&[-0.8; 4000]);
        let mut after = vec![0f32; 960 * 2];
        r.fill(&mut after, 2, &shared, &ring, Duration::ZERO);
        assert!(after[384 * 2..].iter().all(|&s| s == -0.8));
        let all = [out, after].concat();
        assert!(largest_step(&all) < 0.01, "{}", largest_step(&all));
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn denormals_become_zero_on_the_audio_thread() {
        std::thread::spawn(|| {
            let tiny = std::hint::black_box(f32::MIN_POSITIVE);
            assert!(tiny * std::hint::black_box(0.5) > 0.0);
            no_denormals();
            assert_eq!(tiny * std::hint::black_box(0.5), 0.0);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn only_dead_streams_are_rebuilt() {
        use cpal::ErrorKind as K;
        assert!(needs_rebuild(K::StreamInvalidated));
        assert!(needs_rebuild(K::DeviceNotAvailable));
        assert!(needs_rebuild(K::BackendError));
        assert!(!needs_rebuild(K::Xrun));
        assert!(!needs_rebuild(K::RealtimeDenied));
        assert!(!needs_rebuild(K::DeviceChanged));
    }

    #[test]
    fn meter_sorts_bass_from_treble() {
        let rate = 48000.0;
        let level = |hz: f32| {
            let mut m = Meter::new(rate as u32);
            let tone: Vec<f32> = (0..9600)
                .flat_map(|i| {
                    let s = (i as f32 * hz * 2.0 * std::f32::consts::PI / rate).sin();
                    [s, s]
                })
                .collect();
            let out: [AtomicU32; 4] = Default::default();
            m.measure(&tone, &out);
            out.map(|a| f32::from_bits(a.load(Ordering::Relaxed)))
        };
        let low = level(60.0);
        let high = level(8000.0);
        assert!(low[0] > low[3] * 4.0, "{low:?}");
        assert!(high[3] > high[0] * 4.0, "{high:?}");
    }
}
