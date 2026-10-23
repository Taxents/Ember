//! The playback engine: one decoder thread feeding one ring, one output stream.
//!
//! Gapless: the next track is opened ~30 s before the current one ends, and its first sample
//! is written straight after the current one's last into the same ring. A timeline of marks
//! (ring frame → track) turns the callback's frame counter into "which track, what position"
//! without the callback knowing anything about tracks.

use super::output::{self, Output, Shared, SharedConsumer};
use super::resample::Resampler;
use super::source::{self, Source};
use super::stream::HttpStream;
use crate::audio::eq::EqSettings;
use crate::yt::{Resolver, StreamInfo};
use crate::{Error, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use rtrb::{Producer, RingBuffer};
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
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

#[derive(Debug, Clone)]
pub enum Event {
    /// A track became audible (also after a gapless hand-over).
    Started {
        id: String,
        duration_ms: Option<u64>,
    },
    /// The last track ended and nothing follows.
    Finished,
    /// A track couldn't be played.
    Failed { id: String, error: Error },
    /// The output device changed or failed.
    Device(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub id: String,
    pub ms: u64,
    pub duration_ms: Option<u64>,
    pub paused: bool,
}

enum Cmd {
    Play(String),
    SetNext(Option<String>),
    Pause,
    Resume,
    Seek(u64),
    Stop,
    Loaded {
        generation: u64,
        next: bool,
        id: String,
        result: Result<Loaded>,
    },
    DeviceError(String),
    Quit,
}

pub struct Engine {
    cmd: Sender<Cmd>,
    events: Receiver<Event>,
    shared: Arc<Shared>,
    timeline: Arc<Mutex<Timeline>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Engine {
    pub fn new(resolver: Arc<Resolver>) -> Result<Self> {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let shared = Shared::new();
        let timeline = Arc::new(Mutex::new(Timeline::default()));
        let mut w = Worker::new(
            resolver,
            shared.clone(),
            timeline.clone(),
            cmd_tx.clone(),
            ev_tx,
        )?;
        let thread = std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || w.run(cmd_rx))
            .map_err(|e| Error::Device(e.to_string()))?;
        Ok(Engine {
            cmd: cmd_tx,
            events: ev_rx,
            shared,
            timeline,
            thread: Some(thread),
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
    pub fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    /// What is audible right now.
    pub fn position(&self) -> Option<Position> {
        let tl = self.timeline.lock().unwrap();
        let played = self.shared.played.load(Ordering::Relaxed);
        let m = tl.audible(played)?;
        let frames = played - m.at + m.offset;
        Some(Position {
            id: m.id.clone(),
            ms: frames * 1000 / tl.rate.max(1) as u64,
            duration_ms: m.duration_ms,
            paused: self.shared.paused.load(Ordering::Relaxed),
        })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
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
}

impl Timeline {
    fn audible(&self, played: u64) -> Option<&Mark> {
        self.marks.iter().rev().find(|m| m.at <= played)
    }
}

// ---- worker ----------------------------------------------------------------------------------

/// A track opened off-thread, before the device rate is applied.
struct Loaded {
    info: StreamInfo,
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
    fn new(id: String, l: Loaded, dev_rate: u32) -> Self {
        let rate = l.src.rate();
        let duration_ms = l
            .src
            .total_frames()
            .map(|f| f * 1000 / rate as u64)
            .or(l.info.duration_ms);
        Track {
            id,
            rs: (rate != dev_rate).then(|| Resampler::new(rate, dev_rate)),
            src: l.src,
            decoded: 0,
            eof: false,
            duration_ms,
        }
    }

    /// Reposition to `ms`; returns the exact position used, in device frames.
    fn seek(&mut self, ms: u64, dev_rate: u32) -> Result<u64> {
        let rate = self.src.rate() as u64;
        let mut frame = ms * rate / 1000;
        if let Some(t) = self.src.total_frames() {
            frame = frame.min(t.saturating_sub(1));
        }
        self.src.seek(frame)?;
        self.decoded = frame;
        self.eof = false;
        if self.rs.is_some() {
            self.rs = Some(Resampler::new(rate as u32, dev_rate));
        }
        Ok(frame * dev_rate as u64 / rate)
    }

    fn remaining(&self) -> Option<Duration> {
        let total = self.src.total_frames()?;
        let left = total.saturating_sub(self.decoded);
        Some(Duration::from_millis(left * 1000 / self.src.rate() as u64))
    }
}

struct Worker {
    resolver: Arc<Resolver>,
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
    loading_next: bool,
    /// A track asked for with `Play` is being opened.
    loading_play: bool,
    generation: u64,
    finished_sent: bool,
    idle_since: Option<Instant>,
}

impl Worker {
    fn new(
        resolver: Arc<Resolver>,
        shared: Arc<Shared>,
        timeline: Arc<Mutex<Timeline>>,
        cmd_tx: Sender<Cmd>,
        events: Sender<Event>,
    ) -> Result<Self> {
        let (p, c) = RingBuffer::new(2);
        let mut w = Worker {
            resolver,
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
            loading_next: false,
            loading_play: false,
            generation: 0,
            finished_sent: true,
            idle_since: Some(Instant::now()),
        };
        w.open_output()?;
        Ok(w)
    }

    fn open_output(&mut self) -> Result<()> {
        self.output = None;
        let (dev, cfg) = output::device_format()?;
        let rate = cfg.sample_rate();
        let cap = ((rate as f64 * RING_SECONDS) as usize) * CH;
        let (p, c) = RingBuffer::new(cap);
        self.producer = p;
        *self.consumer.lock().unwrap() = c;
        // Everything in flight is gone with the old ring.
        self.shared.played.store(self.written, Ordering::Relaxed);
        self.carry.clear();
        self.dev_rate = rate;
        self.timeline.lock().unwrap().rate = rate;
        let tx = self.cmd_tx.clone();
        let out = Output::open(
            &dev,
            &cfg,
            self.shared.clone(),
            self.consumer.clone(),
            move |e| {
                let _ = tx.send(Cmd::DeviceError(e.to_string()));
            },
        )?;
        log::info!(
            "output: {} Hz, {} ch, format {}",
            rate,
            out.channels,
            cfg.sample_format()
        );
        self.output = Some(out);
        Ok(())
    }

    fn run(&mut self, rx: Receiver<Cmd>) {
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
            self.pump();
            self.announce();
            self.manage_output();
        }
    }

    fn handle(&mut self, c: Cmd) -> bool {
        match c {
            Cmd::Quit => return false,
            Cmd::Play(id) => self.play(id),
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
            Cmd::Stop => {
                self.generation += 1;
                self.flush();
                self.cur = None;
                self.prev = None;
                self.next = None;
                self.next_id = None;
                self.loading_next = false;
                self.loading_play = false;
                self.finished_sent = true;
            }
            Cmd::Loaded {
                generation,
                next,
                id,
                result,
            } => self.loaded(generation, next, id, result),
            Cmd::DeviceError(e) => self.device_error(e),
        }
        true
    }

    fn play(&mut self, id: String) {
        self.generation += 1;
        self.flush();
        self.prev = None;
        self.next_id = None;
        self.loading_next = false;
        self.finished_sent = false;
        // Skipping to the preloaded track starts instantly.
        if let Some(t) = self.next.take().filter(|t| t.id == id) {
            self.cur = None;
            self.start(t, 0, true);
            return;
        }
        self.cur = None;
        self.next = None;
        self.loading_play = true;
        self.spawn_load(id, false);
    }

    fn set_next(&mut self, id: Option<String>) {
        if self.next.as_ref().map(|t| &t.id) == id.as_ref() && id.is_some() {
            return;
        }
        if self.next_id == id {
            return;
        }
        self.next = None;
        self.loading_next = false;
        self.next_id = id;
    }

    fn seek(&mut self, ms: u64) {
        let played = self.shared.played.load(Ordering::Relaxed);
        let audible = self
            .timeline
            .lock()
            .unwrap()
            .audible(played)
            .map(|m| m.id.clone());
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
            Ok(offset) => self.start(t, offset, false),
            Err(e) => self.fail(t.id, e),
        }
    }

    /// Makes `t` the decoding track, audible from the current end of the ring.
    fn start(&mut self, t: Track, offset: u64, announce: bool) {
        let at = self.written + (self.carry.len() / CH) as u64;
        self.timeline.lock().unwrap().marks.push_back(Mark {
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
    }

    /// Drops everything queued for output.
    fn flush(&mut self) {
        {
            let mut c = self.consumer.lock().unwrap();
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
        self.timeline.lock().unwrap().marks.clear();
        self.shared.expecting.store(false, Ordering::Relaxed);
    }

    fn spawn_load(&mut self, id: String, next: bool) {
        let generation = self.generation;
        let tx = self.cmd_tx.clone();
        let resolver = self.resolver.clone();
        std::thread::Builder::new()
            .name("track-load".into())
            .spawn(move || {
                let result = load(&resolver, &id);
                let _ = tx.send(Cmd::Loaded {
                    generation,
                    next,
                    id,
                    result,
                });
            })
            .expect("spawn loader");
    }

    fn loaded(&mut self, generation: u64, next: bool, id: String, result: Result<Loaded>) {
        if generation != self.generation {
            return;
        }
        if next {
            if self.next_id.as_deref() != Some(&id) {
                return;
            }
            self.loading_next = false;
            self.next_id = None;
            match result {
                Ok(l) => self.next = Some(Track::new(id, l, self.dev_rate)),
                Err(e) => self.fail(id, e),
            }
        } else {
            self.loading_play = false;
            match result {
                Ok(l) => {
                    let t = Track::new(id, l, self.dev_rate);
                    self.start(t, 0, true);
                }
                Err(e) => {
                    // The queue decides what follows a failed track.
                    self.fail(id, e);
                    self.finished_sent = true;
                }
            }
        }
    }

    fn fail(&mut self, id: String, error: Error) {
        log::error!("{id}: {error}");
        let _ = self.events.send(Event::Failed { id, error });
    }

    /// How long the loop may sleep; `None` = until the next command.
    fn wait_time(&self) -> Option<Duration> {
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
            || self.cur.as_ref().is_some_and(|t| !t.eof)
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
                    // Gapless hand-over: the next track starts right after the last sample.
                    self.prev = self.cur.take();
                    self.start(n, 0, true);
                    continue;
                }
                self.maybe_preload();
                return;
            }
            self.pcm.clear();
            let more = match t.src.read(&mut self.pcm) {
                Ok(m) => m,
                Err(e) => {
                    let id = t.id.clone();
                    t.eof = true;
                    self.fail(id, e);
                    continue;
                }
            };
            t.decoded += (self.pcm.len() / CH) as u64;
            match &mut t.rs {
                Some(rs) => {
                    rs.process(&self.pcm, &mut self.carry);
                    if !more {
                        rs.finish(&mut self.carry);
                    }
                }
                None => self.carry.extend_from_slice(&self.pcm),
            }
            if !more {
                t.eof = true;
            }
            self.maybe_preload();
        }
    }

    fn maybe_preload(&mut self) {
        if self.next.is_some() || self.loading_next {
            return;
        }
        let Some(id) = self.next_id.clone() else {
            return;
        };
        let due = match &self.cur {
            Some(t) => t.eof || t.remaining().is_none_or(|r| r <= PRELOAD_BEFORE),
            None => false,
        };
        if due {
            self.loading_next = true;
            self.spawn_load(id, true);
        }
    }

    /// Emits `Started` for tracks that just became audible; notices the end of playback.
    fn announce(&mut self) {
        let played = self.shared.played.load(Ordering::Relaxed);
        let mut started = Vec::new();
        {
            let mut tl = self.timeline.lock().unwrap();
            for m in tl
                .marks
                .iter_mut()
                .filter(|m| !m.announced && m.at <= played)
            {
                m.announced = true;
                started.push(Event::Started {
                    id: m.id.clone(),
                    duration_ms: m.duration_ms,
                });
            }
            // Keep only the audible mark and those after it.
            while tl.marks.len() > 1 && tl.marks[1].at <= played {
                tl.marks.pop_front();
            }
            // Once the decoding track is audible, the previous one can't be sought back to.
            if let (Some(cur), Some(front)) = (&self.cur, tl.marks.front())
                && front.id == cur.id
                && front.at <= played
            {
                self.prev = None;
            }
        }
        for e in started {
            let _ = self.events.send(e);
        }
        let drained = self.carry.is_empty() && played >= self.written;
        let ended = self.cur.as_ref().is_none_or(|t| t.eof) && self.next.is_none();
        let waiting = self.loading_next || self.loading_play || self.next_id.is_some();
        if drained && ended && !waiting && !self.finished_sent {
            self.finished_sent = true;
            self.cur = None;
            self.prev = None;
            self.shared.expecting.store(false, Ordering::Relaxed);
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

    /// Device lost or changed: reopen on the (new) default device and resume where we were.
    fn device_error(&mut self, e: String) {
        log::warn!("output device error: {e}");
        let _ = self.events.send(Event::Device(e));
        let played = self.shared.played.load(Ordering::Relaxed);
        let resume = {
            let tl = self.timeline.lock().unwrap();
            tl.audible(played).map(|m| {
                (
                    m.id.clone(),
                    (played - m.at + m.offset) * 1000 / tl.rate.max(1) as u64,
                )
            })
        };
        let was_running = self.output.as_ref().is_some_and(|o| o.is_running());
        if let Err(e) = self.open_output() {
            log::error!("reopening output failed: {e}");
            return;
        }
        // The ring was rebuilt at the new rate: resamplers must match it.
        for t in [&mut self.cur, &mut self.next, &mut self.prev]
            .into_iter()
            .flatten()
        {
            let rate = t.src.rate();
            t.rs = (rate != self.dev_rate).then(|| Resampler::new(rate, self.dev_rate));
        }
        if let Some((id, ms)) = resume {
            self.seek_audible(Some(id), ms);
        }
        if was_running && let Some(o) = &mut self.output {
            o.start();
        }
    }
}

/// Resolve, open the stream and parse headers: everything up to the first decodable packet.
fn load(resolver: &Arc<Resolver>, id: &str) -> Result<Loaded> {
    let t0 = Instant::now();
    let info = resolver.resolve(id)?;
    let t_resolve = t0.elapsed();
    let r = resolver.clone();
    let vid = id.to_string();
    let refresh = Box::new(move || {
        r.invalidate(&vid);
        r.refresh(&vid).map(|i| i.url)
    });
    let stream = match HttpStream::open(info.url.clone(), info.content_length, refresh) {
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
    Ok(Loaded { info, src })
}
