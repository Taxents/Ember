//! The joining side: dials the host, relays messages, keeps the clock estimate.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender};
use iroh::Endpoint;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

use crate::clock::{ClockEstimator, ClockModel};
use crate::net::{self, Net, Session, Target};
use crate::wire::{self, Control, F_CLOCK, F_CTRL, F_MSG, F_RAW, close};
use crate::{Error, now_us};

const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const FAST_PING: Duration = Duration::from_millis(250);
const FAST_PING_FOR: Duration = Duration::from_secs(3);
const SLOW_PING: Duration = Duration::from_secs(1);

/// Something that happened in a joined session.
#[derive(Debug, Clone, PartialEq)]
pub enum GuestEvent<Down> {
    /// The handshake finished; messages flow from now on.
    Connected { host_name: String },
    /// Enough clock samples are in: [`Guest::host_to_local_us`] works from now on.
    ClockReady,
    /// A message from the host.
    Msg(Down),
    /// Raw bytes from the host, in order with [`GuestEvent::Msg`].
    Raw(Vec<u8>),
    /// The session is over for this guest; the text is ready to show
    /// ("The host ended the session", "Lost connection", "Couldn't reach the host...").
    /// Always the last event.
    Closed(String),
}

#[derive(Default)]
struct ClockState {
    est: ClockEstimator,
    model: Option<ClockModel>,
    rtt_us: Option<u64>,
}

struct Inner<Down> {
    events: Receiver<GuestEvent<Down>>,
    out: mpsc::UnboundedSender<Bytes>,
    clock: Arc<Mutex<ClockState>>,
    session: Session,
}

/// A joined Listen Along session. Cheap to clone, `Send + Sync`.
/// Dropping the last clone (or [`Guest::close`]) leaves the session.
pub struct Guest<Up, Down> {
    inner: Arc<Inner<Down>>,
    _up: PhantomData<fn(&Up)>,
}

impl<Up, Down> Clone for Guest<Up, Down> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _up: PhantomData,
        }
    }
}

impl<Up, Down> Guest<Up, Down>
where
    Up: Serialize,
    Down: DeserializeOwned + Send + 'static,
{
    /// Starts joining the session behind an invite `code` (anything
    /// [`parse_code`](crate::parse_code) accepts). Returns immediately;
    /// progress arrives as [`GuestEvent::Connected`] or [`GuestEvent::Closed`]
    /// (dialing gives up after 15 s). `Err` only for a malformed code or if the
    /// local runtime can't start.
    pub fn join(code: &str, name: &str) -> Result<Self, Error> {
        let code = crate::parse_code(code).ok_or(Error::InvalidCode)?;
        let id = crate::code_to_id(&code).ok_or(Error::InvalidCode)?;
        Self::join_on(Net::Public, Target { id, direct: Vec::new() }, name)
    }

    /// Joins a [`Host::start_local`](crate::Host::start_local) host by its
    /// [`Host::local_code`](crate::Host::local_code). Tests only.
    #[doc(hidden)]
    pub fn join_local(local_code: &str, name: &str) -> Result<Self, Error> {
        let (code, addr) = local_code.split_once('@').ok_or(Error::InvalidCode)?;
        let id = crate::code_to_id(code).ok_or(Error::InvalidCode)?;
        let addr = addr.parse().map_err(|_| Error::InvalidCode)?;
        Self::join_on(Net::Local, Target { id, direct: vec![addr] }, name)
    }

    fn join_on(net: Net, target: Target, name: &str) -> Result<Self, Error> {
        now_us();
        let (session, stop) = Session::new().map_err(|e| Error::Network(e.to_string()))?;
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let clock = Arc::new(Mutex::new(ClockState::default()));
        let handle = session
            .handle()
            .ok_or_else(|| Error::Network("runtime unavailable".into()))?;
        let main = handle.spawn(guest_main::<Down>(
            net,
            target,
            wire::clean_name(name),
            ev_tx,
            out_tx.clone(),
            out_rx,
            clock.clone(),
            stop,
        ));
        session.set_main(main);
        Ok(Self {
            inner: Arc::new(Inner {
                events: ev_rx,
                out: out_tx,
                clock,
                session,
            }),
            _up: PhantomData,
        })
    }

    /// Session events.
    pub fn events(&self) -> &Receiver<GuestEvent<Down>> {
        &self.inner.events
    }

    /// Sends a message to the host. Messages sent before
    /// [`GuestEvent::Connected`] are queued and delivered after the handshake;
    /// after [`GuestEvent::Closed`] they are dropped.
    pub fn send(&self, msg: &Up) {
        match serde_json::to_vec(msg) {
            Ok(v) => {
                let _ = self.inner.out.send(wire::frame(F_MSG, &v));
            }
            Err(e) => log::error!("together: couldn't encode message: {e}"),
        }
    }

    /// Converts a host [`now_us`](crate::now_us) timestamp to the local
    /// [`now_us`](crate::now_us) timeline. None until [`GuestEvent::ClockReady`].
    /// Saturates at 0 for host times before the local epoch.
    pub fn host_to_local_us(&self, host_us: u64) -> Option<u64> {
        self.model().map(|m| m.host_to_local(host_us))
    }

    /// Converts a local [`now_us`](crate::now_us) timestamp to host time.
    /// None until [`GuestEvent::ClockReady`].
    pub fn local_to_host_us(&self, local_us: u64) -> Option<u64> {
        self.model().map(|m| m.local_to_host(local_us))
    }

    /// Latest filtered round trip to the host in microseconds (median of the
    /// last 8 clock pings). None before the first pong.
    pub fn rtt_us(&self) -> Option<u64> {
        self.inner.clock.lock().ok()?.rtt_us
    }

    /// Leaves the session (waits up to ~1.5 s to tell the host).
    pub fn close(self) {
        self.inner.session.stop();
    }

    fn model(&self) -> Option<ClockModel> {
        self.inner.clock.lock().ok()?.model
    }
}

async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|s| *s).await;
}

#[allow(clippy::too_many_arguments)]
async fn guest_main<Down: DeserializeOwned + Send + 'static>(
    net: Net,
    target: Target,
    name: String,
    events: Sender<GuestEvent<Down>>,
    out_tx: mpsc::UnboundedSender<Bytes>,
    out_rx: mpsc::UnboundedReceiver<Bytes>,
    clock: Arc<Mutex<ClockState>>,
    mut stop: watch::Receiver<bool>,
) {
    let ep = tokio::select! {
        r = net::bind(net, false) => r,
        _ = stopped(&mut stop) => return,
    };
    let ep = match ep {
        Ok(ep) => ep,
        Err(e) => {
            log::error!("together: couldn't bind endpoint: {e}");
            let _ = events.send(GuestEvent::Closed(format!("Couldn't start networking ({e})")));
            return;
        }
    };
    let ctx = Ctx {
        events: &events,
        clock: &clock,
        out_tx: &out_tx,
    };
    let reason = run(&ep, &target, name, ctx, out_rx, &mut stop).await;
    let _ = timeout(Duration::from_secs(1), ep.close()).await;
    if let Some(reason) = reason {
        log::info!("together: session closed: {reason}");
        let _ = events.send(GuestEvent::Closed(reason));
    }
}

struct Ctx<'a, Down> {
    events: &'a Sender<GuestEvent<Down>>,
    clock: &'a Mutex<ClockState>,
    out_tx: &'a mpsc::UnboundedSender<Bytes>,
}

impl<Down> Clone for Ctx<'_, Down> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<Down> Copy for Ctx<'_, Down> {}

impl<Down> Ctx<'_, Down> {
    fn on_pong(&self, payload: &[u8], t4: u64) {
        let Some((t1, t2, t3)) = wire::parse_pong(payload) else { return };
        let Ok(mut c) = self.clock.lock() else { return };
        if !c.est.add(t1, t2, t3, t4) {
            return;
        }
        let was_ready = c.model.is_some();
        c.model = c.est.model();
        c.rtt_us = c.est.rtt_us();
        if !was_ready && c.model.is_some() {
            drop(c);
            log::info!("together: clock ready");
            let _ = self.events.send(GuestEvent::ClockReady);
        }
    }
}

/// Dials, handshakes and runs the session. Returns the reason it ended,
/// or None when the user left.
async fn run<Down: DeserializeOwned>(
    ep: &Endpoint,
    target: &Target,
    name: String,
    ctx: Ctx<'_, Down>,
    out_rx: mpsc::UnboundedReceiver<Bytes>,
    stop: &mut watch::Receiver<bool>,
) -> Option<String> {
    let started = Instant::now();
    let dial = tokio::select! {
        r = timeout(DIAL_TIMEOUT, dial(ep, target)) => r,
        _ = stopped(stop) => return None,
    };
    let Ok(conn) = dial else {
        log::warn!("together: dial timed out");
        return Some(UNREACHABLE.into());
    };
    log::info!("together: connected in {} ms", started.elapsed().as_millis());

    let hs = tokio::select! {
        r = timeout(HANDSHAKE_TIMEOUT, handshake(&conn, name)) => r,
        _ = stopped(stop) => {
            conn.close(close::NORMAL.into(), b"bye");
            return None;
        }
    };
    let (send, mut recv, host_name) = match hs {
        Ok(Ok(v)) => v,
        Ok(Err(())) | Err(_) => {
            let reason = conn
                .close_reason()
                .map(|e| net::guest_close_reason(&e))
                .unwrap_or_else(|| "The host didn't answer".into());
            conn.close(close::PROTOCOL.into(), b"handshake");
            return Some(reason);
        }
    };
    let _ = ctx.events.send(GuestEvent::Connected { host_name });

    let writer = tokio::spawn(write_frames(send, out_rx));
    let outcome = tokio::select! {
        r = read_host(&mut recv, ctx) => r,
        _ = read_datagrams(&conn, ctx) => Ok(()),
        _ = ping_loop(&conn, ctx) => Ok(()),
        _ = stopped(stop) => {
            conn.close(close::NORMAL.into(), b"bye");
            writer.abort();
            return None;
        }
    };
    writer.abort();
    let reason = match outcome {
        Err(()) => {
            conn.close(close::PROTOCOL.into(), b"bad data");
            "Lost connection (the host sent data this version of Ember can't read)".to_string()
        }
        Ok(()) => match conn.close_reason() {
            Some(e) => net::guest_close_reason(&e),
            None => {
                conn.close(close::NORMAL.into(), b"bye");
                "The host ended the session".to_string()
            }
        },
    };
    Some(reason)
}

/// Dials until it works; the caller bounds it with [`DIAL_TIMEOUT`]. Retrying
/// matters right after a host starts: its address takes a few seconds to be
/// published, and early lookups fail fast.
async fn dial(ep: &Endpoint, target: &Target) -> Connection {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match ep.connect(target.addr(), wire::ALPN).await {
            Ok(c) => return c,
            Err(e) => {
                log::info!("together: dial attempt {attempt} failed: {e}");
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }
    }
}

const UNREACHABLE: &str =
    "Couldn't reach the host. Check the invite code and that the host is still sharing.";

async fn handshake(conn: &Connection, name: String) -> Result<(SendStream, RecvStream, String), ()> {
    let (mut send, mut recv) = conn.open_bi().await.map_err(|_| ())?;
    let hello = wire::control_frame(&Control::Hello {
        proto: wire::PROTO,
        name,
    });
    send.write_chunk(hello).await.map_err(|_| ())?;
    match wire::read_frame(&mut recv).await {
        Ok(Some((F_CTRL, p))) => match serde_json::from_slice::<Control>(&p) {
            Ok(Control::Welcome { name }) => Ok((send, recv, wire::clean_name(&name))),
            _ => Err(()),
        },
        _ => Err(()),
    }
}

async fn write_frames(mut send: SendStream, mut rx: mpsc::UnboundedReceiver<Bytes>) {
    while let Some(f) = rx.recv().await {
        if send.write_chunk(f).await.is_err() {
            return;
        }
    }
}

/// Reads host frames until the stream ends; `Err` on undecodable data.
async fn read_host<Down: DeserializeOwned>(recv: &mut RecvStream, ctx: Ctx<'_, Down>) -> Result<(), ()> {
    loop {
        match wire::read_frame(recv).await {
            Ok(None) => return Ok(()),
            Err(e) => {
                log::debug!("together: host stream ended: {e}");
                return Ok(());
            }
            Ok(Some((F_MSG, p))) => match serde_json::from_slice::<Down>(&p) {
                Ok(m) => {
                    let _ = ctx.events.send(GuestEvent::Msg(m));
                }
                Err(e) => {
                    log::warn!("together: unreadable message from host: {e}");
                    return Err(());
                }
            },
            Ok(Some((F_RAW, p))) => {
                let _ = ctx.events.send(GuestEvent::Raw(p));
            }
            Ok(Some((F_CLOCK, p))) => ctx.on_pong(&p, now_us()),
            Ok(Some((F_CTRL, _))) => {}
            Ok(Some((kind, _))) => {
                log::warn!("together: unknown frame kind {kind} from host");
                return Err(());
            }
        }
    }
}

async fn read_datagrams<Down>(conn: &Connection, ctx: Ctx<'_, Down>) {
    loop {
        match conn.read_datagram().await {
            Ok(d) => ctx.on_pong(&d, now_us()),
            Err(_) => {
                if conn.close_reason().is_some() {
                    return;
                }
                std::future::pending::<()>().await;
            }
        }
    }
}

async fn ping_loop<Down>(conn: &Connection, ctx: Ctx<'_, Down>) {
    let start = Instant::now();
    let mut warned = false;
    loop {
        let rtt = ctx.clock.lock().ok().and_then(|c| c.rtt_us).unwrap_or(0);
        let p = wire::ping(now_us(), rtt);
        let dgram_ok = conn.max_datagram_size().is_some_and(|m| m >= p.len())
            && conn.send_datagram(p.clone()).is_ok();
        if !dgram_ok {
            if !warned {
                log::info!("together: datagrams unavailable, pinging over the stream");
                warned = true;
            }
            let _ = ctx.out_tx.send(wire::frame(F_CLOCK, &p));
        }
        let wait = if start.elapsed() < FAST_PING_FOR { FAST_PING } else { SLOW_PING };
        tokio::time::sleep(wait).await;
    }
}
