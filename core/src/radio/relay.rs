//! Listen Along for live stations: the host's exact compressed bytes, replayed on listeners.
//!
//! Two stations decode to the same samples only if their decoders read the same bytes from the
//! same first byte, so the host does not tell listeners "play station X" but forwards what its
//! own decoder reads:
//!
//! * **Host** — the app sets a [`set_tap`] while hosting. Every station [`open`](super::live::open)ed
//!   while it is set becomes an *epoch*: a [`TapEvent::Start`] before its first byte, then each
//!   byte its pipe accepts (ICY metadata stripped, reconnections included, in decoder order) as
//!   [`TapEvent::Data`], then one [`TapEvent::End`] when the station gives up, its source is
//!   dropped, or the tap is replaced or cleared (a running station keeps the tap it was opened
//!   with until then, and never picks up a new one: hosting mid-station means reopening it).
//!   Bytes a replaced connection pushes after its replacement are refused by the pipe and never
//!   tapped. Taps run on the network thread under the pipe's lock: they must only queue.
//! * **Listener** — the app calls [`expect_relay`] for the station, feeds the host's events into
//!   it ([`Relay::start`], [`Relay::push`], [`Relay::finish`]) and loads the station; the engine
//!   finds the relay with [`take_relay`] and plays it through [`open_relay`] instead of the
//!   network. A relay never reconnects: it waits for bytes, and ends after `finish`.

use super::live::{self, LiveSource, OPEN_WAIT, Pipe};
use crate::audio::Event;
use crate::{Error, Result};
use crossbeam_channel::Sender;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

// ---- host: the tap -----------------------------------------------------------------------------

/// What a hosting station tells the tap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapEvent<'a> {
    /// A station was opened: a new epoch. Comes before any of its bytes.
    Start {
        epoch: u64,
        /// Play id (`radio:<channel>`).
        id: &'a str,
        content_type: &'a str,
        kbps: Option<u32>,
    },
    /// The next audio bytes of the epoch, exactly as the host's decoder will read them.
    Data { epoch: u64, bytes: &'a [u8] },
    /// No more bytes for this epoch.
    End { epoch: u64 },
}

pub type Tap = Arc<dyn Fn(TapEvent<'_>) + Send + Sync>;

static TAP: Mutex<Option<Tap>> = Mutex::new(None);
/// Bumped by every [`set_tap`]: running stations notice their tap was replaced.
static TAP_VERSION: AtomicU64 = AtomicU64::new(0);
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Sets (or clears) the tap for stations opened from now on. Stations already tapped send
/// `End` through their old tap at their next bytes and stop tapping.
pub fn set_tap(tap: Option<Tap>) {
    let mut t = TAP.lock().unwrap();
    *t = tap;
    TAP_VERSION.fetch_add(1, Ordering::SeqCst);
}

/// One station's tap, held by its pipe.
pub(super) struct Tapping {
    epoch: u64,
    version: u64,
    f: Tap,
}

impl Tapping {
    /// Passes accepted bytes on. False (after sending `End`) once the tap was replaced.
    pub(super) fn data(&self, bytes: &[u8]) -> bool {
        if TAP_VERSION.load(Ordering::SeqCst) != self.version {
            self.end();
            return false;
        }
        if !bytes.is_empty() {
            (self.f)(TapEvent::Data {
                epoch: self.epoch,
                bytes,
            });
        }
        true
    }

    pub(super) fn end(&self) {
        (self.f)(TapEvent::End { epoch: self.epoch });
    }
}

/// A station is being opened: if hosting, starts its epoch (sending `Start`).
pub(super) fn capture(id: &str, content_type: &str, kbps: Option<u32>) -> Option<Tapping> {
    let (f, version) = {
        let t = TAP.lock().unwrap();
        (t.clone()?, TAP_VERSION.load(Ordering::SeqCst))
    };
    let epoch = EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    f(TapEvent::Start {
        epoch,
        id,
        content_type,
        kbps,
    });
    Some(Tapping { epoch, version, f })
}

// ---- listener: relays ---------------------------------------------------------------------------

/// A listener's copy of one host epoch. Cheap to clone (shared inside).
#[derive(Clone)]
pub struct Relay(Arc<Inner>);

struct Inner {
    /// Unbounded: [`Relay::push`] never waits.
    pipe: Arc<Pipe>,
    /// Content type and bitrate, once [`Relay::start`]ed.
    meta: Mutex<Meta>,
    cv: Condvar,
}

#[derive(Default)]
struct Meta {
    start: Option<(String, Option<u32>)>,
    finished: bool,
}

impl Relay {
    fn new() -> Self {
        Relay(Arc::new(Inner {
            pipe: Pipe::unbounded(),
            meta: Mutex::new(Meta::default()),
            cv: Condvar::new(),
        }))
    }

    /// The host's `Start`: what the bytes are. Later calls are ignored (one relay, one epoch).
    pub fn start(&self, content_type: &str, kbps: Option<u32>) {
        let mut m = self.0.meta.lock().unwrap();
        if m.start.is_none() {
            m.start = Some((content_type.to_string(), kbps));
            self.0.cv.notify_all();
        }
    }

    /// The host's next bytes, in order. Never waits: the buffer is unbounded, since dropping
    /// or refusing bytes would desync the decoder. The host sends at the station's real-time
    /// rate (plus its connect burst), so it only grows while this listener isn't playing
    /// (about 1 MB a minute at 128 kbit/s). After the source is dropped, bytes are discarded.
    pub fn push(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.0.pipe.push(0, bytes);
        }
    }

    /// The host's `End`: the source plays what it has, then ends.
    pub fn finish(&self) {
        self.0.pipe.finish(0);
        self.0.meta.lock().unwrap().finished = true;
        self.0.cv.notify_all();
    }

    /// Bytes waiting to be decoded.
    pub fn buffered(&self) -> usize {
        self.0.pipe.buffered()
    }
}

fn relays() -> &'static Mutex<HashMap<String, Relay>> {
    static R: OnceLock<Mutex<HashMap<String, Relay>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Registers a new, empty relay for station `id` (replacing any not yet taken) and returns it
/// for the app to feed.
pub fn expect_relay(id: &str) -> Relay {
    let r = Relay::new();
    relays().lock().unwrap().insert(id.to_string(), r.clone());
    r
}

/// The relay registered for `id`, if any, removed from the registry (the engine's side).
pub fn take_relay(id: &str) -> Option<Relay> {
    relays().lock().unwrap().remove(id)
}

/// Drops every relay not yet taken (ending them).
pub fn clear_relays() {
    for (_, r) in relays().lock().unwrap().drain() {
        r.finish();
    }
}

/// Plays a relay: waits (like [`live::open`]) for `start` and the first bytes, probes and
/// buffers as for a station. The source never touches the network: short of bytes it stays
/// stalled; after `finish` it ends once drained. No `Title` events (titles come from the host).
pub fn open_relay(id: &str, relay: Relay, events: Option<Sender<Event>>) -> Result<LiveSource> {
    drop(events);
    let until = Instant::now() + OPEN_WAIT;
    let (ct, kbps) = {
        let mut m = relay.0.meta.lock().unwrap();
        loop {
            if let Some(s) = &m.start {
                break s.clone();
            }
            let now = Instant::now();
            if m.finished || now >= until {
                return Err(Error::Net("the host sent no audio".into()));
            }
            m = relay.0.cv.wait_timeout(m, until - now).unwrap().0;
        }
    };
    live::from_relay(relay.0.pipe.clone(), id, &ct, kbps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry() {
        let a = expect_relay("radio:relayA");
        let b = expect_relay("radio:relayA");
        assert!(!Arc::ptr_eq(&a.0, &b.0), "replaced");
        assert!(Arc::ptr_eq(&take_relay("radio:relayA").unwrap().0, &b.0));
        assert!(take_relay("radio:relayA").is_none());
        let c = expect_relay("radio:relayB");
        clear_relays();
        assert!(take_relay("radio:relayB").is_none());
        assert!(c.0.meta.lock().unwrap().finished);
    }

    #[test]
    fn open_fails_fast_without_start() {
        let r = expect_relay("radio:relayC");
        r.finish();
        let t = Instant::now();
        assert!(open_relay("radio:relayC", take_relay("radio:relayC").unwrap(), None).is_err());
        assert!(t.elapsed() < OPEN_WAIT);
    }
}
