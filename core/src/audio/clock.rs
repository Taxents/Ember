//! One time scale for the decoder and the render callback: nanoseconds since a process-wide
//! epoch, on the same clock as [`Instant`] (QPC on Windows), so "play this at that instant" can
//! be stored in an atomic and compared in the callback.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn epoch() -> Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    *E.get_or_init(Instant::now)
}

/// Now, in ns since the epoch.
pub fn now_ns() -> u64 {
    ns_of(Instant::now())
}

/// `t` in ns since the epoch (0 for anything before it).
pub fn ns_of(t: Instant) -> u64 {
    t.saturating_duration_since(epoch()).as_nanos() as u64
}

/// The instant `ns` after the epoch.
pub fn instant_of(ns: u64) -> Instant {
    epoch() + Duration::from_nanos(ns)
}
