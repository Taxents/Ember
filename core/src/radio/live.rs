//! Playing a live station: HTTP stream → ICY demux → byte pipe → symphonia → the engine.
//!
//! * A `radio-net` thread reads the endless HTTP body, strips the ICY metadata (sending each
//!   new `StreamTitle` as [`Event::Title`]) and appends the audio bytes to a bounded [`Pipe`].
//! * The decoder thread reads the pipe through symphonia (MP3 or AAC/ADTS, not seekable, no
//!   length). It only reads once a packet's worth of bytes is buffered ([`Source::stalled`]),
//!   so it never waits on the network and the audio thread never sees it.
//! * Drops: the network thread reconnects by itself (same URL first, then a fresh one from
//!   Radio Garden), with backoff, and gives up after a few failures in a row. Both demuxers
//!   resync on frame headers, so the decoder just carries on. A connection that goes silent
//!   without closing is replaced by a watchdog after [`STALL`] (and its read gives up after
//!   [`READ_TIMEOUT`], so its thread ends too). Failures count across all of these; after
//!   [`RETRIES`] in a row the source ends with [`NOT_RESPONDING`].
//! * Latency: about 0.75 s is buffered before playback starts, and the pipe holds at most
//!   [`CAP`] bytes, so a paused station never falls far behind.
//! * Listen Along: while hosting, every byte the pipe accepts is also tapped; a listener's
//!   source decodes a relay's pipe instead of the network ([`super::relay`]).

use super::icy::IcyDemux;
use super::relay::Tapping;
use crate::audio::Event;
use crate::audio::source::{Source, push_stereo};
use crate::{Error, Result};
use crossbeam_channel::Sender;
use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

/// Pipe capacity (about 16 s of a 128 kbit/s stream). The network side waits when it's full.
pub const CAP: usize = 256 * 1024;
const READ_CHUNK: usize = 8 * 1024;
/// Bytes buffered before the decoder reads (covers any MP3 frame and usual ADTS ones).
pub const MIN_AHEAD: usize = 6 * 1024;
/// The most the demuxer takes from the pipe at once, so little sits unseen in its buffer
/// while the pipe looks starved.
const READ_MAX: usize = 4 * 1024;
/// Starved this long: the connection is dead, start another.
pub const STALL: Duration = Duration::from_secs(12);
/// A read of the stream that waits this long gives up (the connection went quiet).
pub const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Why a station's source ended after it stopped answering.
pub const NOT_RESPONDING: &str = "the station isn't responding";
/// Longest wait for audio when opening.
pub(super) const OPEN_WAIT: Duration = Duration::from_secs(8);
/// Longest a playing decoder's read waits for the rest of a packet. Reads only start with
/// [`MIN_AHEAD`] buffered, so this is only reached by a packet (or junk run) bigger than that
/// with the network gone quiet. Running out of it ends the source with an error rather than
/// skipping the packet: the bytes the demuxer already took would be lost on this side only,
/// and a Listen Along listener fed the same bytes would decode different frames from then on.
const PACKET_WAIT: Duration = Duration::from_secs(5);
/// Failed connections in a row before giving up.
const RETRIES: u32 = 5;
/// A connection that held this long resets the failure count.
const HELD: Duration = Duration::from_secs(30);

// ---- pipe -------------------------------------------------------------------------------------

struct Buf {
    q: VecDeque<u8>,
    /// No more data will come (the network side gave up).
    eof: bool,
    /// The reading side is gone.
    closed: bool,
    /// The connection allowed to write; a replaced one finds its pushes refused.
    generation: u64,
    /// Since when the decoder has had too little to read.
    starved: Option<Instant>,
    /// Listen Along: every byte the pipe accepts is also handed here (see [`super::relay`]).
    tap: Option<Tapping>,
}

/// Bounded byte queue between the network thread and the decoder.
pub struct Pipe {
    m: Mutex<Buf>,
    cv: Condvar,
    /// `push` waits while this much is buffered.
    cap: usize,
}

impl Pipe {
    pub fn new() -> Arc<Self> {
        Self::with_cap(CAP)
    }

    /// A pipe whose `push` never waits (a Listen Along relay: dropping or holding back bytes
    /// would desync the listener).
    pub fn unbounded() -> Arc<Self> {
        Self::with_cap(usize::MAX)
    }

    fn with_cap(cap: usize) -> Arc<Self> {
        Arc::new(Pipe {
            m: Mutex::new(Buf {
                q: VecDeque::with_capacity(64 * 1024),
                eof: false,
                closed: false,
                generation: 0,
                starved: Some(Instant::now()),
                tap: None,
            }),
            cv: Condvar::new(),
            cap,
        })
    }

    /// Starts tapping: every byte accepted from now on is also passed to `tap`.
    pub(super) fn set_tap(&self, tap: Tapping) {
        self.m.lock().unwrap().tap = Some(tap);
    }

    /// Appends, waiting while full. False once the reader is gone or `generation` was replaced.
    ///
    /// The tap sees the bytes here, under the lock, only once they are accepted: in exactly the
    /// order the decoder will read them, and never the refused pushes of a replaced connection.
    pub fn push(&self, generation: u64, data: &[u8]) -> bool {
        let mut b = self.m.lock().unwrap();
        while b.q.len() >= self.cap && !b.closed && b.generation == generation {
            b = self.cv.wait(b).unwrap();
        }
        if b.closed || b.generation != generation {
            return false;
        }
        if let Some(t) = &b.tap
            && !t.data(data)
        {
            b.tap = None;
        }
        b.q.extend(data);
        if b.q.len() >= MIN_AHEAD {
            b.starved = None;
        }
        self.cv.notify_all();
        true
    }

    /// The network side gave up: reads return the end of the stream.
    pub fn finish(&self, generation: u64) {
        let mut b = self.m.lock().unwrap();
        if b.generation == generation {
            b.eof = true;
            if let Some(t) = b.tap.take() {
                t.end();
            }
            self.cv.notify_all();
        }
    }

    /// The reader is gone: the network side stops.
    pub fn close(&self) {
        let mut b = self.m.lock().unwrap();
        b.closed = true;
        if let Some(t) = b.tap.take() {
            t.end();
        }
        self.cv.notify_all();
    }

    fn live(&self, generation: u64) -> bool {
        let b = self.m.lock().unwrap();
        !b.closed && b.generation == generation
    }

    /// Replaces the writing connection; returns the new generation.
    fn next_generation(&self) -> u64 {
        let mut b = self.m.lock().unwrap();
        b.generation += 1;
        b.starved = Some(Instant::now());
        self.cv.notify_all();
        b.generation
    }

    pub fn buffered(&self) -> usize {
        self.m.lock().unwrap().q.len()
    }

    /// Can the decoder read without waiting on the network?
    pub fn ready(&self, min: usize) -> bool {
        let b = self.m.lock().unwrap();
        b.q.len() >= min || b.eof || b.closed
    }

    /// How long the decoder has been starved, if it is.
    fn starved_for(&self) -> Option<Duration> {
        let b = self.m.lock().unwrap();
        if b.eof || b.closed {
            return None;
        }
        b.starved.map(|t| t.elapsed())
    }

    /// Waits until `n` bytes are buffered or the stream ended, at most `max`. True if they are.
    pub fn wait_for(&self, n: usize, max: Duration) -> bool {
        let until = Instant::now() + max;
        let mut b = self.m.lock().unwrap();
        while b.q.len() < n && !b.eof && !b.closed {
            let now = Instant::now();
            if now >= until {
                return false;
            }
            b = self.cv.wait_timeout(b, until - now).unwrap().0;
        }
        b.q.len() >= n
    }

    /// Sleeps `d` unless the reader goes away first (then false).
    fn sleep(&self, generation: u64, d: Duration) -> bool {
        let until = Instant::now() + d;
        let mut b = self.m.lock().unwrap();
        loop {
            if b.closed || b.generation != generation {
                return false;
            }
            let now = Instant::now();
            if now >= until {
                return true;
            }
            b = self.cv.wait_timeout(b, until - now).unwrap().0;
        }
    }

    /// Reads what's there; waits briefly when empty. `Ok(0)` at the end.
    fn read_into(&self, out: &mut [u8], wait: Duration) -> io::Result<usize> {
        let until = Instant::now() + wait;
        let mut b = self.m.lock().unwrap();
        while b.q.is_empty() {
            if b.eof || b.closed {
                return Ok(0);
            }
            let now = Instant::now();
            if now >= until {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "stream starved"));
            }
            b = self.cv.wait_timeout(b, until - now).unwrap().0;
        }
        let n = out.len().min(b.q.len());
        let (a, c) = b.q.as_slices();
        let k = n.min(a.len());
        out[..k].copy_from_slice(&a[..k]);
        out[k..n].copy_from_slice(&c[..n - k]);
        b.q.drain(..n);
        if b.q.len() < MIN_AHEAD && b.starved.is_none() {
            b.starved = Some(Instant::now());
        }
        self.cv.notify_all();
        Ok(n)
    }
}

/// The decoder's end of a pipe: a non-seekable symphonia `MediaSource`.
pub struct PipeReader {
    pipe: Arc<Pipe>,
    /// How long a read may wait for data (long while opening, short while playing).
    wait: Arc<Mutex<Duration>>,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let wait = *self.wait.lock().unwrap();
        let n = buf.len().min(READ_MAX);
        self.pipe.read_into(&mut buf[..n], wait)
    }
}

impl Seek for PipeReader {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "live stream"))
    }
}

impl symphonia::core::io::MediaSource for PipeReader {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

impl Drop for PipeReader {
    fn drop(&mut self) {
        self.pipe.close();
    }
}

// ---- network ----------------------------------------------------------------------------------

/// An opened stream: headers read, body not yet.
pub struct Conn {
    body: Box<dyn Read + Send>,
    metaint: usize,
    pub content_type: String,
    pub kbps: Option<u32>,
}

/// No overall or body timeout (the body never ends); connect and response timeouts, and one
/// on each read.
fn stream_agent() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| agent_reading_within(READ_TIMEOUT))
}

fn agent_reading_within(read: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(8)))
        .timeout_recv_response(Some(Duration::from_secs(8)))
        .timeout_per_read(Some(read))
        .max_idle_connections(0)
        .build()
        .into()
}

/// `icy-br` ("128", sometimes "128,128"), sanity-checked.
pub fn parse_kbps(v: &str) -> Option<u32> {
    v.split(',')
        .next()?
        .trim()
        .parse()
        .ok()
        .filter(|k| (8..=1500).contains(k))
}

pub fn connect(url: &str) -> Result<Conn> {
    let resp = stream_agent()
        .get(url)
        .header("User-Agent", crate::net::BROWSER_UA)
        .header("Referer", super::REFERER)
        .header("Icy-MetaData", "1")
        .call()
        .map_err(|e| Error::Net(format!("{}: {e}", crate::net::short(url))))?;
    let h = |k: &str| {
        resp.headers()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
    };
    let content_type = h("content-type").unwrap_or_default().to_ascii_lowercase();
    if content_type.starts_with("text/") || content_type.contains("mpegurl") {
        return Err(Error::Media(format!(
            "not an audio stream ({content_type})"
        )));
    }
    Ok(Conn {
        metaint: h("icy-metaint").and_then(|m| m.parse().ok()).unwrap_or(0),
        kbps: h("icy-br").as_deref().and_then(parse_kbps),
        content_type,
        body: Box::new(resp.into_body().into_reader()),
    })
}

/// Shared by the connection threads of one playing station.
struct Feed {
    pipe: Arc<Pipe>,
    play_id: String,
    channel: String,
    url: Mutex<String>,
    events: Option<Sender<Event>>,
    title: Mutex<Option<String>>,
    /// Failed connections in a row, the watchdog's replacements included.
    fails: AtomicU32,
    /// It stopped trying: the source ends with [`NOT_RESPONDING`].
    gave_up: AtomicBool,
}

impl Feed {
    fn new(
        pipe: Arc<Pipe>,
        play_id: &str,
        channel: &str,
        url: String,
        events: Option<Sender<Event>>,
    ) -> Arc<Self> {
        Arc::new(Feed {
            pipe,
            play_id: play_id.to_string(),
            channel: channel.to_string(),
            url: Mutex::new(url),
            events,
            title: Mutex::new(None),
            fails: AtomicU32::new(0),
            gave_up: AtomicBool::new(false),
        })
    }

    /// Counts a failure; returns how many in a row.
    fn failed(&self) -> u32 {
        self.fails.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Too many failures: the pipe ends, and with it the source.
    fn give_up(&self, generation: u64) {
        log::warn!("{}: not responding, giving up", self.play_id);
        self.gave_up.store(true, Ordering::Relaxed);
        self.pipe.finish(generation);
    }

    fn title(&self, t: &str) {
        let mut last = self.title.lock().unwrap();
        if last.as_deref() == Some(t) {
            return;
        }
        *last = Some(t.to_string());
        log::info!("{}: now playing \u{201c}{t}\u{201d}", self.play_id);
        if let Some(tx) = &self.events {
            let _ = tx.send(Event::Title {
                id: self.play_id.clone(),
                title: t.to_string(),
            });
        }
    }

    fn spawn(self: &Arc<Self>, generation: u64, first: Option<Conn>) {
        let feed = self.clone();
        let spawned = std::thread::Builder::new()
            .name("radio-net".into())
            .spawn(move || feed.run(generation, first));
        if let Err(e) = spawned {
            log::error!("radio: no network thread: {e}");
            self.pipe.finish(generation);
        }
    }

    /// One connection after another until the reader is gone or the station is unreachable.
    fn run(&self, generation: u64, mut conn: Option<Conn>) {
        loop {
            let c = match conn.take() {
                Some(c) => c,
                None => {
                    let fails = self.fails.load(Ordering::Relaxed);
                    let backoff = Duration::from_millis(500 << fails.min(4));
                    if fails > 0 && !self.pipe.sleep(generation, backoff) {
                        return;
                    }
                    // The station may have moved: ask Radio Garden again after a failure.
                    if fails >= 1
                        && let Ok(u) = super::stream_url(&self.channel)
                    {
                        *self.url.lock().unwrap() = u;
                    }
                    let url = self.url.lock().unwrap().clone();
                    match connect(&url) {
                        Ok(c) => c,
                        Err(e) => {
                            let fails = self.failed();
                            log::warn!("{}: reconnect {fails} failed: {e}", self.play_id);
                            if fails > RETRIES {
                                return self.give_up(generation);
                            }
                            continue;
                        }
                    }
                }
            };
            if !self.pipe.live(generation) {
                return;
            }
            let since = Instant::now();
            let why = self.pump(c, generation);
            if !self.pipe.live(generation) {
                return;
            }
            if since.elapsed() >= HELD {
                self.fails.store(0, Ordering::Relaxed);
            }
            let fails = self.failed();
            log::info!("{}: {why}, reconnecting ({fails})", self.play_id);
            if fails > RETRIES {
                return self.give_up(generation);
            }
        }
    }

    /// Copies one connection's audio into the pipe. Returns why it stopped.
    fn pump(&self, c: Conn, generation: u64) -> String {
        let Conn {
            mut body, metaint, ..
        } = c;
        let mut demux = IcyDemux::new(metaint);
        let mut buf = vec![0u8; READ_CHUNK];
        let mut last_meta: Vec<u8> = Vec::new();
        loop {
            let n = match body.read(&mut buf) {
                Ok(0) => return "stream ended".into(),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return format!("stream read: {e}"),
            };
            let audio = demux.feed(&mut buf[..n], |m| {
                // Servers repeat the same block every interval: parse only changes.
                if m != last_meta.as_slice() {
                    last_meta.clear();
                    last_meta.extend_from_slice(m);
                    if let Some(t) = super::icy::stream_title(m) {
                        self.title(&t);
                    }
                }
            });
            if audio > 0 && !self.pipe.push(generation, &buf[..audio]) {
                return "closed".into();
            }
        }
    }
}

// ---- source -----------------------------------------------------------------------------------

/// A playing station, as the engine's source.
pub struct LiveSource {
    pipe: Arc<Pipe>,
    play_id: String,
    /// The network side; none for a Listen Along relay (it never reconnects by itself).
    feed: Option<Arc<Feed>>,
    fmt: Box<dyn FormatReader>,
    dec: Box<dyn AudioDecoder>,
    track_id: u32,
    rate: u32,
    tmp: Vec<f32>,
    errors: u32,
    /// "MP3 · 128 kbit/s".
    pub quality: String,
}

impl LiveSource {
    pub fn buffered(&self) -> usize {
        self.pipe.buffered()
    }
}

impl Source for LiveSource {
    fn rate(&self) -> u32 {
        self.rate
    }

    fn total_frames(&self) -> Option<u64> {
        None
    }

    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        use symphonia::core::errors::Error as E;
        let p = match self.fmt.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => return self.ended(),
            Err(E::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return self.ended(),
            // The pipe ran dry mid-packet for PACKET_WAIT: skipping would lose bytes (see there).
            Err(E::IoError(e)) => return Err(Error::Net(format!("radio stream: {e}"))),
            Err(e) => return self.error(format!("demux: {e}")),
        };
        if p.track_id != self.track_id {
            return Ok(true);
        }
        let ab = match self.dec.decode(&p) {
            Ok(ab) => ab,
            // A frame cut by a reconnect, or a damaged one: skip it.
            Err(e) => return self.error(format!("decode: {e}")),
        };
        if ab.spec().rate() != self.rate {
            return Err(Error::Media(format!(
                "stream changed rate ({} to {} Hz)",
                self.rate,
                ab.spec().rate()
            )));
        }
        let ch = ab.spec().channels().count().max(1);
        self.tmp.resize(ab.samples_interleaved(), 0.0);
        ab.copy_to_slice_interleaved(&mut self.tmp);
        push_stereo(out, &self.tmp, ch, 1.0);
        self.errors = 0;
        Ok(true)
    }

    fn seek(&mut self, _frame: u64) -> Result<()> {
        Err(Error::Media("live radio can't seek".into()))
    }

    fn live(&self) -> bool {
        true
    }

    fn stalled(&self) -> bool {
        let pipe = &self.pipe;
        if pipe.ready(MIN_AHEAD) {
            return false;
        }
        // A connection that went quiet without closing: replace it, unless that has failed
        // too often already. (A relay just waits.)
        if let Some(feed) = &self.feed
            && pipe.starved_for().is_some_and(|d| d >= STALL)
        {
            let fails = feed.failed();
            let generation = pipe.next_generation();
            if fails > RETRIES {
                feed.give_up(generation);
            } else {
                log::warn!("{}: stalled, reconnecting ({fails})", self.play_id);
                feed.spawn(generation, None);
            }
        }
        true
    }
}

impl LiveSource {
    /// The stream ended: as streams do (a relay's), or because the station stopped answering.
    fn ended(&self) -> Result<bool> {
        if self
            .feed
            .as_ref()
            .is_some_and(|f| f.gave_up.load(Ordering::Relaxed))
        {
            return Err(Error::Net(NOT_RESPONDING.into()));
        }
        Ok(false)
    }

    /// A bad packet: skip it, unless they keep coming.
    fn error(&mut self, e: String) -> Result<bool> {
        self.errors += 1;
        if self.errors == 1 {
            log::warn!("{}: {e}", self.play_id);
        }
        if self.errors > 200 {
            return Err(Error::Media(e));
        }
        Ok(true)
    }
}

/// What symphonia should expect from a content type.
fn hint(content_type: &str) -> Hint {
    let mut h = Hint::new();
    let ct = content_type.split(';').next().unwrap_or("").trim();
    match ct {
        "audio/mpeg" | "audio/mp3" | "audio/mpeg3" | "audio/x-mpeg" => {
            h.with_extension("mp3");
        }
        "audio/aac" | "audio/aacp" | "audio/x-aac" | "audio/x-aacp" => {
            h.with_extension("aac");
        }
        _ => {}
    }
    if !ct.is_empty() {
        h.mime_type(ct);
    }
    h
}

/// "MP3 · 128 kbit/s".
pub fn quality(content_type: &str, kbps: Option<u32>) -> String {
    let ct = content_type.split(';').next().unwrap_or("").trim();
    let codec = if ct.contains("aac") {
        "AAC"
    } else if ct.contains("mpeg") || ct.contains("mp3") {
        "MP3"
    } else {
        "Stream"
    };
    match kbps {
        Some(k) => format!("{codec} \u{b7} {k} kbit/s"),
        None => codec.to_string(),
    }
}

/// Opens station `id` (`radio:<channel>`): resolves, connects, probes and buffers about 0.75 s.
/// Blocks on the network (run it off the decoder thread). ICY titles go to `events`.
pub fn open(id: &str, events: Option<Sender<Event>>) -> Result<LiveSource> {
    let channel = super::channel_of(id)
        .ok_or_else(|| Error::Unavailable(format!("{id} is not a station")))?;
    let url = super::stream_url(channel)?;
    let conn = connect(&url)?;
    open_conn(id, channel, url, conn, events)
}

/// A source decoding a Listen Along relay's pipe: no network side, so it never reconnects.
pub(super) fn from_relay(
    pipe: Arc<Pipe>,
    play_id: &str,
    content_type: &str,
    kbps: Option<u32>,
) -> Result<LiveSource> {
    from_pipe(pipe, play_id, None, content_type, kbps)
}

/// [`open`] with the connection already made (tests and probes).
pub fn open_conn(
    id: &str,
    channel: &str,
    url: String,
    conn: Conn,
    events: Option<Sender<Event>>,
) -> Result<LiveSource> {
    let pipe = Pipe::new();
    let ct = conn.content_type.clone();
    let kbps = conn.kbps;
    // Hosting Listen Along: a new epoch, announced before the first byte.
    if let Some(t) = super::relay::capture(id, &ct, kbps) {
        pipe.set_tap(t);
    }
    let feed = Feed::new(pipe.clone(), id, channel, url, events);
    feed.spawn(0, Some(conn));
    from_pipe(feed.pipe.clone(), id, Some(feed), &ct, kbps)
}

/// Probes and buffers a pipe someone else fills (the station's [`Feed`], or a relay).
///
/// Given the same bytes, what this decodes doesn't depend on when they arrive: reads that
/// find the pipe short only wait (in arbitrary chunks, which symphonia's buffered stream
/// hides), and a wait that runs out fails the open or ends the source ([`PACKET_WAIT`]) rather
/// than dropping what was half read.
fn from_pipe(
    pipe: Arc<Pipe>,
    play_id: &str,
    feed: Option<Arc<Feed>>,
    content_type: &str,
    kbps: Option<u32>,
) -> Result<LiveSource> {
    if !pipe.wait_for(MIN_AHEAD, OPEN_WAIT) {
        // Stops the network side too (it would otherwise wait on a full pipe forever).
        pipe.close();
        return Err(Error::Net("the station sent no audio".into()));
    }
    let wait = Arc::new(Mutex::new(OPEN_WAIT));
    let reader = PipeReader {
        pipe: pipe.clone(),
        wait: wait.clone(),
    };
    let mss = MediaSourceStream::new(
        Box::new(reader),
        MediaSourceStreamOptions {
            buffer_len: 64 * 1024,
        },
    );
    let fmt = symphonia::default::get_probe()
        .probe(
            &hint(content_type),
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| Error::Media(format!("radio stream ({content_type}): {e}")))?;
    let track = fmt
        .default_track(TrackType::Audio)
        .ok_or_else(|| Error::Media("radio: no audio track".into()))?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| Error::Media("radio: no audio parameters".into()))?
        .clone();
    let track_id = track.id;
    let rate = params
        .sample_rate
        .filter(|&r| r > 0)
        .ok_or_else(|| Error::Media("radio: no sample rate".into()))?;
    let dec = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| Error::Media(format!("radio decoder: {e}")))?;
    // About 0.75 s ahead of what the decoder holds back: enough to ride out jitter, with
    // little delay. (Servers that burst on connect fill this at once.)
    let per_sec = kbps.map_or(16_000, |k| k as usize * 125);
    let ahead = MIN_AHEAD + READ_MAX + (per_sec * 3 / 4).min(64 * 1024);
    pipe.wait_for(ahead, OPEN_WAIT);
    // From here reads only happen when enough is buffered, so they rarely wait at all.
    *wait.lock().unwrap() = PACKET_WAIT;
    Ok(LiveSource {
        pipe,
        play_id: play_id.to_string(),
        feed,
        fmt,
        dec,
        track_id,
        rate,
        tmp: Vec::new(),
        errors: 0,
        quality: quality(content_type, kbps),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// An ICY stream: a title block after every `metaint` audio bytes.
    fn icy(audio: &[u8], metaint: usize, title: impl Fn(usize) -> String) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, chunk) in audio.chunks(metaint).enumerate() {
            out.extend_from_slice(chunk);
            if chunk.len() == metaint {
                let m = format!("StreamTitle='{}';", title(i));
                let blocks = m.len().div_ceil(16);
                out.push(blocks as u8);
                out.extend_from_slice(m.as_bytes());
                out.resize(out.len() + blocks * 16 - m.len(), 0);
            }
        }
        out
    }

    /// Hands out small, uneven chunks, like a socket.
    struct Trickle(Cursor<Vec<u8>>, usize);
    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.1 = self.1 % 7 + 1;
            let n = buf.len().min(self.1 * 97);
            self.0.read(&mut buf[..n])
        }
    }

    fn feed(events: Option<Sender<Event>>) -> Arc<Feed> {
        Feed::new(Pipe::new(), "radio:test", "test", String::new(), events)
    }

    fn conn(body: Vec<u8>, metaint: usize) -> Conn {
        Conn {
            body: Box::new(Trickle(Cursor::new(body), 0)),
            metaint,
            content_type: "audio/mpeg".into(),
            kbps: Some(128),
        }
    }

    #[test]
    fn pump_strips_metadata_and_reports_changed_titles() {
        let audio: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 256) as u8).collect();
        let stream = icy(&audio, 1500, |i| {
            if i < 3 {
                "A - One".into()
            } else {
                "B - Two".into()
            }
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        let f = feed(Some(tx));
        let why = f.pump(conn(stream, 1500), 0);
        assert_eq!(why, "stream ended");
        let titles: Vec<String> = rx
            .try_iter()
            .map(|e| match e {
                Event::Title { title, .. } => title,
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(
            titles,
            ["A - One", "B - Two"],
            "repeated blocks are reported once"
        );
        f.pipe.finish(0);
        let mut r = PipeReader {
            pipe: f.pipe.clone(),
            wait: Arc::new(Mutex::new(Duration::from_millis(10))),
        };
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, audio);
    }

    #[test]
    fn pipe_generations_and_close() {
        let pipe = Pipe::new();
        assert!(!pipe.ready(4));
        assert!(pipe.push(0, b"hello"));
        let g = pipe.next_generation();
        assert!(!pipe.push(0, b"x"), "a replaced connection can't write");
        assert!(pipe.push(g, b"!"));
        pipe.finish(0); // stale: ignored
        assert!(!pipe.ready(100));
        let mut buf = [0u8; 16];
        let n = pipe.read_into(&mut buf, Duration::from_millis(1)).unwrap();
        assert_eq!(&buf[..n], b"hello!");
        assert!(
            pipe.read_into(&mut buf, Duration::from_millis(1)).is_err(),
            "starved"
        );
        assert!(pipe.starved_for().is_some());
        pipe.close();
        assert!(!pipe.push(g, b"x"), "reader gone");
        assert_eq!(pipe.read_into(&mut buf, Duration::ZERO).unwrap(), 0);
    }

    /// MPEG-1 Layer III frames of digital silence: 128 kbit/s, 44.1 kHz, stereo, no side info.
    fn silent_mp3(frames: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..frames {
            let mut f = vec![0u8; 417];
            f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            out.extend_from_slice(&f);
        }
        out
    }

    #[test]
    fn decodes_an_icy_mp3_stream_through_the_pipe() {
        let stream = icy(&silent_mp3(60), 4000, |_| "X - Y".into());
        let f = feed(None);
        let pump = {
            let f = f.clone();
            std::thread::spawn(move || {
                f.pump(conn(stream, 4000), 0);
                f.pipe.finish(0);
            })
        };
        let mut src = from_pipe(
            f.pipe.clone(),
            "radio:test",
            Some(f),
            "audio/mpeg",
            Some(128),
        )
        .unwrap();
        assert_eq!(src.rate(), 44_100);
        assert!(src.live() && src.total_frames().is_none());
        assert!(src.seek(0).is_err());
        let mut pcm = Vec::new();
        while src.read(&mut pcm).unwrap() {}
        pump.join().unwrap();
        let frames = pcm.len() / 2;
        assert!((55 * 1152..=60 * 1152).contains(&frames), "{frames} frames");
        assert!(pcm.iter().all(|s| s.abs() < 1e-6));
        assert_eq!(src.quality, "MP3 \u{b7} 128 kbit/s");
    }

    /// MPEG-1 Layer III frames (128 kbit/s, 44.1 kHz, stereo): plain side info over
    /// pseudo-random Huffman data. Not music, but varied, non-silent samples.
    fn noisy_mp3(frames: usize) -> Vec<u8> {
        let mut seed = 0x2545_f491_u32;
        let mut out = Vec::new();
        for _ in 0..frames {
            let mut f = vec![0u8; 417];
            f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            for b in &mut f[36..] {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *b = (seed >> 24) as u8;
            }
            // Side info (32 bytes): main_data_begin 0 (no reservoir), private, scfsi, then per
            // granule and channel: part2_3_length, big_values, global_gain, scalefac_compress,
            // no window switching, table_select ×3, region0/1 counts, flags.
            let mut bits: Vec<(u32, u32)> = vec![(0, 9), (0, 3), (0, 8)];
            for _ in 0..4 {
                bits.extend([(700, 12), (120, 9), (170, 8), (0, 4), (0, 1)]);
                bits.extend([
                    (7, 5),
                    (7, 5),
                    (7, 5),
                    (7, 4),
                    (7, 3),
                    (0, 1),
                    (0, 1),
                    (0, 1),
                ]);
            }
            let mut pos = 32;
            for (v, n) in bits {
                for i in (0..n).rev() {
                    if v >> i & 1 == 1 {
                        f[pos / 8] |= 0x80 >> (pos % 8);
                    }
                    pos += 1;
                }
            }
            assert_eq!(pos, 36 * 8);
            out.extend_from_slice(&f);
        }
        out
    }

    /// Decodes to the end like the engine does: reads only when not stalled.
    fn decode_all(mut src: LiveSource) -> Vec<f32> {
        let mut pcm = Vec::new();
        loop {
            if src.stalled() {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            if !src.read(&mut pcm).unwrap() {
                return pcm;
            }
        }
    }

    #[test]
    fn a_relay_fed_the_tap_decodes_bit_identical_pcm() {
        use crate::radio::relay::{self, TapEvent};
        #[derive(Default)]
        struct Log {
            starts: Vec<(u64, String, String, Option<u32>)>,
            bytes: Vec<u8>,
            ends: Vec<u64>,
        }
        let audio = noisy_mp3(90);
        let stream = icy(&audio, 3000, |i| format!("Song {i}"));
        let log = Arc::new(Mutex::new(Log::default()));
        let l = log.clone();
        relay::set_tap(Some(Arc::new(move |e: TapEvent<'_>| {
            let mut l = l.lock().unwrap();
            match e {
                TapEvent::Start {
                    epoch,
                    id,
                    content_type,
                    kbps,
                } => l.starts.push((epoch, id.into(), content_type.into(), kbps)),
                TapEvent::Data { epoch, bytes } => {
                    assert_eq!(Some(epoch), l.starts.last().map(|s| s.0));
                    l.bytes.extend_from_slice(bytes);
                }
                TapEvent::End { epoch } => l.ends.push(epoch),
            }
        })));
        // The host: a station opened while hosting, as `open_conn` does.
        let f = feed(None);
        f.pipe
            .set_tap(relay::capture("radio:test", "audio/mpeg", Some(128)).unwrap());
        relay::set_tap(None); // a running station keeps tapping until its next bytes...
        let host = {
            let f = f.clone();
            std::thread::spawn(move || {
                f.pump(conn(stream, 3000), 0);
                f.pipe.finish(0);
            })
        };
        let src = from_pipe(
            f.pipe.clone(),
            "radio:test",
            Some(f),
            "audio/mpeg",
            Some(128),
        );
        host.join().unwrap();
        let host_pcm = decode_all(src.unwrap());
        assert!(host_pcm.len() / 2 > 80 * 1152, "{} samples", host_pcm.len());
        assert!(host_pcm.iter().any(|s| s.abs() > 1e-3), "not silence");

        let log = log.lock().unwrap();
        // ...which it didn't get: clearing the tap ended the epoch at once.
        assert_eq!(log.starts.len(), 1);
        assert_eq!(log.ends, [log.starts[0].0]);
        assert!(log.bytes.is_empty());
        drop(log);

        // Tapping for real this time.
        let log = Arc::new(Mutex::new(Log::default()));
        let l = log.clone();
        relay::set_tap(Some(Arc::new(move |e: TapEvent<'_>| {
            let mut l = l.lock().unwrap();
            match e {
                TapEvent::Start { epoch, .. } => l.starts.push((epoch, "".into(), "".into(), None)),
                TapEvent::Data { bytes, .. } => l.bytes.extend_from_slice(bytes),
                TapEvent::End { epoch } => l.ends.push(epoch),
            }
        })));
        let f = feed(None);
        f.pipe
            .set_tap(relay::capture("radio:test", "audio/mpeg", Some(128)).unwrap());
        let stream = icy(&audio, 3000, |i| format!("Song {i}"));
        let host = {
            let f = f.clone();
            std::thread::spawn(move || {
                f.pump(conn(stream, 3000), 0);
                f.pipe.finish(0);
            })
        };
        let src = from_pipe(
            f.pipe.clone(),
            "radio:test",
            Some(f),
            "audio/mpeg",
            Some(128),
        );
        host.join().unwrap();
        let host_pcm = decode_all(src.unwrap());
        relay::set_tap(None);
        let tapped = {
            let l = log.lock().unwrap();
            assert_eq!(l.starts.len(), 1);
            assert_eq!(l.ends, [l.starts[0].0], "one End, after the last byte");
            l.bytes.clone()
        };
        assert_eq!(tapped, audio, "the tap sees exactly the decoder's bytes");

        // Listeners: the same bytes, cut differently and arriving at their own pace.
        for chunk in [1, 13, 417, 4000, usize::MAX] {
            let id = format!("radio:listener{}", chunk.min(99_999));
            let r = relay::expect_relay(&id);
            let feeder = {
                let r = r.clone();
                let bytes = tapped.clone();
                std::thread::spawn(move || {
                    r.start("audio/mpeg", Some(128));
                    for (i, c) in bytes.chunks(chunk).enumerate() {
                        r.push(c);
                        if i % 50 == 0 {
                            std::thread::yield_now();
                        }
                    }
                    r.finish();
                })
            };
            let r = relay::take_relay(&id).unwrap();
            let src = relay::open_relay(&id, r, None).unwrap();
            assert!(src.feed.is_none());
            let pcm = decode_all(src);
            feeder.join().unwrap();
            assert_eq!(pcm.len(), host_pcm.len(), "chunk {chunk}");
            assert!(
                pcm.iter()
                    .zip(&host_pcm)
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "chunk {chunk}: PCM differs"
            );
        }
    }

    /// A station that sends its headers and some audio, then nothing (without closing): the
    /// read gives up instead of blocking its thread forever.
    #[test]
    fn a_silent_connection_times_out() {
        use std::io::Write;
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/stream", server.local_addr().unwrap());
        let held = std::thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            let mut req = [0u8; 1024];
            let _ = s.read(&mut req);
            s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n",
            )
            .unwrap();
            s.flush().unwrap();
            std::thread::sleep(Duration::from_secs(2));
            drop(s);
        });
        let resp = agent_reading_within(Duration::from_millis(300))
            .get(&url)
            .call()
            .unwrap();
        let mut body = resp.into_body().into_reader();
        let mut buf = [0u8; 64];
        let t = Instant::now();
        let mut got = 0;
        let err = loop {
            match body.read(&mut buf) {
                Ok(0) => panic!("ended"),
                Ok(n) => got += n,
                Err(e) => break e,
            }
        };
        assert_eq!(got, 4);
        assert!(
            t.elapsed() < Duration::from_millis(1500),
            "{:?}: {err}",
            t.elapsed()
        );
        held.join().unwrap();
    }

    /// Failures count across the watchdog's replacements; once there were too many, the
    /// source ends with an error the app can show, not as if the station had ended.
    #[test]
    fn the_watchdog_carries_failures_and_gives_up() {
        let f = feed(None);
        assert!(f.pipe.push(0, &silent_mp3(80)));
        let mut src = from_pipe(
            f.pipe.clone(),
            "radio:test",
            Some(f.clone()),
            "audio/mpeg",
            Some(128),
        )
        .unwrap();
        let mut pcm = Vec::new();
        while !src.stalled() {
            assert!(src.read(&mut pcm).unwrap());
        }
        assert!(
            !f.gave_up.load(Ordering::Relaxed),
            "starved, not yet stalled"
        );
        // Earlier connections failed already; this stall is one too many.
        f.fails.store(RETRIES, Ordering::Relaxed);
        f.pipe.m.lock().unwrap().starved = Some(Instant::now() - STALL);
        assert!(src.stalled());
        assert!(f.gave_up.load(Ordering::Relaxed));
        assert_eq!(f.fails.load(Ordering::Relaxed), RETRIES + 1);
        let err = loop {
            match src.read(&mut pcm) {
                Ok(true) => {}
                Ok(false) => panic!("ended quietly"),
                Err(e) => break e,
            }
        };
        assert!(
            matches!(&err, Error::Net(m) if m == NOT_RESPONDING),
            "{err}"
        );
    }

    #[test]
    fn hints_and_quality() {
        assert_eq!(quality("audio/aacp", Some(64)), "AAC \u{b7} 64 kbit/s");
        assert_eq!(
            quality("audio/mpeg; charset=x", parse_kbps("320,320")),
            "MP3 \u{b7} 320 kbit/s"
        );
        assert_eq!(
            quality("application/octet-stream", parse_kbps("0")),
            "Stream"
        );
        assert_eq!(parse_kbps("abc"), None);
    }
}
