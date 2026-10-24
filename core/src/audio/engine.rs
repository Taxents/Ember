//! The playback engine: one decoder thread feeding one ring, one output stream.
//!
//! Gapless: the next track is opened ~30 s before the current one ends, and its first sample
//! is written straight after the current one's last into the same ring. A timeline of marks
//! (ring frame → track) turns the callback's frame counter into "which track, what position"
//! without the callback knowing anything about tracks.
//!
//! Synced mode (Listen Along): every start waits at a gate in the ring (see `output`) until an
//! instant shared by everyone listening. A start nobody has timed yet is held, announced with
//! [`Event::Cueing`] and, once enough is decoded, [`Event::Cued`]; [`Engine::start_at`] then
//! says when. Nothing is corrected while a song plays.

use super::clock;
use super::output::{self, Format, HOLD, Output, Shared, SharedConsumer};
use super::resample::Resampler;
use super::source::{self, Source};
use super::stream::{Cancel, HttpStream};
use crate::audio::eq::EqSettings;
use crate::yt::Resolver;
use crate::{Error, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use rtrb::{Producer, RingBuffer};
use std::any::Any;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const CH: usize = 2;
/// Ring length.
const RING_SECONDS: f64 = 1.0;
/// Start decoding the next track this long before the current one ends.
const PRELOAD_BEFORE: Duration = Duration::from_secs(30);
/// Stop the device stream after this long paused or idle.
const IDLE_STOP: Duration = Duration::from_secs(5);
/// Wake-up bounds while playing (the ring holds a second, so waking at half is plenty).
const TICK_MIN: Duration = Duration::from_millis(20);
const TICK_MAX: Duration = Duration::from_millis(250);
/// After a device error, wait this long before reopening: Windows reports a switch as a burst
/// (old endpoint invalidated, new default, …) and a fresh endpoint (Bluetooth especially) may
/// refuse a client for a moment.
const REOPEN_SETTLE: Duration = Duration::from_millis(150);
/// Retry a failed reopen after this long, times the attempts so far (capped).
const REOPEN_RETRY: Duration = Duration::from_millis(500);
const REOPEN_RETRY_MAX: Duration = Duration::from_secs(5);
/// The rate of an output without a device (tests).
const NULL_RATE: u32 = 48000;

/// Opens a track: resolve, connect, parse headers ([`load`]; a fake one in tests). The token
/// stops its download.
type Loader = Arc<dyn Fn(&str, &Cancel, Sender<Event>) -> Result<Loaded> + Send + Sync>;
/// How long dropping the engine waits for the decoder thread.
const QUIT_WAIT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub enum Event {
    /// A track became audible (also after a gapless hand-over).
    Started {
        id: String,
        duration_ms: Option<u64>,
    },
    /// The last track ended and nothing follows.
    Finished,
    /// A track couldn't be played. `handed_over`: it broke off while playing and the next
    /// track takes over by itself (a `Started` for it follows); nothing needs to be done.
    Failed {
        id: String,
        error: Error,
        handed_over: bool,
    },
    /// The output device changed or failed.
    Device(String),
    /// The output device can't play anything we can render (only DSD, say). Said once; the next
    /// play tries again.
    DeviceUnsupported(String),
    /// A bug in the engine: what was playing is dropped, and the engine carries on (the next
    /// `play` works as usual).
    EngineError(String),
    /// A live station said what it plays now (its ICY `StreamTitle`; empty: nothing said).
    Title { id: String, title: String },
    /// Synced mode: a start from `ms` into `id` is being prepared and will wait for a time.
    Cueing { id: String, ms: u64 },
    /// Synced mode: that start has audio ready and waits for [`Engine::start_at`].
    Cued { id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub id: String,
    pub ms: u64,
    pub duration_ms: Option<u64>,
    pub paused: bool,
}

/// Exactly where playback is: `ms` into `id` is heard at `at`.
#[derive(Debug, Clone)]
pub struct Timing {
    pub id: String,
    pub ms: f64,
    pub at: Instant,
    pub duration_ms: Option<u64>,
    pub paused: bool,
    /// Waiting at a synced start (`ms` is where it will start, `at` means nothing yet).
    pub held: bool,
}

impl Timing {
    /// What is heard at `t` (playing on from `at`; a held or paused one stays put).
    pub fn ms_at(&self, t: Instant) -> f64 {
        if self.held || self.paused {
            return self.ms;
        }
        if t >= self.at {
            self.ms + (t - self.at).as_secs_f64() * 1000.0
        } else {
            self.ms - (self.at - t).as_secs_f64() * 1000.0
        }
    }

    /// When `ms` into the track is heard.
    pub fn instant_of(&self, ms: f64) -> Instant {
        let d = Duration::from_secs_f64((ms - self.ms).abs() / 1000.0);
        if ms >= self.ms {
            self.at + d
        } else {
            self.at - d
        }
    }
}

enum Cmd {
    Play(String),
    Cue {
        id: String,
        ms: u64,
        when: Option<Instant>,
    },
    StartAt(Instant),
    NextAt {
        id: String,
        when: Instant,
    },
    Synced(bool),
    SetNext(Option<String>),
    Pause,
    Resume,
    Seek(u64),
    Stop,
    /// A track became audible before the decoder noticed: say so now.
    Announce,
    Loaded {
        serial: u64,
        id: String,
        result: Result<Loaded>,
    },
    /// From the stream opened as output number `output`.
    DeviceError {
        output: u64,
        error: String,
    },
    Quit,
}

pub struct Engine {
    cmd: Sender<Cmd>,
    events: Receiver<Event>,
    shared: Arc<Shared>,
    timeline: Arc<Mutex<Timeline>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Stops every download the engine started.
    cancel: Cancel,
    /// Disconnects when the decoder thread ends.
    done: Receiver<()>,
}

impl Engine {
    pub fn new(resolver: Arc<Resolver>) -> Result<Self> {
        Self::with_loader(
            Arc::new(move |id, cancel, events| load(&resolver, id, cancel, events)),
            false,
        )
    }

    /// An engine opening tracks with `loader`; `null_output`: no device at all.
    fn with_loader(loader: Loader, null_output: bool) -> Result<Self> {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let shared = Shared::new();
        let timeline = Arc::new(Mutex::new(Timeline::default()));
        let cancel = Cancel::new();
        let mut w = Worker::new(
            loader,
            null_output,
            shared.clone(),
            timeline.clone(),
            cmd_tx.clone(),
            ev_tx,
        )?;
        w.cancel = cancel.clone();
        let (done_tx, done) = crossbeam_channel::bounded::<()>(0);
        let thread = std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || {
                let _done = done_tx;
                // A panic drops what was playing, not the engine.
                while let Err(p) = catch_unwind(AssertUnwindSafe(|| w.run(&cmd_rx))) {
                    w.recover(panic_message(&*p));
                }
            })
            .map_err(|e| Error::Device(e.to_string()))?;
        Ok(Engine {
            cmd: cmd_tx,
            events: ev_rx,
            shared,
            timeline,
            thread: Some(thread),
            cancel,
            done,
        })
    }

    /// Play `id` now, replacing whatever is playing.
    pub fn play(&self, id: &str) {
        let _ = self.cmd.send(Cmd::Play(id.into()));
    }
    /// The track to continue with, gaplessly, when the current one ends.
    pub fn set_next(&self, id: Option<&str>) {
        let _ = self.cmd.send(Cmd::SetNext(id.map(Into::into)));
    }
    pub fn pause(&self) {
        let _ = self.cmd.send(Cmd::Pause);
    }
    pub fn resume(&self) {
        let _ = self.cmd.send(Cmd::Resume);
    }
    pub fn seek(&self, ms: u64) {
        let _ = self.cmd.send(Cmd::Seek(ms));
    }
    pub fn stop(&self) {
        let _ = self.cmd.send(Cmd::Stop);
    }
    /// Synced mode: every start waits at a gate (see the module docs).
    pub fn set_synced(&self, on: bool) {
        let _ = self.cmd.send(Cmd::Synced(on));
    }
    /// Plays `id` from `ms`, heard from `when` (`None`: held until [`Engine::start_at`]).
    pub fn cue(&self, id: &str, ms: u64, when: Option<Instant>) {
        let _ = self.cmd.send(Cmd::Cue {
            id: id.into(),
            ms,
            when,
        });
    }
    /// A held start is heard from `when`.
    pub fn start_at(&self, when: Instant) {
        let _ = self.cmd.send(Cmd::StartAt(when));
    }
    /// The next track `id` (see [`Engine::set_next`]) starts at `when`, after silence if the
    /// current one ends sooner.
    pub fn set_next_at(&self, id: &str, when: Instant) {
        self.set_next(Some(id));
        let _ = self.cmd.send(Cmd::NextAt {
            id: id.into(),
            when,
        });
    }
    /// Exactly what is heard when (see [`Timing`]).
    pub fn timing(&self) -> Option<Timing> {
        let (frame, ns) = self.shared.anchor()?;
        let tl = lock(&self.timeline);
        let m = tl.audible(frame)?;
        let held = self.shared.gate() == Some(m.at) && frame == m.at;
        let frames = frame - m.at + m.offset;
        Some(Timing {
            id: m.id.clone(),
            ms: frames as f64 * 1000.0 / tl.rate.max(1) as f64,
            at: clock::instant_of(ns),
            duration_ms: m.duration_ms,
            paused: self.shared.paused.load(Ordering::Relaxed),
            held,
        })
    }
    /// How late the last synced start was heard (0: on time).
    pub fn start_late(&self) -> Duration {
        Duration::from_nanos(self.shared.gate_late.load(Ordering::Relaxed))
    }
    pub fn set_volume(&self, v: f32) {
        self.shared
            .volume
            .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    pub fn set_eq(&self, eq: &EqSettings) {
        self.shared.eq.set(eq);
    }
    pub fn events(&self) -> &Receiver<Event> {
        &self.events
    }
    /// How loud the last moment of audio was in four bands, bass first (RMS, before volume).
    pub fn levels(&self) -> [f32; 4] {
        self.shared
            .levels
            .each_ref()
            .map(|a| f32::from_bits(a.load(Ordering::Relaxed)))
    }

    pub fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    /// What is heard right now (the device's latency taken off what was handed to it).
    pub fn position(&self) -> Option<Position> {
        position(&self.shared, &self.timeline, Some(&self.cmd))
    }

    /// A handle that reads the position from another thread (a video kept in step with the
    /// audio).
    pub fn clock(&self) -> Clock {
        Clock {
            shared: self.shared.clone(),
            timeline: self.timeline.clone(),
            cmd: self.cmd.clone(),
        }
    }
}

/// Reads what is audible, from any thread. Cheap: a lock held for a few loads.
#[derive(Clone)]
pub struct Clock {
    shared: Arc<Shared>,
    timeline: Arc<Mutex<Timeline>>,
    cmd: Sender<Cmd>,
}

impl Clock {
    pub fn position(&self) -> Option<Position> {
        position(&self.shared, &self.timeline, Some(&self.cmd))
    }
}

/// The ring frame heard now: what was taken from the ring, less what the device still holds
/// (from the callback's anchor). Paused, what was taken is all heard by now.
fn heard(shared: &Shared, rate: u32) -> u64 {
    let played = shared.played.load(Ordering::Relaxed);
    if shared.paused.load(Ordering::Relaxed) {
        return played;
    }
    let Some((frame, ns)) = shared.anchor() else {
        return played;
    };
    let ahead = ns.saturating_sub(clock::now_ns()) as u128 * rate as u128 / 1_000_000_000;
    frame.saturating_sub(ahead as u64).min(played)
}

/// What is heard now. `cmd`: wake the decoder when that is a track it hasn't announced yet.
fn position(
    shared: &Shared,
    timeline: &Mutex<Timeline>,
    cmd: Option<&Sender<Cmd>>,
) -> Option<Position> {
    let mut tl = lock(timeline);
    let heard = heard(shared, tl.rate);
    if let Some(cmd) = cmd
        && !tl.nudged
        && tl.marks.iter().any(|m| !m.announced && m.at <= heard)
    {
        tl.nudged = true;
        let _ = cmd.send(Cmd::Announce);
    }
    // Before the first mark (just after a seek or start): the place it starts from.
    let m = tl.audible(heard).or(tl.marks.front())?;
    let frames = heard.saturating_sub(m.at) + m.offset;
    Some(Position {
        id: m.id.clone(),
        ms: frames * 1000 / tl.rate.max(1) as u64,
        duration_ms: m.duration_ms,
        paused: shared.paused.load(Ordering::Relaxed),
    })
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Downloads first, so nothing the decoder could be waiting on keeps it.
        self.cancel.cancel();
        let _ = self.cmd.send(Cmd::Quit);
        let ended = matches!(
            self.done.recv_timeout(QUIT_WAIT),
            Err(RecvTimeoutError::Disconnected)
        );
        match self.thread.take() {
            Some(t) if ended => {
                let _ = t.join();
            }
            // Still busy: leave it to end by itself rather than hang the caller.
            Some(_) => log::warn!("decoder thread did not stop in time"),
            None => {}
        }
    }
}

// ---- timeline --------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Mark {
    /// Ring frame index where this track (or seek position) begins.
    at: u64,
    id: String,
    /// Device frames into the track at `at`.
    offset: u64,
    duration_ms: Option<u64>,
    announced: bool,
}

#[derive(Default)]
struct Timeline {
    marks: VecDeque<Mark>,
    rate: u32,
    /// The decoder was asked to announce (see [`Cmd::Announce`]); cleared when it has.
    nudged: bool,
}

impl Timeline {
    fn audible(&self, played: u64) -> Option<&Mark> {
        self.marks.iter().rev().find(|m| m.at <= played)
    }
}

// ---- worker ----------------------------------------------------------------------------------

/// A track opened off-thread, before the device rate is applied.
struct Loaded {
    /// The length YouTube reported (live radio: none).
    duration_ms: Option<u64>,
    src: Box<dyn Source>,
}

struct Track {
    id: String,
    src: Box<dyn Source>,
    rs: Option<Resampler>,
    /// Source frames decoded so far (from the last seek point).
    decoded: u64,
    eof: bool,
    duration_ms: Option<u64>,
}

impl Track {
    fn new(id: String, l: Loaded, dev_rate: u32) -> Result<Self> {
        let rate = l.src.rate();
        // A length of zero says nothing.
        let duration_ms = l
            .src
            .total_frames()
            .map(|f| f.saturating_mul(1000) / rate.max(1) as u64)
            .filter(|&ms| ms > 0)
            .or(l.duration_ms.filter(|&ms| ms > 0));
        Ok(Track {
            id,
            rs: (rate != dev_rate)
                .then(|| Resampler::new(rate, dev_rate))
                .transpose()?,
            src: l.src,
            decoded: 0,
            eof: false,
            duration_ms,
        })
    }

    /// Reposition to `ms`; returns the exact position used, in device frames.
    fn seek(&mut self, ms: u64, dev_rate: u32) -> Result<u64> {
        let rate = self.src.rate() as u64;
        if self.src.live() {
            // Live radio carries on from where the stream is (after a device change).
            if self.rs.is_some() {
                self.rs = Some(Resampler::new(rate as u32, dev_rate)?);
            }
            return Ok(0);
        }
        let mut frame = ms.saturating_mul(rate) / 1000;
        if let Some(t) = self.src.total_frames() {
            frame = frame.min(t.saturating_sub(1));
        }
        self.src.seek(frame)?;
        self.decoded = frame;
        self.eof = false;
        if self.rs.is_some() {
            self.rs = Some(Resampler::new(rate as u32, dev_rate)?);
        }
        Ok(frame.saturating_mul(dev_rate as u64) / rate)
    }

    fn remaining(&self) -> Option<Duration> {
        let total = self.src.total_frames()?;
        let left = total.saturating_sub(self.decoded);
        Some(Duration::from_millis(
            left.saturating_mul(1000) / self.src.rate().max(1) as u64,
        ))
    }
}

struct Worker {
    loader: Loader,
    /// Parent of every load's token.
    cancel: Cancel,
    /// No device: the ring is read by whoever drives the worker (tests).
    null_output: bool,
    shared: Arc<Shared>,
    timeline: Arc<Mutex<Timeline>>,
    cmd_tx: Sender<Cmd>,
    events: Sender<Event>,
    output: Option<Output>,
    dev_rate: u32,
    producer: Producer<f32>,
    consumer: SharedConsumer,
    /// Frames pushed into the ring so far.
    written: u64,
    /// Decoded audio waiting for ring space.
    carry: Vec<f32>,
    pcm: Vec<f32>,
    /// Being decoded.
    cur: Option<Track>,
    /// Fully decoded but possibly still audible (kept so a seek can go back to it).
    prev: Option<Track>,
    next: Option<Track>,
    next_id: Option<String>,
    /// `next_id` is being opened: that load's serial and the token that stops it.
    next_load: Option<(u64, Cancel)>,
    /// A track asked for with `Play` is being opened.
    play_load: Option<(u64, Cancel)>,
    /// Numbers loads: a result that is no longer wanted matches neither of the above.
    load_serial: u64,
    /// Outputs opened so far; the current one is number `outputs`.
    outputs: u64,
    /// The device takes no format we render: why. Reopening waits for a play until it's gone.
    unsupported: Option<String>,
    finished_sent: bool,
    idle_since: Option<Instant>,
    /// The output must be (re)opened at this time (device changed or lost, or none yet).
    reopen_at: Option<Instant>,
    reopen_failures: u32,
    /// Synced mode (see the module docs).
    synced: bool,
    /// The next start's position and time, from `Cue`.
    pending: Option<(u64, Option<Instant>)>,
    /// When the next track starts, once said.
    next_when: Option<(String, Instant)>,
    /// The track waiting at the gate; whether its opening was logged.
    gate_id: Option<String>,
    gate_logged: bool,
    /// A held start that hasn't said `Cued` yet.
    cue_wait: Option<String>,
}

impl Worker {
    fn new(
        loader: Loader,
        null_output: bool,
        shared: Arc<Shared>,
        timeline: Arc<Mutex<Timeline>>,
        cmd_tx: Sender<Cmd>,
        events: Sender<Event>,
    ) -> Result<Self> {
        let (p, c) = RingBuffer::new(2);
        let mut w = Worker {
            loader,
            cancel: Cancel::new(),
            null_output,
            shared,
            timeline,
            cmd_tx,
            events,
            output: None,
            dev_rate: 48000,
            producer: p,
            consumer: Arc::new(Mutex::new(c)),
            written: 0,
            carry: Vec::new(),
            pcm: Vec::with_capacity(16384),
            cur: None,
            prev: None,
            next: None,
            next_id: None,
            next_load: None,
            play_load: None,
            load_serial: 0,
            outputs: 0,
            unsupported: None,
            finished_sent: true,
            idle_since: Some(Instant::now()),
            reopen_at: None,
            reopen_failures: 0,
            synced: false,
            pending: None,
            next_when: None,
            gate_id: None,
            gate_logged: true,
            cue_wait: None,
        };
        // No device yet (none plugged in, or busy): keep trying, playback starts once one is.
        if let Err(e) = w.open_output() {
            log::warn!("no output device yet: {e}");
            w.reopen_failed();
        }
        Ok(w)
    }

    /// Opens a stopped stream on the current default device. Returns whether the ring had to be
    /// rebuilt (the device rate changed), dropping what was queued in it. On failure only the
    /// old stream is gone: ring, timeline and position stay as they were.
    fn open_output(&mut self) -> Result<bool> {
        self.output = None;
        self.unsupported = None;
        let dev = match self.null_output {
            true => None,
            false => match output::device_format()? {
                Format::Usable(dev, cfg) => Some((dev, cfg)),
                Format::Unsupported(why) => {
                    self.unsupported = Some(why.clone());
                    return Err(Error::Device(why));
                }
            },
        };
        let rate = dev.as_ref().map_or(NULL_RATE, |(_, cfg)| cfg.sample_rate());
        let cap = ((rate as f64 * RING_SECONDS) as usize) * CH;
        // Same rate: the queued audio is still right for the new device, so keep it (seamless).
        let keep = rate == self.dev_rate && self.producer.buffer().capacity() == cap;
        let (producer, consumer) = if keep {
            (None, self.consumer.clone())
        } else {
            let (p, c) = RingBuffer::new(cap);
            (Some(p), Arc::new(Mutex::new(c)))
        };
        let tx = self.cmd_tx.clone();
        self.outputs += 1;
        let output = self.outputs;
        let out = match &dev {
            Some((dev, cfg)) => {
                Output::open(dev, cfg, self.shared.clone(), consumer.clone(), move |e| {
                    let _ = tx.send(Cmd::DeviceError {
                        output,
                        error: e.to_string(),
                    });
                })?
            }
            None => Output::null(rate),
        };
        if let Some(p) = producer {
            self.producer = p;
            self.consumer = consumer;
            // Everything in flight is gone with the old ring.
            self.shared.played.store(self.written, Ordering::Relaxed);
            self.carry.clear();
            self.dev_rate = rate;
            lock(&self.timeline).rate = rate;
            self.shared.reset_eq.store(true, Ordering::Relaxed);
        }
        log::info!(
            "output: {} Hz, {} ch, format {}{}",
            rate,
            out.channels,
            dev.map_or("none".into(), |(_, cfg)| cfg.sample_format().to_string()),
            if keep { "" } else { " (ring rebuilt)" }
        );
        self.output = Some(out);
        Ok(!keep)
    }

    fn run(&mut self, rx: &Receiver<Cmd>) {
        loop {
            let got = match self.wait_time() {
                Some(t) => rx.recv_timeout(t),
                // Nothing to do until told: no wake-ups at all while paused or idle.
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match got {
                Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(c) => {
                    if !self.handle(c) {
                        return;
                    }
                    while let Ok(c) = rx.try_recv() {
                        if !self.handle(c) {
                            return;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            if self.reopen_at.is_some_and(|t| t <= Instant::now()) {
                self.reopen();
            }
            self.pump();
            self.announce();
            self.manage_output();
        }
    }

    fn handle(&mut self, c: Cmd) -> bool {
        match c {
            Cmd::Quit => return false,
            Cmd::Play(id) => {
                self.pending = None;
                self.retry_unsupported();
                self.play(id);
            }
            Cmd::Cue { id, ms, when } => {
                self.retry_unsupported();
                self.shared.paused.store(false, Ordering::Relaxed);
                self.pending = Some((ms, when));
                self.play(id);
            }
            Cmd::StartAt(when) => {
                if self.shared.gate().is_some() {
                    self.shared.open_gate(clock::ns_of(when));
                    self.cue_wait = None;
                }
            }
            Cmd::NextAt { id, when } => {
                if self.gate_id.as_deref() == Some(id.as_str())
                    && self.shared.gate().is_some()
                    && self.shared.gate_when.load(Ordering::SeqCst) == HOLD
                {
                    // Already waiting at its start.
                    self.shared.open_gate(clock::ns_of(when));
                    self.cue_wait = None;
                } else {
                    self.next_when = Some((id, when));
                }
            }
            Cmd::Synced(on) => {
                self.synced = on;
                if !on && self.shared.gate().is_some() {
                    self.shared.open_gate(0);
                }
                self.next_when = None;
            }
            Cmd::SetNext(id) => self.set_next(id),
            Cmd::Pause => {
                self.shared.paused.store(true, Ordering::Relaxed);
            }
            Cmd::Resume => {
                self.shared.paused.store(false, Ordering::Relaxed);
                if let Some(o) = &mut self.output {
                    o.start();
                }
            }
            Cmd::Seek(ms) => self.seek(ms),
            // The loop announces after every command.
            Cmd::Announce => {}
            Cmd::Stop => self.stop(),
            Cmd::Loaded { serial, id, result } => self.loaded(serial, id, result),
            Cmd::DeviceError { output, error } => self.device_error(output, error),
        }
        true
    }

    fn stop(&mut self) {
        self.flush();
        self.cur = None;
        self.prev = None;
        self.next = None;
        self.next_id = None;
        drop_load(&mut self.next_load);
        drop_load(&mut self.play_load);
        self.pending = None;
        self.finished_sent = true;
    }

    /// After a panic in [`Worker::run`]: what was playing may be half-changed, so it goes; the
    /// worker then carries on as after a stop.
    fn recover(&mut self, msg: String) {
        log::error!("decoder panicked: {msg}");
        self.stop();
        let _ = self.events.send(Event::EngineError(msg));
    }

    fn play(&mut self, id: String) {
        self.flush();
        self.prev = None;
        self.finished_sent = false;
        // A track still opening for an earlier play isn't wanted any more: stop its download.
        drop_load(&mut self.play_load);
        let opening_next = self.next_id.take().filter(|n| *n == id).is_some();
        // Skipping to the preloaded track starts instantly.
        if let Some(t) = self.next.take().filter(|t| t.id == id) {
            drop_load(&mut self.next_load);
            self.cur = None;
            self.begin(t);
            return;
        }
        self.cur = None;
        self.next = None;
        if opening_next && self.next_load.is_some() {
            // Already being opened ahead: that load becomes this one.
            self.play_load = self.next_load.take();
        } else {
            drop_load(&mut self.next_load);
            self.spawn_load(id, false);
        }
    }

    fn set_next(&mut self, id: Option<String>) {
        let Some(id) = id else {
            // Nothing follows: whatever was opened or being opened ahead goes.
            self.next = None;
            self.next_id = None;
            drop_load(&mut self.next_load);
            self.next_when = None;
            return;
        };
        if self.next.as_ref().is_some_and(|t| t.id == id) || self.next_id.as_ref() == Some(&id) {
            return;
        }
        self.next = None;
        drop_load(&mut self.next_load);
        if self.next_when.as_ref().is_some_and(|(i, _)| *i != id) {
            self.next_when = None;
        }
        self.next_id = Some(id);
    }

    fn seek(&mut self, ms: u64) {
        if self.play_load.is_some() {
            // Still opening: it starts there instead.
            let when = self.pending.and_then(|(_, w)| w);
            self.pending = Some((ms, when));
            return;
        }
        let played = self.shared.played.load(Ordering::Relaxed);
        let audible = lock(&self.timeline).audible(played).map(|m| m.id.clone());
        // Live radio can't be sought: keep playing.
        if let (Some(a), Some(t)) = (&audible, &self.cur)
            && *a == t.id
            && t.src.live()
        {
            return;
        }
        self.seek_audible(audible, ms);
    }

    fn seek_audible(&mut self, audible: Option<String>, ms: u64) {
        // The audible track may be the previous one if the decoder already moved on.
        if audible.is_some() && self.prev.as_ref().map(|t| &t.id) == audible.as_ref() {
            let mut back = self.cur.take();
            if let Some(t) = &mut back {
                let _ = t.seek(0, self.dev_rate);
            }
            if back.is_some() {
                self.next = back;
            }
            self.cur = self.prev.take();
        }
        let Some(mut t) = self.cur.take() else { return };
        self.flush();
        match t.seek(ms, self.dev_rate) {
            Ok(offset) => {
                let id = t.id.clone();
                let at = self.start(t, offset, false);
                if self.synced {
                    self.gate(at, id, ms, None);
                }
            }
            Err(e) => self.fail(t.id, e),
        }
    }

    /// Starts a track asked for with `Play` or `Cue`: from where `Cue` said, and in synced mode
    /// at the gate.
    fn begin(&mut self, mut t: Track) {
        let (ms, when) = self.pending.take().unwrap_or((0, None));
        let mut offset = 0;
        if ms > 0 && !t.src.live() {
            match t.seek(ms, self.dev_rate) {
                Ok(o) => offset = o,
                Err(e) => return self.fail(t.id, e),
            }
        }
        let id = t.id.clone();
        let at = self.start(t, offset, true);
        if self.synced {
            self.gate(at, id, ms, when);
        }
    }

    /// Ring frame `at` (the start of `id`, `ms` in) waits until `when` (`None`: held).
    fn gate(&mut self, at: u64, id: String, ms: u64, when: Option<Instant>) {
        self.shared.set_gate(at, when.map_or(HOLD, clock::ns_of));
        self.gate_id = Some(id.clone());
        self.gate_logged = false;
        if when.is_none() {
            self.cue_wait = Some(id.clone());
            let _ = self.events.send(Event::Cueing { id, ms });
        } else {
            self.cue_wait = None;
        }
    }

    /// Makes `t` the decoding track, audible from the current end of the ring (that frame).
    fn start(&mut self, t: Track, offset: u64, announce: bool) -> u64 {
        let at = self.written + (self.carry.len() / CH) as u64;
        lock(&self.timeline).marks.push_back(Mark {
            at,
            id: t.id.clone(),
            offset,
            duration_ms: t.duration_ms,
            announced: !announce,
        });
        self.cur = Some(t);
        self.finished_sent = false;
        self.shared.expecting.store(true, Ordering::Relaxed);
        if let Some(o) = &mut self.output {
            o.start();
        }
        at
    }

    /// Drops everything queued for output.
    fn flush(&mut self) {
        self.shared.flushes.fetch_add(1, Ordering::Relaxed);
        {
            // Marks go before `played` jumps, under the lock readers take: nobody sees the old
            // track run on through the dropped audio.
            let mut tl = lock(&self.timeline);
            tl.marks.clear();
            let mut c = lock(&self.consumer);
            let n = c.slots();
            if let Ok(chunk) = c.read_chunk(n) {
                chunk.commit_all();
            }
            self.shared
                .played
                .fetch_add((n / CH) as u64, Ordering::Relaxed);
        }
        self.carry.clear();
        self.shared.reset_eq.store(true, Ordering::Relaxed);
        self.shared.clear_gate();
        self.gate_id = None;
        self.cue_wait = None;
        self.shared.expecting.store(false, Ordering::Relaxed);
    }

    /// Starts opening `id`, as the next track or as the one to play now.
    fn spawn_load(&mut self, id: String, next: bool) {
        self.load_serial += 1;
        let serial = self.load_serial;
        let tx = self.cmd_tx.clone();
        let loader = self.loader.clone();
        let events = self.events.clone();
        let cancel = self.cancel.child();
        let slot = Some((serial, cancel.clone()));
        if next {
            self.next_load = slot;
        } else {
            self.play_load = slot;
        }
        let load_id = id.clone();
        let spawned = std::thread::Builder::new()
            .name("track-load".into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| loader(&id, &cancel, events)))
                    .unwrap_or_else(|p| {
                        Err(Error::Media(format!(
                            "opening panicked: {}",
                            panic_message(&*p)
                        )))
                    });
                let _ = tx.send(Cmd::Loaded { serial, id, result });
            });
        if let Err(e) = spawned {
            let e = Error::Device(format!("can't start loading: {e}"));
            self.loaded(serial, load_id, Err(e));
        }
    }

    fn loaded(&mut self, serial: u64, id: String, result: Result<Loaded>) {
        let is = |l: &Option<(u64, Cancel)>| l.as_ref().is_some_and(|(s, _)| *s == serial);
        if is(&self.next_load) {
            self.next_load = None;
            self.next_id = None;
            match result.and_then(|l| Track::new(id.clone(), l, self.dev_rate)) {
                Ok(t) => self.next = Some(t),
                Err(e) => self.fail(id, e),
            }
        } else if is(&self.play_load) {
            self.play_load = None;
            match result.and_then(|l| Track::new(id.clone(), l, self.dev_rate)) {
                Ok(t) => self.begin(t),
                Err(e) => {
                    // The queue decides what follows a failed track.
                    self.fail(id, e);
                    self.finished_sent = true;
                }
            }
        }
    }

    fn fail(&mut self, id: String, error: Error) {
        self.report(id, error, false);
    }

    fn report(&mut self, id: String, error: Error, handed_over: bool) {
        log::error!("{id}: {error}");
        let _ = self.events.send(Event::Failed {
            id,
            error,
            handed_over,
        });
    }

    /// How long the loop may sleep; `None` = until the next command.
    fn wait_time(&self) -> Option<Duration> {
        let t = self.decode_wait();
        let Some(at) = self.reopen_at else { return t };
        let r = at.saturating_duration_since(Instant::now());
        Some(t.map_or(r, |t| t.min(r)))
    }

    fn decode_wait(&self) -> Option<Duration> {
        if self.can_decode() {
            return Some(Duration::ZERO);
        }
        let output_on = self.output.as_ref().is_some_and(|o| o.is_running());
        if !output_on {
            return None;
        }
        if self.shared.paused.load(Ordering::Relaxed) || (self.cur.is_none() && self.finished_sent)
        {
            // Only the idle timer is left to run.
            let waited = self.idle_since.map_or(Duration::ZERO, |t| t.elapsed());
            return Some(IDLE_STOP.saturating_sub(waited).max(TICK_MIN));
        }
        // Wake when the ring is down to half.
        let cap = self.producer.buffer().capacity();
        let filled = (cap - self.ring_free()) / CH;
        let above_half = filled.saturating_sub(cap / CH / 2);
        let t = Duration::from_millis(above_half as u64 * 1000 / self.dev_rate.max(1) as u64);
        Some(t.clamp(TICK_MIN, TICK_MAX))
    }

    fn ring_free(&self) -> usize {
        self.producer.slots()
    }

    fn can_decode(&self) -> bool {
        let room = self.ring_free() > self.dev_rate as usize / 4 * CH;
        let work = !self.carry.is_empty()
            || self
                .cur
                .as_ref()
                .is_some_and(|t| !t.eof && !t.src.stalled())
            || (self.cur.as_ref().is_some_and(|t| t.eof) && self.next.is_some());
        room && work
    }

    /// Decodes until the ring is full or there's nothing left to decode.
    fn pump(&mut self) {
        loop {
            if !self.carry.is_empty() {
                let (done, _) = self.producer.push_partial_slice(&self.carry);
                let n = done.len();
                self.carry.drain(..n);
                self.written += (n / CH) as u64;
                if !self.carry.is_empty() {
                    return;
                }
            }
            if self.ring_free() < 4096 * CH {
                return;
            }
            let Some(t) = self.cur.as_mut() else { return };
            if t.eof {
                if let Some(n) = self.next.take() {
                    // Gapless hand-over: the next track starts right after the last sample (in
                    // synced mode at its time, after silence if this one ended sooner).
                    self.prev = self.cur.take();
                    let id = n.id.clone();
                    let when = self
                        .next_when
                        .take()
                        .filter(|(i, _)| *i == id)
                        .map(|(_, w)| w);
                    let at = self.start(n, 0, true);
                    if self.synced {
                        self.gate(at, id, 0, when);
                    }
                    continue;
                }
                self.maybe_preload();
                return;
            }
            // Waiting for the network: back to the commands, decode on the next wake-up.
            if t.src.stalled() {
                return;
            }
            self.pcm.clear();
            let more = match t.src.read(&mut self.pcm) {
                Ok(m) => m,
                Err(Error::WouldBlock) => return,
                Err(e) => {
                    // Ends here: what follows (opened or on its way) carries on gaplessly.
                    let id = t.id.clone();
                    t.eof = true;
                    let handed_over =
                        self.next.is_some() || self.next_load.is_some() || self.next_id.is_some();
                    self.report(id, e, handed_over);
                    continue;
                }
            };
            t.decoded += (self.pcm.len() / CH) as u64;
            let resampled = match &mut t.rs {
                Some(rs) => rs.process(&self.pcm, &mut self.carry).and_then(|()| {
                    if more {
                        Ok(())
                    } else {
                        rs.finish(&mut self.carry)
                    }
                }),
                None => {
                    self.carry.extend_from_slice(&self.pcm);
                    Ok(())
                }
            };
            if let Err(e) = resampled {
                let id = t.id.clone();
                t.eof = true;
                let handed_over =
                    self.next.is_some() || self.next_load.is_some() || self.next_id.is_some();
                self.report(id, e, handed_over);
                continue;
            }
            if !more {
                t.eof = true;
            }
            self.maybe_preload();
        }
    }

    fn maybe_preload(&mut self) {
        if self.next.is_some() || self.next_load.is_some() {
            return;
        }
        let Some(id) = self.next_id.clone() else {
            return;
        };
        let due = match &self.cur {
            // Nothing follows live radio by itself.
            Some(t) if t.src.live() => false,
            Some(t) => t.eof || t.remaining().is_none_or(|r| r <= PRELOAD_BEFORE),
            None => false,
        };
        if due {
            self.spawn_load(id, true);
        }
    }

    /// Emits `Started` for tracks that just became audible; notices the end of playback.
    fn announce(&mut self) {
        let played = self.shared.played.load(Ordering::Relaxed);
        let mut started = Vec::new();
        {
            let mut tl = lock(&self.timeline);
            // Tracks start when they are heard, not when the device takes them.
            let heard = heard(&self.shared, tl.rate);
            tl.nudged = false;
            for m in tl
                .marks
                .iter_mut()
                .filter(|m| !m.announced && m.at <= heard)
            {
                m.announced = true;
                started.push(Event::Started {
                    id: m.id.clone(),
                    duration_ms: m.duration_ms,
                });
            }
            // Keep only the audible mark and those after it.
            while tl.marks.len() > 1 && tl.marks[1].at <= heard {
                tl.marks.pop_front();
            }
            // Once the decoding track is audible, the previous one can't be sought back to.
            if let (Some(cur), Some(front)) = (&self.cur, tl.marks.front())
                && front.id == cur.id
                && front.at <= heard
            {
                self.prev = None;
            }
        }
        for e in started {
            let _ = self.events.send(e);
        }
        if let Some(gate) = self.shared.gate() {
            // A held start is ready once a little is decoded past its gate.
            let ahead = (self.written + (self.carry.len() / CH) as u64).saturating_sub(gate);
            let eof = self.cur.as_ref().is_none_or(|t| t.eof);
            if self.cue_wait.is_some() && (ahead >= self.dev_rate as u64 * 3 / 10 || eof) {
                let id = self.cue_wait.take().unwrap_or_default();
                let _ = self.events.send(Event::Cued { id });
            }
        } else if !self.gate_logged {
            self.gate_logged = true;
            let late = self.shared.gate_late.load(Ordering::Relaxed) / 1_000;
            log::info!(
                "{}: synced start, {} us late",
                self.gate_id.as_deref().unwrap_or("?"),
                late
            );
        }
        let drained = self.carry.is_empty() && played >= self.written;
        let ended = self.cur.as_ref().is_none_or(|t| t.eof) && self.next.is_none();
        let waiting =
            self.next_load.is_some() || self.play_load.is_some() || self.next_id.is_some();
        if drained && ended && !waiting && !self.finished_sent {
            self.finished_sent = true;
            self.cur = None;
            self.prev = None;
            self.shared.expecting.store(false, Ordering::Relaxed);
            // Nothing is audible any more: no position, so play and previous start afresh.
            lock(&self.timeline).marks.clear();
            let _ = self.events.send(Event::Finished);
        } else if drained && ended {
            // Waiting on a late next track: silence here isn't an underrun.
            self.shared.expecting.store(false, Ordering::Relaxed);
        }
    }

    fn manage_output(&mut self) {
        let paused = self.shared.paused.load(Ordering::Relaxed);
        let idle = paused || (self.cur.is_none() && self.finished_sent);
        match (idle, self.idle_since) {
            (true, None) => self.idle_since = Some(Instant::now()),
            (true, Some(t)) if t.elapsed() >= IDLE_STOP => {
                if let Some(o) = &mut self.output
                    && o.is_running()
                {
                    log::debug!("output stopped (idle)");
                    o.stop();
                }
            }
            (false, _) => self.idle_since = None,
            _ => {}
        }
    }

    /// Device lost or changed: reopen on the (new) default device shortly. Errors come in
    /// bursts, so they are coalesced into one reopen; those from a stream already replaced are
    /// old news.
    fn device_error(&mut self, output: u64, e: String) {
        if output != self.outputs {
            log::debug!("output device error from a replaced stream: {e}");
            return;
        }
        log::warn!("output device error: {e}");
        let _ = self.events.send(Event::Device(e));
        if self.reopen_at.is_none() {
            self.reopen_at = Some(Instant::now() + REOPEN_SETTLE);
        }
    }

    fn reopen_failed(&mut self) {
        if let Some(why) = &self.unsupported {
            // Trying again won't help: say so once, and wait for a play (the default device
            // may have changed by then).
            self.reopen_at = None;
            let _ = self.events.send(Event::DeviceUnsupported(why.clone()));
            return;
        }
        self.reopen_failures += 1;
        let wait = (REOPEN_RETRY * self.reopen_failures).min(REOPEN_RETRY_MAX);
        self.reopen_at = Some(Instant::now() + wait);
    }

    /// A play after [`Event::DeviceUnsupported`]: try the (perhaps different) device again.
    fn retry_unsupported(&mut self) {
        if self.unsupported.take().is_some() {
            self.reopen_failures = 0;
            self.reopen_at = Some(Instant::now());
        }
    }

    /// Reopens on the current default device and carries on where we were: same track and
    /// position, paused stays paused. Volume and EQ live in `Shared` and carry over by themselves.
    fn reopen(&mut self) {
        self.reopen_at = None;
        // Stop the old stream first so the position read below is final.
        self.output = None;
        // What the lost device held but never played is played again.
        let resume = position(&self.shared, &self.timeline, None).map(|p| (p.id, p.ms));
        let rebuilt = match self.open_output() {
            Ok(r) => r,
            Err(e) => {
                log::error!(
                    "reopening output failed (attempt {}): {e}",
                    self.reopen_failures + 1
                );
                self.reopen_failed();
                return;
            }
        };
        self.reopen_failures = 0;
        if rebuilt {
            // The ring was rebuilt at the new rate: resamplers must match it, and what was queued
            // is gone, so decode again from the audible position (live radio just carries on).
            for t in [&mut self.cur, &mut self.next, &mut self.prev]
                .into_iter()
                .flatten()
            {
                let rate = t.src.rate();
                match (rate != self.dev_rate)
                    .then(|| Resampler::new(rate, self.dev_rate))
                    .transpose()
                {
                    Ok(rs) => t.rs = rs,
                    Err(e) => {
                        // It can't go on at the new rate: let it end.
                        log::error!("{}: {e}", t.id);
                        t.rs = None;
                        t.eof = true;
                    }
                }
            }
            if let Some((id, ms)) = resume {
                self.seek_audible(Some(id), ms);
            }
        }
        let paused = self.shared.paused.load(Ordering::Relaxed);
        let played = self.shared.played.load(Ordering::Relaxed);
        let busy = self.cur.is_some() || played < self.written || !self.carry.is_empty();
        if let Some(o) = &mut self.output {
            if busy && !paused {
                o.start();
            } else {
                // Paused or idle: `Resume` / `Play` start it.
                o.stop();
            }
        }
    }
}

/// Stops a load that is no longer wanted (its result is then ignored).
fn drop_load(load: &mut Option<(u64, Cancel)>) {
    if let Some((_, cancel)) = load.take() {
        cancel.cancel();
    }
}

/// Locks `m`, also after a panic elsewhere while it was held: everything kept under these locks
/// stays consistent between statements, and the engine recovers (see [`Worker::recover`]).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn panic_message(p: &(dyn Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// A refreshed URL may only continue the same file: another format (or a re-encode) spliced
/// in mid-download would be garbage.
fn same_file(itag: u32, len: Option<u64>, fresh: &crate::yt::StreamInfo) -> Result<()> {
    if fresh.itag == itag && (len.is_none() || fresh.content_length == len) {
        return Ok(());
    }
    Err(Error::Unavailable(format!(
        "the stream changed (itag {itag} became {}, {:?} bytes became {:?})",
        fresh.itag, len, fresh.content_length
    )))
}

/// Resolve, open the stream and parse headers: everything up to the first decodable packet.
fn load(
    resolver: &Arc<Resolver>,
    id: &str,
    cancel: &Cancel,
    events: Sender<Event>,
) -> Result<Loaded> {
    let t0 = Instant::now();
    if crate::radio::is_station(id) {
        // Listening along: the host's relayed bytes, not a connection of our own.
        let src = match crate::radio::relay::take_relay(id) {
            Some(r) => crate::radio::relay::open_relay(id, r, None)?,
            None => crate::radio::live::open(id, Some(events))?,
        };
        log::info!(
            "{id}: live {} ready in {} ms ({} bytes buffered)",
            src.quality,
            t0.elapsed().as_millis(),
            src.buffered()
        );
        return Ok(Loaded {
            duration_ms: None,
            src: Box::new(src),
        });
    }
    let info = resolver.resolve(id)?;
    let t_resolve = t0.elapsed();
    let r = resolver.clone();
    let vid = id.to_string();
    let (itag, len) = (info.itag, info.content_length);
    let refresh = Box::new(move || {
        r.invalidate(&vid);
        let fresh = r.refresh(&vid)?;
        same_file(itag, len, &fresh)?;
        Ok(fresh.url)
    });
    let stream = match HttpStream::open_with(
        info.url.clone(),
        info.content_length,
        refresh,
        cancel.clone(),
    ) {
        Ok(s) => s,
        Err(e) => {
            resolver.invalidate(id);
            return Err(e);
        }
    };
    let src = source::open(&info, stream)?;
    log::info!(
        "{id}: itag {} ready in {} ms (resolve {} ms)",
        info.itag,
        t0.elapsed().as_millis(),
        t_resolve.as_millis()
    );
    Ok(Loaded {
        duration_ms: info.duration_ms,
        src,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const RATE: u64 = NULL_RATE as u64;

    /// A test track at the null output's rate: `frames` of `level`, breaking where asked.
    #[derive(Clone)]
    struct Spec {
        frames: u64,
        level: f32,
        fail_at: Option<u64>,
        /// Panic (a bug) on reading this far in.
        panic_at: Option<u64>,
        load_error: bool,
        load_panic: bool,
        load_delay: Duration,
    }

    impl Default for Spec {
        fn default() -> Self {
            Spec {
                frames: 2 * RATE,
                level: 0.5,
                fail_at: None,
                panic_at: None,
                load_error: false,
                load_panic: false,
                load_delay: Duration::ZERO,
            }
        }
    }

    struct Tone {
        spec: Spec,
        pos: u64,
    }

    impl Source for Tone {
        fn rate(&self) -> u32 {
            NULL_RATE
        }
        fn total_frames(&self) -> Option<u64> {
            Some(self.spec.frames)
        }
        fn read(&mut self, out: &mut Vec<f32>) -> Result<bool> {
            if self.spec.fail_at.is_some_and(|f| self.pos >= f) {
                return Err(Error::Media("broken packet".into()));
            }
            if self.spec.panic_at.is_some_and(|f| self.pos >= f) {
                panic!("test panic");
            }
            let n = 960.min(self.spec.frames - self.pos);
            out.extend(std::iter::repeat_n(self.spec.level, n as usize * CH));
            self.pos += n;
            Ok(self.pos < self.spec.frames)
        }
        fn seek(&mut self, frame: u64) -> Result<()> {
            self.pos = frame;
            Ok(())
        }
    }

    /// Opens [`Tone`]s as `specs` say, noting each load in `loads`.
    fn tone_loader(
        specs: Arc<Mutex<HashMap<String, Spec>>>,
        loads: Arc<Mutex<Vec<String>>>,
        cancelled: Arc<Mutex<Vec<String>>>,
    ) -> Loader {
        Arc::new(move |id, cancel, _| {
            loads.lock().unwrap().push(id.into());
            let spec = specs.lock().unwrap().get(id).cloned().unwrap_or_default();
            std::thread::sleep(spec.load_delay);
            if cancel.is_cancelled() {
                cancelled.lock().unwrap().push(id.into());
                return Err(Error::Cancelled);
            }
            if spec.load_panic {
                panic!("test panic");
            }
            if spec.load_error {
                return Err(Error::Unavailable("test".into()));
            }
            Ok(Loaded {
                duration_ms: None,
                src: Box::new(Tone { spec, pos: 0 }),
            })
        })
    }

    /// A worker without a device or a thread: each step is driven by the test.
    struct Rig {
        w: Worker,
        rx: Receiver<Cmd>,
        events: Receiver<Event>,
        specs: Arc<Mutex<HashMap<String, Spec>>>,
        loads: Arc<Mutex<Vec<String>>>,
        /// Loads that found themselves cancelled.
        cancelled: Arc<Mutex<Vec<String>>>,
    }

    impl Rig {
        fn new() -> Rig {
            let (tx, rx) = crossbeam_channel::unbounded();
            let (etx, events) = crossbeam_channel::unbounded();
            let specs: Arc<Mutex<HashMap<String, Spec>>> = Arc::default();
            let loads: Arc<Mutex<Vec<String>>> = Arc::default();
            let cancelled: Arc<Mutex<Vec<String>>> = Arc::default();
            let loader = tone_loader(specs.clone(), loads.clone(), cancelled.clone());
            let timeline = Arc::new(Mutex::new(Timeline::default()));
            let w = Worker::new(loader, true, Shared::new(), timeline, tx, etx).unwrap();
            Rig {
                w,
                rx,
                events,
                specs,
                loads,
                cancelled,
            }
        }

        fn track(&self, id: &str, f: impl FnOnce(&mut Spec)) {
            f(self.specs.lock().unwrap().entry(id.into()).or_default());
        }

        fn cmd(&mut self, c: Cmd) {
            assert!(self.w.handle(c));
            self.settle();
        }

        fn loading(&self) -> bool {
            self.w.play_load.is_some() || self.w.next_load.is_some()
        }

        /// Lets the worker catch up: loads in, decoded as far as the ring allows.
        fn settle(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                while let Ok(c) = self.rx.try_recv() {
                    self.w.handle(c);
                }
                self.w.pump();
                self.w.announce();
                if !self.loading() || Instant::now() > deadline {
                    return;
                }
                if let Ok(c) = self.rx.recv_timeout(Duration::from_millis(500)) {
                    self.w.handle(c);
                }
            }
        }

        /// The device plays `ms` of audio, 10 ms per callback (silence once the ring is dry).
        fn play_out(&mut self, ms: u64) {
            for _ in 0..ms / 10 {
                self.consume(10);
                self.settle();
            }
        }

        /// The device takes `ms` from the ring, with the worker not running meanwhile.
        fn consume(&mut self, ms: u64) {
            if self.w.shared.paused.load(Ordering::Relaxed) {
                return;
            }
            let mut c = self.w.consumer.lock().unwrap();
            let n = (c.slots() / CH).min((RATE * ms / 1000) as usize);
            if let Ok(chunk) = c.read_chunk(n * CH) {
                chunk.commit_all();
            }
            self.w.shared.played.fetch_add(n as u64, Ordering::Relaxed);
        }

        fn position(&self) -> Option<Position> {
            position(&self.w.shared, &self.w.timeline, None)
        }

        /// Events since the last call, as short strings.
        fn events(&self) -> Vec<String> {
            self.events
                .try_iter()
                .map(|e| match e {
                    Event::Started { id, .. } => format!("started {id}"),
                    Event::Finished => "finished".into(),
                    Event::Failed {
                        id,
                        handed_over: true,
                        ..
                    } => format!("failed {id}, handed over"),
                    Event::Failed { id, .. } => format!("failed {id}"),
                    Event::EngineError(_) => "engine error".into(),
                    e => format!("{e:?}"),
                })
                .collect()
        }

        fn loads(&self) -> Vec<String> {
            self.loads.lock().unwrap().clone()
        }
    }

    #[test]
    fn a_refreshed_url_must_be_the_same_file() {
        let info = |itag, len| crate::yt::StreamInfo {
            video_id: "a".into(),
            url: "https://example.invalid/".into(),
            itag,
            mime: "audio/webm".into(),
            content_length: len,
            duration_ms: None,
            sample_rate: None,
            expires_at: 0,
        };
        assert!(same_file(251, Some(100), &info(251, Some(100))).is_ok());
        assert!(same_file(251, Some(100), &info(140, Some(100))).is_err());
        assert!(same_file(251, Some(100), &info(251, Some(101))).is_err());
        assert!(same_file(251, None, &info(251, Some(101))).is_ok());
    }

    #[test]
    fn a_track_starts_and_finishes() {
        let mut r = Rig::new();
        r.cmd(Cmd::Play("a".into()));
        r.play_out(100);
        assert_eq!(r.events(), ["started a"]);
        assert_eq!(r.position().map(|p| p.id), Some("a".into()));
        r.play_out(2500);
        assert_eq!(r.events(), ["finished"]);
    }

    #[test]
    fn the_next_track_follows_gaplessly() {
        let mut r = Rig::new();
        r.track("a", |s| s.frames = RATE + 123);
        r.cmd(Cmd::Play("a".into()));
        r.cmd(Cmd::SetNext(Some("b".into())));
        r.play_out(4000);
        assert_eq!(r.events(), ["started a", "started b", "finished"]);
        assert_eq!(r.w.shared.played.load(Ordering::Relaxed), 3 * RATE + 123);
        assert_eq!(r.loads(), ["a", "b"]);
    }

    #[test]
    fn playing_the_preloaded_track_reuses_it() {
        let mut r = Rig::new();
        r.cmd(Cmd::Play("a".into()));
        r.cmd(Cmd::SetNext(Some("b".into())));
        r.play_out(100);
        r.cmd(Cmd::Play("b".into()));
        r.play_out(100);
        assert_eq!(r.events(), ["started a", "started b"]);
        assert_eq!(r.loads(), ["a", "b"]);
    }

    #[test]
    fn a_panic_while_opening_fails_the_track() {
        let mut r = Rig::new();
        r.track("a", |s| s.load_panic = true);
        r.cmd(Cmd::Play("a".into()));
        r.play_out(100);
        assert_eq!(r.events(), ["failed a"]);
        r.cmd(Cmd::Play("b".into()));
        r.play_out(100);
        assert_eq!(r.events(), ["started b"]);
    }

    #[test]
    fn a_panic_while_decoding_drops_the_track_not_the_engine() {
        let specs: Arc<Mutex<HashMap<String, Spec>>> = Arc::default();
        specs.lock().unwrap().insert(
            "a".into(),
            Spec {
                panic_at: Some(RATE / 10),
                ..Default::default()
            },
        );
        let e =
            Engine::with_loader(tone_loader(specs, Arc::default(), Arc::default()), true).unwrap();
        let until = |want: &dyn Fn(&Event) -> bool| loop {
            match e.events().recv_timeout(Duration::from_secs(5)) {
                Ok(ev) if want(&ev) => return ev,
                Ok(_) => {}
                Err(err) => panic!("no such event: {err}"),
            }
        };
        e.play("a");
        let ev = until(&|ev| matches!(ev, Event::EngineError(_)));
        assert!(matches!(ev, Event::EngineError(m) if m.contains("test panic")));
        e.play("b");
        until(&|ev| matches!(ev, Event::Started { id, .. } if id == "b"));
        assert!(e.position().is_some_and(|p| p.id == "b"));
    }

    #[test]
    fn locks_left_poisoned_by_a_panic_still_work() {
        let mut r = Rig::new();
        r.cmd(Cmd::Play("a".into()));
        r.play_out(100);
        let timeline = r.w.timeline.clone();
        let _ = std::thread::spawn(move || {
            let _held = timeline.lock().unwrap();
            panic!("test panic");
        })
        .join();
        assert!(r.w.timeline.is_poisoned());
        r.w.recover("test".into());
        assert_eq!(r.events(), ["started a", "engine error"]);
        assert_eq!(r.position(), None);
        r.cmd(Cmd::Play("b".into()));
        r.play_out(100);
        assert_eq!(r.events(), ["started b"]);
        assert_eq!(r.position().unwrap().id, "b");
    }

    #[test]
    fn fast_skips_stop_the_downloads_they_leave_behind() {
        let mut r = Rig::new();
        for id in ["a", "b"] {
            r.track(id, |s| s.load_delay = Duration::from_millis(100));
        }
        // Each skip comes before the last track has opened.
        for id in ["a", "b", "c"] {
            assert!(r.w.handle(Cmd::Play(id.into())));
        }
        r.play_out(300);
        assert_eq!(r.events(), ["started c"]);
        // The skipped loads wake up to find themselves stopped.
        assert!(within(Duration::from_secs(2), || r
            .cancelled
            .lock()
            .unwrap()
            .len()
            == 2));
        let mut cancelled = r.cancelled.lock().unwrap().clone();
        cancelled.sort();
        assert_eq!(cancelled, ["a", "b"]);
    }

    #[test]
    fn playing_the_track_being_opened_ahead_uses_that_load() {
        let mut r = Rig::new();
        r.track("a", |s| s.frames = RATE / 2);
        r.track("b", |s| s.load_delay = Duration::from_millis(100));
        r.cmd(Cmd::Play("a".into()));
        assert!(r.w.handle(Cmd::SetNext(Some("b".into()))));
        r.consume(100);
        r.w.pump();
        assert!(r.w.next_load.is_some());
        // Skipped to before it finished opening.
        r.cmd(Cmd::Play("b".into()));
        r.play_out(100);
        assert_eq!(r.events(), ["started a", "started b"]);
        assert_eq!(r.loads(), ["a", "b"]);
        assert!(r.cancelled.lock().unwrap().is_empty());
    }

    #[test]
    fn an_unsupported_device_is_reported_once_and_retried_on_play() {
        let mut r = Rig::new();
        r.w.unsupported = Some("DSD only".into());
        r.w.reopen_failed();
        assert!(r.w.reopen_at.is_none());
        assert_eq!(
            r.events(),
            [format!("{:?}", Event::DeviceUnsupported("DSD only".into()))]
        );
        assert!(r.w.handle(Cmd::Play("a".into())));
        assert!(r.w.unsupported.is_none());
        assert!(r.w.reopen_at.is_some());
    }

    #[test]
    fn an_error_from_a_replaced_output_is_ignored() {
        let mut r = Rig::new();
        let old = r.w.outputs;
        r.w.reopen();
        assert_eq!(r.w.outputs, old + 1);
        r.w.handle(Cmd::DeviceError {
            output: old,
            error: "gone".into(),
        });
        assert!(r.w.reopen_at.is_none());
        assert!(r.events().is_empty());
        r.w.handle(Cmd::DeviceError {
            output: old + 1,
            error: "gone".into(),
        });
        assert!(r.w.reopen_at.is_some());
        assert_eq!(r.events().len(), 1);
    }

    #[test]
    fn a_track_that_cannot_open_fails() {
        let mut r = Rig::new();
        r.track("a", |s| s.load_error = true);
        r.cmd(Cmd::Play("a".into()));
        r.play_out(100);
        assert_eq!(r.events(), ["failed a"]);
        assert_eq!(r.position(), None);
    }

    #[test]
    fn seek_moves_the_position() {
        let mut r = Rig::new();
        r.cmd(Cmd::Play("a".into()));
        r.play_out(100);
        r.cmd(Cmd::Seek(1500));
        assert_eq!(r.position().map(|p| p.ms), Some(1500));
        r.play_out(100);
        assert_eq!(r.position().map(|p| p.ms), Some(1600));
    }

    #[test]
    fn a_seek_never_shows_the_old_place_running_on() {
        let mut r = Rig::new();
        r.cmd(Cmd::Play("a".into()));
        r.play_out(100);
        r.cmd(Cmd::Seek(1000));
        let (shared, timeline) = (r.w.shared.clone(), r.w.timeline.clone());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut worst = 0;
                while !stop.load(Ordering::Relaxed) {
                    if let Some(p) = position(&shared, &timeline, None) {
                        worst = worst.max(p.ms);
                    }
                }
                worst
            })
        };
        // Each seek drops most of a second of queued audio.
        for _ in 0..300 {
            r.consume(10);
            r.cmd(Cmd::Seek(1000));
        }
        stop.store(true, Ordering::Relaxed);
        let worst = reader.join().unwrap();
        assert!(worst <= 1010, "position jumped to {worst} ms");
    }

    #[test]
    fn the_position_is_what_is_heard_not_what_the_device_took() {
        let mut r = Rig::new();
        r.cmd(Cmd::Play("a".into()));
        r.play_out(500);
        // The last callback said its frames are heard 100 ms from now.
        let played = r.w.shared.played.load(Ordering::Relaxed);
        r.w.shared.set_anchor(played, clock::now_ns() + 100_000_000);
        let ms = r.position().unwrap().ms;
        assert!((398..=400).contains(&ms), "{ms}");
        // Paused, the device plays out what it has.
        r.cmd(Cmd::Pause);
        assert_eq!(r.position().unwrap().ms, 500);
    }

    #[test]
    fn a_track_heard_before_the_decoder_wakes_is_announced_at_once() {
        let mut r = Rig::new();
        r.track("a", |s| s.frames = RATE / 2);
        r.cmd(Cmd::Play("a".into()));
        r.cmd(Cmd::SetNext(Some("b".into())));
        r.play_out(100);
        assert_eq!(r.events(), ["started a"]);
        // The device moves into b while the decoder sleeps.
        r.consume(300);
        r.consume(300);
        let tx = r.w.cmd_tx.clone();
        let p = position(&r.w.shared, &r.w.timeline, Some(&tx)).unwrap();
        assert_eq!(p.id, "b");
        // Asked once, however often the position is read.
        let _ = position(&r.w.shared, &r.w.timeline, Some(&tx));
        assert!(matches!(r.rx.try_recv(), Ok(Cmd::Announce)));
        assert!(r.rx.try_recv().is_err());
        r.cmd(Cmd::Announce);
        assert_eq!(r.events(), ["started b"]);
    }

    #[test]
    fn a_seek_far_past_the_end_lands_at_the_end() {
        let mut r = Rig::new();
        r.cmd(Cmd::Play("a".into()));
        r.play_out(100);
        r.cmd(Cmd::Seek(u64::MAX));
        assert!(r.position().unwrap().ms < 2000);
        r.play_out(500);
        assert_eq!(r.events(), ["started a", "finished"]);
    }

    #[test]
    fn pause_holds_the_position() {
        let mut r = Rig::new();
        r.cmd(Cmd::Play("a".into()));
        r.play_out(200);
        r.cmd(Cmd::Pause);
        r.play_out(200);
        let p = r.position().unwrap();
        assert_eq!((p.ms, p.paused), (200, true));
        r.cmd(Cmd::Resume);
        r.play_out(100);
        assert_eq!(r.position().map(|p| p.ms), Some(300));
    }

    /// Raw f32 stereo over a non-blocking `HttpStream`, as the Opus decoder reads.
    struct Raw {
        s: HttpStream,
        buf: Vec<u8>,
    }

    impl Source for Raw {
        fn rate(&self) -> u32 {
            NULL_RATE
        }
        fn total_frames(&self) -> Option<u64> {
            Some(self.s.len() / 8)
        }
        fn read(&mut self, out: &mut Vec<f32>) -> Result<bool> {
            use std::io::{Read, Seek};
            let pos = self.s.stream_position().unwrap();
            let n = (960 * 8).min(self.s.len() - pos) as usize;
            self.buf.resize(n, 0);
            if let Err(e) = self.s.read_exact(&mut self.buf) {
                self.s.seek(std::io::SeekFrom::Start(pos)).unwrap();
                return Err(match e.kind() {
                    std::io::ErrorKind::WouldBlock => Error::WouldBlock,
                    _ => Error::Net(e.to_string()),
                });
            }
            out.extend(
                self.buf
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| b[0] as f32 / 1000.0),
            );
            Ok(pos + (n as u64) < self.s.len())
        }
        fn stalled(&self) -> bool {
            self.s.stalled(960 * 8)
        }
        fn seek(&mut self, frame: u64) -> Result<()> {
            use std::io::Seek;
            self.s.seek(std::io::SeekFrom::Start(frame * 8)).unwrap();
            Ok(())
        }
    }

    /// An engine on its own thread, without a device, playing raw audio from `url`.
    fn raw_engine(url: String) -> Engine {
        let loader: Loader = Arc::new(move |_, cancel, _| {
            let refresh = Box::new(|| Err(Error::Cancelled));
            let mut s = HttpStream::open_with(url.clone(), None, refresh, cancel.clone())?;
            s.set_nonblocking(true);
            Ok(Loaded {
                duration_ms: None,
                src: Box::new(Raw { s, buf: Vec::new() }),
            })
        });
        Engine::with_loader(loader, true).unwrap()
    }

    /// Waits up to `limit` for `f`.
    fn within(limit: Duration, f: impl Fn() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < limit {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        f()
    }

    #[test]
    fn a_stalled_download_never_holds_up_commands() {
        use crate::audio::testserver::{self, Behaviour};
        // The first unit comes, then nothing: the decoder runs into the stall at once.
        let server = testserver::serve(
            testserver::file(4 << 20),
            Behaviour {
                stall_after: Some(300_000),
                ..Default::default()
            },
        );
        let e = raw_engine(server.url.clone());
        e.play("a");
        std::thread::sleep(Duration::from_millis(500));
        e.pause();
        assert!(within(Duration::from_millis(100), || e
            .shared
            .paused
            .load(Ordering::Relaxed)));
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            drop(e);
            let _ = tx.send(());
        });
        assert!(rx.recv_timeout(Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn clearing_the_next_track_drops_a_preloaded_one() {
        let mut r = Rig::new();
        r.cmd(Cmd::Play("a".into()));
        r.cmd(Cmd::SetNext(Some("b".into())));
        r.play_out(100);
        assert!(r.w.next.is_some());
        r.cmd(Cmd::SetNext(None));
        r.play_out(2500);
        assert_eq!(r.events(), ["started a", "finished"]);
    }

    #[test]
    fn an_error_while_playing_hands_over_to_the_next_track() {
        let mut r = Rig::new();
        // Past the first second, so it is decoded after the next track is known.
        r.track("a", |s| s.fail_at = Some(RATE * 3 / 2));
        r.cmd(Cmd::Play("a".into()));
        r.cmd(Cmd::SetNext(Some("b".into())));
        r.play_out(3500);
        assert_eq!(
            r.events(),
            [
                "started a",
                "failed a, handed over",
                "started b",
                "finished"
            ]
        );
        assert_eq!(r.loads(), ["a", "b"]);
    }

    #[test]
    fn an_error_with_nothing_next_ends_playback() {
        let mut r = Rig::new();
        r.track("a", |s| s.fail_at = Some(RATE * 3 / 2));
        r.cmd(Cmd::Play("a".into()));
        r.play_out(2000);
        assert_eq!(r.events(), ["started a", "failed a", "finished"]);
    }

    #[test]
    fn clearing_the_next_track_while_it_loads_drops_it() {
        let mut r = Rig::new();
        r.track("b", |s| s.load_delay = Duration::from_millis(50));
        r.cmd(Cmd::Play("a".into()));
        assert!(r.w.handle(Cmd::SetNext(Some("b".into()))));
        r.consume(200);
        r.w.pump();
        assert!(r.w.next_load.is_some());
        r.cmd(Cmd::SetNext(None));
        r.play_out(2500);
        assert_eq!(r.events(), ["started a", "finished"]);
    }

    #[test]
    fn nothing_is_audible_after_the_end() {
        let mut r = Rig::new();
        r.cmd(Cmd::Play("a".into()));
        r.play_out(2500);
        assert_eq!(r.events(), ["started a", "finished"]);
        assert_eq!(r.position(), None);
    }

    #[test]
    fn a_seek_while_loading_is_where_it_starts() {
        let mut r = Rig::new();
        r.track("a", |s| s.load_delay = Duration::from_millis(100));
        assert!(r.w.handle(Cmd::Play("a".into())));
        assert!(r.w.handle(Cmd::Seek(1000)));
        r.settle();
        r.play_out(100);
        assert_eq!(r.position().map(|p| p.ms), Some(1100));
    }
}
