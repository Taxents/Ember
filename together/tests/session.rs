//! End-to-end tests: a host and guests in one process.
//!
//! The `local_*` tests run over loopback without relays (no internet needed).
//! `public_relay_session` uses number0's relays and DNS discovery like the app
//! does; run it with `cargo test -p together -- --ignored`.
//!
//! The `raw_*` tests speak the wire format by hand, as a broken or hostile guest would.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};
use together::{Guest, GuestEvent, Host, HostEvent, PeerId};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
enum Up {
    Hi(String),
    Count(u32),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
enum Down {
    Welcome { song: String },
    Seq(u32),
}

fn next<T: std::fmt::Debug>(rx: &Receiver<T>, secs: u64, what: &str) -> T {
    rx.recv_timeout(Duration::from_secs(secs))
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Waits for an event matching `f`, skipping others.
fn wait_for<T: std::fmt::Debug, R>(
    rx: &Receiver<T>,
    secs: u64,
    what: &str,
    mut f: impl FnMut(T) -> Option<R>,
) -> R {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
        if let Some(r) = f(ev) {
            return r;
        }
    }
}

fn joined(host: &Host<Up, Down>) -> (PeerId, String) {
    wait_for(host.events(), 10, "Joined", |e| match e {
        HostEvent::Joined { peer, name } => Some((peer, name)),
        _ => None,
    })
}

fn exercise(host: Host<Up, Down>, guest: Guest<Up, Down>) {
    let t0 = Instant::now();
    let host_name = wait_for(guest.events(), 20, "Connected", |e| match e {
        GuestEvent::Connected { host_name } => Some(host_name),
        GuestEvent::Closed(r) => panic!("closed: {r}"),
        _ => None,
    });
    println!("connected after {:?}", t0.elapsed());
    assert_eq!(host_name, "Hostess");
    let (peer, name) = joined(&host);
    assert_eq!(name, "Guesty");
    assert_eq!(host.peers(), vec![peer]);

    // Guest -> host.
    guest.send(&Up::Hi("hello".into()));
    for i in 0..50 {
        guest.send(&Up::Count(i));
    }
    let m = wait_for(host.events(), 10, "Hi", |e| match e {
        HostEvent::Msg { peer: p, msg } => Some((p, msg)),
        _ => None,
    });
    assert_eq!(m, (peer, Up::Hi("hello".into())));
    for i in 0..50 {
        match next(host.events(), 10, "Count") {
            HostEvent::Msg {
                msg: Up::Count(n), ..
            } => assert_eq!(n, i),
            e => panic!("unexpected {e:?}"),
        }
    }

    // Host -> guest: messages and raw frames interleaved, order kept.
    host.send(peer, &Down::Welcome { song: "abc".into() });
    for i in 0..200u32 {
        if i % 2 == 0 {
            host.broadcast(&Down::Seq(i));
        } else {
            let raw: Vec<u8> = (0..4000u32).map(|k| (k + i) as u8).collect();
            if i % 4 == 1 {
                host.send_raw(peer, &raw);
            } else {
                host.broadcast_raw(&raw);
            }
        }
    }
    let mut expect = 0u32;
    let mut got_welcome = false;
    while expect < 200 {
        match next(guest.events(), 10, "Down") {
            GuestEvent::Msg(Down::Welcome { song }) => {
                assert_eq!(song, "abc");
                assert_eq!(expect, 0);
                got_welcome = true;
            }
            GuestEvent::Msg(Down::Seq(n)) => {
                assert_eq!(n, expect);
                expect += 1;
            }
            GuestEvent::Raw(b) => {
                assert_eq!(b.len(), 4000);
                assert_eq!(b[0], expect as u8);
                assert_eq!(b[3999], (3999 + expect) as u8);
                expect += 1;
            }
            GuestEvent::ClockReady => {}
            e => panic!("unexpected {e:?}"),
        }
    }
    assert!(got_welcome);

    // Clock: same process, so the offset must be ~0.
    let deadline = Instant::now() + Duration::from_secs(10);
    while guest.host_to_local_us(0).is_none() {
        assert!(Instant::now() < deadline, "clock never got ready");
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("clock ready after {:?}", t0.elapsed());
    for _ in 0..5 {
        let now = together::now_us();
        let h = guest.local_to_host_us(now).unwrap();
        let l = guest.host_to_local_us(now).unwrap();
        let err = h as i64 - now as i64;
        println!("offset error {err} us, rtt {:?} us", guest.rtt_us());
        assert!(err.abs() < 1_000, "offset error {err} us");
        assert!((l as i64 - now as i64).abs() < 1_000);
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(guest.rtt_us().is_some());
    assert!(host.rtt_us(peer).is_some());
    let t = together::to_instant(together::now_us() + 1000);
    assert!(t > Instant::now());

    // Host leaves: guest is told.
    host.close();
    let reason = wait_for(guest.events(), 10, "Closed", |e| match e {
        GuestEvent::Closed(r) => Some(r),
        _ => None,
    });
    assert_eq!(reason, "The host ended the session");
}

#[test]
fn local_session() {
    let host = Host::<Up, Down>::start_local("Hostess").unwrap();
    assert!(host.code().starts_with("ember-"));
    assert_eq!(
        together::parse_code(&format!("join: {} !", host.code())),
        Some(host.code())
    );
    let guest = Guest::<Up, Down>::join_local(&host.local_code().unwrap(), "Guesty").unwrap();
    exercise(host, guest);
}

#[test]
fn local_kick_leave_and_bad_data() {
    let host = Host::<Up, Down>::start_local("H").unwrap();
    let code = host.local_code().unwrap();

    // Kick.
    let g1 = Guest::<Up, Down>::join_local(&code, "one").unwrap();
    let (p1, _) = joined(&host);
    wait_for(g1.events(), 10, "Connected", |e| {
        matches!(e, GuestEvent::Connected { .. }).then_some(())
    });
    host.kick(p1);
    let r = wait_for(g1.events(), 10, "Closed", |e| match e {
        GuestEvent::Closed(r) => Some(r),
        _ => None,
    });
    assert_eq!(r, "The host removed you from the session");
    wait_for(host.events(), 10, "Left", |e| {
        matches!(e, HostEvent::Left { peer } if peer == p1).then_some(())
    });
    host.send(p1, &Down::Seq(1)); // departed peer: ignored

    // Guest leaves on drop.
    let g2 = Guest::<Up, Down>::join_local(&code, "two").unwrap();
    let (p2, _) = joined(&host);
    drop(g2);
    wait_for(host.events(), 10, "Left", |e| {
        matches!(e, HostEvent::Left { peer } if peer == p2).then_some(())
    });

    // A guest speaking a different message type gets dropped, no panic.
    let g3 = Guest::<String, Down>::join_local(&code, "three").unwrap();
    let (p3, _) = joined(&host);
    g3.send(&"not an Up".to_string());
    wait_for(host.events(), 10, "Left", |e| {
        matches!(e, HostEvent::Left { peer } if peer == p3).then_some(())
    });
    wait_for(g3.events(), 10, "Closed", |e| {
        matches!(e, GuestEvent::Closed(_)).then_some(())
    });
    assert!(host.peers().is_empty());
}

#[test]
fn local_session_full() {
    let host = Host::<Up, Down>::start_local("H").unwrap();
    let code = host.local_code().unwrap();
    let guests: Vec<_> = (0..together::MAX_GUESTS)
        .map(|i| Guest::<Up, Down>::join_local(&code, &format!("g{i}")).unwrap())
        .collect();
    for _ in 0..together::MAX_GUESTS {
        joined(&host);
    }
    let extra = Guest::<Up, Down>::join_local(&code, "late").unwrap();
    let r = wait_for(extra.events(), 15, "Closed", |e| match e {
        GuestEvent::Closed(r) => Some(r),
        GuestEvent::Connected { .. } => panic!("should be refused"),
        _ => None,
    });
    assert_eq!(r, "The session is full");
    assert_eq!(host.peers().len(), together::MAX_GUESTS);
    drop(guests);
}

/// A dropped link (the network going away for 20 s): the guest rejoins by itself and the
/// host sees it come back as a new peer.
#[test]
fn local_guest_rejoins_after_link_drop() {
    let host = Host::<Up, Down>::start_local("H").unwrap();
    let guest = Guest::<Up, Down>::join_local(&host.local_code().unwrap(), "Back").unwrap();
    wait_for(guest.events(), 10, "Connected", |e| {
        matches!(e, GuestEvent::Connected { .. }).then_some(())
    });
    let (p1, _) = joined(&host);
    // Let the clock settle first: it carries over the rejoin.
    wait_for(guest.events(), 10, "ClockReady", |e| {
        matches!(e, GuestEvent::ClockReady).then_some(())
    });

    let t0 = Instant::now();
    host.drop_link(p1, Duration::from_secs(20));
    wait_for(guest.events(), 10, "Reconnecting", |e| match e {
        GuestEvent::Reconnecting => Some(()),
        GuestEvent::Closed(r) => panic!("closed: {r}"),
        _ => None,
    });
    wait_for(host.events(), 10, "Left", |e| {
        matches!(e, HostEvent::Left { peer } if peer == p1).then_some(())
    });
    // Sent while the link is down: dropped, not delivered late.
    guest.send(&Up::Hi("stale".into()));
    let host_name = wait_for(guest.events(), 45, "Connected again", |e| match e {
        GuestEvent::Connected { host_name } => Some(host_name),
        GuestEvent::Closed(r) => panic!("closed: {r}"),
        _ => None,
    });
    println!("back after {:?}", t0.elapsed());
    assert!(t0.elapsed() >= Duration::from_secs(19));
    assert_eq!(host_name, "H");
    let (p2, name) = joined(&host);
    assert_ne!(p1, p2);
    assert_eq!(name, "Back");
    assert!(
        guest.host_to_local_us(0).is_some(),
        "the clock carried over"
    );

    guest.send(&Up::Hi("fresh".into()));
    let m = wait_for(host.events(), 10, "Hi", |e| match e {
        HostEvent::Msg { peer, msg } => Some((peer, msg)),
        _ => None,
    });
    assert_eq!(m, (p2, Up::Hi("fresh".into())));
    host.broadcast(&Down::Seq(7));
    wait_for(guest.events(), 10, "Seq", |e| {
        matches!(e, GuestEvent::Msg(Down::Seq(7))).then_some(())
    });

    // Ending the session isn't a drop: no rejoining.
    host.close();
    let r = wait_for(guest.events(), 10, "Closed", |e| match e {
        GuestEvent::Closed(r) => Some(r),
        GuestEvent::Reconnecting => panic!("rejoining an ended session"),
        _ => None,
    });
    assert_eq!(r, "The host ended the session");
}

mod raw {
    //! A hand-rolled guest: the wire format without the `together` client.

    use super::*;
    use iroh::endpoint::{Connection, ConnectionError, RecvStream, SendStream, presets};
    use iroh::{Endpoint, EndpointAddr, TransportAddr};

    const ALPN: &[u8] = b"ember/together/1";
    pub const F_CTRL: u8 = 0;
    pub const F_MSG: u8 = 1;
    pub const F_CLOCK: u8 = 3;
    /// The `close::PROTOCOL` application close code.
    pub const PROTOCOL: u64 = 5;

    pub fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    pub async fn connect(local_code: &str) -> Result<(Endpoint, Connection), String> {
        let (code, addr) = local_code.split_once('@').unwrap();
        let id = iroh::PublicKey::from_z32(code.strip_prefix("ember-").unwrap()).unwrap();
        let addr: SocketAddr = addr.parse().unwrap();
        let ep = Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .bind_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let to = EndpointAddr::from_parts(id, [TransportAddr::Ip(addr)]);
        match ep.connect(to, ALPN).await {
            Ok(c) => Ok((ep, c)),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn header(kind: u8, len: usize) -> Vec<u8> {
        let mut b = vec![kind];
        b.extend_from_slice(&(len as u32).to_le_bytes());
        b
    }

    pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut b = header(kind, payload.len());
        b.extend_from_slice(payload);
        b
    }

    pub async fn read_frame(recv: &mut RecvStream) -> (u8, Vec<u8>) {
        let mut h = [0u8; 5];
        recv.read_exact(&mut h).await.unwrap();
        let len = u32::from_le_bytes([h[1], h[2], h[3], h[4]]) as usize;
        let mut p = vec![0u8; len];
        recv.read_exact(&mut p).await.unwrap();
        (h[0], p)
    }

    /// Says hello the way `Guest` does and reads the welcome.
    pub async fn hello(conn: &Connection, name: &str) -> (SendStream, RecvStream) {
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let hello = format!(r#"{{"Hello":{{"proto":2,"name":"{name}"}}}}"#);
        send.write_all(&frame(F_CTRL, hello.as_bytes()))
            .await
            .unwrap();
        let (kind, p) = read_frame(&mut recv).await;
        assert_eq!(kind, F_CTRL);
        assert!(String::from_utf8_lossy(&p).contains("Welcome"));
        (send, recv)
    }

    /// The application close code the host ended the connection with, if it did within `secs`.
    pub async fn closed_with(conn: &Connection, secs: u64) -> Option<u64> {
        match tokio::time::timeout(Duration::from_secs(secs), conn.closed()).await {
            Ok(ConnectionError::ApplicationClosed(c)) => Some(c.error_code.into_inner()),
            Ok(e) => panic!("closed otherwise: {e}"),
            Err(_) => None,
        }
    }
}

/// Frames before the handshake and oversized ones are refused from their header alone,
/// before the host reads (or waits for) a byte of the payload.
#[test]
fn raw_oversized_and_early_frames_are_refused() {
    let host = Host::<Up, Down>::start_local("H").unwrap();
    let code = host.local_code().unwrap();
    raw::rt().block_on(async {
        // A huge "hello": dropped at once, never joins.
        let (_ep, conn) = raw::connect(&code).await.unwrap();
        let (mut send, _recv) = conn.open_bi().await.unwrap();
        send.write_all(&raw::header(raw::F_CTRL, 100_000))
            .await
            .unwrap();
        assert_eq!(raw::closed_with(&conn, 5).await, Some(raw::PROTOCOL));

        // A message that's just too big for a guest (256 KB), after a proper hello.
        let (_ep, conn) = raw::connect(&code).await.unwrap();
        let (mut send, _recv) = raw::hello(&conn, "big").await;
        send.write_all(&raw::header(raw::F_MSG, 256 * 1024 + 1))
            .await
            .unwrap();
        assert_eq!(raw::closed_with(&conn, 5).await, Some(raw::PROTOCOL));

        // An oversized clock frame.
        let (_ep, conn) = raw::connect(&code).await.unwrap();
        let (mut send, _recv) = raw::hello(&conn, "tick").await;
        send.write_all(&raw::header(raw::F_CLOCK, 5_000))
            .await
            .unwrap();
        assert_eq!(raw::closed_with(&conn, 5).await, Some(raw::PROTOCOL));

        // Just under the limit: the host waits for the rest.
        let (_ep, conn) = raw::connect(&code).await.unwrap();
        let (mut send, _recv) = raw::hello(&conn, "fine").await;
        send.write_all(&raw::header(raw::F_MSG, 256 * 1024))
            .await
            .unwrap();
        assert_eq!(raw::closed_with(&conn, 1).await, None);
    });
    // Two guests got past their hello; both of the oversized ones are gone again.
    let mut joins = 0;
    let mut lefts = 0;
    while let Ok(e) = host.events().recv_timeout(Duration::from_millis(500)) {
        match e {
            HostEvent::Joined { .. } => joins += 1,
            HostEvent::Left { .. } => lefts += 1,
            _ => {}
        }
    }
    assert_eq!(joins, 3);
    assert!(lefts >= 2);
}

/// More than 8 connections stuck before their hello: the rest are refused until one finishes.
#[test]
fn raw_handshakes_are_capped() {
    let host = Host::<Up, Down>::start_local("H").unwrap();
    let code = host.local_code().unwrap();
    raw::rt().block_on(async {
        let mut idle = Vec::new();
        for _ in 0..8 {
            idle.push(raw::connect(&code).await.unwrap());
        }
        // Give the host a moment to have taken all eight in.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(raw::connect(&code).await.is_err(), "a ninth was let in");
        // One says hello: room again.
        let (_send, _recv) = raw::hello(&idle[0].1, "first").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_ep, conn) = raw::connect(&code).await.unwrap();
        let (_send, _recv) = raw::hello(&conn, "late").await;
    });
    drop(host);
}

/// The guest-reported round trip is ignored: a guest claiming `u64::MAX` gets the round trip
/// the host measures itself (loopback: well under 100 ms), so starts aren't pushed back.
#[test]
fn raw_malicious_rtt_is_ignored() {
    let host = Host::<Up, Down>::start_local("H").unwrap();
    let code = host.local_code().unwrap();
    let rt = raw::rt();
    let peer = rt.block_on(async {
        let (ep, conn) = raw::connect(&code).await.unwrap();
        let (mut send, recv) = raw::hello(&conn, "liar").await;
        let (peer, _) = joined(&host);
        // Answers the host's pings honestly (it can only ever add delay)...
        let echo = conn.clone();
        tokio::spawn(async move {
            while let Ok(d) = echo.read_datagram().await {
                if d.len() == 9 && d[0] == b'H' {
                    let mut e = d.to_vec();
                    e[0] = b'E';
                    let _ = echo.send_datagram(e.into());
                }
            }
        });
        // ...while claiming an endless round trip in its own pings.
        for i in 0..20u64 {
            let mut p = vec![b'P'];
            p.extend_from_slice(&(1_000 + i).to_le_bytes());
            p.extend_from_slice(&u64::MAX.to_le_bytes());
            send.write_all(&raw::frame(raw::F_CLOCK, &p)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let rtt = host.rtt_us(peer).expect("the host measured a round trip");
        println!("host-measured rtt {rtt} us");
        assert!(rtt < 100_000, "rtt {rtt} us");
        drop(recv);
        conn.close(0u32.into(), b"bye");
        ep.close().await;
        peer
    });
    // Gone again: no round trip.
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.rtt_us(peer).is_some() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn invalid_code() {
    assert!(matches!(
        Guest::<Up, Down>::join("hello there", "x"),
        Err(together::Error::InvalidCode)
    ));
}

#[test]
#[ignore = "needs the internet (number0 relays + DNS discovery)"]
fn public_relay_session() {
    let t = Instant::now();
    let host = Host::<Up, Down>::start("Hostess").unwrap();
    println!("host bound in {:?}, code {}", t.elapsed(), host.code());
    // Paste-mangled code with surrounding text, as from Discord.
    let pasted = format!("yo join `{}` now", host.code().to_uppercase());
    let guest = Guest::<Up, Down>::join(&pasted, "Guesty").unwrap();
    exercise(host, guest);
}
