//! Lyrics: word-timed when anyone has them, else line-timed, else plain.
//!
//! Sources, best first: Unison (Better Lyrics' crowdsourced database: syllable, word or line
//! timed TTML, keyed by videoId), YouTube Music's own timed lyrics, LRCLIB's synced ones, then
//! plain text from either. Line-timed lyrics get estimated word timing so every synced line can
//! fill as it's sung. Results are cached on disk, one file per video.

use crate::yt::Client;
use crate::{Error, Result, net, paths};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Lyrics {
    pub lines: Vec<Line>,
    pub timing: Timing,
    /// Where they came from ("Unison", "Musixmatch", "LRCLIB"…), for the log; never shown.
    pub source: String,
}

/// How finely the source timed the words. Line-timed lyrics still carry word times, estimated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Timing {
    #[default]
    None,
    Line,
    Word,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Line {
    /// Milliseconds; both 0 for plain lyrics.
    pub start: u64,
    pub end: u64,
    /// Words or syllables in order. A piece that ends a word carries its trailing space, so
    /// joining the texts gives the line ("es", "press", "o ").
    pub words: Vec<Word>,
    /// Who sings it in a duet: 0 the main voice, 1 the other one, 2 together.
    pub agent: u8,
    /// Background vocals, shown smaller under the line they belong to.
    pub background: bool,
    /// The line in another language, and spelled in Latin letters, when the source has them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub translation: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub roman: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Word {
    pub start: u64,
    pub end: u64,
    pub text: String,
}

impl Line {
    pub fn text(&self) -> String {
        let s: String = self.words.iter().map(|w| w.text.as_str()).collect();
        s.trim().to_string()
    }
}

impl Lyrics {
    pub fn synced(&self) -> bool {
        self.timing != Timing::None
    }
}

/// What finds a song's lyrics.
pub struct Query<'a> {
    pub video_id: &'a str,
    pub title: &'a str,
    pub artist: &'a str,
    pub album: Option<&'a str>,
    pub duration_ms: Option<u64>,
    /// The watch page's lyrics tab (`WatchNext::lyrics_id`), when the caller already has it.
    pub lyrics_id: Option<&'a str>,
    /// Ask Unison and LRCLIB too (they're sent the title, artist, album and length). When
    /// false only YouTube is asked.
    pub third_party: bool,
}

/// Cache format; a change here invalidates old files.
const CACHE_VERSION: u64 = 1;
/// Misses and line-timed finds are retried after a week (Unison grows); word-timed ones keep.
const RETRY_SECS: u64 = 7 * 86_400;

/// The best lyrics there are for the song, from the cache when it has a fresh answer.
pub fn find(client: &Client, q: &Query) -> Option<Lyrics> {
    let path = paths::cache_dir()
        .join("lyrics")
        .join(format!("{}.json", safe_name(q.video_id)));
    if let Some(c) = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Cached>(&b).ok())
        .filter(|c| c.v == CACHE_VERSION)
    {
        let word = c.lyrics.as_ref().is_some_and(|l| l.timing == Timing::Word);
        if word || paths::unix_now().saturating_sub(c.at) < RETRY_SECS {
            return c.lyrics;
        }
    }
    let (found, sure) = fetch(client, q);
    // An answer is only kept when every better source answered, so a network hiccup doesn't
    // pin a worse result (or a miss) for a week. YouTube's alone isn't kept either: it would
    // stand in for the better ones once they're allowed again.
    if sure && q.third_party {
        let c = Cached {
            v: CACHE_VERSION,
            at: paths::unix_now(),
            lyrics: found.clone(),
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(b) = serde_json::to_vec(&c)
            && let Err(e) = paths::write_atomic(&path, &b)
        {
            log::info!("lyrics cache: {e}");
        }
    }
    found
}

#[derive(Serialize, Deserialize)]
struct Cached {
    v: u64,
    at: u64,
    lyrics: Option<Lyrics>,
}

/// Every source in order; also whether all the ones tried before the answer really answered.
fn fetch(client: &Client, q: &Query) -> (Option<Lyrics>, bool) {
    let mut sure = true;
    let failed = |sure: &mut bool, what: &str, e: Error| {
        log::info!("lyrics {} {what}: {e}", q.video_id);
        *sure = false;
    };
    let mut plain = None;
    if q.third_party {
        match unison(q) {
            Ok(Some(l)) if l.synced() => return (Some(estimated(l)), sure),
            Ok(l) => plain = plain.or(l),
            Err(e) => failed(&mut sure, "unison", e),
        }
    }
    let lyrics_id = match q.lyrics_id {
        Some(id) => Some(id.to_string()),
        None => match client.watch_next(Some(q.video_id), None, None) {
            Ok(w) => w.lyrics_id,
            Err(e) => {
                failed(&mut sure, "next", e);
                None
            }
        },
    };
    if let Some(id) = &lyrics_id {
        match client.timed_lyrics(id) {
            Ok(Some(l)) => return (Some(estimated(l)), sure),
            Ok(None) => {}
            Err(e) => failed(&mut sure, "youtube timed", e),
        }
    }
    if q.third_party {
        match lrclib(q) {
            Ok(Some(l)) if l.synced() => return (Some(estimated(l)), sure),
            Ok(l) => plain = plain.or(l),
            Err(e) => failed(&mut sure, "lrclib", e),
        }
    }
    if plain.is_none()
        && let Some(id) = &lyrics_id
    {
        match client.lyrics(id) {
            Ok(l) => plain = l,
            Err(e) => failed(&mut sure, "youtube", e),
        }
    }
    (plain, sure)
}

// ---- Unison ---------------------------------------------------------------------------------

const UNISON: &str = "https://unison.betterlyrics.org/lyrics";

/// Unison's lyrics for the video. Reads only need an `x-key-id`, which Better Lyrics derives
/// from a key pair it makes on first run; ours is a random id made once and kept.
pub fn unison(q: &Query) -> Result<Option<Lyrics>> {
    let mut url = format!(
        "{UNISON}?v={}&song={}&artist={}",
        encode(q.video_id),
        encode(q.title),
        encode(q.artist)
    );
    if let Some(ms) = q.duration_ms {
        url += &format!("&duration={}", (ms + 500) / 1000);
    }
    if let Some(a) = q.album {
        url += &format!("&album={}", encode(a));
    }
    let key = key_id();
    let v = match net::get_json(&url, &[("x-key-id", &key), ("User-Agent", net::BROWSER_UA)]) {
        Ok(v) => v,
        Err(Error::Net(e)) if e.contains("HTTP 404") => return Ok(None),
        Err(e) => return Err(e),
    };
    let d = &v["data"];
    let Some(text) = d["lyrics"].as_str().filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    let l = match d["format"].as_str() {
        Some("ttml") => parse_ttml(text),
        Some("lrc") => lrc_lyrics(text, ""),
        _ => plain(text, ""),
    };
    let mut l = fixed_timing(l);
    if l.lines.is_empty() {
        return Ok(None);
    }
    l.source = "Unison".into();
    Ok(Some(l))
}

/// Our Unison key id: 64 hex digits, like Better Lyrics' SHA-256 of its public key. Random,
/// made once and kept, so Unison can tell requests apart without knowing who sends them.
fn key_id() -> String {
    let mut key = KEY.lock().unwrap();
    if let Some(k) = &*key {
        return k.clone();
    }
    let path = key_path();
    let k = std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_else(|| {
            let k = random_hex();
            let _ = paths::write_atomic(&path, k.as_bytes());
            k
        });
    *key = Some(k.clone());
    k
}

static KEY: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn key_path() -> std::path::PathBuf {
    paths::config_dir().join("unison-key-id.txt")
}

/// Forgets the Unison key id; the next request makes a new one, unlinked from the old.
pub fn reset_key_id() {
    let mut key = KEY.lock().unwrap();
    *key = None;
    let _ = std::fs::remove_file(key_path());
}

/// 32 random bytes from the system, in hex.
fn random_hex() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("system random numbers");
    b.iter().map(|b| format!("{b:02x}")).collect()
}

// ---- LRCLIB --------------------------------------------------------------------------------

/// LRCLIB's lyrics for the song (lrclib.net: free, no key). An exact match first, then a search
/// for one within 3 s of the song's length.
pub fn lrclib(q: &Query) -> Result<Option<Lyrics>> {
    const UA: (&str, &str) = ("User-Agent", "Ember (private music app)");
    let mut url = format!(
        "https://lrclib.net/api/get?track_name={}&artist_name={}",
        encode(q.title),
        encode(q.artist)
    );
    if let Some(a) = q.album {
        url += &format!("&album_name={}", encode(a));
    }
    if let Some(ms) = q.duration_ms {
        url += &format!("&duration={}", ms.div_ceil(1000));
    }
    let v = match net::get_json(&url, &[UA]) {
        Ok(v) => v,
        Err(Error::Net(e)) if e.contains("HTTP 404") => {
            let url = format!(
                "https://lrclib.net/api/search?track_name={}&artist_name={}",
                encode(q.title),
                encode(q.artist)
            );
            let all = net::get_json(&url, &[UA])?;
            let close = |r: &&Value| match (q.duration_ms, r["duration"].as_f64()) {
                (Some(ms), Some(d)) => (ms as f64 / 1000.0 - d).abs() <= 3.0,
                _ => true,
            };
            let rows = all.as_array().map(Vec::as_slice).unwrap_or_default();
            match rows
                .iter()
                .filter(close)
                .find(|r| r["syncedLyrics"].is_string())
                .or_else(|| rows.iter().find(close))
            {
                Some(r) => r.clone(),
                None => return Ok(None),
            }
        }
        Err(e) => return Err(e),
    };
    if let Some(s) = v["syncedLyrics"].as_str().filter(|s| !s.trim().is_empty()) {
        let l = lrc_lyrics(s, "LRCLIB");
        if !l.lines.is_empty() {
            return Ok(Some(l));
        }
    }
    Ok(v["plainLyrics"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| plain(s, "LRCLIB")))
}

// ---- building ------------------------------------------------------------------------------

/// Plain text, a line per line.
pub fn plain(text: &str, source: &str) -> Lyrics {
    Lyrics {
        lines: text
            .lines()
            .map(|l| Line {
                words: vec![Word {
                    start: 0,
                    end: 0,
                    text: l.trim().to_string(),
                }],
                ..Default::default()
            })
            .collect(),
        timing: Timing::None,
        source: source.into(),
    }
}

/// Line-timed lyrics from (start, end, text) triples, as YouTube gives them.
pub fn from_lines(lines: impl IntoIterator<Item = (u64, u64, String)>, source: &str) -> Lyrics {
    Lyrics {
        lines: lines
            .into_iter()
            .map(|(start, end, text)| Line {
                start,
                end: end.max(start),
                words: vec![Word {
                    start,
                    end: end.max(start),
                    text: text.trim().to_string(),
                }],
                ..Default::default()
            })
            .collect(),
        timing: Timing::Line,
        source: source.into(),
    }
}

/// LRC text as lyrics, word-timed when it's enhanced LRC.
fn lrc_lyrics(text: &str, source: &str) -> Lyrics {
    let lines = parse_lrc(text);
    let words = lines.iter().any(|l| l.words.len() > 1);
    Lyrics {
        lines,
        timing: if words { Timing::Word } else { Timing::Line },
        source: source.into(),
    }
}

/// Says `Word` only when the lines really have several timed pieces.
fn fixed_timing(mut l: Lyrics) -> Lyrics {
    if l.timing == Timing::Word && !l.lines.iter().any(|x| x.words.len() > 1) {
        l.timing = Timing::Line;
    }
    l
}

/// Gives line-timed lyrics word times: each line's words share its time by length, at no more
/// than a natural singing pace, so a line before a long break doesn't crawl.
pub fn estimated(mut l: Lyrics) -> Lyrics {
    if l.timing != Timing::Line {
        return l;
    }
    for line in &mut l.lines {
        let text = line.text();
        let pieces: Vec<&str> = text.split_inclusive(' ').collect();
        if pieces.len() < 2 {
            continue;
        }
        let weight = |p: &str| p.trim().chars().count() as u64 + 1;
        let total: u64 = pieces.iter().map(|p| weight(p)).sum();
        let span = (line.end - line.start).min(400 + total * 75);
        let mut at = line.start;
        let mut used = 0;
        line.words = pieces
            .iter()
            .map(|p| {
                used += weight(p);
                let end = line.start + span * used / total;
                let w = Word {
                    start: at,
                    end,
                    text: p.to_string(),
                };
                at = end;
                w
            })
            .collect();
    }
    l
}

// ---- LRC ------------------------------------------------------------------------------------

/// "[01:02.34] words" lines, in time order, each ending where the next starts. Lines can carry
/// several stamps; "[ar: …]" tags are skipped; enhanced LRC's "<01:02.50>" word stamps become
/// word times. Empty lines only end the line before them.
pub fn parse_lrc(s: &str) -> Vec<Line> {
    let mut stamped: Vec<(u64, String)> = Vec::new();
    for raw in s.lines() {
        let mut rest = raw.trim();
        let mut times = Vec::new();
        while let Some(r) = rest.strip_prefix('[') {
            let Some((stamp, after)) = r.split_once(']') else {
                break;
            };
            match stamp_ms(stamp) {
                Some(ms) => times.push(ms),
                None => break,
            }
            rest = after;
        }
        for ms in times {
            stamped.push((ms, rest.trim().to_string()));
        }
    }
    stamped.sort_by_key(|l| l.0);
    let mut out = Vec::new();
    for (i, (start, text)) in stamped.iter().enumerate() {
        let next = stamped.get(i + 1).map(|n| n.0);
        let words = lrc_words(text, *start);
        if words.iter().all(|w| w.text.trim().is_empty()) {
            continue;
        }
        let last = words.last().map_or(*start, |w| w.end.max(w.start));
        let end = next.unwrap_or(last.max(start + 5000)).max(*start);
        let mut words = words;
        // The last word runs to the line's end unless it was stamped.
        if let Some(w) = words.last_mut()
            && w.end <= w.start
        {
            w.end = end;
        }
        out.push(Line {
            start: *start,
            end,
            words,
            ..Default::default()
        });
    }
    out
}

/// A line's "<mm:ss.xx>word" pieces; one untimed word when it has no stamps.
fn lrc_words(text: &str, start: u64) -> Vec<Word> {
    if !text.contains('<') {
        return vec![Word {
            start,
            end: 0,
            text: text.to_string(),
        }];
    }
    let mut words: Vec<Word> = Vec::new();
    let mut rest = text;
    let mut at = start;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('<')
            && let Some((stamp, after)) = r.split_once('>')
            && let Some(ms) = stamp_ms(stamp)
        {
            if let Some(w) = words.last_mut()
                && w.end <= w.start
            {
                w.end = ms;
            }
            at = ms;
            rest = after;
            continue;
        }
        let cut = rest
            .char_indices()
            .skip(1)
            .find(|c| c.1 == '<')
            .map_or(rest.len(), |c| c.0);
        let piece = &rest[..cut];
        rest = &rest[cut..];
        if piece.trim().is_empty() {
            if let Some(w) = words.last_mut() {
                w.text.push(' ');
            }
            continue;
        }
        words.push(Word {
            start: at,
            end: 0,
            text: piece.to_string(),
        });
    }
    if let Some(w) = words.last_mut() {
        w.text = w.text.trim_end().to_string();
    }
    words
}

fn stamp_ms(s: &str) -> Option<u64> {
    let (m, sec) = s.split_once(':')?;
    let m: u64 = m.trim().parse().ok()?;
    let sec: f64 = sec.trim().parse().ok()?;
    Some(m * 60_000 + (sec * 1000.0).round() as u64)
}

// ---- TTML -----------------------------------------------------------------------------------

/// A minimal TTML reader for lyrics: `<p begin end ttm:agent>` lines, `<span begin end>` words
/// or syllables (a space between spans ends a word), `ttm:role="x-bg"` background vocals, and
/// the head's `ttm:agent`s for duets. Inline translations and romanisations (`x-translation`,
/// `x-roman` spans) are kept beside the line, not in it.
pub fn parse_ttml(s: &str) -> Lyrics {
    let mut agents: Vec<(String, u8)> = Vec::new();
    let mut persons = 0u8;
    let mut lines: Vec<Line> = Vec::new();
    let mut word_timed = false;

    // Inside a <p>: the line and its background line being built.
    let mut in_p = false;
    let mut main = Line::default();
    let mut bg = Line::default();
    // Open spans, innermost last: (is background, is skipped, has its own time, kept beside
    // the line: 1 a translation, 2 a romanisation).
    let mut spans: Vec<(bool, bool, bool, u8)> = Vec::new();
    let mut p_time = (0u64, 0u64);

    for tok in xml_tokens(s) {
        match tok {
            Tok::Open {
                name,
                attrs,
                closed,
            } => match name {
                "ttm:agent" if !in_p => {
                    let id = attr(&attrs, "xml:id").unwrap_or_default().to_string();
                    let n = match attr(&attrs, "type") {
                        Some("person") | None => {
                            persons += 1;
                            if persons == 1 { 0 } else { 1 }
                        }
                        _ => 2,
                    };
                    agents.push((id, n));
                }
                "p" => {
                    in_p = !closed;
                    p_time = (
                        attr(&attrs, "begin").and_then(ttml_ms).unwrap_or(0),
                        attr(&attrs, "end").and_then(ttml_ms).unwrap_or(0),
                    );
                    let agent = attr(&attrs, "ttm:agent")
                        .and_then(|a| agents.iter().find(|x| x.0 == a))
                        .map_or(0, |x| x.1);
                    main = Line {
                        start: p_time.0,
                        end: p_time.1,
                        agent,
                        ..Default::default()
                    };
                    bg = Line {
                        agent,
                        background: true,
                        ..Default::default()
                    };
                    spans.clear();
                }
                "span" if in_p && !closed => {
                    let role = attr(&attrs, "ttm:role").unwrap_or("");
                    let (pbg, pskip, pside) =
                        spans.last().map_or((false, false, 0), |s| (s.0, s.1, s.3));
                    let timed = attr(&attrs, "begin").is_some();
                    let is_bg = pbg || role == "x-bg";
                    let side = match role {
                        "x-translation" => 1,
                        "x-roman" => 2,
                        _ => pside,
                    };
                    let skip = pskip || side != 0;
                    spans.push((is_bg, skip, timed, side));
                    if timed && !skip {
                        word_timed = true;
                        let target = if is_bg { &mut bg } else { &mut main };
                        target.words.push(Word {
                            start: attr(&attrs, "begin").and_then(ttml_ms).unwrap_or(0),
                            end: attr(&attrs, "end").and_then(ttml_ms).unwrap_or(0),
                            text: String::new(),
                        });
                    }
                }
                _ => {}
            },
            Tok::Close(name) => match name {
                "span" if in_p => {
                    spans.pop();
                }
                "p" if in_p => {
                    in_p = false;
                    for mut l in [std::mem::take(&mut main), std::mem::take(&mut bg)] {
                        if let Some(w) = l.words.last_mut() {
                            w.text = w.text.trim_end().to_string();
                        }
                        l.words.retain(|w| !w.text.is_empty());
                        if l.words.is_empty() {
                            continue;
                        }
                        l.translation = l.translation.trim().to_string();
                        l.roman = l.roman.trim().to_string();
                        if l.background {
                            l.start = l.words.iter().map(|w| w.start).min().unwrap_or(0);
                            l.end = l.words.iter().map(|w| w.end).max().unwrap_or(0);
                        }
                        lines.push(l);
                    }
                }
                _ => {}
            },
            Tok::Text(t) if in_p => {
                let (is_bg, skip, timed, side) =
                    spans.last().copied().unwrap_or((false, false, false, 0));
                let text = unescape(t);
                let target = if is_bg { &mut bg } else { &mut main };
                if skip {
                    let beside = match side {
                        1 => &mut target.translation,
                        2 => &mut target.roman,
                        _ => continue,
                    };
                    *beside += &collapse(&text);
                    continue;
                }
                if timed {
                    // A timed span's own text.
                    if let Some(w) = target.words.last_mut() {
                        w.text += &collapse(&text);
                    }
                } else if text.trim().is_empty() {
                    // Space between spans: the word before it ends.
                    if let Some(w) = target.words.last_mut()
                        && !w.text.ends_with(' ')
                    {
                        w.text.push(' ');
                    }
                } else {
                    // Untimed text: a line-timed <p>, or loose text between spans.
                    let (start, end) = target
                        .words
                        .last()
                        .map_or(p_time, |w| (w.end, w.end.max(p_time.1)));
                    target.words.push(Word {
                        start,
                        end,
                        text: collapse(&text),
                    });
                }
            }
            _ => {}
        }
    }
    Lyrics {
        lines,
        timing: if word_timed {
            Timing::Word
        } else {
            Timing::Line
        },
        source: String::new(),
    }
}

/// Collapses runs of whitespace to single spaces, keeping a leading or trailing one.
fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = true;
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    if space && !out.is_empty() {
        out.push(' ');
    }
    out
}

/// "8.835", "0:16.844", "1:02:03.5", "12.3s", "500ms".
fn ttml_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(ms) = s.strip_suffix("ms") {
        return ms.parse::<f64>().ok().map(|v| v.round() as u64);
    }
    let s = s.strip_suffix('s').unwrap_or(s);
    let mut secs = 0.0;
    for part in s.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some((secs * 1000.0).round() as u64)
}

enum Tok<'a> {
    Open {
        name: &'a str,
        attrs: Vec<(&'a str, &'a str)>,
        closed: bool,
    },
    Close(&'a str),
    Text(&'a str),
}

/// Tags and text of an XML document; comments, declarations and processing instructions are
/// dropped. Tolerant: anything malformed ends the scan.
fn xml_tokens(s: &str) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            out.push(Tok::Text(rest));
            break;
        };
        if lt > 0 {
            out.push(Tok::Text(&rest[..lt]));
        }
        rest = &rest[lt..];
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.find("-->").map_or("", |i| &r[i + 3..]);
            continue;
        }
        let Some(gt) = tag_end(rest) else {
            break;
        };
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            out.push(Tok::Close(name.trim()));
            continue;
        }
        let closed = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let (name, mut a) = tag
            .split_once(|c: char| c.is_whitespace())
            .unwrap_or((tag, ""));
        let mut attrs = Vec::new();
        while let Some(eq) = a.find('=') {
            let key = a[..eq].trim();
            let v = a[eq + 1..].trim_start();
            let Some(q) = v.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                break;
            };
            let Some(close) = v[1..].find(q) else {
                break;
            };
            attrs.push((key, &v[1..1 + close]));
            a = &v[close + 2..];
        }
        out.push(Tok::Open {
            name,
            attrs,
            closed,
        });
    }
    out
}

/// Where a tag starting at `s[0] == '<'` ends, skipping '>' inside quoted attributes.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

fn attr<'a>(attrs: &[(&str, &'a str)], key: &str) -> Option<&'a str> {
    attrs.iter().find(|a| a.0 == key).map(|a| a.1)
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out += &rest[..i];
        rest = &rest[i..];
        let Some(semi) = rest.find(';').filter(|&j| j <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let c = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out + rest
}

// ---- playback -------------------------------------------------------------------------------

/// The main (not background) line playing at `ms`: the last one that has started. Lines are in
/// time order, so this is a binary search.
pub fn line_at(lines: &[Line], ms: u64) -> Option<usize> {
    let n = lines.partition_point(|l| l.start <= ms);
    (0..n).rev().find(|&i| !lines[i].background)
}

/// A video id fit for a file name.
fn safe_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc_lines_in_order() {
        let l = parse_lrc("[ar: Someone]\n[00:12.50] Second\n[00:01.00][00:20.00]Chorus\n\n");
        let got: Vec<_> = l.iter().map(|l| (l.start, l.end, l.text())).collect();
        assert_eq!(
            got,
            [
                (1000, 12500, "Chorus".to_string()),
                (12500, 20000, "Second".to_string()),
                (20000, 25000, "Chorus".to_string())
            ]
        );
        assert_eq!(line_at(&l, 500), None);
        assert_eq!(line_at(&l, 13000), Some(1));
    }

    #[test]
    fn lrc_empty_lines_end_the_one_before() {
        let l = parse_lrc("[00:01.00]One\n[00:04.00]\n[00:09.00]Two");
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].start, l[0].end), (1000, 4000));
    }

    #[test]
    fn enhanced_lrc_words() {
        let l = parse_lrc("[00:01.00]<00:01.00>Hello <00:01.50>wor<00:01.80>ld<00:02.40>\n");
        let w: Vec<_> = l[0]
            .words
            .iter()
            .map(|w| (w.start, w.end, w.text.as_str()))
            .collect();
        assert_eq!(
            w,
            [
                (1000, 1500, "Hello "),
                (1500, 1800, "wor"),
                (1800, 2400, "ld")
            ]
        );
        assert_eq!(l[0].text(), "Hello world");
    }

    #[test]
    fn ttml_words_syllables_background_and_agents() {
        let t = r#"<tt><head><metadata><ttm:agent type="person" xml:id="v1"/><ttm:agent type="person" xml:id="v2"/><ttm:agent type="group" xml:id="v3"/></metadata></head>
<body><div><p begin="8.835" end="12.339" ttm:agent="v1"><span begin="8.835" end="9.155">Now</span> <span begin="9.155" end="9.5">es</span><span begin="9.5" end="10.0">press</span><span begin="10.0" end="10.5">o</span><span ttm:role="x-bg"><span begin="10.6" end="11">(Yes</span> <span begin="11" end="12.339">it&apos;s)</span></span></p>
<p begin="1:00.0" end="1:02.5" ttm:agent="v2">Line &amp; only</p><p begin="1:03" end="1:04" ttm:agent="v3"><span begin="1:03" end="1:04">All</span><span ttm:role="x-translation">Tous</span></p></div></body></tt>"#;
        let l = parse_ttml(t);
        assert_eq!(l.timing, Timing::Word);
        let lines: Vec<_> = l
            .lines
            .iter()
            .map(|x| (x.text(), x.agent, x.background, x.start, x.end))
            .collect();
        assert_eq!(
            lines,
            [
                ("Now espresso".into(), 0, false, 8835, 12339),
                ("(Yes it's)".into(), 0, true, 10600, 12339),
                ("Line & only".into(), 1, false, 60000, 62500),
                ("All".into(), 2, false, 63000, 64000),
            ]
        );
        let w: Vec<_> = l.lines[0].words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(w, ["Now ", "es", "press", "o"]);
        assert_eq!(l.lines[3].translation, "Tous");
        assert_eq!(l.lines[0].translation, "");
    }

    #[test]
    fn ttml_times() {
        assert_eq!(ttml_ms("8.835"), Some(8835));
        assert_eq!(ttml_ms("3:41.493"), Some(221_493));
        assert_eq!(ttml_ms("1:02:03.5"), Some(3_723_500));
        assert_eq!(ttml_ms("12.3s"), Some(12_300));
        assert_eq!(ttml_ms("500ms"), Some(500));
    }

    #[test]
    fn estimated_words_share_the_line() {
        let l = estimated(from_lines([(1000, 3000, "aa bb".into())], ""));
        let w: Vec<_> = l.lines[0]
            .words
            .iter()
            .map(|w| (w.start, w.end, w.text.as_str()))
            .collect();
        // 6 weights at 75 ms + 400 ms: 850 ms of the line's 2 s.
        assert_eq!(w, [(1000, 1425, "aa "), (1425, 1850, "bb")]);
    }
}
