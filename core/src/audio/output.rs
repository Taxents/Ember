//! Audio output: cpal (WASAPI shared mode) at the device's mix rate.
//!
//! The render callback is wait-free: it never blocks, allocates or logs. It pops stereo frames
//! from the ring, runs the EQ (skipped when flat) and volume (skipped at 100%), and writes them
//! to the device. With a flat EQ, full volume and a matching rate, samples pass through
//! untouched: bit-perfect.

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
}

impl Shared {
    pub fn new() -> Arc<Self> {
        let s = Shared::default();
        s.volume.store(1f32.to_bits(), Ordering::Relaxed);
        Arc::new(s)
    }
}

pub type SharedConsumer = Arc<Mutex<Consumer<f32>>>;

pub struct Output {
    stream: cpal::Stream,
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
    /// Builds a stopped stream on `dev`. `on_error` is told about device loss/changes.
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
            stream,
            rate,
            channels,
            running: false,
        })
    }

    pub fn start(&mut self) {
        if !self.running {
            if let Err(e) = self.stream.play() {
                log::error!("output start failed: {e}");
            }
            self.running = true;
        }
    }

    /// Stops the device stream entirely (0% CPU while idle or paused).
    pub fn stop(&mut self) {
        if self.running {
            let _ = self.stream.pause();
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
    dev.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| r.fill(data, dev_ch, &shared, &ring),
        on_error,
        None,
    )
    .map_err(|e| Error::Device(e.to_string()))
}

/// The callback's private state.
pub struct Render {
    eq: Eq,
    scratch: Vec<f32>,
    gain: f32,
}

impl Render {
    pub fn new(rate: u32) -> Self {
        Render {
            eq: Eq::new(rate, STEP),
            scratch: vec![0.0; STEP * CH],
            gain: 1.0,
        }
    }

    pub fn fill<T: SizedSample + FromSample<f32>>(
        &mut self,
        data: &mut [T],
        dev_ch: usize,
        shared: &Shared,
        ring: &SharedConsumer,
    ) {
        if shared.reset_eq.swap(false, Ordering::Relaxed) {
            self.eq.reset();
        }
        let paused = shared.paused.load(Ordering::Relaxed);
        // The engine holds the lock only for a moment while flushing; output silence then.
        let mut cons = if paused { None } else { ring.try_lock().ok() };
        for out in data.chunks_mut(STEP * dev_ch) {
            let frames = out.len() / dev_ch;
            let got = match cons.as_mut() {
                Some(c) => {
                    let n = (c.slots() / CH).min(frames);
                    let (a, _) = c.pop_partial_slice(&mut self.scratch[..n * CH]);
                    a.len() / CH
                }
                None => 0,
            };
            shared.played.fetch_add(got as u64, Ordering::Relaxed);
            if got < frames && !paused && shared.expecting.load(Ordering::Relaxed) {
                shared.underruns.fetch_add(1, Ordering::Relaxed);
            }
            let buf = &mut self.scratch[..got * CH];
            self.eq.process(&shared.eq, buf);
            let target = f32::from_bits(shared.volume.load(Ordering::Relaxed));
            ramp_gain(buf, &mut self.gain, target);
            write_out(out, buf, dev_ch);
        }
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
        r.fill(&mut out, 2, &shared, &ring);
        let mut out2 = vec![0f32; 10000];
        r.fill(&mut out2, 2, &shared, &ring);
        out.extend(out2);
        assert!(
            input
                .iter()
                .zip(&out)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
        assert_eq!(shared.played.load(Ordering::Relaxed), 10000);
    }
}
