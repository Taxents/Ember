//! Discord Rich Presence: "Listening to Ember", with the song, its artists, cover and progress.
//!
//! The local Discord client is spoken to over its named pipe (`\\.\pipe\discord-ipc-N`) on a
//! thread of its own, so a slow or missing Discord never holds up the window or the audio. The
//! UI thread sends what should show whenever it changes (see [`App::discord_tick`]); the thread
//! keeps only the latest and sends it no faster than Discord allows.

use crate::*;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

/// The Discord application presence shows as: its name ("Ember") is what Discord prints after
/// "Listening to". From the Developer Portal (Applications → Ember → General Information →
/// Application ID).
const DISCORD_APP_ID: &str = "1563863163911413860";
/// The Ember logo, uploaded to that application under Rich Presence → Art Assets.
const LOGO_ASSET: &str = "ember";
/// Discord takes about 5 activity updates per 20 seconds.
const RATE_COUNT: usize = 5;
const RATE_WINDOW: Duration = Duration::from_secs(20);
/// Changes coming quickly (skipping through songs) are let settle for this long.
const SETTLE: Duration = Duration::from_millis(400);
/// What shows is sent again this often, so a Discord restarted meanwhile shows it again.
const REFRESH: Duration = Duration::from_secs(60);
/// Moved by more than this against what Discord has (a seek): it's told again.
const DRIFT_MS: u64 = 2000;

/// The "Show what you're listening to on Discord" setting.
pub static ENABLED: AtomicBool = AtomicBool::new(true);

/// What's playing, as far as Discord is concerned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Now {
    pub title: String,
    pub artists: String,
    pub album: String,
    /// The cover, as an https URL Discord can fetch.
    pub art: Option<String>,
    /// The song on YouTube Music.
    pub link: Option<String>,
    pub playing: bool,
    pub position_ms: u64,
    /// 0 when not known (or live).
    pub duration_ms: u64,
    /// Live radio: `title` is the station, `artists` what it plays now (or its place).
    pub station: bool,
}

/// What Discord is asked to show.
#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    details: String,
    state: String,
    large_image: String,
    large_text: String,
    /// The Ember logo in the corner (left out when it's the large image already).
    small: bool,
    /// Unix milliseconds the song (or station) started at, and the song ends at.
    start_ms: Option<u64>,
    end_ms: Option<u64>,
    button: Option<String>,
}

impl Activity {
    /// The same as far as anyone would see: equal, but for a few seconds' drift of the clock.
    fn same(&self, o: &Activity) -> bool {
        let near = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (Some(a), Some(b)) => a.abs_diff(b) <= DRIFT_MS,
            (a, b) => a == b,
        };
        near(self.start_ms, o.start_ms)
            && near(self.end_ms, o.end_ms)
            && Activity {
                start_ms: None,
                end_ms: None,
                ..self.clone()
            } == Activity {
                start_ms: None,
                end_ms: None,
                ..o.clone()
            }
    }

    /// The `activity` of a SET_ACTIVITY command.
    pub fn json(&self) -> Value {
        let mut assets = json!({
            "large_image": self.large_image,
            "large_text": self.large_text,
        });
        if self.small {
            assets["small_image"] = json!(LOGO_ASSET);
            assets["small_text"] = json!("Ember");
        }
        // Type 2, "Listening to …", and the status says the artists (the state) rather than
        // "Ember" (status display type 1).
        let mut a = json!({
            "type": 2,
            "status_display_type": 1,
            "details": self.details,
            "state": self.state,
            "assets": assets,
        });
        if let Some(start) = self.start_ms {
            a["timestamps"] = json!({ "start": start });
            if let Some(end) = self.end_ms {
                a["timestamps"]["end"] = json!(end);
            }
        }
        if let Some(url) = &self.button {
            a["buttons"] = json!([{ "label": "Listen on YouTube Music", "url": url }]);
        }
        a
    }
}

/// Discord wants 2 to 128 characters in each text.
fn text(s: &str) -> String {
    let s = s.trim();
    let mut out: String = if s.chars().count() > 128 {
        s.chars().take(127).chain(['…']).collect()
    } else {
        s.to_string()
    };
    while out.chars().count() < 2 {
        out.push(' ');
    }
    out
}

/// What Discord should show for `now` at unix time `now_ms`; `None` clears it. Only what's
/// playing shows: paused, Discord shows nothing.
pub fn activity(now: Option<&Now>, now_ms: u64) -> Option<Activity> {
    let n = now?;
    if !n.playing || n.title.trim().is_empty() {
        return None;
    }
    let art = n
        .art
        .clone()
        .filter(|u| u.starts_with("https://") && u.len() <= 256);
    let start = now_ms.saturating_sub(n.position_ms);
    let (state, start_ms, end_ms) = if n.station || n.duration_ms == 0 {
        // Live: how long it's been on.
        (n.artists.clone(), Some(start), None)
    } else {
        (n.artists.clone(), Some(start), Some(start + n.duration_ms))
    };
    let large_text = if n.station {
        "Live radio".to_string()
    } else if n.album.trim().is_empty() {
        n.title.clone()
    } else {
        n.album.clone()
    };
    Some(Activity {
        details: text(&n.title),
        state: text(&state),
        small: art.is_some(),
        large_image: art.unwrap_or_else(|| LOGO_ASSET.to_string()),
        large_text: text(&large_text),
        start_ms,
        end_ms,
        button: n
            .link
            .clone()
            .filter(|u| !n.station && u.starts_with("https://") && u.len() <= 512),
    })
}

/// At most `RATE_COUNT` sends in any `RATE_WINDOW`.
#[derive(Debug, Default)]
pub struct Limiter {
    sent: std::collections::VecDeque<Instant>,
}

impl Limiter {
    /// The soonest the next send may go, from `now`.
    pub fn next(&mut self, now: Instant) -> Instant {
        while self
            .sent
            .front()
            .is_some_and(|&t| now.duration_since(t) >= RATE_WINDOW)
        {
            self.sent.pop_front();
        }
        if self.sent.len() < RATE_COUNT {
            now
        } else {
            self.sent[0] + RATE_WINDOW
        }
    }

    pub fn record(&mut self, at: Instant) {
        self.sent.push_back(at);
    }
}

// ---- the UI side ------------------------------------------------------------------------------

thread_local! {
    /// The thread's inbox, started with the first thing to show.
    static TX: RefCell<Option<crossbeam_channel::Sender<Option<Activity>>>> =
        const { RefCell::new(None) };
    /// What the thread was last asked for.
    static ASKED: RefCell<Option<Activity>> = const { RefCell::new(None) };
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Asks for `now` to show (or nothing), if that's different from what was last asked.
pub fn show(now: Option<&Now>) {
    let want = if ENABLED.load(Ordering::Relaxed) {
        activity(now, unix_ms())
    } else {
        None
    };
    let changed = ASKED.with(|a| {
        let mut a = a.borrow_mut();
        let same = match (&*a, &want) {
            (Some(x), Some(y)) => x.same(y),
            (x, y) => x.is_none() && y.is_none(),
        };
        if !same {
            *a = want.clone();
        }
        !same
    });
    if !changed {
        return;
    }
    TX.with(|tx| {
        let mut tx = tx.borrow_mut();
        if tx.is_none() {
            if want.is_none() {
                return;
            }
            let (s, r) = crossbeam_channel::unbounded();
            if let Err(e) = std::thread::Builder::new()
                .name("discord".into())
                .spawn(move || run(r))
            {
                log::warn!("discord: {e}");
                return;
            }
            *tx = Some(s);
        }
        if let Some(s) = &*tx {
            let _ = s.send(want);
        }
    });
}

impl App {
    /// Tells Discord what plays, from the bar as it shows (so local edits of a song's title
    /// and artists carry over). Called with every tick; only changes go further.
    pub(crate) fn discord_tick(&self, np: &NowPlaying) {
        let Some(id) = self.player.current().filter(|_| np.active) else {
            show(None);
            return;
        };
        let t = self.known.get(id);
        let station = radio::is_station(id);
        let now = Now {
            title: np.title.to_string(),
            artists: np.artist.to_string(),
            album: t
                .and_then(|t| t.album.as_ref())
                .map(|a| a.name.clone())
                .unwrap_or_default(),
            art: t
                .filter(|_| !station)
                .and_then(|t| images::url_for(&t.thumbs, 512))
                .and_then(|u| images::fetchable(&u)),
            link: t
                .filter(|_| !station)
                .map(|t| pages::link(&Item::Track(t.clone()))),
            playing: np.playing,
            position_ms: np.position_ms.max(0) as u64,
            duration_ms: np.duration_ms.max(0) as u64,
            station,
        };
        show(Some(&now));
    }
}

/// The setting changed: off clears what Discord shows.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

// ---- the thread -------------------------------------------------------------------------------

/// A connection to the Discord client.
struct Ipc {
    pipe: std::fs::File,
    nonce: u64,
}

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;

impl Ipc {
    fn connect(app_id: &str) -> std::io::Result<Ipc> {
        let mut last = std::io::Error::from(std::io::ErrorKind::NotFound);
        for i in 0..10 {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!(r"\\.\pipe\discord-ipc-{i}"))
            {
                Ok(pipe) => {
                    let mut c = Ipc { pipe, nonce: 0 };
                    c.write(OP_HANDSHAKE, &json!({ "v": 1, "client_id": app_id }))?;
                    c.reply()?;
                    return Ok(c);
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn write(&mut self, op: u32, v: &Value) -> std::io::Result<()> {
        let body = serde_json::to_vec(v)?;
        let mut frame = Vec::with_capacity(8 + body.len());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);
        self.pipe.write_all(&frame)?;
        self.pipe.flush()
    }

    /// Reads up to the answer to what was sent (answering pings on the way).
    fn reply(&mut self) -> std::io::Result<Value> {
        loop {
            let mut head = [0u8; 8];
            self.pipe.read_exact(&mut head)?;
            let op = u32::from_le_bytes(head[..4].try_into().unwrap());
            let len = u32::from_le_bytes(head[4..].try_into().unwrap()) as usize;
            if len > 1 << 20 {
                return Err(std::io::Error::other("frame too long"));
            }
            let mut body = vec![0u8; len];
            self.pipe.read_exact(&mut body)?;
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            match op {
                OP_FRAME => {
                    if v["evt"] == "ERROR" {
                        log::warn!("discord: {}", v["data"]["message"]);
                    }
                    return Ok(v);
                }
                OP_PING => self.write(OP_PONG, &v)?,
                OP_CLOSE => {
                    return Err(std::io::Error::other(format!(
                        "closed: {}",
                        v["message"].as_str().unwrap_or("")
                    )));
                }
                _ => {}
            }
        }
    }

    fn set_activity(&mut self, a: Option<&Activity>) -> std::io::Result<()> {
        self.nonce += 1;
        self.write(
            OP_FRAME,
            &json!({
                "cmd": "SET_ACTIVITY",
                "args": { "pid": std::process::id(), "activity": a.map(Activity::json) },
                "nonce": self.nonce.to_string(),
            }),
        )?;
        self.reply().map(|_| ())
    }
}

/// The thread: keeps the latest asked for and gets it to Discord, connecting (and again after
/// Discord closes, waiting longer each time it isn't there) only while there's something to show.
fn run(rx: crossbeam_channel::Receiver<Option<Activity>>) {
    let mut conn: Option<Ipc> = None;
    let mut want: Option<Activity> = None;
    // Not yet sent since it changed.
    let mut dirty = false;
    let mut settle = Instant::now();
    let mut limiter = Limiter::default();
    let mut retry = Instant::now();
    let mut backoff = Duration::from_secs(5);
    let mut told_missing = false;
    let mut last_sent = Instant::now();
    loop {
        let now = Instant::now();
        let at = if dirty {
            Some(settle.max(retry).max(limiter.next(now)))
        } else if want.is_some() {
            Some(last_sent + REFRESH)
        } else {
            None
        };
        let got = match at {
            Some(at) => rx.recv_deadline(at),
            None => rx
                .recv()
                .map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected),
        };
        match got {
            Ok(w) => {
                want = w;
                dirty = true;
                settle = Instant::now() + SETTLE;
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
        let Some(a) = &want else {
            // Nothing to show (or turned off): closing the pipe clears it.
            if conn.take().is_some() {
                log::info!("discord: cleared");
            }
            dirty = false;
            continue;
        };
        if conn.is_none() {
            match Ipc::connect(DISCORD_APP_ID) {
                Ok(c) => {
                    log::info!("discord: connected");
                    conn = Some(c);
                    backoff = Duration::from_secs(5);
                    told_missing = false;
                }
                Err(e) => {
                    if !told_missing {
                        log::info!("discord: not reachable ({e}), trying again later");
                        told_missing = true;
                    }
                    retry = Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(120));
                    dirty = true;
                    continue;
                }
            }
        }
        let c = conn.as_mut().expect("connected");
        let now = Instant::now();
        limiter.record(now);
        last_sent = now;
        match c.set_activity(Some(a)) {
            Ok(()) => dirty = false,
            Err(e) => {
                // Discord closed (or restarted): connect again and send it then.
                log::info!("discord: {e}");
                conn = None;
                retry = Instant::now() + Duration::from_secs(2);
                dirty = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song() -> Now {
        Now {
            title: "Song".into(),
            artists: "Artist A & Artist B".into(),
            album: "Album".into(),
            art: Some("https://lh3.googleusercontent.com/x=w512-h512-l90-rj".into()),
            link: Some("https://music.youtube.com/watch?v=abc".into()),
            playing: true,
            position_ms: 30_000,
            duration_ms: 200_000,
            station: false,
        }
    }

    #[test]
    fn playing_song() {
        let a = activity(Some(&song()), 1_000_000).unwrap();
        let j = a.json();
        assert_eq!(j["type"], 2);
        assert_eq!(j["status_display_type"], 1);
        assert_eq!(j["details"], "Song");
        assert_eq!(j["state"], "Artist A & Artist B");
        assert_eq!(j["timestamps"]["start"], 970_000);
        assert_eq!(j["timestamps"]["end"], 1_170_000);
        assert_eq!(
            j["assets"]["large_image"],
            "https://lh3.googleusercontent.com/x=w512-h512-l90-rj"
        );
        assert_eq!(j["assets"]["large_text"], "Album");
        assert_eq!(j["assets"]["small_image"], "ember");
        assert_eq!(j["assets"]["small_text"], "Ember");
        assert_eq!(j["buttons"][0]["label"], "Listen on YouTube Music");
        assert_eq!(
            j["buttons"][0]["url"],
            "https://music.youtube.com/watch?v=abc"
        );
        // Never branded as YouTube Music: the name comes from the Discord application.
        assert!(j.get("name").is_none());
    }

    #[test]
    fn paused_clears() {
        let n = Now {
            playing: false,
            ..song()
        };
        assert_eq!(activity(Some(&n), 1_000_000), None);
    }

    #[test]
    fn station() {
        let n = Now {
            title: "Radio X".into(),
            artists: "Now: Some Tune".into(),
            album: String::new(),
            art: None,
            link: None,
            duration_ms: 0,
            station: true,
            ..song()
        };
        let j = activity(Some(&n), 1_000_000).unwrap().json();
        assert_eq!(j["details"], "Radio X");
        assert_eq!(j["state"], "Now: Some Tune");
        assert_eq!(j["timestamps"]["start"], 970_000);
        assert!(j["timestamps"].get("end").is_none());
        assert_eq!(j["assets"]["large_image"], "ember");
        assert_eq!(j["assets"]["large_text"], "Live radio");
        assert!(j["assets"].get("small_image").is_none());
        assert!(j.get("buttons").is_none());
    }

    #[test]
    fn nothing_clears() {
        assert_eq!(activity(None, 0), None);
        let n = Now {
            title: " ".into(),
            ..song()
        };
        assert_eq!(activity(Some(&n), 0), None);
    }

    #[test]
    fn texts_fit_discord() {
        let n = Now {
            title: "x".repeat(300),
            artists: "A".into(),
            album: String::new(),
            art: Some("http://insecure/x.jpg".into()),
            ..song()
        };
        let a = activity(Some(&n), 1_000_000).unwrap();
        assert_eq!(a.details.chars().count(), 128);
        assert!(a.details.ends_with('…'));
        assert_eq!(a.state.chars().count(), 2);
        // No album: the title instead. No https cover: the logo.
        assert_eq!(a.large_text.chars().count(), 128);
        assert_eq!(a.large_image, "ember");
        assert!(!a.small);
    }

    #[test]
    fn drift_is_the_same_seek_is_not() {
        let a = activity(Some(&song()), 1_000_000).unwrap();
        // A tick later, the clock having moved with the song.
        let later = Now {
            position_ms: 30_250,
            ..song()
        };
        let b = activity(Some(&later), 1_000_400).unwrap();
        assert!(a.same(&b));
        let seeked = Now {
            position_ms: 90_000,
            ..song()
        };
        let c = activity(Some(&seeked), 1_000_400).unwrap();
        assert!(!a.same(&c));
    }

    /// Against the Discord running here: `EMBER_DISCORD_ID=<application id> cargo test -p
    /// music-app discord::tests::live -- --ignored` shows a song for 15 s. Without an id, a
    /// made-up one checks the pipe and framing (Discord answers it with a close).
    #[test]
    #[ignore]
    fn live() {
        let id = std::env::var("EMBER_DISCORD_ID").unwrap_or_default();
        if id.is_empty() {
            let e = Ipc::connect("1").err().expect("a made-up id is refused");
            eprintln!("{e}");
            assert!(e.to_string().starts_with("closed"), "{e}");
            return;
        }
        let mut c = Ipc::connect(&id).expect("connect");
        let a = activity(Some(&song()), unix_ms()).unwrap();
        c.set_activity(Some(&a)).expect("set");
        std::thread::sleep(Duration::from_secs(15));
        c.set_activity(None).expect("clear");
    }

    #[test]
    fn limiter_coalesces_to_five_per_twenty_seconds() {
        let t0 = Instant::now();
        let mut l = Limiter::default();
        for i in 0..5 {
            let t = t0 + Duration::from_secs(i);
            assert_eq!(l.next(t), t);
            l.record(t);
        }
        // The sixth waits for the first to leave the window.
        let t = t0 + Duration::from_secs(5);
        assert_eq!(l.next(t), t0 + RATE_WINDOW);
        let t = t0 + RATE_WINDOW;
        assert_eq!(l.next(t), t);
        l.record(t);
        // Now the second (at 1 s) holds the next back.
        let t = t0 + RATE_WINDOW + Duration::from_millis(500);
        assert_eq!(l.next(t), t0 + Duration::from_secs(1) + RATE_WINDOW);
    }
}
