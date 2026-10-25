//! Listen Along: friends hear what the host plays, in sync.
//!
//! * The host's app runs the queue. Everyone else's mirrors it ([`Player::follower`]) and sends
//!   what they click to the host, who does it for everyone and notes who added which song.
//! * Every start is synced (the engine's synced mode): the host's engine says it is preparing a
//!   start (`Cueing`), everyone prepares the same song from the same point and says `Ready`,
//!   then the host picks a moment a little ahead and everyone starts at it, to the device frame.
//! * Song changes are timed in advance: the host works out when its song ends, and everyone
//!   starts the next one at that moment plus [`GAP`] (the silence absorbs how far the devices
//!   drifted over the song). Nothing is corrected while a song plays, so nothing is ever cut.
//! * Someone joining mid-song prepares the song a few seconds ahead of the host and starts
//!   exactly when the host gets there; nobody else hears a thing. Live radio can't be joined
//!   midway (everyone must decode the same bytes from the same point), so a join restarts the
//!   station for everyone.
//! * Radio: the host relays the station's own bytes and everyone decodes the same audio.
//! * A listener whose link drops keeps playing and rejoins by itself ("Reconnecting to …");
//!   the host takes it back in from the next song start.
//! * What a listener sends is trusted no further than it must be: songs are taken by id (the
//!   host looks up the rest itself), queue edits name the song meant and the queue version they
//!   were made on, the restarts and requests a listener can cause are rate-limited, what they
//!   can add to the queue is capped, and the host times the round trips it waits on itself
//!   (see `together`).
//!
//! [`Player::follower`]: music_core::player::Player::follower

use crate::{App, Ui, with};
use music_core::audio::Event;
use music_core::player::Repeat;
use music_core::radio::{self, relay};
use music_core::yt::model::Track;
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, ModelRc, SharedString, TimerMode, VecModel};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use together::{Guest, GuestEvent, Host, HostEvent, PeerId};

/// Silence between songs: room for the devices' clocks to have drifted apart over a song.
const GAP: Duration = Duration::from_millis(40);
/// Someone joining prepares the song this far ahead of the host.
const JOIN_LEAD_MS: f64 = 4000.0;
/// The least time between saying when to start and starting.
const START_MARGIN: Duration = Duration::from_millis(250);
/// The most a listener's round trip adds to that: a slower one is started late and catches up
/// rather than holding everyone back.
const MAX_RTT_MARGIN: Duration = Duration::from_secs(2);
/// How long a start waits for everyone to be ready; whoever isn't joins it afterwards.
const READY_WAIT: Duration = Duration::from_secs(8);
/// Songs of the queue sent to listeners, from the current one.
const QUEUE_WINDOW: usize = 200;
/// A listener can have itself started again at most this often.
const RESYNC_EVERY: Duration = Duration::from_secs(10);
/// A start heard later than this is reported to the host.
const LATE: Duration = Duration::from_millis(30);
/// Songs new to the host waiting to be looked up, at most; more are left as they are.
const LOOKUPS: usize = 2000;
/// Songs looked up per round trip to the UI thread.
const LOOKUP_CHUNK: usize = 8;
/// Songs taken from one listener request, at most.
const MAX_IDS: usize = 500;
/// The longest a listener can make the queue by adding to it.
const MAX_QUEUE: usize = 5000;
/// Requests a listener can make at once, and how many more a second after that.
const REQUEST_BURST: f64 = 20.0;
const REQUEST_RATE: f64 = 2.0;
/// Network events reach the UI thread in batches, at most this often…
const BATCH_EVERY: Duration = Duration::from_millis(16);
/// …of at most this many, one at a time (the next waits until the last was handled).
const BATCH_MAX: usize = 256;
/// What a station new to the host is called until it's looked up.
const SOME_STATION: &str = "Radio station";

/// Tells sessions apart, so events of one that ended are dropped.
static SESSIONS: AtomicU64 = AtomicU64::new(0);

/// Host to listener.
#[derive(Serialize, Deserialize)]
pub enum Down {
    /// The queue from the current song on, who added each (empty: the host), and its modes.
    /// `ver` changes with every change; queue edits name the one they were made on.
    Queue {
        tracks: Vec<Track>,
        by: Vec<String>,
        shuffle: bool,
        repeat: u8,
        ver: u64,
    },
    Members(Vec<String>),
    /// Get `id` ready from `ms`, held, and say `Ready`.
    Prepare {
        seq: u64,
        id: String,
        ms: u64,
    },
    /// Start what `seq` prepared at this time (host clock, µs).
    Start {
        seq: u64,
        at: u64,
    },
    /// The song after this one starts at this time (host clock, µs; `None`: not known yet).
    Next {
        id: String,
        at: Option<u64>,
    },
    Pause,
    Stop,
    /// A station said what it plays.
    Title {
        id: String,
        title: String,
    },
    /// A station's bytes follow (raw frames: the epoch, 8 bytes little-endian, then bytes).
    RadioStart {
        epoch: u64,
        id: String,
        content_type: String,
        kbps: Option<u32>,
    },
    RadioEnd {
        epoch: u64,
    },
}

/// Listener to host.
#[derive(Serialize, Deserialize)]
pub enum Up {
    /// The clock is in step with the host's: starts can be timed.
    Synced,
    /// Back after a dropped link and still playing along: count me in from the next start.
    InStep,
    Ready {
        seq: u64,
    },
    /// Start me again in step.
    Resync,
    /// The last start was heard this much late (µs).
    Report {
        offset_us: u64,
    },
    Request(Ask),
}

/// What a listener clicked, for the host to do (positions in the queue as shown here). Sent as
/// an [`Ask`].
pub enum Req {
    Toggle,
    Next,
    Previous,
    Seek(u64),
    Jump(usize),
    Play {
        tracks: Vec<Track>,
        start: Option<usize>,
        shuffle: Option<bool>,
    },
    Add {
        tracks: Vec<Track>,
        next: bool,
    },
    Remove(usize),
    Move(usize, usize),
    Shuffle(bool),
    Repeat(u8),
}

/// A [`Req`] as sent: songs by id only, and queue positions (in the queue as sent) with the id
/// of the song meant and the queue version they were seen in.
#[derive(Serialize, Deserialize)]
pub enum Ask {
    Toggle,
    Next,
    Previous,
    Seek(u64),
    Jump {
        at: usize,
        id: String,
        ver: u64,
    },
    Play {
        ids: Vec<String>,
        start: Option<usize>,
        shuffle: Option<bool>,
    },
    Add {
        ids: Vec<String>,
        next: bool,
    },
    Remove {
        at: usize,
        id: String,
        ver: u64,
    },
    Move {
        from: usize,
        to: usize,
        id: String,
        ver: u64,
    },
    Shuffle(bool),
    Repeat(u8),
}

struct Member {
    peer: PeerId,
    name: String,
    /// Its clock is in step: it takes part in starts.
    synced: bool,
    /// The last start it was asked to prepare: the only `Ready` it may answer.
    issued: Option<u64>,
    /// When it last had itself started again.
    resynced: Option<Instant>,
    /// What it may still ask for.
    asks: Allowance,
}

/// A request budget: [`REQUEST_BURST`] at once, refilled at [`REQUEST_RATE`] a second.
struct Allowance {
    left: f64,
    at: Instant,
}

impl Allowance {
    fn new(now: Instant) -> Self {
        Self {
            left: REQUEST_BURST,
            at: now,
        }
    }

    /// Spends one request, if there is one left.
    fn take(&mut self, now: Instant) -> bool {
        let refill = now.saturating_duration_since(self.at).as_secs_f64() * REQUEST_RATE;
        self.left = (self.left + refill).min(REQUEST_BURST);
        self.at = now;
        let ok = self.left >= 1.0;
        if ok {
            self.left -= 1.0;
        }
        ok
    }
}

/// A start everyone gets ready for.
struct Barrier {
    seq: u64,
    id: String,
    waiting: HashSet<PeerId>,
    /// The host's own engine is ready.
    ready: bool,
    until: Instant,
}

pub struct Hosting {
    session: u64,
    link: Host<Up, Down>,
    members: Vec<Member>,
    seq: u64,
    barrier: Option<Barrier>,
    /// Joins being prepared: seq → (who, the position they start from).
    joins: HashMap<u64, (PeerId, f64)>,
    /// The next song's start as last said.
    next_sent: Option<(String, Instant)>,
    /// Who added which song (by video id).
    by: HashMap<String, String>,
    /// Play-order position of the first song sent in the queue.
    window_from: usize,
    /// The queue's version, as last sent.
    ver: u64,
    /// Songs listeners added that are waiting to be looked up or being looked up.
    looking_up: HashSet<String>,
    /// Those waiting, in order.
    backlog: VecDeque<String>,
    /// A lookup is under way.
    lookup_busy: bool,
}

pub struct Listening {
    session: u64,
    link: Guest<Up, Down>,
    connected: bool,
    /// The link dropped and is being dialed again.
    reconnecting: bool,
    host: String,
    members: Vec<String>,
    /// The start being prepared, and when it starts once said (host µs).
    prepared: Option<u64>,
    start: Option<(u64, u64)>,
    /// The next song and its start (host µs), waiting for the clock.
    next: Option<(String, u64)>,
    by: Vec<String>,
    relay: Option<(u64, relay::Relay)>,
    clock: bool,
    /// The queue's version, as last received.
    ver: u64,
    /// A start was timed for then: how late it was heard is checked a little after.
    started: Option<Instant>,
}

pub enum Session {
    Hosting(Box<Hosting>),
    Listening(Box<Listening>),
}

/// Microseconds on this machine's session clock at `t`.
fn us_of(t: Instant) -> u64 {
    let (now, us) = (Instant::now(), together::now_us());
    if t >= now {
        us + (t - now).as_micros() as u64
    } else {
        us.saturating_sub((now - t).as_micros() as u64)
    }
}

/// How far ahead a start is set when the slowest one to tell has this round trip.
fn start_margin(rtt_us: Option<u64>) -> Duration {
    START_MARGIN + Duration::from_micros(rtt_us.unwrap_or(0)).min(MAX_RTT_MARGIN)
}

fn repeat_code(r: Repeat) -> u8 {
    match r {
        Repeat::Off => 0,
        Repeat::All => 1,
        Repeat::One => 2,
    }
}

fn repeat_of(r: u8) -> Repeat {
    match r {
        1 => Repeat::All,
        2 => Repeat::One,
        _ => Repeat::Off,
    }
}

/// A song id a listener may name: a YouTube video id or a station.
fn playable_id(id: &str) -> bool {
    radio::is_station(id)
        || id.len() == 11
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The play-order position a listener meant: `at` in the queue window starting at `from`, if
/// it was seen in version `seen` (the current one, `ver`) and holds `id` there.
fn place(order: &[&str], from: usize, ver: u64, at: usize, id: &str, seen: u64) -> Option<usize> {
    let pos = from.checked_add(at)?;
    (seen == ver && order.get(pos) == Some(&id)).then_some(pos)
}

/// Delivers a session's network events to the UI thread until it ends: in batches of at most
/// [`BATCH_MAX`], at most one per [`BATCH_EVERY`] and one at a time, each dropped once
/// `session` is no longer the one running. Waiting for the UI leaves the rest in the bounded
/// channel, which slows the network side down rather than piling work onto the UI thread.
fn forward<E: Send + 'static>(
    rx: crossbeam_channel::Receiver<E>,
    session: u64,
    f: fn(&mut App, E),
) {
    std::thread::Builder::new()
        .name("together-events".into())
        .spawn(move || {
            let (done_tx, done) = crossbeam_channel::bounded::<()>(1);
            let mut last: Option<Instant> = None;
            while let Ok(first) = rx.recv() {
                if let Some(wait) =
                    last.map(|l| (l + BATCH_EVERY).saturating_duration_since(Instant::now()))
                {
                    std::thread::sleep(wait);
                }
                let mut batch = vec![first];
                batch.extend(rx.try_iter().take(BATCH_MAX - 1));
                last = Some(Instant::now());
                let done_tx = done_tx.clone();
                let sent = slint::invoke_from_event_loop(move || {
                    with(|a| {
                        for e in batch {
                            if a.together_session() != Some(session) {
                                return;
                            }
                            f(a, e);
                        }
                    });
                    let _ = done_tx.try_send(());
                });
                if sent.is_err() {
                    break; // the event loop is gone
                }
                // Bounded, in case the UI never gets to it (it is quitting).
                let _ = done.recv_timeout(Duration::from_secs(1));
            }
        })
        .expect("spawn");
}

/// A song or station new to the host, until it's looked up.
fn placeholder(id: String) -> Track {
    let title = if radio::is_station(&id) {
        SOME_STATION.to_string()
    } else {
        String::new()
    };
    Track {
        video_id: id,
        title,
        ..Default::default()
    }
}

impl App {
    pub(crate) fn hosting(&self) -> bool {
        matches!(self.together, Some(Session::Hosting(_)))
    }

    pub(crate) fn listening(&self) -> bool {
        matches!(self.together, Some(Session::Listening(_)))
    }

    fn together_session(&self) -> Option<u64> {
        match &self.together {
            Some(Session::Hosting(h)) => Some(h.session),
            Some(Session::Listening(l)) => Some(l.session),
            None => None,
        }
    }

    /// Listening along: sends what was clicked to the host (true) instead of doing it here.
    pub(crate) fn ask_host(&mut self, r: Req) -> bool {
        let Some(Session::Listening(l)) = &self.together else {
            return false;
        };
        if l.reconnecting {
            let msg = format!("Not connected to {} right now", l.host);
            self.notify(msg);
            return true;
        }
        let ver = l.ver;
        let (order, _) = self.player.up_next();
        let id_at = |at: usize| order.get(at).map(|s| s.to_string());
        let ask = match r {
            Req::Toggle => Ask::Toggle,
            Req::Next => Ask::Next,
            Req::Previous => Ask::Previous,
            Req::Seek(ms) => Ask::Seek(ms),
            Req::Shuffle(on) => Ask::Shuffle(on),
            Req::Repeat(r) => Ask::Repeat(r),
            Req::Play {
                tracks,
                start,
                shuffle,
            } => Ask::Play {
                ids: tracks.into_iter().map(|t| t.video_id).collect(),
                start,
                shuffle,
            },
            Req::Add { tracks, next } => Ask::Add {
                ids: tracks.into_iter().map(|t| t.video_id).collect(),
                next,
            },
            Req::Jump(at) => match id_at(at) {
                Some(id) => Ask::Jump { at, id, ver },
                None => return true,
            },
            Req::Remove(at) => match id_at(at) {
                Some(id) => Ask::Remove { at, id, ver },
                None => return true,
            },
            Req::Move(from, to) => match id_at(from) {
                Some(id) => Ask::Move { from, to, id, ver },
                None => return true,
            },
        };
        l.link.send(&Up::Request(ask));
        true
    }

    // ---- starting and ending ----------------------------------------------------------------

    pub(crate) fn together_start(&mut self) {
        if self.together.is_some() {
            return;
        }
        let link = match Host::<Up, Down>::start(&self.settings.together_name) {
            Ok(h) => h,
            Err(e) => {
                log::warn!("together: {e}");
                return self.together_status("Couldn't start a session");
            }
        };
        let code = link.code();
        let session = SESSIONS.fetch_add(1, Ordering::Relaxed) + 1;
        forward(link.events().clone(), session, App::host_event);
        // Stations opened from now on are relayed to everyone.
        let tap_link = link.clone();
        relay::set_tap(Some(Arc::new(move |e: relay::TapEvent| match e {
            relay::TapEvent::Start {
                epoch,
                id,
                content_type,
                kbps,
            } => tap_link.broadcast(&Down::RadioStart {
                epoch,
                id: id.to_string(),
                content_type: content_type.to_string(),
                kbps,
            }),
            relay::TapEvent::Data { epoch, bytes } => {
                let mut f = Vec::with_capacity(8 + bytes.len());
                f.extend_from_slice(&epoch.to_le_bytes());
                f.extend_from_slice(bytes);
                tap_link.broadcast_raw(&f);
            }
            relay::TapEvent::End { epoch } => tap_link.broadcast(&Down::RadioEnd { epoch }),
        })));
        self.player.engine.set_synced(true);
        self.together = Some(Session::Hosting(Box::new(Hosting {
            session,
            link,
            members: Vec::new(),
            seq: 0,
            barrier: None,
            joins: HashMap::new(),
            next_sent: None,
            by: HashMap::new(),
            window_from: 0,
            ver: 0,
            looking_up: HashSet::new(),
            backlog: VecDeque::new(),
            lookup_busy: false,
        })));
        self.together_copy_code(&code);
        self.together_status("Invite code copied");
        self.together_timer_on();
        self.show_together();
    }

    pub(crate) fn together_join(&mut self, code: &str) {
        if self.together.is_some() {
            return;
        }
        let Some(code) = together::parse_code(code) else {
            return self.together_status("That isn't an invite code");
        };
        let link = match Guest::<Up, Down>::join(&code, &self.settings.together_name) {
            Ok(g) => g,
            Err(e) => {
                log::warn!("together: {e}");
                return self.together_status("Couldn't reach the host");
            }
        };
        let session = SESSIONS.fetch_add(1, Ordering::Relaxed) + 1;
        forward(link.events().clone(), session, App::guest_event);
        self.together = Some(Session::Listening(Box::new(Listening {
            session,
            link,
            connected: false,
            reconnecting: false,
            host: String::new(),
            members: Vec::new(),
            prepared: None,
            start: None,
            next: None,
            by: Vec::new(),
            relay: None,
            clock: false,
            ver: 0,
            started: None,
        })));
        self.together_status("");
        self.show_together();
    }

    /// Ends hosting or listening (`why`: said in the panel).
    pub(crate) fn together_leave(&mut self, why: &str) {
        let Some(s) = self.together.take() else {
            return;
        };
        self.together_timer.stop();
        match &s {
            Session::Hosting(_) => {
                relay::set_tap(None);
                // Carry on playing as before.
                self.player.engine.set_synced(false);
            }
            Session::Listening(_) => {
                relay::clear_relays();
                self.player.engine.stop();
                self.player.engine.set_synced(false);
                self.player.follower = false;
                self.queue_changed();
                self.show_now_playing(false);
            }
        }
        // Closing tells the other side and waits for that (up to ~1.5 s): not on this thread.
        let _ = std::thread::Builder::new()
            .name("together-close".into())
            .spawn(move || drop(s));
        self.together_status(why);
        self.show_together();
    }

    pub(crate) fn together_resync(&mut self) {
        if let Some(Session::Listening(l)) = &self.together {
            l.link.send(&Up::Resync);
        }
    }

    pub(crate) fn together_copy(&mut self) {
        if let Some(Session::Hosting(h)) = &self.together {
            let code = h.link.code();
            self.together_copy_code(&code);
            self.together_status("Invite code copied");
        }
    }

    fn together_copy_code(&self, code: &str) {
        if let Some(hwnd) = self.hwnd {
            crate::win::copy_text(hwnd, code);
        }
    }

    /// The panel opened: an invite code on the clipboard is offered.
    pub(crate) fn together_opened(&mut self) {
        let found = self
            .hwnd
            .and_then(crate::win::paste_text)
            .and_then(|t| together::parse_code(&t))
            .filter(|c| match &self.together {
                Some(Session::Hosting(h)) => *c != h.link.code(),
                _ => true,
            })
            .unwrap_or_default();
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_together_found(found.into());
        g.set_together_name(self.settings.together_name.clone().into());
    }

    fn together_status(&self, s: &str) {
        self.ui().global::<Ui>().set_together_status(s.into());
    }

    fn together_timer_on(&mut self) {
        self.together_timer
            .start(TimerMode::Repeated, Duration::from_millis(200), || {
                with(|a| a.together_tick());
            });
    }

    /// The panel and the bar button.
    pub(crate) fn show_together(&self) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let (state, host, members): (i32, String, Vec<String>) = match &self.together {
            None => (0, String::new(), Vec::new()),
            Some(Session::Hosting(h)) => (
                2,
                String::new(),
                std::iter::once(format!("{} (you)", self.settings.together_name))
                    .chain(h.members.iter().map(|m| m.name.clone()))
                    .collect(),
            ),
            Some(Session::Listening(l)) if !l.connected => (1, String::new(), Vec::new()),
            Some(Session::Listening(l)) if l.reconnecting => (4, l.host.clone(), l.members.clone()),
            Some(Session::Listening(l)) => (3, l.host.clone(), l.members.clone()),
        };
        g.set_together_state(state);
        g.set_together_host(host.into());
        g.set_together_code(match &self.together {
            Some(Session::Hosting(h)) => h.link.code().into(),
            _ => SharedString::new(),
        });
        let members: Vec<SharedString> = members.into_iter().map(Into::into).collect();
        g.set_together_members(ModelRc::new(VecModel::from(members)));
    }

    /// Who added the queue's songs, for its rows ("" the host or nobody), by play-order index
    /// from the current song.
    pub(crate) fn added_by(&self, id: &str, i: usize) -> Option<String> {
        match &self.together {
            Some(Session::Hosting(h)) => h.by.get(id).cloned(),
            Some(Session::Listening(l)) => l.by.get(i).filter(|s| !s.is_empty()).cloned(),
            None => None,
        }
    }

    // ---- hosting ----------------------------------------------------------------------------

    fn host_event(&mut self, e: HostEvent<Up>) {
        let Some(Session::Hosting(h)) = &mut self.together else {
            return;
        };
        match e {
            HostEvent::Joined { peer, name } => {
                log::info!("together: {name} joined");
                h.members.push(Member {
                    peer,
                    name,
                    synced: false,
                    issued: None,
                    resynced: None,
                    asks: Allowance::new(Instant::now()),
                });
                self.send_members();
                self.send_queue(Some(peer));
                self.show_together();
            }
            HostEvent::Left { peer } => {
                h.members.retain(|m| m.peer != peer);
                h.joins.retain(|_, (p, _)| *p != peer);
                if let Some(b) = &mut h.barrier {
                    b.waiting.remove(&peer);
                }
                self.send_members();
                self.show_together();
                self.try_start();
            }
            HostEvent::Failed(e) => {
                // The session itself still works: only who can reach it is in doubt.
                log::warn!("together: {e}");
                self.notify("Friends outside your network may not be able to join");
            }
            HostEvent::Msg { peer, msg } => {
                let Some(m) = h.members.iter_mut().find(|m| m.peer == peer) else {
                    return;
                };
                match msg {
                    Up::Synced if !m.synced => {
                        m.synced = true;
                        self.bring_in(peer);
                    }
                    // Once is enough; again is asking to be started again.
                    Up::Synced | Up::Resync => {
                        if m.resynced.is_some_and(|t| t.elapsed() < RESYNC_EVERY) {
                            log::info!("together: {} asked to resync too soon", m.name);
                            return;
                        }
                        m.resynced = Some(Instant::now());
                        self.bring_in(peer);
                    }
                    Up::InStep => {
                        // Back from a dropped link, still playing: in from the next start, and
                        // told when that is.
                        log::info!("together: {} is back", m.name);
                        m.synced = true;
                        h.next_sent = None;
                    }
                    Up::Ready { seq } => {
                        if m.issued != Some(seq) {
                            return; // not theirs to answer
                        }
                        m.issued = None;
                        if let Some(b) = h.barrier.as_mut().filter(|b| b.seq == seq) {
                            b.waiting.remove(&peer);
                            self.try_start();
                        } else if let Some((who, ms)) = h.joins.remove(&seq) {
                            self.start_join(seq, who, ms);
                        } else {
                            // Ready too late for its start: join what plays now.
                            self.bring_in(peer);
                        }
                    }
                    Up::Report { offset_us } => {
                        log::info!(
                            "together: {} heard the start {} ms late",
                            m.name,
                            offset_us / 1000
                        );
                    }
                    Up::Request(r) => {
                        if !m.asks.take(Instant::now()) {
                            log::info!("together: {} is asking too fast", m.name);
                            return;
                        }
                        let name = m.name.clone();
                        self.do_request(peer, r, &name);
                    }
                }
            }
        }
    }

    /// Gets a listener playing along with what plays now.
    fn bring_in(&mut self, peer: PeerId) {
        let Some(Session::Hosting(h)) = &mut self.together else {
            return;
        };
        let Some(t) = self.player.engine.timing() else {
            return; // Nothing plays: the next start includes them.
        };
        if h.barrier.is_some() || t.held {
            return; // A start is being prepared: the next `Ready` round includes them.
        }
        if radio::is_station(&t.id) {
            // Nobody can join a station midway: start it again for everyone.
            self.player.play_current();
            return;
        }
        h.seq += 1;
        let seq = h.seq;
        if let Some(m) = h.members.iter_mut().find(|m| m.peer == peer) {
            m.issued = Some(seq);
        }
        if t.paused {
            // Ready and held: the resume's start includes them.
            h.link.send(
                peer,
                &Down::Prepare {
                    seq,
                    id: t.id,
                    ms: t.ms as u64,
                },
            );
            return;
        }
        let ms = t.ms_at(Instant::now()) + JOIN_LEAD_MS;
        h.joins.insert(seq, (peer, ms));
        h.link.send(
            peer,
            &Down::Prepare {
                seq,
                id: t.id,
                ms: ms as u64,
            },
        );
        h.next_sent = None;
    }

    /// A joining listener is ready from `ms`: they start when the host gets there.
    fn start_join(&mut self, seq: u64, peer: PeerId, ms: f64) {
        let Some(Session::Hosting(h)) = &mut self.together else {
            return;
        };
        let Some(t) = self.player.engine.timing().filter(|t| !t.held && !t.paused) else {
            return;
        };
        let at = t.instant_of(ms.floor());
        if at < Instant::now() + start_margin(h.link.rtt_us(peer)) {
            // They took too long: again, further ahead.
            log::info!("together: join too slow, again");
            return self.bring_in(peer);
        }
        h.link.send(peer, &Down::Start { seq, at: us_of(at) });
    }

    /// Everyone is ready (or the wait is over): start together a moment from now.
    fn try_start(&mut self) {
        let Some(Session::Hosting(h)) = &mut self.together else {
            return;
        };
        let Some(b) = &h.barrier else { return };
        if !b.ready || !b.waiting.is_empty() && Instant::now() < b.until {
            return;
        }
        let rtt = h.members.iter().filter_map(|m| h.link.rtt_us(m.peer)).max();
        let at = Instant::now() + start_margin(rtt);
        let Some(b) = h.barrier.take() else { return };
        if !b.waiting.is_empty() {
            log::info!(
                "together: {} not ready, starting without them",
                b.waiting.len()
            );
        }
        self.player.engine.start_at(at);
        h.link.broadcast(&Down::Start {
            seq: b.seq,
            at: us_of(at),
        });
        h.next_sent = None;
        // Those not ready catch up once they are (their late `Ready` brings them in).
    }

    /// The host's engine is preparing a start: everyone prepares the same.
    fn host_cueing(&mut self, id: &str, ms: u64) {
        let Some(Session::Hosting(h)) = &mut self.together else {
            return;
        };
        h.seq += 1;
        h.joins.clear();
        h.next_sent = None;
        let waiting = h
            .members
            .iter()
            .filter(|m| m.synced)
            .map(|m| m.peer)
            .collect();
        for m in &mut h.members {
            m.issued = Some(h.seq);
        }
        h.barrier = Some(Barrier {
            seq: h.seq,
            id: id.to_string(),
            waiting,
            ready: false,
            until: Instant::now() + READY_WAIT,
        });
        h.link.broadcast(&Down::Prepare {
            seq: h.seq,
            id: id.to_string(),
            ms,
        });
    }

    /// Every 200 ms: a start that waited long enough, and the next song's time.
    fn together_tick(&mut self) {
        match &self.together {
            Some(Session::Hosting(_)) => {
                self.try_start();
                self.time_next();
            }
            Some(Session::Listening(_)) => self.listener_tick(),
            None => self.together_timer.stop(),
        }
    }

    /// Works out when the next song starts and tells everyone (again if it changed).
    fn time_next(&mut self) {
        let Some(Session::Hosting(h)) = &mut self.together else {
            return;
        };
        let Some(t) = self.player.engine.timing() else {
            return;
        };
        if t.held || t.paused || h.barrier.is_some() || radio::is_station(&t.id) {
            return;
        }
        let Some(d) = t.duration_ms else { return };
        let Some(next) = self.player.upcoming() else {
            return;
        };
        let at = t.instant_of(d as f64) + GAP;
        let same = h.next_sent.as_ref().is_some_and(|(id, w)| {
            *id == next && (if *w > at { *w - at } else { at - *w }) < Duration::from_millis(2)
        });
        if same {
            return;
        }
        self.player.engine.set_next_at(&next, at);
        h.link.broadcast(&Down::Next {
            id: next.clone(),
            at: Some(us_of(at)),
        });
        h.next_sent = Some((next, at));
    }

    fn send_members(&self) {
        let Some(Session::Hosting(h)) = &self.together else {
            return;
        };
        let names = std::iter::once(format!("{} (host)", self.settings.together_name))
            .chain(h.members.iter().map(|m| m.name.clone()))
            .collect();
        h.link.broadcast(&Down::Members(names));
    }

    /// Sends the queue from the current song (to one listener, or everyone).
    fn send_queue(&mut self, to: Option<PeerId>) {
        let Some(Session::Hosting(h)) = &mut self.together else {
            return;
        };
        let (order, at) = self.player.up_next();
        h.window_from = at;
        let ids: Vec<&str> = order.iter().skip(at).take(QUEUE_WINDOW).copied().collect();
        let tracks: Vec<Track> = ids
            .iter()
            .map(|id| {
                self.known.get(*id).cloned().unwrap_or_else(|| Track {
                    video_id: id.to_string(),
                    ..Default::default()
                })
            })
            .collect();
        let by = ids
            .iter()
            .map(|id| h.by.get(*id).cloned().unwrap_or_default())
            .collect();
        let msg = Down::Queue {
            tracks,
            by,
            shuffle: self.player.shuffle(),
            repeat: repeat_code(self.player.repeat),
            ver: h.ver,
        };
        match to {
            Some(p) => h.link.send(p, &msg),
            None => h.link.broadcast(&msg),
        }
    }

    /// The queue changed here.
    pub(crate) fn together_queue_changed(&mut self) {
        if let Some(Session::Hosting(h)) = &mut self.together {
            h.ver += 1;
            h.next_sent = None;
            self.send_queue(None);
        }
    }

    /// The host paused.
    pub(crate) fn together_paused(&mut self) {
        if let Some(Session::Hosting(h)) = &self.together {
            h.link.broadcast(&Down::Pause);
        }
    }

    /// The host resumes: a synced start from where it paused.
    pub(crate) fn together_resume(&mut self) -> bool {
        if !self.hosting() {
            return false;
        }
        let Some(t) = self.player.engine.timing() else {
            return false;
        };
        let ms = if radio::is_station(&t.id) {
            0
        } else {
            t.ms as u64
        };
        self.player.engine.cue(&t.id, ms, None);
        true
    }

    /// A station's title, for listeners (who get its bytes, not its titles).
    pub(crate) fn together_title(&self, id: &str, title: &str) {
        if let Some(Session::Hosting(h)) = &self.together {
            h.link.broadcast(&Down::Title {
                id: id.to_string(),
                title: title.to_string(),
            });
        }
    }

    /// Does what a listener asked, noting them as who added songs.
    fn do_request(&mut self, peer: PeerId, r: Ask, name: &str) {
        let (from, ver) = match &self.together {
            Some(Session::Hosting(h)) => (h.window_from, h.ver),
            _ => return,
        };
        // A queue position as the listener saw it, if that's still the song they meant.
        let find = |a: &App, at: usize, id: &str, seen: u64| {
            place(&a.player.up_next().0, from, ver, at, id, seen)
        };
        match r {
            Ask::Toggle => self.play_pause(),
            Ask::Next => self.skip(1),
            Ask::Previous => self.skip(-1),
            Ask::Seek(ms) => self.player.engine.seek(ms),
            Ask::Jump { at, id, ver: seen } => match find(self, at, &id, seen) {
                Some(pos) => self.player.jump(pos),
                None => self.stale_request(peer),
            },
            Ask::Play {
                ids,
                start,
                shuffle,
            } => {
                let start_id = start.and_then(|s| ids.get(s)).cloned();
                let (tracks, new) = self.guest_tracks(ids, name);
                let start = start_id.and_then(|id| tracks.iter().position(|t| t.video_id == id));
                if !tracks.is_empty() {
                    self.play_list(tracks, start, shuffle);
                }
                // Song placeholders until they're looked up (the queue shows their ids
                // meanwhile); stations keep theirs, a better name than their ids.
                for id in new.iter().filter(|id| !radio::is_station(id)) {
                    self.known.remove(id);
                }
                self.look_up(new);
            }
            Ask::Add { ids, next } => {
                let room = MAX_QUEUE.saturating_sub(self.player.up_next().0.len());
                if ids.len() > room {
                    log::info!(
                        "together: {name} added {} songs, room for {room}",
                        ids.len()
                    );
                }
                let ids: Vec<String> = ids.into_iter().take(room).collect();
                let (tracks, new) = self.guest_tracks(ids, name);
                let ids: Vec<String> = tracks.into_iter().map(|t| t.video_id).collect();
                if ids.is_empty() {
                    return;
                }
                if next {
                    self.player.insert_next(ids);
                } else {
                    self.player.append(ids);
                }
                self.queue_changed();
                self.look_up(new);
            }
            Ask::Remove { at, id, ver: seen } => match find(self, at, &id, seen) {
                Some(pos) => {
                    self.player.remove(pos);
                    self.queue_changed();
                }
                None => self.stale_request(peer),
            },
            Ask::Move {
                from: a,
                to: b,
                id,
                ver: seen,
            } => match (find(self, a, &id, seen), from.checked_add(b)) {
                (Some(p), Some(q)) => {
                    self.player.move_item(p, q);
                    self.queue_changed();
                }
                _ => self.stale_request(peer),
            },
            Ask::Shuffle(on) => {
                self.player.set_shuffle(on);
                self.queue_changed();
            }
            Ask::Repeat(r) => {
                self.player.set_repeat(repeat_of(r));
                self.queue_changed();
            }
        }
    }

    /// A listener edited a queue that has changed since: it gets the current one instead.
    fn stale_request(&mut self, peer: PeerId) {
        log::info!("together: a queue edit from {peer} was for an older queue");
        self.send_queue(Some(peer));
    }

    /// The songs a listener named (the first [`MAX_IDS`]), as the host knows them, noting who
    /// added them. Only their ids are taken; songs new here get placeholders (their ids, also
    /// returned) to look up, and stations new here are kept as [`SOME_STATION`] meanwhile.
    fn guest_tracks(&mut self, ids: Vec<String>, name: &str) -> (Vec<Track>, Vec<String>) {
        let mut new = Vec::new();
        let tracks: Vec<Track> = ids
            .into_iter()
            .filter(|id| playable_id(id))
            .take(MAX_IDS)
            .map(|id| {
                if let Some(t) = self.known.get(&id) {
                    return t.clone();
                }
                if let Some(t) = crate::radio::known_station(&id) {
                    return t;
                }
                new.push(id.clone());
                let t = placeholder(id);
                if radio::is_station(&t.video_id) {
                    self.known
                        .insert(t.video_id.clone(), crate::edits::applied(t.clone()));
                }
                t
            })
            .collect();
        if let Some(Session::Hosting(h)) = &mut self.together {
            for t in &tracks {
                h.by.insert(t.video_id.clone(), name.to_string());
            }
        }
        (tracks, new)
    }

    /// Queues songs and stations listeners added that are new here to be looked up (up to
    /// [`LOOKUPS`] waiting), and starts on them.
    fn look_up(&mut self, ids: Vec<String>) {
        let Some(Session::Hosting(h)) = &mut self.together else {
            return;
        };
        for id in ids {
            if h.looking_up.len() >= LOOKUPS {
                log::info!("together: too many songs to look up, the rest stay as they are");
                break;
            }
            if h.looking_up.insert(id.clone()) {
                h.backlog.push_back(id);
            }
        }
        self.next_lookup();
    }

    /// Looks up the next few waiting, one lookup at a time; [`App::looked_up`] goes on.
    fn next_lookup(&mut self) {
        let Some(Session::Hosting(h)) = &mut self.together else {
            return;
        };
        if h.lookup_busy || h.backlog.is_empty() {
            return;
        }
        let n = h.backlog.len().min(LOOKUP_CHUNK);
        let asked: Vec<String> = h.backlog.drain(..n).collect();
        h.lookup_busy = true;
        let (client, session) = (self.client.clone(), h.session);
        crate::workers::spawn(move || {
            let found: Vec<Track> = asked
                .iter()
                .filter_map(|id| match radio::channel_of(id) {
                    Some(c) => radio::station(c).ok().map(|st| crate::radio::track(&st)),
                    None => {
                        let w = client.watch_next(Some(id), None, None).ok()?;
                        w.tracks.into_iter().find(|t| t.video_id == *id)
                    }
                })
                .collect();
            let _ = slint::invoke_from_event_loop(move || {
                with(|a| a.looked_up(session, asked, found));
            });
        });
    }

    fn looked_up(&mut self, session: u64, asked: Vec<String>, found: Vec<Track>) {
        let changed = !found.is_empty();
        for t in found {
            // A station replaces its placeholder; a song the host has since met stays as is.
            if radio::is_station(&t.video_id) {
                self.known
                    .insert(t.video_id.clone(), crate::edits::applied(t));
            } else {
                self.known.entry(t.video_id.clone()).or_insert(t);
            }
        }
        if let Some(Session::Hosting(h)) = &mut self.together
            && h.session == session
        {
            for id in &asked {
                h.looking_up.remove(id);
            }
            h.lookup_busy = false;
            if changed {
                self.queue_changed();
            }
            self.next_lookup();
        }
    }

    // ---- listening --------------------------------------------------------------------------

    fn guest_event(&mut self, e: GuestEvent<Down>) {
        let Some(Session::Listening(l)) = &mut self.together else {
            return;
        };
        match e {
            GuestEvent::Connected { host_name } if l.connected => {
                // Rejoined after a dropped link: still playing along, so in from the next start.
                log::info!("together: back with {host_name}");
                l.reconnecting = false;
                l.host = host_name;
                if l.clock {
                    let playing = self.player.engine.timing().is_some();
                    l.link.send(if playing { &Up::InStep } else { &Up::Synced });
                }
                self.show_together();
            }
            GuestEvent::Connected { host_name } => {
                log::info!("together: listening along with {host_name}");
                l.connected = true;
                l.host = host_name;
                // Whatever played here stops; the host's queue shows instead.
                self.player.engine.stop();
                self.player.engine.set_synced(true);
                self.player.follower = true;
                self.together_timer_on();
                self.together_status("");
                self.show_together();
            }
            GuestEvent::Reconnecting => {
                // What plays carries on meanwhile.
                log::info!("together: lost the link to {}, rejoining", l.host);
                l.reconnecting = true;
                l.start = None;
                l.prepared = None;
                self.show_together();
            }
            GuestEvent::ClockReady => {
                l.clock = true;
                l.link.send(&Up::Synced);
                self.listener_tick();
            }
            GuestEvent::Closed(why) => self.together_leave(&why),
            GuestEvent::Raw(bytes) => {
                if bytes.len() >= 8
                    && let Some((epoch, relay)) = &l.relay
                    && bytes[..8] == epoch.to_le_bytes()
                {
                    relay.push(&bytes[8..]);
                }
            }
            GuestEvent::Msg(m) => self.guest_msg(m),
        }
    }

    fn guest_msg(&mut self, m: Down) {
        let Some(Session::Listening(l)) = &mut self.together else {
            return;
        };
        match m {
            Down::Queue {
                tracks,
                by,
                shuffle: _,
                repeat,
                ver,
            } => {
                l.by = by;
                l.ver = ver;
                let ids: Vec<String> = tracks.iter().map(|t| t.video_id.clone()).collect();
                // Songs the host is still looking up come bare: what's known here stays.
                for t in tracks {
                    if !t.title.is_empty() || !self.known.contains_key(&t.video_id) {
                        self.known
                            .insert(t.video_id.clone(), crate::edits::applied(t));
                    }
                }
                self.player.repeat = repeat_of(repeat);
                self.player.restore(ids, 0);
                self.queue_changed();
                self.show_now_playing(self.player.engine.position().is_some());
            }
            Down::Members(names) => {
                l.members = names;
                self.show_together();
            }
            Down::Prepare { seq, id, ms } => {
                l.prepared = Some(seq);
                l.start = None;
                l.next = None;
                self.player.engine.cue(&id, ms, None);
            }
            Down::Start { seq, at } => {
                if l.prepared == Some(seq) {
                    l.start = Some((seq, at));
                    self.listener_tick();
                }
            }
            Down::Next { id, at } => match at {
                Some(at) => {
                    l.next = Some((id, at));
                    self.listener_tick();
                }
                None => self.player.engine.set_next(Some(&id)),
            },
            Down::Pause => self.player.engine.pause(),
            Down::Stop => {
                l.prepared = None;
                self.player.engine.stop();
            }
            Down::Title { id, title } => self.station_title(&id, &title),
            Down::RadioStart {
                epoch,
                id,
                content_type,
                kbps,
            } => {
                if let Some((_, old)) = l.relay.take() {
                    old.finish();
                }
                let relay = relay::expect_relay(&id);
                relay.start(&content_type, kbps);
                l.relay = Some((epoch, relay));
            }
            Down::RadioEnd { epoch } => {
                if let Some((e, relay)) = &l.relay
                    && *e == epoch
                {
                    relay.finish();
                }
            }
        }
    }

    /// Applies starts and next-song times once the clock can place them, and reports a start
    /// that was heard late.
    fn listener_tick(&mut self) {
        let Some(Session::Listening(l)) = &mut self.together else {
            return;
        };
        let local = |l: &Listening, at: u64| l.link.host_to_local_us(at).map(together::to_instant);
        if let Some((_, at)) = l.start
            && let Some(t) = local(l, at)
        {
            l.start = None;
            l.prepared = None;
            if t < Instant::now() {
                log::warn!(
                    "together: start came {} ms late",
                    (Instant::now() - t).as_millis()
                );
            }
            l.started = Some(t.max(Instant::now()));
            self.player.engine.start_at(t);
        }
        if let Some((id, at)) = l.next.clone()
            && let Some(t) = local(l, at)
        {
            l.next = None;
            self.player.engine.set_next_at(&id, t);
        }
        // How late the start was heard is known once it was: tell the host.
        if let Some(t) = l.started
            && Instant::now() >= t + Duration::from_secs(1)
        {
            l.started = None;
            let late = self.player.engine.start_late();
            if late > LATE {
                log::info!("together: start heard {} ms late", late.as_millis());
                l.link.send(&Up::Report {
                    offset_us: late.as_micros() as u64,
                });
            }
        }
    }

    /// The engine's synced-start events.
    pub(crate) fn together_engine(&mut self, e: &Event) {
        match (e, &mut self.together) {
            (Event::Cueing { id, ms }, Some(Session::Hosting(_))) => {
                let (id, ms) = (id.clone(), *ms);
                self.host_cueing(&id, ms);
            }
            (Event::Cued { id }, Some(Session::Hosting(h))) => {
                if let Some(b) = h.barrier.as_mut().filter(|b| b.id == *id) {
                    b.ready = true;
                    self.try_start();
                }
            }
            (Event::Cued { .. }, Some(Session::Listening(l))) => {
                if let Some(seq) = l.prepared {
                    l.link.send(&Up::Ready { seq });
                }
            }
            // The host's own start failed: nobody waits for it any more (the player has moved
            // on, and that song's start makes a new one).
            (Event::Failed { id, .. }, Some(Session::Hosting(h)))
                if h.barrier.as_ref().is_some_and(|b| b.id == *id) =>
            {
                log::info!("together: {id} failed to start, skipping it");
                h.barrier = None;
                h.joins.clear();
                h.link.broadcast(&Down::Stop);
            }
            (Event::Finished, Some(Session::Hosting(h))) => h.link.broadcast(&Down::Stop),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listener_asking_too_fast_is_held_back() {
        let t0 = Instant::now();
        let mut a = Allowance::new(t0);
        assert!((0..REQUEST_BURST as usize).all(|_| a.take(t0)));
        assert!(!a.take(t0), "the burst is spent");
        assert!(
            a.take(t0 + Duration::from_millis(500)),
            "one more after half a second"
        );
        assert!(!a.take(t0 + Duration::from_millis(500)));
        // A long quiet spell refills only up to the burst.
        let later = t0 + Duration::from_secs(3600);
        assert!((0..REQUEST_BURST as usize).all(|_| a.take(later)));
        assert!(!a.take(later));
    }

    #[test]
    fn stations_new_here_get_a_name_meanwhile() {
        assert_eq!(placeholder("radio:hcx4T119".into()).title, SOME_STATION);
        assert_eq!(placeholder("dQw4w9WgXcQ".into()).title, "");
    }

    #[test]
    fn a_slow_listener_delays_a_start_by_at_most_two_seconds() {
        assert_eq!(start_margin(None), START_MARGIN);
        assert_eq!(
            start_margin(Some(80_000)),
            START_MARGIN + Duration::from_millis(80)
        );
        assert_eq!(start_margin(Some(u64::MAX)), START_MARGIN + MAX_RTT_MARGIN);
    }

    #[test]
    fn queue_edits_find_the_song_meant() {
        let order = ["a", "b", "c", "d"];
        // Window from 1: position 1 in it is "c".
        assert_eq!(place(&order, 1, 7, 1, "c", 7), Some(2));
        assert_eq!(place(&order, 1, 7, 1, "c", 6), None, "older queue");
        assert_eq!(place(&order, 1, 7, 1, "d", 7), None, "another song there");
        assert_eq!(place(&order, 1, 7, 9, "c", 7), None, "past the end");
        assert_eq!(place(&order, usize::MAX, 7, 1, "c", 7), None, "overflow");
        assert_eq!(place(&order, 1, 7, usize::MAX, "c", 7), None, "overflow");
    }

    #[test]
    fn song_ids_listeners_may_name() {
        assert!(playable_id("dQw4w9WgXcQ"));
        assert!(playable_id("radio:hcx4T119"));
        assert!(!playable_id(""));
        assert!(!playable_id("dQw4w9WgXc"));
        assert!(!playable_id("../../etc/pw"));
        assert!(!playable_id("radio:../x"));
    }
}
