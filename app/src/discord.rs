//! Discord Rich Presence: "Listening to Ember", with the song, its artists, cover and progress,
//! and, while hosting Listen Along, Discord's invites to it.
//!
//! The local Discord client is spoken to over its named pipe (`\\.\pipe\discord-ipc-N`) on a
//! thread of its own, so a slow or missing Discord never holds up the window or the audio. The
//! UI thread sends what should show whenever it changes (see [`App::discord_tick`]); the thread
//! keeps only the latest and sends it no faster than Discord allows.
//!
//! Invites: the host's activity carries a party and the Listen Along code as its join secret, so
//! Discord offers "Invite" (in a chat's + menu) and "Ask to Join" (on the host's profile). A
//! friend's Discord hands the code to their Ember (`ACTIVITY_JOIN`), starting it first if needs
//! be (Ember registers itself as the application's launch command), and it joins.

use crate::*;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
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
/// How often the pipe is looked at for invites (and for Discord having gone).
const POLL: Duration = Duration::from_millis(500);
/// Moved by more than this against what Discord has (a seek): it's told again.
const DRIFT_MS: u64 = 2000;

/// The "Show on Discord" setting.
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
    /// Hosting Listen Along: its invite code and how many are in it (the host too).
    pub party: Option<(String, usize)>,
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
    /// Listen Along: the party's id (not the code: everyone seeing the presence sees it), its
    /// size and most, and the code, which Discord gives only to those let in.
    party: Option<(String, usize, usize)>,
    join: Option<String>,
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
        // Type 2: "Listening to Ember".
        let mut a = json!({
            "type": 2,
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
        if let Some((id, size, max)) = &self.party {
            a["party"] = json!({ "id": id, "size": [size, max] });
        }
        if let Some(secret) = &self.join {
            a["secrets"] = json!({ "join": secret });
        } else if let Some(url) = &self.button {
            // Discord doesn't take buttons along with a join secret.
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

/// A party id for a Listen Along code that doesn't give the code away.
fn party_id(code: &str) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(code.as_bytes());
    let hex: String = h[..12].iter().map(|b| format!("{b:02x}")).collect();
    format!("ember-{hex}")
}

/// What Discord should show for `now` at unix time `now_ms`; `None` clears it.
pub fn activity(now: Option<&Now>, now_ms: u64) -> Option<Activity> {
    let n = now?;
    if n.title.trim().is_empty() {
        return None;
    }
    let art = n
        .art
        .clone()
        .filter(|u| u.starts_with("https://") && u.len() <= 256);
    let start = now_ms.saturating_sub(n.position_ms);
    let (state, start_ms, end_ms) = if !n.playing {
        let state = if n.artists.trim().is_empty() {
            "Paused".to_string()
        } else {
            format!("Paused · {}", n.artists.trim())
        };
        (state, None, None)
    } else if n.station || n.duration_ms == 0 {
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
        party: n
            .party
            .as_ref()
            .map(|(code, size)| (party_id(code), (*size).max(1), together::MAX_GUESTS + 1)),
        join: n.party.as_ref().map(|(code, _)| code.clone()),
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

/// To the thread.
enum Msg {
    Show(Option<Activity>),
    /// The setting: off closes the connection (and with it what shows).
    Enabled(bool),
    /// Let in someone who asked to join.
    LetIn(String),
}

/// From Discord, for the UI thread.
#[derive(Debug, PartialEq)]
enum Heard {
    /// A friend's invite was accepted here: the Listen Along code.
    Join(String),
    /// Someone asks to join (Discord user id, their name).
    JoinRequest(String, String),
}

// ---- the UI side ------------------------------------------------------------------------------

thread_local! {
    /// The thread's inbox.
    static TX: RefCell<Option<crossbeam_channel::Sender<Msg>>> = const { RefCell::new(None) };
    /// What the thread was last asked for.
    static ASKED: RefCell<Option<Activity>> = const { RefCell::new(None) };
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn send(m: Msg) {
    TX.with(|tx| {
        let mut tx = tx.borrow_mut();
        if tx.is_none() {
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
            let _ = s.send(m);
        }
    });
}

/// Asks for `now` to show (or nothing), if that's different from what was last asked.
fn show(now: Option<&Now>) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let want = activity(now, unix_ms());
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
    if changed {
        send(Msg::Show(want));
    }
}

/// The setting, at start and when it changes. On, the connection stays open (for invites) even
/// with nothing playing; off closes it, which clears what shows.
pub fn set_enabled(on: bool) {
    let was = ENABLED.swap(on, Ordering::Relaxed);
    if on == was && TX.with(|tx| tx.borrow().is_some()) {
        return;
    }
    // Shown again from the next tick.
    ASKED.with(|a| *a.borrow_mut() = None);
    if on || TX.with(|tx| tx.borrow().is_some()) {
        send(Msg::Enabled(on));
    }
    if on {
        std::thread::spawn(register);
    }
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
            party: self.together_party(),
        };
        show(Some(&now));
    }

    fn discord_heard(&mut self, h: Heard) {
        match h {
            Heard::Join(secret) => {
                let Some(code) = together::parse_code(&secret) else {
                    log::warn!("discord: a join that isn't a Listen Along code");
                    return;
                };
                log::info!("discord: joining from an invite");
                if self.together_party().is_some_and(|(c, _)| c == code) {
                    return; // our own
                }
                if self.together.is_some() {
                    self.together_leave("");
                }
                self.together_join(&code);
                self.ui().global::<Ui>().set_together_open(true);
            }
            Heard::JoinRequest(user, name) => {
                if self.together_party().is_none() {
                    return;
                }
                self.notify_with(
                    format!("{name} wants to listen along"),
                    "Let in",
                    move |_| send(Msg::LetIn(user)),
                );
            }
        }
    }
}

/// Discord starts `ember.exe` for an invite accepted while it isn't running (as the old
/// discord-rpc library set it up: a `discord-<app id>` URL protocol). Not for development builds,
/// which would take the installed app's place.
fn register() {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
    use windows::core::HSTRING;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let exe = exe.to_string_lossy().to_string();
    if exe.contains(r"\target\") {
        return;
    }
    let key = format!(r"Software\Classes\discord-{DISCORD_APP_ID}");
    let set = |sub: &str, name: Option<&str>, value: &str| {
        let path = if sub.is_empty() {
            key.clone()
        } else {
            format!(r"{key}\{sub}")
        };
        let data: Vec<u16> = value.encode_utf16().chain([0]).collect();
        let name = name.map(HSTRING::from);
        // SAFETY: the strings outlive the call; the data is a NUL-terminated UTF-16 string.
        let r = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(path),
                name.as_ref().map_or(windows::core::PCWSTR::null(), |n| {
                    windows::core::PCWSTR(n.as_ptr())
                }),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            )
        };
        if r.is_err() {
            log::warn!("discord: registering the launch command: {r:?}");
        }
    };
    set("", None, &format!("URL:Run game {DISCORD_APP_ID} protocol"));
    set("", Some("URL Protocol"), "");
    set("DefaultIcon", None, &exe);
    set(r"shell\open\command", None, &format!("\"{exe}\""));
}

// ---- the thread -------------------------------------------------------------------------------

/// A connection to the Discord client.
struct Ipc {
    pipe: std::fs::File,
    nonce: u64,
    /// Events read along the way, to hand on.
    heard: Vec<Heard>,
}

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;

/// An event Discord dispatched, if it's one Ember acts on.
fn heard(v: &Value) -> Option<Heard> {
    let data = &v["data"];
    match v["evt"].as_str()? {
        "ACTIVITY_JOIN" => Some(Heard::Join(data["secret"].as_str()?.to_string())),
        "ACTIVITY_JOIN_REQUEST" => {
            let u = &data["user"];
            let name = u["global_name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or(u["username"].as_str())
                .unwrap_or("Someone");
            Some(Heard::JoinRequest(
                u["id"].as_str()?.to_string(),
                name.to_string(),
            ))
        }
        _ => None,
    }
}

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
                    let mut c = Ipc {
                        pipe,
                        nonce: 0,
                        heard: Vec::new(),
                    };
                    c.write(OP_HANDSHAKE, &json!({ "v": 1, "client_id": app_id }))?;
                    c.frame()?; // READY
                    for evt in ["ACTIVITY_JOIN", "ACTIVITY_JOIN_REQUEST"] {
                        c.command(json!({ "cmd": "SUBSCRIBE", "evt": evt, "args": {} }))?;
                    }
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

    /// Reads the next message (answering pings on the way).
    fn frame(&mut self) -> std::io::Result<Value> {
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
                OP_FRAME => return Ok(v),
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

    /// Takes in a message that isn't an answer: an event.
    fn take_event(&mut self, v: &Value) {
        if v["cmd"] == "DISPATCH"
            && let Some(h) = heard(v)
        {
            self.heard.push(h);
        }
    }

    /// Sends a command and reads up to its answer (keeping events that come first).
    fn command(&mut self, mut cmd: Value) -> std::io::Result<Value> {
        self.nonce += 1;
        let nonce = self.nonce.to_string();
        cmd["nonce"] = json!(nonce);
        self.write(OP_FRAME, &cmd)?;
        loop {
            let v = self.frame()?;
            if v["nonce"] == nonce.as_str() {
                if v["evt"] == "ERROR" {
                    log::warn!("discord: {}: {}", cmd["cmd"], v["data"]["message"]);
                }
                return Ok(v);
            }
            self.take_event(&v);
        }
    }

    /// Reads what's waiting, without blocking; an error when Discord has gone.
    fn poll(&mut self) -> std::io::Result<()> {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Pipes::PeekNamedPipe;
        loop {
            let mut avail = 0u32;
            // SAFETY: the handle is the open pipe's, and `avail` outlives the call.
            unsafe {
                PeekNamedPipe(
                    HANDLE(self.pipe.as_raw_handle()),
                    None,
                    0,
                    None,
                    Some(&mut avail),
                    None,
                )
            }
            .map_err(|e| std::io::Error::other(e.message()))?;
            if avail < 8 {
                return Ok(());
            }
            let v = self.frame()?;
            self.take_event(&v);
        }
    }

    fn set_activity(&mut self, a: Option<&Activity>) -> std::io::Result<Value> {
        self.command(json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": std::process::id(), "activity": a.map(Activity::json) },
        }))
    }
}

/// The thread: keeps the latest asked for and gets it to Discord. While the setting is on it
/// stays connected, for invites (connecting again after Discord closes, waiting longer each time
/// it isn't there).
fn run(rx: crossbeam_channel::Receiver<Msg>) {
    let mut enabled = false;
    let mut conn: Option<Ipc> = None;
    let mut want: Option<Activity> = None;
    // `want` not yet sent since it changed.
    let mut dirty = false;
    let mut settle = Instant::now();
    let mut limiter = Limiter::default();
    let mut retry = Instant::now();
    let mut backoff = Duration::from_secs(5);
    let mut told_missing = false;
    let mut let_in: Vec<String> = Vec::new();
    loop {
        let now = Instant::now();
        let at = if !enabled {
            None
        } else if conn.is_none() {
            Some(retry)
        } else if dirty {
            Some((now + POLL).min(settle.max(limiter.next(now))))
        } else {
            Some(now + POLL)
        };
        let got = match at {
            Some(at) => rx.recv_deadline(at),
            None => rx
                .recv()
                .map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected),
        };
        match got {
            Ok(Msg::Show(w)) => {
                want = w;
                dirty = true;
                settle = Instant::now() + SETTLE;
            }
            Ok(Msg::Enabled(on)) => {
                enabled = on;
                if !on && conn.take().is_some() {
                    log::info!("discord: off");
                }
                want = None;
                dirty = false;
                retry = Instant::now();
                backoff = Duration::from_secs(5);
            }
            Ok(Msg::LetIn(user)) => let_in.push(user),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
        if !enabled {
            continue;
        }
        let now = Instant::now();
        if conn.is_none() {
            if now < retry {
                continue;
            }
            match Ipc::connect(DISCORD_APP_ID) {
                Ok(c) => {
                    log::info!("discord: connected");
                    conn = Some(c);
                    backoff = Duration::from_secs(5);
                    told_missing = false;
                    dirty = want.is_some();
                }
                Err(e) => {
                    if !told_missing {
                        log::info!("discord: not reachable ({e}), trying again later");
                        told_missing = true;
                    }
                    retry = now + backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(120));
                    continue;
                }
            }
        }
        let c = conn.as_mut().expect("connected");
        let mut r = c.poll();
        for user in let_in.drain(..) {
            if r.is_ok() {
                r = c
                    .command(json!({
                        "cmd": "SEND_ACTIVITY_JOIN_INVITE",
                        "args": { "user_id": user, "pid": std::process::id() },
                    }))
                    .map(|_| ());
            }
        }
        if r.is_ok() && dirty && now >= settle && limiter.next(now) <= now {
            limiter.record(now);
            r = c.set_activity(want.as_ref()).map(|_| ());
            if r.is_ok() {
                dirty = false;
            }
        }
        for h in c.heard.drain(..) {
            let _ = slint::invoke_from_event_loop(move || {
                with(|a| a.discord_heard(h));
            });
        }
        if let Err(e) = r {
            // Discord closed (or restarted): connect again, and show it then.
            log::info!("discord: {e}");
            conn = None;
            retry = Instant::now() + Duration::from_secs(2);
            dirty = want.is_some();
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
            party: None,
        }
    }

    #[test]
    fn playing_song() {
        let a = activity(Some(&song()), 1_000_000).unwrap();
        let j = a.json();
        assert_eq!(j["type"], 2);
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
        assert!(j.get("party").is_none() && j.get("secrets").is_none());
        // Never branded as YouTube Music: the name comes from the Discord application.
        assert!(j.get("name").is_none());
    }

    #[test]
    fn hosting_invites() {
        let code = "ember-abc";
        let n = Now {
            party: Some((code.into(), 3)),
            ..song()
        };
        let j = activity(Some(&n), 1_000_000).unwrap().json();
        assert_eq!(j["secrets"]["join"], code);
        let id = j["party"]["id"].as_str().unwrap();
        assert!(id.starts_with("ember-") && !id.contains("abc") && id.len() <= 128);
        assert_eq!(j["party"]["size"], json!([3, together::MAX_GUESTS + 1]));
        // No buttons with a join secret.
        assert!(j.get("buttons").is_none());
        // Someone joining is told again.
        let more = Now {
            party: Some((code.into(), 4)),
            ..song()
        };
        let a = activity(Some(&n), 1_000_000).unwrap();
        assert!(!a.same(&activity(Some(&more), 1_000_000).unwrap()));
    }

    #[test]
    fn paused_has_no_clock() {
        let n = Now {
            playing: false,
            ..song()
        };
        let j = activity(Some(&n), 1_000_000).unwrap().json();
        assert!(j.get("timestamps").is_none());
        assert_eq!(j["state"], "Paused · Artist A & Artist B");
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
        let paused = activity(
            Some(&Now {
                playing: false,
                ..song()
            }),
            1_000_400,
        )
        .unwrap();
        assert!(!a.same(&paused));
    }

    #[test]
    fn events() {
        let join =
            json!({ "cmd": "DISPATCH", "evt": "ACTIVITY_JOIN", "data": { "secret": "ember-x" } });
        assert_eq!(heard(&join), Some(Heard::Join("ember-x".into())));
        let ask = json!({ "cmd": "DISPATCH", "evt": "ACTIVITY_JOIN_REQUEST", "data": {
            "user": { "id": "42", "username": "pal", "global_name": "Pal" } } });
        assert_eq!(
            heard(&ask),
            Some(Heard::JoinRequest("42".into(), "Pal".into()))
        );
        let ready = json!({ "cmd": "DISPATCH", "evt": "READY", "data": {} });
        assert_eq!(heard(&ready), None);
    }

    /// Against the Discord running here: `EMBER_DISCORD_ID=<application id> cargo test -p
    /// music-app discord::tests::live -- --ignored --nocapture` shows a song (with an invite
    /// to a made-up Listen Along) for 30 s. Without an id, a made-up one checks the pipe and
    /// framing (Discord answers it with a close).
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
        let n = Now {
            party: Some(("ember-test".into(), 1)),
            ..song()
        };
        let a = activity(Some(&n), unix_ms()).unwrap();
        let r = c.set_activity(Some(&a)).expect("set");
        eprintln!("{r}");
        assert_ne!(r["evt"], "ERROR", "{r}");
        for _ in 0..60 {
            std::thread::sleep(POLL);
            c.poll().expect("poll");
            for h in c.heard.drain(..) {
                eprintln!("heard {h:?}");
            }
        }
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
