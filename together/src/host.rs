//! The hosting side: accepts guests, fans messages out, answers clock pings.

use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender};
use iroh::Endpoint;
use iroh::endpoint::{Connection, Incoming, RecvStream, SendStream};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::time::timeout;

use crate::clock::MAX_RTT_US;
use crate::net::{self, Net, Session};
use crate::wire::{self, Control, F_CLOCK, F_CTRL, F_MSG, F_RAW, close};
use crate::{Error, PeerId, now_us};

/// Most guests one host accepts.
pub const MAX_GUESTS: usize = 10;
/// Frames queued per guest before it is considered too slow and dropped.
const QUEUE: usize = 2048;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections allowed to be between accepting and their `Hello` at once; more are refused.
const MAX_HANDSHAKES: usize = 8;
/// Round trips the host's estimate is the median of.
const RTT_SAMPLES: usize = 8;
/// Pings awaiting their echo; an echo of anything else is ignored.
const RTT_PENDING: usize = 16;

/// Something that happened in a hosted session.
#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent<Up> {
    /// A guest finished the handshake and can now be sent to.
    Joined { peer: PeerId, name: String },
    /// A guest left, was kicked, or its connection was lost.
    Left { peer: PeerId },
    /// A message from a guest.
    Msg { peer: PeerId, msg: Up },
    /// A problem with the session as a whole (for example the relay servers
    /// can't be reached, so friends may not be able to join). The session
    /// keeps running; show it to the user.
    Failed(String),
}

struct Peer {
    tx: mpsc::Sender<Bytes>,
    rtt: Arc<Rtt>,
    conn: Connection,
    /// The guest's endpoint (TLS-authenticated, and the same when it rejoins).
    remote: iroh::EndpointId,
}

/// The host's own measure of one guest's round trip: its pings ([`wire::host_ping`]) and their
/// echoes, timed on the host's clock. Nothing the guest says about its delay is trusted; the
/// most it can do is answer slowly, and a round trip counts as at most [`MAX_RTT_US`].
#[derive(Default)]
struct Rtt {
    m: Mutex<RttState>,
    /// Median of the recent round trips, µs (0: none yet).
    us: AtomicU64,
}

#[derive(Default)]
struct RttState {
    pending: VecDeque<u64>,
    recent: VecDeque<u64>,
}

impl Rtt {
    fn sent(&self, t: u64) {
        if let Ok(mut m) = self.m.lock() {
            if m.pending.len() == RTT_PENDING {
                m.pending.pop_front();
            }
            m.pending.push_back(t);
        }
    }

    fn echoed(&self, t: u64, now: u64) {
        let Ok(mut m) = self.m.lock() else { return };
        // Only pings sent and not yet answered count (each once).
        let Some(i) = m.pending.iter().position(|&p| p == t) else {
            return;
        };
        m.pending.drain(..=i);
        if m.recent.len() == RTT_SAMPLES {
            m.recent.pop_front();
        }
        m.recent
            .push_back(now.saturating_sub(t).clamp(1, MAX_RTT_US));
        let mut v: Vec<u64> = m.recent.iter().copied().collect();
        v.sort_unstable();
        self.us.store(v[v.len() / 2], Ordering::Relaxed);
    }
}

struct Shared<Up> {
    name: String,
    peers: Mutex<HashMap<PeerId, Peer>>,
    next_id: AtomicU64,
    events: Sender<HostEvent<Up>>,
    stop: watch::Receiver<bool>,
    /// Test hook: connections are refused until then (see [`Host::drop_link`]).
    refuse_until: Mutex<Option<Instant>>,
}

impl<Up> Shared<Up> {
    fn emit(&self, e: HostEvent<Up>) {
        crate::deliver(&self.events, e, &self.stop);
    }

    fn refusing(&self) -> bool {
        self.refuse_until
            .lock()
            .ok()
            .and_then(|r| *r)
            .is_some_and(|t| Instant::now() < t)
    }

    /// Removes a peer and closes its connection; its task then reports `Left`.
    fn drop_peer(&self, peer: PeerId, code: u32, reason: &[u8]) {
        let removed = self.peers.lock().ok().and_then(|mut p| p.remove(&peer));
        if let Some(p) = removed {
            p.conn.close(code.into(), reason);
        }
    }

    fn push(&self, peer: PeerId, frame: Bytes) {
        let full = match self.peers.lock() {
            Ok(peers) => match peers.get(&peer) {
                Some(p) => matches!(
                    p.tx.try_send(frame),
                    Err(mpsc::error::TrySendError::Full(_))
                ),
                None => false,
            },
            Err(_) => false,
        };
        if full {
            log::warn!("together: guest {peer} can't keep up, dropping it");
            self.drop_peer(peer, close::TOO_SLOW, b"too slow");
        }
    }

    fn push_all(&self, frame: Bytes) {
        let slow: Vec<PeerId> = match self.peers.lock() {
            Ok(peers) => peers
                .iter()
                .filter(|(_, p)| {
                    matches!(
                        p.tx.try_send(frame.clone()),
                        Err(mpsc::error::TrySendError::Full(_))
                    )
                })
                .map(|(id, _)| *id)
                .collect(),
            Err(_) => Vec::new(),
        };
        for peer in slow {
            log::warn!("together: guest {peer} can't keep up, dropping it");
            self.drop_peer(peer, close::TOO_SLOW, b"too slow");
        }
    }
}

struct Inner<Up> {
    shared: Arc<Shared<Up>>,
    events: Receiver<HostEvent<Up>>,
    code: String,
    local_addr: Option<SocketAddr>,
    session: Session,
}

/// A hosted Listen Along session.
///
/// Cheap to clone (all clones share one session) and `Send + Sync`, so the UI
/// thread and a network thread can both hold one. The session ends when
/// [`Host::close`] is called or the last clone is dropped: every guest is
/// disconnected and sees [`GuestEvent::Closed`](crate::GuestEvent::Closed)
/// with "The host ended the session".
pub struct Host<Up, Down> {
    inner: Arc<Inner<Up>>,
    _down: PhantomData<fn(&Down)>,
}

impl<Up, Down> Clone for Host<Up, Down> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _down: PhantomData,
        }
    }
}

impl<Up, Down> Host<Up, Down>
where
    Up: DeserializeOwned + Send + 'static,
    Down: Serialize,
{
    /// Starts hosting: binds a fresh endpoint (new identity, so a new invite
    /// code every session) and starts accepting guests. Blocks only for the
    /// local socket bind (a few ms); reaching the relay servers continues in
    /// the background. `name` is shown to guests.
    pub fn start(name: &str) -> Result<Self, Error> {
        Self::start_on(Net::Public, name)
    }

    /// Loopback-only host without relays or address lookup, for tests.
    /// Pair with [`Host::local_code`] and [`Guest::join_local`](crate::Guest::join_local).
    #[doc(hidden)]
    pub fn start_local(name: &str) -> Result<Self, Error> {
        Self::start_on(Net::Local, name)
    }

    fn start_on(net: Net, name: &str) -> Result<Self, Error> {
        now_us(); // pin the epoch
        let (session, stop) = Session::new().map_err(|e| Error::Network(e.to_string()))?;
        let ep = session
            .block_on(net::bind(net, true))
            .ok_or_else(|| Error::Network("runtime unavailable".into()))?
            .map_err(Error::Network)?;
        let code = format!("{}{}", crate::CODE_PREFIX, ep.id().to_z32());
        let local_addr = if net == Net::Local {
            net::local_addr(&ep)
        } else {
            None
        };
        let (tx, rx) = crossbeam_channel::bounded(crate::EVENTS);
        let shared = Arc::new(Shared {
            name: wire::clean_name(name),
            peers: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            events: tx,
            stop: stop.clone(),
            refuse_until: Mutex::new(None),
        });
        let handle = session
            .handle()
            .ok_or_else(|| Error::Network("runtime unavailable".into()))?;
        let main = handle.spawn(accept_loop::<Up>(net, ep, shared.clone(), stop));
        session.set_main(main);
        log::info!("together: hosting started");
        Ok(Self {
            inner: Arc::new(Inner {
                shared,
                events: rx,
                code,
                local_addr,
                session,
            }),
            _down: PhantomData,
        })
    }

    /// The invite code: `ember-` followed by the 52-character z-base-32
    /// endpoint id (lowercase letters and digits only, so it survives chat apps).
    pub fn code(&self) -> String {
        self.inner.code.clone()
    }

    /// For [`Host::start_local`] hosts: the code plus `@127.0.0.1:port`.
    #[doc(hidden)]
    pub fn local_code(&self) -> Option<String> {
        self.inner
            .local_addr
            .map(|a| format!("{}@{a}", self.inner.code))
    }

    /// Session events. Poll with `try_recv` from the UI thread or block on it
    /// from a worker thread.
    pub fn events(&self) -> &Receiver<HostEvent<Up>> {
        &self.inner.events
    }

    /// Guests currently connected.
    pub fn peers(&self) -> Vec<PeerId> {
        let mut v: Vec<PeerId> = self
            .inner
            .shared
            .peers
            .lock()
            .map(|p| p.keys().copied().collect())
            .unwrap_or_default();
        v.sort_unstable();
        v
    }

    /// Sends a message to one guest (ordered with everything else sent to it).
    /// Unknown or departed peers are ignored.
    pub fn send(&self, peer: PeerId, msg: &Down) {
        if let Some(f) = encode(msg) {
            self.inner.shared.push(peer, f);
        }
    }

    /// Sends a message to every connected guest.
    pub fn broadcast(&self, msg: &Down) {
        if let Some(f) = encode(msg) {
            self.inner.shared.push_all(f);
        }
    }

    /// Sends raw bytes to one guest; arrives as [`GuestEvent::Raw`](crate::GuestEvent::Raw)
    /// in order with messages.
    pub fn send_raw(&self, peer: PeerId, bytes: &[u8]) {
        self.inner.shared.push(peer, wire::frame(F_RAW, bytes));
    }

    /// Sends raw bytes to every connected guest (encoded once).
    pub fn broadcast_raw(&self, bytes: &[u8]) {
        self.inner.shared.push_all(wire::frame(F_RAW, bytes));
    }

    /// The round trip to the guest in microseconds, as the host measured it (median of its
    /// last 8 pings, each counted as at most 5 s), or None before the first echo.
    pub fn rtt_us(&self, peer: PeerId) -> Option<u64> {
        let peers = self.inner.shared.peers.lock().ok()?;
        match peers.get(&peer)?.rtt.us.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        }
    }

    /// Disconnects a guest ("The host removed you from the session").
    /// A [`HostEvent::Left`] follows.
    pub fn kick(&self, peer: PeerId) {
        self.inner.shared.drop_peer(peer, close::KICKED, b"removed");
    }

    /// Test hook: the guest's link drops as if the network went away, and nobody can connect
    /// for `refuse_for`. The guest sees a lost connection and rejoins by itself.
    #[doc(hidden)]
    pub fn drop_link(&self, peer: PeerId, refuse_for: Duration) {
        if let Ok(mut r) = self.inner.shared.refuse_until.lock() {
            *r = Some(Instant::now() + refuse_for);
        }
        self.inner
            .shared
            .drop_peer(peer, close::LINK_LOST, b"link lost");
    }

    /// Ends the session for everyone, waiting up to ~1.5 s for the close to be
    /// delivered. Other clones become inert.
    pub fn close(self) {
        self.inner.session.stop();
    }
}

fn encode<T: Serialize>(msg: &T) -> Option<Bytes> {
    match serde_json::to_vec(msg) {
        Ok(v) => Some(wire::frame(F_MSG, &v)),
        Err(e) => {
            log::error!("together: couldn't encode message: {e}");
            None
        }
    }
}

async fn stopped(stop: &mut watch::Receiver<bool>) {
    // Either a stop request or the session being dropped.
    let _ = stop.wait_for(|s| *s).await;
}

async fn accept_loop<Up: DeserializeOwned + Send + 'static>(
    net: Net,
    ep: Endpoint,
    shared: Arc<Shared<Up>>,
    mut stop: watch::Receiver<bool>,
) {
    if net == Net::Public {
        let (ep2, sh2) = (ep.clone(), shared.clone());
        tokio::spawn(async move {
            let t = now_us();
            if timeout(Duration::from_secs(20), ep2.online())
                .await
                .is_err()
            {
                log::warn!("together: endpoint not online after 20 s");
                sh2.emit(HostEvent::Failed(
                    "Couldn't reach the Listen Along servers. Friends may not be able to join; check your internet connection.".into(),
                ));
            } else {
                log::info!("together: host online after {} ms", (now_us() - t) / 1000);
            }
        });
    }
    let handshakes = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    loop {
        let inc = tokio::select! {
            _ = stopped(&mut stop) => break,
            inc = ep.accept() => match inc {
                Some(inc) => inc,
                None => break,
            },
        };
        if shared.refusing() {
            inc.refuse();
            continue;
        }
        match handshakes.clone().try_acquire_owned() {
            Ok(permit) => {
                tokio::spawn(serve_guest(inc, shared.clone(), stop.clone(), permit));
            }
            Err(_) => {
                log::info!("together: too many connections handshaking, refusing one");
                inc.refuse();
            }
        }
    }
    let peers: Vec<Peer> = shared
        .peers
        .lock()
        .map(|mut p| p.drain().map(|(_, v)| v).collect())
        .unwrap_or_default();
    for p in &peers {
        p.conn.close(close::HOST_ENDED.into(), b"host ended");
    }
    let _ = timeout(Duration::from_secs(1), ep.close()).await;
    log::info!("together: hosting stopped");
}

async fn serve_guest<Up: DeserializeOwned + Send + 'static>(
    inc: Incoming,
    shared: Arc<Shared<Up>>,
    mut stop: watch::Receiver<bool>,
    handshaking: OwnedSemaphorePermit,
) {
    let conn = match timeout(HANDSHAKE_TIMEOUT, inc).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            log::info!("together: incoming connection failed: {e}");
            return;
        }
        Err(_) => return,
    };
    let (send, mut recv) = match timeout(HANDSHAKE_TIMEOUT, conn.accept_bi()).await {
        Ok(Ok(s)) => s,
        _ => {
            conn.close(close::PROTOCOL.into(), b"no stream");
            return;
        }
    };
    let hello = wire::read_frame(&mut recv, |_| wire::MAX_CTRL);
    let name = match timeout(HANDSHAKE_TIMEOUT, hello).await {
        Ok(Ok(Some((F_CTRL, p)))) => match serde_json::from_slice::<Control>(&p) {
            Ok(Control::Hello { proto, name }) if proto == wire::PROTO => wire::clean_name(&name),
            _ => {
                conn.close(close::PROTOCOL.into(), b"bad hello");
                return;
            }
        },
        _ => {
            conn.close(close::PROTOCOL.into(), b"no hello");
            return;
        }
    };
    drop(handshaking);

    let (tx, rx) = mpsc::channel::<Bytes>(QUEUE);
    let _ = tx.try_send(wire::control_frame(&Control::Welcome {
        name: shared.name.clone(),
    }));
    let rtt = Arc::new(Rtt::default());
    let remote = conn.remote_id();
    let peer = {
        let Ok(mut peers) = shared.peers.lock() else {
            return;
        };
        // The same guest again (rejoining before its old link timed out): the old one goes,
        // rather than lingering as a second member until QUIC notices. Its task reports `Left`.
        let old: Vec<PeerId> = peers
            .iter()
            .filter(|(_, p)| p.remote == remote)
            .map(|(id, _)| *id)
            .collect();
        for id in old {
            if let Some(p) = peers.remove(&id) {
                log::info!("together: guest {id} is back on a new link");
                p.conn.close(close::NORMAL.into(), b"replaced");
            }
        }
        if peers.len() >= MAX_GUESTS {
            drop(peers);
            log::info!("together: refusing a guest, session full");
            conn.close(close::FULL.into(), b"full");
            return;
        }
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        peers.insert(
            id,
            Peer {
                tx: tx.clone(),
                rtt: rtt.clone(),
                conn: conn.clone(),
                remote,
            },
        );
        id
    };
    log::info!("together: guest {peer} joined");
    shared.emit(HostEvent::Joined { peer, name });
    let writer = tokio::spawn(write_frames(send, rx));

    let ended = tokio::select! {
        r = read_guest(&mut recv, peer, &shared, &tx, &rtt) => r,
        _ = answer_datagrams(&conn, &tx, &rtt) => Ok(()),
        _ = ping_guest(&conn, &tx, &rtt) => Ok(()),
        _ = stopped(&mut stop) => return,
    };
    if let Err(code) = ended {
        conn.close(code.into(), b"protocol error");
    }
    let still_listed = shared
        .peers
        .lock()
        .map(|mut p| p.remove(&peer).is_some())
        .unwrap_or(false);
    if still_listed {
        conn.close(close::NORMAL.into(), b"");
    }
    drop(tx);
    writer.abort();
    log::info!(
        "together: guest {peer} left ({})",
        conn.close_reason()
            .map(|r| r.to_string())
            .unwrap_or_default()
    );
    shared.emit(HostEvent::Left { peer });
}

async fn write_frames(mut send: SendStream, mut rx: mpsc::Receiver<Bytes>) {
    while let Some(f) = rx.recv().await {
        if send.write_chunk(f).await.is_err() {
            return;
        }
    }
    let _ = send.finish();
}

/// Reads guest frames until the stream ends. `Err(close code)` on bad data.
async fn read_guest<Up: DeserializeOwned>(
    recv: &mut RecvStream,
    peer: PeerId,
    shared: &Shared<Up>,
    tx: &mpsc::Sender<Bytes>,
    rtt: &Rtt,
) -> Result<(), u32> {
    loop {
        match wire::read_frame(recv, wire::max_up).await {
            Ok(None) => return Ok(()),
            Err(e) if e.starts_with(wire::TOO_LARGE) => {
                log::warn!("together: guest {peer}: {e}");
                return Err(close::PROTOCOL);
            }
            Err(e) => {
                log::debug!("together: guest {peer} stream ended: {e}");
                return Ok(());
            }
            Ok(Some((F_MSG, p))) => match serde_json::from_slice::<Up>(&p) {
                Ok(msg) => shared.emit(HostEvent::Msg { peer, msg }),
                Err(e) => {
                    log::warn!("together: guest {peer} sent an unreadable message: {e}");
                    return Err(close::PROTOCOL);
                }
            },
            Ok(Some((F_CLOCK, p))) => {
                if let Some(pong) = on_clock(&p, rtt) {
                    let _ = tx.try_send(wire::frame(F_CLOCK, &pong));
                }
            }
            Ok(Some((F_CTRL, _))) => {}
            Ok(Some((kind, _))) => {
                log::warn!("together: guest {peer} sent unknown frame kind {kind}");
                return Err(close::PROTOCOL);
            }
        }
    }
}

/// A clock message from a guest: its ping gets the pong returned here; the echo of one of
/// ours is a round trip measured.
fn on_clock(p: &[u8], rtt: &Rtt) -> Option<Bytes> {
    let t2 = now_us();
    if let Some((t1, _guest_rtt)) = wire::parse_ping(p) {
        return Some(wire::pong(t1, t2, now_us()));
    }
    if let Some(t) = wire::parse_stamp(b'E', p) {
        rtt.echoed(t, t2);
    }
    None
}

/// Pings the guest (on the same schedule as its own pings) so the host knows the round trip
/// without asking. Never returns.
async fn ping_guest(conn: &Connection, tx: &mpsc::Sender<Bytes>, rtt: &Rtt) {
    let start = Instant::now();
    loop {
        let t = now_us();
        rtt.sent(t);
        let p = wire::host_ping(t);
        let dgram_ok = conn.max_datagram_size().is_some_and(|m| m >= p.len())
            && conn.send_datagram(p.clone()).is_ok();
        if !dgram_ok {
            let _ = tx.try_send(wire::frame(F_CLOCK, &p));
        }
        tokio::time::sleep(wire::ping_every(start.elapsed())).await;
    }
}

/// Answers clock pings arriving as datagrams. Never returns while the
/// connection is alive (pends if datagrams are unsupported).
async fn answer_datagrams(conn: &Connection, tx: &mpsc::Sender<Bytes>, rtt: &Rtt) {
    loop {
        let d = match conn.read_datagram().await {
            Ok(d) => d,
            Err(_) => {
                if conn.close_reason().is_some() {
                    return;
                }
                std::future::pending::<()>().await;
                return;
            }
        };
        let Some(p) = on_clock(&d, rtt) else {
            continue;
        };
        if conn.send_datagram(p.clone()).is_err() {
            let _ = tx.try_send(wire::frame(F_CLOCK, &p));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtt_counts_only_its_own_pings() {
        let r = Rtt::default();
        r.echoed(5, 10); // never sent
        assert_eq!(r.us.load(Ordering::Relaxed), 0);
        r.sent(100);
        r.sent(200);
        r.echoed(200, 1_200);
        assert_eq!(r.us.load(Ordering::Relaxed), 1_000);
        r.echoed(100, 50_000); // older than one already answered: gone
        r.echoed(200, 9_000); // answered twice
        assert_eq!(r.us.load(Ordering::Relaxed), 1_000);
        // A guest that answers very late counts as at most MAX_RTT_US.
        for i in 0..RTT_SAMPLES as u64 {
            r.sent(1_000 + i);
            r.echoed(1_000 + i, u64::MAX);
        }
        assert_eq!(r.us.load(Ordering::Relaxed), MAX_RTT_US);
    }
}
