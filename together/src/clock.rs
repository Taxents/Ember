//! NTP-style estimation of the host clock from the guest's point of view.
//!
//! Each round trip gives four timestamps: `t1` (guest send), `t2` (host
//! receive), `t3` (host send), `t4` (guest receive). With `θ = host - guest`:
//!
//! * apparent forward delay  `fwd  = t2 - t1 =  θ + d_fwd`
//! * apparent backward delay `back = t4 - t3 = -θ + d_back`
//!
//! The textbook per-sample estimate is `((t2 - t1) + (t3 - t4)) / 2` with round
//! trip `(t4 - t1) - (t3 - t2)`. Queueing, scheduling and timestamping noise
//! only ever *add* delay, so the cleanest information is in the smallest
//! delays. Rather than trusting the samples with the smallest round trip
//! (classic min-RTT filtering), we take the minimum forward and the minimum
//! backward delay separately over a recent window:
//! `θ ≈ (min fwd - min back) / 2`. That is exact whenever at least one packet
//! in the window went each way without queueing, even if no single round trip
//! was clean, which makes it much less sensitive to asymmetric jitter (typical
//! on relayed or Wi-Fi paths). Only asymmetry of the base path delay remains,
//! which no two-way method can observe.
//!
//! The two machines' clocks drift apart by a few (up to tens of) ppm. Once
//! samples span a minute, the minima become lines: the lower envelopes of the
//! forward and backward delays over the last 4 minutes (the LP fit used for
//! clock skew estimation, i.e. the lower convex hull edge spanning the mean
//! time). Their slopes give the drift rate and their gap the offset. Before
//! that (first minute) the plain minima over the last 30 s are used.
//!
//! Simulated accuracy (see tests; 20 ms base delay, 10% of packets +80 ms,
//! 5% loss): with 0.5/2 ms mean forward/back jitter every estimate after the
//! first 3 s is within 0.5 ms (median ~15 us); with 2/8 ms jitter median ~60
//! us, p95 ~350 us; with 20 ppm drift, within 0.5 ms once it settles (~2 min),
//! within 1 ms before.

use std::collections::VecDeque;

/// Samples needed before an estimate is offered.
pub(crate) const MIN_SAMPLES: usize = 8;
/// Samples kept for the drift fit (~8 minutes at one ping per second).
const HISTORY: usize = 320;
/// Offset window: the minima are taken over samples at most this old...
const WINDOW_US: f64 = 30_000_000.0;
/// ...but always over at least this many of the newest samples.
const WINDOW_MIN_SAMPLES: usize = 8;
/// The drift-aware fit uses samples at most this old...
const FIT_WINDOW_US: f64 = 240_000_000.0;
/// ...once there are this many spanning at least this long.
const FIT_MIN_SAMPLES: usize = 16;
const FIT_MIN_SPAN_US: f64 = 60_000_000.0;
/// Real clocks drift by at most tens of ppm; anything bigger is noise.
const MAX_SLOPE: f64 = 200e-6;
/// Round trips longer than this are useless and dropped.
const MAX_RTT_US: u64 = 5_000_000;

#[derive(Clone, Copy, Debug)]
struct Sample {
    /// Local (guest) time the sample was taken (midpoint of t1 and t4).
    at_us: f64,
    fwd_us: f64,
    back_us: f64,
    rtt_us: u64,
}

/// A fitted relation between the local and the host clock:
/// `host = local + offset + slope * (local - ref)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClockModel {
    pub ref_local_us: f64,
    pub offset_us: f64,
    pub slope: f64,
}

impl ClockModel {
    pub(crate) fn local_to_host(&self, local_us: u64) -> u64 {
        let l = local_us as f64;
        let h = l + self.offset_us + self.slope * (l - self.ref_local_us);
        h.max(0.0).round() as u64
    }

    pub(crate) fn host_to_local(&self, host_us: u64) -> u64 {
        // Invert host = l + o + s (l - r)  =>  l = (host - o + s r) / (1 + s)
        let h = host_us as f64;
        let l = (h - self.offset_us + self.slope * self.ref_local_us) / (1.0 + self.slope);
        l.max(0.0).round() as u64
    }
}

#[derive(Default, Debug)]
pub(crate) struct ClockEstimator {
    samples: VecDeque<Sample>,
}

/// `(min fwd - min back) / 2` over samples detrended to `ref_us`, and the
/// local time that estimate belongs to.
fn split_min<'a>(samples: impl IntoIterator<Item = &'a Sample>, slope: f64, ref_us: f64) -> Option<(f64, f64)> {
    let mut f: Option<(f64, f64)> = None;
    let mut b: Option<(f64, f64)> = None;
    for s in samples {
        let drift = slope * (s.at_us - ref_us);
        let fwd = s.fwd_us - drift;
        let back = s.back_us + drift;
        if f.is_none_or(|(v, _)| fwd < v) {
            f = Some((fwd, s.at_us));
        }
        if b.is_none_or(|(v, _)| back < v) {
            b = Some((back, s.at_us));
        }
    }
    let ((fwd, fa), (back, ba)) = (f?, b?);
    Some(((fwd - back) / 2.0, (fa + ba) / 2.0))
}

impl ClockEstimator {
    /// Adds one round trip. Returns false (and ignores it) when the
    /// timestamps are inconsistent.
    pub(crate) fn add(&mut self, t1: u64, t2: u64, t3: u64, t4: u64) -> bool {
        if t4 < t1 || t3 < t2 {
            return false;
        }
        let rtt = (t4 - t1).saturating_sub(t3 - t2);
        if rtt > MAX_RTT_US {
            return false;
        }
        let (t1, t2, t3, t4) = (t1 as i128, t2 as i128, t3 as i128, t4 as i128);
        if self.samples.len() == HISTORY {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample {
            at_us: (t1 + t4) as f64 / 2.0,
            fwd_us: (t2 - t1) as f64,
            back_us: (t4 - t3) as f64,
            rtt_us: rtt,
        });
        true
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.samples.len()
    }

    /// Current round trip: median of the last 8 samples (follows changes
    /// quickly but ignores single spikes). None before the first sample.
    pub(crate) fn rtt_us(&self) -> Option<u64> {
        let mut recent: Vec<u64> = self.samples.iter().rev().take(8).map(|s| s.rtt_us).collect();
        if recent.is_empty() {
            return None;
        }
        recent.sort_unstable();
        Some(recent[recent.len() / 2])
    }

    /// The current clock model, or None until [`MIN_SAMPLES`] are in.
    pub(crate) fn model(&self) -> Option<ClockModel> {
        let n = self.samples.len();
        if n < MIN_SAMPLES {
            return None;
        }
        let newest = self.samples.back()?.at_us;
        let fit: Vec<&Sample> = self
            .samples
            .iter()
            .filter(|s| newest - s.at_us <= FIT_WINDOW_US)
            .collect();
        let span = fit.last().map_or(0.0, |l| l.at_us) - fit.first().map_or(0.0, |f| f.at_us);
        if fit.len() >= FIT_MIN_SAMPLES && span >= FIT_MIN_SPAN_US {
            let fwd: Vec<(f64, f64)> = fit.iter().map(|s| (s.at_us - newest, s.fwd_us)).collect();
            let back: Vec<(f64, f64)> = fit.iter().map(|s| (s.at_us - newest, s.back_us)).collect();
            if let (Some((af, bf)), Some((ab, bb))) = (lower_envelope(&fwd), lower_envelope(&back)) {
                let slope = (bf - bb) / 2.0;
                if slope.is_finite() && slope.abs() <= MAX_SLOPE {
                    return Some(ClockModel {
                        ref_local_us: newest,
                        offset_us: (af - ab) / 2.0,
                        slope,
                    });
                }
            }
        }
        let window = self
            .samples
            .iter()
            .enumerate()
            .filter(|(i, s)| newest - s.at_us <= WINDOW_US || n - i <= WINDOW_MIN_SAMPLES)
            .map(|(_, s)| s);
        let (offset, _) = split_min(window, 0.0, newest)?;
        Some(ClockModel {
            ref_local_us: newest,
            offset_us: offset,
            slope: 0.0,
        })
    }
}

/// The line under all points (sorted by x) that hugs them most closely
/// (minimum summed gap, the linear-programming fit used for clock skew
/// estimation). It runs along the lower convex hull edge spanning the mean x.
/// Returns (value at x = 0, slope).
fn lower_envelope(pts: &[(f64, f64)]) -> Option<(f64, f64)> {
    let mut hull: Vec<(f64, f64)> = Vec::with_capacity(pts.len());
    for &p in pts {
        while hull.len() >= 2 {
            let (o, a) = (hull[hull.len() - 2], hull[hull.len() - 1]);
            let cross = (a.0 - o.0) * (p.1 - o.1) - (a.1 - o.1) * (p.0 - o.0);
            if cross <= 0.0 {
                hull.pop();
            } else {
                break;
            }
        }
        hull.push(p);
    }
    if hull.len() < 2 {
        return None;
    }
    let xm = pts.iter().map(|p| p.0).sum::<f64>() / pts.len() as f64;
    let i = hull.windows(2).position(|w| w[1].0 >= xm).unwrap_or(hull.len() - 2);
    let (p, q) = (hull[i], hull[i + 1]);
    let dx = q.0 - p.0;
    if dx <= 0.0 {
        return None;
    }
    let slope = (q.1 - p.1) / dx;
    Some((p.1 - slope * p.0, slope))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small deterministic PRNG (xorshift64*).
    struct Rng(u64);
    impl Rng {
        fn next_f64(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        }
        /// Exponentially distributed with the given mean.
        fn exp(&mut self, mean: f64) -> f64 {
            -mean * (1.0 - self.next_f64()).ln()
        }
    }

    pub(super) struct Path {
        pub offset0_us: f64,
        pub drift: f64,
        pub base_us: f64,
        pub jit_fwd_us: f64,
        pub jit_back_us: f64,
    }

    /// Simulates the real ping schedule (250 ms for 3 s, then 1 s) over a
    /// jittery, asymmetric path with occasional big spikes and 5% loss.
    /// Returns (seconds elapsed, samples so far, estimation error in us) at
    /// every sample once a model exists, and the final estimator.
    pub(super) fn simulate(seed: u64, p: &Path, total_s: f64) -> (Vec<(f64, usize, f64, f64)>, ClockEstimator) {
        let mut rng = Rng(seed | 1);
        let host_of = |local: f64| local + p.offset0_us + p.drift * local;
        let mut est = ClockEstimator::default();
        let mut out = Vec::new();
        let start = 1_000_000_000_000.0; // guest local time in us (~11.6 days)
        let mut t = start;
        while t - start < total_s * 1e6 {
            let t1 = t;
            let elapsed = t - start;
            t += if elapsed < 3e6 { 250_000.0 } else { 1_000_000.0 };
            if rng.next_f64() < 0.05 {
                continue; // lost
            }
            let mut fwd = p.base_us + rng.exp(p.jit_fwd_us);
            let mut back = p.base_us + rng.exp(p.jit_back_us);
            if rng.next_f64() < 0.1 {
                fwd += rng.exp(80_000.0);
            }
            if rng.next_f64() < 0.1 {
                back += rng.exp(80_000.0);
            }
            let proc = 20.0 + rng.exp(100.0);
            let t2 = host_of(t1 + fwd);
            let t3 = t2 + proc;
            let t4 = t1 + fwd + proc + back;
            est.add(t1 as u64, t2 as u64, t3 as u64, t4 as u64);
            if let Some(m) = est.model() {
                // Check a conversion a little in the future, as playback scheduling does.
                let probe = t4 + 500_000.0;
                let truth = host_of(probe);
                let got = m.local_to_host(probe as u64) as f64;
                out.push((elapsed / 1e6, est.len(), got - truth, m.slope));
            }
        }
        (out, est)
    }

    #[test]
    fn needs_min_samples() {
        let mut e = ClockEstimator::default();
        for i in 0..(MIN_SAMPLES as u64 - 1) {
            assert!(e.add(1000 + i, 5000 + i, 5001 + i, 1100 + i));
            assert!(e.model().is_none());
        }
        assert!(e.rtt_us().is_some());
        e.add(2000, 6000, 6001, 2100);
        assert!(e.model().is_some());
    }

    #[test]
    fn rejects_bad_samples() {
        let mut e = ClockEstimator::default();
        assert!(!e.add(100, 0, 0, 50)); // t4 < t1
        assert!(!e.add(100, 10, 5, 200)); // t3 < t2
        assert!(!e.add(0, 0, 0, 10_000_000)); // absurd rtt
        assert_eq!(e.len(), 0);
    }

    #[test]
    fn exact_without_jitter() {
        let mut e = ClockEstimator::default();
        let off = 5_000_000i64;
        for i in 0..10u64 {
            let t1 = 1_000_000 + i * 250_000;
            let t2 = (t1 as i64 + 7_000 + off) as u64;
            e.add(t1, t2, t2 + 30, t1 + 14_030);
        }
        let m = e.model().unwrap();
        assert!((m.offset_us - off as f64).abs() < 1.0, "{m:?}");
    }

    fn errors_after(p: &Path, seeds: u64, total_s: f64, after_s: f64) -> Vec<f64> {
        let mut errs = Vec::new();
        for seed in 1..=seeds {
            let (pts, _) = simulate(seed * 7919, p, total_s);
            errs.extend(pts.iter().filter(|x| x.0 >= after_s).map(|x| x.2.abs()));
        }
        errs.sort_by(|a, b| a.total_cmp(b));
        errs
    }

    fn pct(errs: &[f64], q: f64) -> f64 {
        errs[((errs.len() - 1) as f64 * q) as usize]
    }

    #[test]
    fn asymmetric_jitter_within_half_ms() {
        // 20 ms base each way, 4x asymmetric queueing jitter (mean 0.5 ms
        // forward, 2 ms back), 10% of packets delayed by ~80 ms more, 5% lost,
        // 3 ppm drift. Every estimate after the 3 s burst must be within 0.5 ms.
        let path = Path {
            offset0_us: 123_456_789.0,
            drift: 3e-6,
            base_us: 20_000.0,
            jit_fwd_us: 500.0,
            jit_back_us: 2_000.0,
        };
        let errs = errors_after(&path, 40, 300.0, 3.0);
        let max = pct(&errs, 1.0);
        assert!(max <= 500.0, "max error {max:.0} us");
        assert!(pct(&errs, 0.5) <= 50.0, "median error {:.0} us", pct(&errs, 0.5));
    }

    #[test]
    fn heavy_asymmetric_jitter() {
        // Much worse path: mean jitter 2 ms forward, 8 ms back.
        let path = Path {
            offset0_us: -55_555_555.0,
            drift: 5e-6,
            base_us: 30_000.0,
            jit_fwd_us: 2_000.0,
            jit_back_us: 8_000.0,
        };
        let errs = errors_after(&path, 40, 300.0, 3.0);
        assert!(pct(&errs, 0.5) <= 150.0, "median {:.0} us", pct(&errs, 0.5));
        assert!(pct(&errs, 0.95) <= 500.0, "p95 {:.0} us", pct(&errs, 0.95));
        assert!(pct(&errs, 1.0) <= 1_500.0, "max {:.0} us", pct(&errs, 1.0));
    }

    #[test]
    fn negative_offset_and_drift() {
        // Host clock 16 minutes behind the guest and running 20 ppm fast.
        let path = Path {
            offset0_us: -987_654_321.0,
            drift: 20e-6,
            base_us: 15_000.0,
            jit_fwd_us: 1_000.0,
            jit_back_us: 4_000.0,
        };
        for seed in 1..20u64 {
            let (pts, est) = simulate(seed, &path, 600.0);
            for (secs, n, err, slope) in pts {
                // Drift is fitted after a minute and settles within two.
                if secs >= 120.0 {
                    assert!(err.abs() <= 500.0, "seed {seed} at {secs:.1}s (n={n}): error {err:.0} us slope {slope:e}");
                } else if secs >= 3.0 {
                    assert!(err.abs() <= 1_000.0, "seed {seed} at {secs:.1}s (n={n}): error {err:.0} us");
                }
            }
            let slope = est.model().unwrap().slope;
            assert!((slope - 20e-6).abs() < 3e-6, "seed {seed}: slope {slope}");
        }
    }

    #[test]
    fn model_inverse_roundtrip() {
        let m = ClockModel { ref_local_us: 5e9, offset_us: -1234.5, slope: 5e-6 };
        for l in [5_000_000_000u64, 5_060_000_000, 4_900_000_000] {
            let h = m.local_to_host(l);
            let back = m.host_to_local(h);
            assert!((back as i64 - l as i64).abs() <= 1);
        }
    }
}
