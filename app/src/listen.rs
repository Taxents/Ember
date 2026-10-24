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
//!
//! [`Player::follower`]: music_core::player::Player::follower

use crate::{App, Ui, with};
use music_core::audio::Event;
use music_core::player::Repeat;
use music_core::radio::{self, relay};
use music_core::yt::model::Track;
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, ModelRc, SharedString, TimerMode, VecModel};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use together::{Guest, GuestEvent, Host, HostEvent, PeerId};

/// Silence between songs: room for the devices' clocks to have drifted apart over a song.
const GAP: Duration = Duration::from_millis(40);
/// Someone joining prepares the song this far ahead of the host.
const JOIN_LEAD_MS: f64 = 4000.0;
/// The least time between saying when to start and starting.
const START_MARGIN: Duration = Duration::from_millis(250);
/// How long a start waits for everyone to be ready; whoever isn't joins it afterwards.
const READY_WAIT: Duration = Duration::from_secs(8);
/// Songs of the queue sent to listeners, from the current one.
const QUEUE_WINDOW: usize = 200;

/// Host to listener.
#[derive(Serialize, Deserialize)]
pub enum Down {
    /// The queue from the current song on, who added each (empty: the host), and its modes.
    Queue {
        tracks: Vec<Track>,
        by: Vec<String>,
        shuffle: bool,
        repeat: u8,
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
    Ready {
        seq: u64,
    },
    /// Start me again in step.
    Resync,
    Request(Req),
}

/// What a listener clicked, for the host to do. Positions are in the queue as sent.
#[derive(Serialize, Deserialize)]
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

struct Member {
    peer: PeerId,
    name: String,
    /// Its clock is in step: it takes part in starts.
    synced: bool,
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
}

pub struct Listening {
    link: Guest<Up, Down>,
    connected: bool,
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

/// Delivers a session's network events to the UI thread until it ends.
fn forward<E: Send + 'static>(rx: crossbeam_channel::Receiver<E>, f: fn(&mut App, E)) {
    std::thread::Builder::new()
        .name("together-events".into())
        .spawn(move || {
            for e in rx {
                let _ = slint::invoke_from_event_loop(move || {
                    with(|a| f(a, e));
                });
            }
        })
        .expect("spawn");
}

impl App {
    pub(crate) fn hosting(&self) -> bool {
        matches!(self.together, Some(Session::Hosting(_)))
    }

    pub(crate) fn listening(&self) -> bool {
        matches!(self.together, Some(Session::Listening(_)))
    }

    /// Listening along: sends what was clicked to the host (true) instead of doing it here.
    pub(crate) fn ask_host(&mut self, r: Req) -> bool {
        let Some(Session::Listening(l)) = &self.together else {
            return false;
        };
        l.link.send(&Up::Request(r));
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
        forward(link.events().clone(), App::host_event);
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
            link,
            members: Vec::new(),
            seq: 0,
            barrier: None,
            joins: HashMap::new(),
            next_sent: None,
            by: HashMap::new(),
            window_from: 0,
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
        forward(link.events().clone(), App::guest_event);
        self.together = Some(Session::Listening(Box::new(Listening {
            link,
            connected: false,
            host: String::new(),
            members: Vec::new(),
            prepared: None,
            start: None,
            next: None,
            by: Vec::new(),
            relay: None,
            clock: false,
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
        match s {
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
                log::warn!("together: {e}");
                self.together_leave("The session stopped working");
            }
            HostEvent::Msg { peer, msg } => match msg {
                Up::Synced => {
                    if let Some(m) = h.members.iter_mut().find(|m| m.peer == peer) {
                        m.synced = true;
                    }
                    self.bring_in(peer);
                }
                Up::Resync => self.bring_in(peer),
                Up::Ready { seq } => {
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
                Up::Request(r) => {
                    let name = h
                        .members
                        .iter()
                        .find(|m| m.peer == peer)
                        .map(|m| m.name.clone())
                        .unwrap_or_default();
                    self.do_request(r, &name);
                }
            },
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
        let margin = START_MARGIN + Duration::from_micros(h.link.rtt_us(peer).unwrap_or(0));
        if at < Instant::now() + margin {
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
        let rtt = h
            .members
            .iter()
            .filter_map(|m| h.link.rtt_us(m.peer))
            .max()
            .unwrap_or(0);
        let at = Instant::now() + START_MARGIN + Duration::from_micros(rtt);
        let b = h.barrier.take().unwrap();
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
        };
        match to {
            Some(p) => h.link.send(p, &msg),
            None => h.link.broadcast(&msg),
        }
    }

    /// The queue changed here.
    pub(crate) fn together_queue_changed(&mut self) {
        if self.hosting() {
            self.send_queue(None);
            if let Some(Session::Hosting(h)) = &mut self.together {
                h.next_sent = None;
            }
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
    fn do_request(&mut self, r: Req, name: &str) {
        let from = match &self.together {
            Some(Session::Hosting(h)) => h.window_from,
            _ => return,
        };
        let tag = |a: &mut App, tracks: &[Track]| {
            if let Some(Session::Hosting(h)) = &mut a.together {
                for t in tracks {
                    h.by.insert(t.video_id.clone(), name.to_string());
                }
            }
            for t in tracks {
                a.known.insert(t.video_id.clone(), t.clone());
            }
        };
        match r {
            Req::Toggle => self.play_pause(),
            Req::Next => self.skip(1),
            Req::Previous => self.skip(-1),
            Req::Seek(ms) => self.player.engine.seek(ms),
            Req::Jump(pos) => self.player.jump(from + pos),
            Req::Play {
                tracks,
                start,
                shuffle,
            } => {
                tag(self, &tracks);
                self.play_list(tracks, start, shuffle);
            }
            Req::Add { tracks, next } => {
                tag(self, &tracks);
                let ids = tracks.into_iter().map(|t| t.video_id).collect();
                if next {
                    self.player.insert_next(ids);
                } else {
                    self.player.append(ids);
                }
                self.queue_changed();
            }
            Req::Remove(pos) => {
                self.player.remove(from + pos);
                self.queue_changed();
            }
            Req::Move(a, b) => {
                self.player.move_item(from + a, from + b);
                self.queue_changed();
            }
            Req::Shuffle(on) => {
                self.player.set_shuffle(on);
                self.queue_changed();
            }
            Req::Repeat(r) => {
                self.player.set_repeat(repeat_of(r));
                self.queue_changed();
            }
        }
    }

    // ---- listening --------------------------------------------------------------------------

    fn guest_event(&mut self, e: GuestEvent<Down>) {
        let Some(Session::Listening(l)) = &mut self.together else {
            return;
        };
        match e {
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
            } => {
                l.by = by;
                let ids: Vec<String> = tracks.iter().map(|t| t.video_id.clone()).collect();
                for t in tracks {
                    self.known.insert(t.video_id.clone(), t);
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

    /// Applies starts and next-song times once the clock can place them.
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
            self.player.engine.start_at(t);
        }
        if let Some((id, at)) = l.next.clone()
            && let Some(t) = local(l, at)
        {
            l.next = None;
            self.player.engine.set_next_at(&id, t);
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
            (Event::Finished, Some(Session::Hosting(h))) => h.link.broadcast(&Down::Stop),
            _ => {}
        }
    }
}
