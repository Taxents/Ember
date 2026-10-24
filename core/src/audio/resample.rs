//! Fixed-ratio, high-quality sample-rate conversion (only used when a song's rate differs from
//! the device's, e.g. 44.1 kHz AAC on a 48 kHz device).
//!
//! The resampler's delay is removed and the tail flushed, so each song comes out at exactly
//! `round(frames × ratio)` frames: gapless survives resampling.

use crate::{Error, Result};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler as _};

const CH: usize = 2;
const CHUNK: usize = 1024;

pub struct Resampler {
    r: Fft<f32>,
    ratio: f64,
    pending: Vec<f32>,
    out: Vec<f32>,
    delay_left: usize,
    in_total: u64,
    out_total: u64,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Result<Self> {
        if from == 0 || to == 0 {
            return Err(Error::Media(format!("can't resample {from} Hz to {to} Hz")));
        }
        let r = Fft::<f32>::new(from as usize, to as usize, CHUNK, CH, FixedSync::Input)
            .map_err(|e| Error::Media(format!("resampler: {e}")))?;
        let out = vec![0.0; r.output_frames_max() * CH];
        let delay_left = r.output_delay();
        Ok(Resampler {
            r,
            ratio: to as f64 / from as f64,
            pending: Vec::with_capacity(CHUNK * CH * 2),
            out,
            delay_left,
            in_total: 0,
            out_total: 0,
        })
    }

    /// Resamples `input` (interleaved stereo), appending what's ready to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        self.in_total += (input.len() / CH) as u64;
        self.pending.extend_from_slice(input);
        while self.pending.len() >= CHUNK * CH {
            self.run(CHUNK, out)?;
            self.pending.drain(..CHUNK * CH);
        }
        Ok(())
    }

    /// Flushes the remaining input and the resampler's delay; output is then exact.
    pub fn finish(&mut self, out: &mut Vec<f32>) -> Result<()> {
        let want = (self.in_total as f64 * self.ratio).round() as u64;
        let rest = self.pending.len() / CH;
        self.pending.resize(CHUNK * CH, 0.0);
        self.run(rest, out)?;
        self.pending.clear();
        self.pending.resize(CHUNK * CH, 0.0);
        while self.out_total < want {
            self.run(0, out)?;
        }
        // Drop anything produced past the exact length.
        let extra = (self.out_total - want) as usize * CH;
        out.truncate(out.len() - extra.min(out.len()));
        self.out_total = want;
        self.pending.clear();
        Ok(())
    }

    fn run(&mut self, valid: usize, out: &mut Vec<f32>) -> Result<()> {
        let bad = |e: &dyn std::fmt::Display| Error::Media(format!("resampler: {e}"));
        let input = InterleavedSlice::new(&self.pending, CH, CHUNK).map_err(|e| bad(&e))?;
        let frames_out = self.out.len() / CH;
        let mut output =
            InterleavedSlice::new_mut(&mut self.out, CH, frames_out).map_err(|e| bad(&e))?;
        let idx = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: (valid < CHUNK).then_some(valid),
            active_channels_mask: None,
        };
        let (_, n) = self
            .r
            .process_into_buffer(&input, &mut output, Some(&idx))
            .map_err(|e| bad(&e))?;
        let skip = self.delay_left.min(n);
        self.delay_left -= skip;
        out.extend_from_slice(&self.out[skip * CH..n * CH]);
        self.out_total += (n - skip) as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_of_zero_is_an_error() {
        assert!(Resampler::new(0, 48000).is_err());
        assert!(Resampler::new(44100, 0).is_err());
    }

    #[test]
    fn exact_length_and_alignment() {
        // A click at a known frame must land at the scaled frame, and lengths must be exact.
        let n = 44100 + 123;
        let mut input = vec![0.0f32; n * CH];
        input[1000 * CH] = 1.0;
        input[1000 * CH + 1] = 1.0;
        let mut r = Resampler::new(44100, 48000).unwrap();
        let mut out = Vec::new();
        for c in input.chunks(777 * CH) {
            r.process(c, &mut out).unwrap();
        }
        r.finish(&mut out).unwrap();
        assert_eq!(
            out.len() / CH,
            (n as f64 * 48000.0 / 44100.0).round() as usize
        );
        let peak = (0..out.len() / CH)
            .max_by(|a, b| out[a * CH].total_cmp(&out[b * CH]))
            .unwrap();
        let expect = (1000.0 * 48000.0 / 44100.0f64).round() as i64;
        assert!(
            (peak as i64 - expect).abs() <= 1,
            "peak at {peak}, expected {expect}"
        );
    }
}
