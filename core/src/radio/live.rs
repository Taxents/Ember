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
//!   without closing is replaced by a watchdog after [`STALL`].
//! * Latency: about 0.75 s is buffered before playback starts, and the pipe holds at most
//!   [`CAP`] bytes, so a paused station never falls far behind.

use super::icy::IcyDemux;
use crate::audio::Event;
use crate::audio::source::{Source, push_stereo};
use crate::{Error, Result};
use crossbeam_channel::Sender;
use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
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
/// Longest wait for audio when opening.
const OPEN_WAIT: Duration = Duration::from_secs(8);
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
}

/// Bounded byte queue between the network thread and the decoder.
pub struct Pipe {
    m: Mutex<Buf>,
    cv: Condvar,
}

impl Pipe {
    pub fn new() -> Arc<Self> {
        Arc::new(Pipe {
            m: Mutex::new(Buf {
                q: VecDeque::with_capacity(64 * 1024),
                eof: false,
                closed: false,
                generation: 0,
                starved: Some(Instant::now()),
            }),
            cv: Condvar::new(),
        })
    }

    /// Appends, waiting while full. False once the reader is gone or `generation` was replaced.
    pub fn push(&self, generation: u64, data: &[u8]) -> bool {
        let mut b = self.m.lock().unwrap();
        while b.q.len() >= CAP && !b.closed && b.generation == generation {
            b = self.cv.wait(b).unwrap();
        }
        if b.closed || b.generation != generation {
            return false;
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
            self.cv.notify_all();
        }
    }

    /// The reader is gone: the network side stops.
    pub fn close(&self) {
        self.m.lock().unwrap().closed = true;
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

/// No overall or body timeout (the body never ends); connect and response timeouts only.
fn stream_agent() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(8)))
            .timeout_recv_response(Some(Duration::from_secs(8)))
            .max_idle_connections(0)
            .build()
            .into()
    })
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
}

impl Feed {
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
        let mut fails = 0u32;
        loop {
            let c = match conn.take() {
                Some(c) => c,
                None => {
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
                            fails += 1;
                            log::warn!("{}: reconnect {fails} failed: {e}", self.play_id);
                            if fails > RETRIES {
                                self.pipe.finish(generation);
                                return;
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
                fails = 0;
            }
            fails += 1;
            log::info!("{}: {why}, reconnecting ({fails})", self.play_id);
            if fails > RETRIES {
                self.pipe.finish(generation);
                return;
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
    feed: Arc<Feed>,
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
        self.feed.pipe.buffered()
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
            Ok(None) => return Ok(false),
            Err(E::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
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
        let pipe = &self.feed.pipe;
        if pipe.ready(MIN_AHEAD) {
            return false;
        }
        // A connection that went quiet without closing: replace it.
        if pipe.starved_for().is_some_and(|d| d >= STALL) {
            log::warn!("{}: stalled, reconnecting", self.feed.play_id);
            let generation = pipe.next_generation();
            self.feed.spawn(generation, None);
        }
        true
    }
}

impl LiveSource {
    /// A bad packet: skip it, unless they keep coming.
    fn error(&mut self, e: String) -> Result<bool> {
        self.errors += 1;
        if self.errors == 1 {
            log::warn!("{}: {e}", self.feed.play_id);
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
    let feed = Arc::new(Feed {
        pipe: pipe.clone(),
        play_id: id.to_string(),
        channel: channel.to_string(),
        url: Mutex::new(url),
        events,
        title: Mutex::new(None),
    });
    feed.spawn(0, Some(conn));
    from_pipe(feed, &ct, kbps)
}

fn from_pipe(feed: Arc<Feed>, content_type: &str, kbps: Option<u32>) -> Result<LiveSource> {
    let pipe = feed.pipe.clone();
    if !pipe.wait_for(MIN_AHEAD, OPEN_WAIT) {
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
    // From here reads only happen when enough is buffered; never wait long on the network.
    *wait.lock().unwrap() = Duration::from_millis(500);
    Ok(LiveSource {
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
        Arc::new(Feed {
            pipe: Pipe::new(),
            play_id: "radio:test".into(),
            channel: "test".into(),
            url: Mutex::new(String::new()),
            events,
            title: Mutex::new(None),
        })
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
        let mut src = from_pipe(f, "audio/mpeg", Some(128)).unwrap();
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
