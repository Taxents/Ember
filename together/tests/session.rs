//! End-to-end tests: a host and guests in one process.
//!
//! The `local_*` tests run over loopback without relays (no internet needed).
//! `public_relay_session` uses number0's relays and DNS discovery like the app
//! does; run it with `cargo test -p together -- --ignored`.

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
