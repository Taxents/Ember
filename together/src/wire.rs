//! Framing on the per-guest stream and the clock datagrams.
//!
//! Stream frame: `[kind: u8][len: u32 LE][payload: len bytes]`.

use bytes::{BufMut, Bytes, BytesMut};
use iroh::endpoint::RecvStream;
use serde::{Deserialize, Serialize};

pub(crate) const ALPN: &[u8] = b"ember/together/1";
/// Bumped when the frame contents change incompatibly (the ALPN covers framing).
pub(crate) const PROTO: u32 = 1;

/// Internal control message (JSON).
pub(crate) const F_CTRL: u8 = 0;
/// Application message (JSON of `Up` / `Down`).
pub(crate) const F_MSG: u8 = 1;
/// Raw bytes host -> guest.
pub(crate) const F_RAW: u8 = 2;
/// Clock ping/pong carried on the stream when datagrams are unavailable.
pub(crate) const F_CLOCK: u8 = 3;

/// Upper bound on a frame payload; a bigger length means a broken peer.
pub(crate) const MAX_FRAME: usize = 8 * 1024 * 1024;

/// QUIC application close codes.
pub(crate) mod close {
    pub(crate) const NORMAL: u32 = 0;
    pub(crate) const HOST_ENDED: u32 = 1;
    pub(crate) const KICKED: u32 = 2;
    pub(crate) const FULL: u32 = 3;
    pub(crate) const TOO_SLOW: u32 = 4;
    pub(crate) const PROTOCOL: u32 = 5;
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum Control {
    Hello { proto: u32, name: String },
    Welcome { name: String },
}

pub(crate) fn frame(kind: u8, payload: &[u8]) -> Bytes {
    let mut b = BytesMut::with_capacity(5 + payload.len());
    b.put_u8(kind);
    b.put_u32_le(payload.len() as u32);
    b.put_slice(payload);
    b.freeze()
}

pub(crate) fn control_frame(c: &Control) -> Bytes {
    // Serializing these plain structs cannot fail.
    frame(F_CTRL, &serde_json::to_vec(c).unwrap_or_default())
}

/// Reads one frame. `Ok(None)` when the peer finished the stream cleanly.
pub(crate) async fn read_frame(recv: &mut RecvStream) -> Result<Option<(u8, Vec<u8>)>, String> {
    let mut header = [0u8; 5];
    match recv.read_exact(&mut header).await {
        Ok(()) => {}
        Err(iroh::endpoint::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    }
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME {
        return Err(format!("frame too large ({len} bytes)"));
    }
    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some((header[0], payload)))
}

/// Ping: `'P' t1 guest_rtt` (guest_rtt 0 = unknown).
pub(crate) fn ping(t1: u64, guest_rtt: u64) -> Bytes {
    let mut b = BytesMut::with_capacity(17);
    b.put_u8(b'P');
    b.put_u64_le(t1);
    b.put_u64_le(guest_rtt);
    b.freeze()
}

pub(crate) fn parse_ping(b: &[u8]) -> Option<(u64, u64)> {
    if b.len() != 17 || b[0] != b'P' {
        return None;
    }
    Some((le(&b[1..9]), le(&b[9..17])))
}

/// Pong: `'Q' t1 t2 t3`.
pub(crate) fn pong(t1: u64, t2: u64, t3: u64) -> Bytes {
    let mut b = BytesMut::with_capacity(25);
    b.put_u8(b'Q');
    b.put_u64_le(t1);
    b.put_u64_le(t2);
    b.put_u64_le(t3);
    b.freeze()
}

pub(crate) fn parse_pong(b: &[u8]) -> Option<(u64, u64, u64)> {
    if b.len() != 25 || b[0] != b'Q' {
        return None;
    }
    Some((le(&b[1..9]), le(&b[9..17]), le(&b[17..25])))
}

fn le(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(b);
    u64::from_le_bytes(a)
}

/// Keeps display names printable and short.
pub(crate) fn clean_name(name: &str) -> String {
    let s: String = name.chars().filter(|c| !c.is_control()).take(48).collect();
    let s = s.trim();
    if s.is_empty() {
        "Someone".to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_pong_roundtrip() {
        assert_eq!(parse_ping(&ping(1, 2)), Some((1, 2)));
        assert_eq!(parse_pong(&pong(3, 4, u64::MAX)), Some((3, 4, u64::MAX)));
        assert_eq!(parse_ping(&pong(3, 4, 5)), None);
        assert_eq!(parse_pong(b"Qshort"), None);
    }

    #[test]
    fn frame_layout() {
        let f = frame(F_RAW, b"abc");
        assert_eq!(&f[..], &[2, 3, 0, 0, 0, b'a', b'b', b'c']);
    }

    #[test]
    fn names() {
        assert_eq!(clean_name("  \u{7}  "), "Someone");
        assert_eq!(clean_name(" Ana\n"), "Ana");
        assert_eq!(clean_name(&"x".repeat(100)).len(), 48);
    }
}
