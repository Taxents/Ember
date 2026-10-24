//! Transport and clock for Ember's "Listen Along".
//!
//! A [`Host`] binds an [iroh](https://docs.rs/iroh) endpoint (QUIC, dialed by
//! public key, hole punching via number0's public relays, n0 DNS/pkarr address
//! lookup) and hands out an invite code. A [`Guest`] dials that code. Each guest
//! has one ordered, reliable bidirectional stream carrying length-prefixed
//! frames: JSON-encoded app messages (`Up` guest -> host, `Down` host -> guest)
//! and raw byte frames (host -> guest), so ordering between messages and raw
//! audio is kept. Clock pings travel as QUIC datagrams so they never queue
//! behind data (falling back to the stream if datagrams are unavailable).
//!
//! The guest estimates `host clock - guest clock` with min-RTT-filtered NTP
//! samples plus a drift slope, see [`Guest::host_to_local_us`]. Both sides
//! timestamp with [`now_us`].
//!
//! The host measures each guest's round trip itself (its own pings, echoed by
//! the guest), so a guest can't make everyone's starts wait on a made-up delay.
//!
//! Everything public is synchronous and thread-safe. Each session runs its own
//! small tokio runtime (2 worker threads) which lives exactly as long as the
//! [`Host`] / [`Guest`]. Events are delivered on bounded crossbeam channels
//! ([`EVENTS`]): when nobody drains them, reading from the network waits, so a
//! peer that floods messages is held back by QUIC flow control instead of
//! filling memory.

mod clock;
mod guest;
mod host;
mod net;
mod wire;

use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub use guest::{Guest, GuestEvent};
pub use host::{Host, HostEvent, MAX_GUESTS};

/// Events queued per session before reading from the network waits for them to be taken.
pub const EVENTS: usize = 1024;

/// Identifies a guest within one hosted session (1, 2, 3, ...; never reused
/// within the session). Not the iroh key.
pub type PeerId = u64;

static EPOCH: OnceLock<Instant> = OnceLock::new();

fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

/// Microseconds since a process-wide epoch (the first call into this crate).
/// Monotonic; on Windows backed by QueryPerformanceCounter.
pub fn now_us() -> u64 {
    epoch().elapsed().as_micros() as u64
}

/// The [`Instant`] for a [`now_us`] value (inverse of [`now_us`]).
pub fn to_instant(local_us: u64) -> Instant {
    epoch() + Duration::from_micros(local_us)
}

/// Queues an event, waiting while the channel is full (see [`EVENTS`]) unless the session is
/// stopping or nobody listens any more. Inside the session's runtime the wait doesn't hold up
/// its other tasks.
pub(crate) fn deliver<E>(
    tx: &crossbeam_channel::Sender<E>,
    e: E,
    stop: &tokio::sync::watch::Receiver<bool>,
) {
    let e = match tx.try_send(e) {
        Ok(()) | Err(crossbeam_channel::TrySendError::Disconnected(_)) => return,
        Err(crossbeam_channel::TrySendError::Full(e)) => e,
    };
    let send = move || {
        let mut e = e;
        loop {
            match tx.send_timeout(e, Duration::from_millis(50)) {
                Ok(()) | Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => return,
                Err(crossbeam_channel::SendTimeoutError::Timeout(back)) => {
                    if *stop.borrow() {
                        return;
                    }
                    e = back;
                }
            }
        }
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(send);
    } else {
        send();
    }
}

/// Errors returned synchronously by [`Host::start`] and [`Guest::join`].
/// Everything after that is reported through events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The text doesn't contain a valid invite code.
    InvalidCode,
    /// The network endpoint or runtime couldn't be started.
    Network(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::InvalidCode => f.write_str("That doesn't look like a Listen Along invite code"),
            Error::Network(e) => write!(f, "Couldn't start networking: {e}"),
        }
    }
}

impl std::error::Error for Error {}

pub(crate) const CODE_PREFIX: &str = "ember-";
/// z-base-32 of a 32-byte key.
const KEY_CHARS: usize = 52;
const Z32: &str = "ybndrfg8ejkmcpqxot1uwisza345h769";

pub(crate) fn code_to_id(code: &str) -> Option<iroh::EndpointId> {
    let key = code.strip_prefix(CODE_PREFIX)?;
    iroh::PublicKey::from_z32(key).ok()
}

/// Finds an invite code in arbitrary text (a whole clipboard, a chat message
/// with words around it, different case, backticks or other markup) and returns
/// it normalised (`ember-` + 52 lowercase z-base-32 characters). A bare
/// 52-character key without the prefix is accepted too.
pub fn parse_code(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let valid = |key: &str| {
        key.len() == KEY_CHARS
            && key.chars().all(|c| Z32.contains(c))
            && iroh::PublicKey::from_z32(key).is_ok()
    };
    let token_end = |s: &str| {
        s.find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(s.len())
    };
    let mut from = 0;
    while let Some(pos) = lower[from..].find(CODE_PREFIX) {
        let start = from + pos + CODE_PREFIX.len();
        let rest = &lower[start..];
        let key = &rest[..token_end(rest)];
        if valid(key) {
            return Some(format!("{CODE_PREFIX}{key}"));
        }
        from = start;
    }
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|t| valid(t))
        .map(|key| format!("{CODE_PREFIX}{key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_code() -> String {
        let key = iroh::SecretKey::from_bytes(&[7u8; 32]).public();
        format!("ember-{}", key.to_z32())
    }

    #[test]
    fn code_shape() {
        let c = sample_code();
        assert_eq!(c.len(), 6 + 52);
        assert!(
            c.chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        );
        assert!(code_to_id(&c).is_some());
    }

    #[test]
    fn parse_in_noise() {
        let c = sample_code();
        assert_eq!(parse_code(&c).as_deref(), Some(c.as_str()));
        assert_eq!(parse_code(&format!("  {c}\n")).as_deref(), Some(c.as_str()));
        assert_eq!(
            parse_code(&format!("join me: `{c}` thx!")).as_deref(),
            Some(c.as_str())
        );
        assert_eq!(parse_code(&c.to_uppercase()).as_deref(), Some(c.as_str()));
        assert_eq!(
            parse_code(&format!("ember-nope then {c}")).as_deref(),
            Some(c.as_str())
        );
        assert_eq!(parse_code(&format!("**{c}**")).as_deref(), Some(c.as_str()));
        let bare = &c[6..];
        assert_eq!(
            parse_code(&format!("code {bare}")).as_deref(),
            Some(c.as_str())
        );
    }

    #[test]
    fn parse_rejects() {
        let c = sample_code();
        assert_eq!(parse_code(""), None);
        assert_eq!(parse_code("ember-"), None);
        assert_eq!(parse_code(&c[..c.len() - 1]), None);
        assert_eq!(parse_code(&format!("{c}x")), None);
        // 'l' and 'v' aren't in the z-base-32 alphabet.
        let bad = format!("ember-{}", "l".repeat(52));
        assert_eq!(parse_code(&bad), None);
        assert!(matches!(
            Guest::<(), ()>::join("hello", "me"),
            Err(Error::InvalidCode)
        ));
    }

    #[test]
    fn time_roundtrip() {
        let a = now_us();
        let i = to_instant(a);
        let b = now_us();
        assert!(b >= a);
        let back = i.duration_since(epoch()).as_micros() as u64;
        assert_eq!(back, a);
    }
}
