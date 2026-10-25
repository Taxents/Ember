//! Listening stats. YouTube keeps no per-listener counts it would share, so every song heard in
//! Ember (long enough to count, as for YouTube's history) goes into `listens.jsonl` in the config
//! folder, and the Artists page adds them up: who and what you play most, who's rising, who's
//! fading. Live radio isn't logged.
//!
//! The file is JSON lines, only ever appended to: a song's details once (and again if they
//! change), then one short line per play, `[unix time, video id, seconds heard]`. A year of
//! heavy listening is a few hundred kB, read in a moment at start.

use music_core::yt::model::{Named, Thumb, Track};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const FILE: &str = "listens.jsonl";
const DAY: u64 = 24 * 3600;
/// "This month" when telling who's rising or fading.
const MONTH: u64 = 30 * DAY;
/// What "before" is compared with: the three months before this one.
const BEFORE: u64 = 3 * MONTH;

/// A song as the log keeps it: enough to list it and play it again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Song {
    #[serde(rename = "s")]
    pub id: String,
    #[serde(rename = "t")]
    pub title: String,
    /// Name and browse id.
    #[serde(rename = "a", default)]
    pub artists: Vec<(String, Option<String>)>,
    #[serde(rename = "l", default, skip_serializing_if = "Option::is_none")]
    pub album: Option<(String, Option<String>)>,
    /// Its largest picture: url, width, height.
    #[serde(rename = "i", default, skip_serializing_if = "Option::is_none")]
    pub thumb: Option<(String, u32, u32)>,
    #[serde(rename = "d", default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl Song {
    pub fn of(t: &Track) -> Song {
        Song {
            id: t.video_id.clone(),
            title: t.title.clone(),
            artists: t
                .artists
                .iter()
                .map(|a| (a.name.clone(), a.id.clone()))
                .collect(),
            album: t
                .album
                .as_ref()
                .filter(|a| !a.name.is_empty())
                .map(|a| (a.name.clone(), a.id.clone())),
            thumb: t
                .thumbs
                .iter()
                .max_by_key(|th| th.width)
                .map(|th| (th.url.clone(), th.width, th.height)),
            duration_ms: t.duration_ms,
        }
    }

    pub fn track(&self) -> Track {
        let named = |(name, id): &(String, Option<String>)| Named {
            name: name.clone(),
            id: id.clone(),
        };
        Track {
            video_id: self.id.clone(),
            title: self.title.clone(),
            artists: self.artists.iter().map(named).collect(),
            album: self.album.as_ref().map(named),
            duration_ms: self.duration_ms,
            thumbs: self.thumbs(),
            ..Default::default()
        }
    }

    pub fn thumbs(&self) -> Vec<Thumb> {
        self.thumb
            .iter()
            .map(|(url, width, height)| Thumb {
                url: url.clone(),
                width: *width,
                height: *height,
            })
            .collect()
    }
}

/// A line of the file.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Line {
    Play(u64, String, u32),
    Song(Song),
}

/// One play: when it started being heard, the song (an index into the log's songs), and how
/// long was heard.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Play {
    pub at: u64,
    pub song: usize,
    pub secs: u32,
}

#[derive(Debug, Default, Clone)]
pub struct Log {
    pub songs: Vec<Song>,
    index: HashMap<String, usize>,
    pub plays: Vec<Play>,
}

impl Log {
    /// The log as written. A line that doesn't read (half a line, after a crash) is skipped.
    pub fn parse(text: &str) -> Log {
        let mut log = Log::default();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<Line>(line) {
                Ok(Line::Song(s)) => {
                    log.put(s);
                }
                Ok(Line::Play(at, id, secs)) => {
                    if let Some(&song) = log.index.get(&id) {
                        log.plays.push(Play { at, song, secs });
                    }
                }
                Err(_) => {}
            }
        }
        log
    }

    pub fn load() -> Log {
        let path = music_core::paths::config_dir().join(FILE);
        std::fs::read_to_string(path)
            .map(|t| Log::parse(&t))
            .unwrap_or_default()
    }

    /// Adds a play, returning the lines that record it: the song's details first when they're
    /// new or changed.
    pub fn add(&mut self, t: &Track, at: u64, secs: u32) -> String {
        let s = Song::of(t);
        let mut out = String::new();
        let known = self.index.get(&s.id).map(|&i| &self.songs[i]);
        if known != Some(&s) {
            out += &serde_json::to_string(&Line::Song(s.clone())).expect("json");
            out.push('\n');
        }
        let song = self.put(s);
        self.plays.push(Play { at, song, secs });
        out += &serde_json::to_string(&Line::Play(at, t.video_id.clone(), secs)).expect("json");
        out.push('\n');
        out
    }

    /// Forgets every play: the file goes.
    pub fn clear() {
        let path = music_core::paths::config_dir().join(FILE);
        if let Err(e) = std::fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("clearing {FILE}: {e}");
        }
    }

    /// Adds a play and appends it to the file.
    pub fn record(&mut self, t: &Track, at: u64, secs: u32) {
        use std::io::Write;
        let lines = self.add(t, at, secs);
        let path = music_core::paths::config_dir().join(FILE);
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(lines.as_bytes()));
        if let Err(e) = res {
            log::warn!("saving {FILE}: {e}");
        }
    }

    /// Keeps a song's details (the latest win), returning its index.
    fn put(&mut self, s: Song) -> usize {
        match self.index.get(&s.id) {
            Some(&i) => {
                self.songs[i] = s;
                i
            }
            None => {
                self.index.insert(s.id.clone(), self.songs.len());
                self.songs.push(s);
                self.songs.len() - 1
            }
        }
    }
}

/// How far back the Artists page looks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Period {
    Month,
    HalfYear,
    Year,
    All,
}

impl Period {
    pub const EVERY: [Period; 4] = [Period::Month, Period::HalfYear, Period::Year, Period::All];

    /// As it goes in the page's route.
    pub fn id(self) -> &'static str {
        match self {
            Period::Month => "4w",
            Period::HalfYear => "6m",
            Period::Year => "1y",
            Period::All => "all",
        }
    }

    pub fn from_id(id: &str) -> Period {
        Period::EVERY
            .into_iter()
            .find(|p| p.id() == id)
            .unwrap_or(Period::Month)
    }

    /// "in the last 4 weeks".
    pub fn phrase(self) -> &'static str {
        match self {
            Period::Month => "in the last 4 weeks",
            Period::HalfYear => "in the last 6 months",
            Period::Year => "in the last year",
            Period::All => "since you started listening in Ember",
        }
    }

    /// When it starts, as unix time.
    pub fn since(self, now: u64) -> u64 {
        match self {
            Period::Month => now.saturating_sub(28 * DAY),
            Period::HalfYear => now.saturating_sub(182 * DAY),
            Period::Year => now.saturating_sub(365 * DAY),
            Period::All => 0,
        }
    }
}

/// An artist, song or album added up over some time.
#[derive(Debug, Clone, PartialEq)]
pub struct Total {
    /// What tells them apart: a browse id, else the name.
    pub key: String,
    pub name: String,
    /// Browse id (artists, albums) or video id (songs).
    pub id: Option<String>,
    pub plays: u32,
    pub secs: u64,
    /// The song heard longest among these plays (an index into the log's songs), for a
    /// picture.
    pub top_song: usize,
    /// The first and last play counted.
    pub first: u64,
    pub last: u64,
}

fn artist_key(name: &str, id: &Option<String>) -> String {
    id.clone()
        .unwrap_or_else(|| format!("name:{}", name.to_lowercase()))
}

/// Who a song counts for: each of its artists, in full.
fn artists(s: &Song) -> Vec<(String, String, Option<String>)> {
    s.artists
        .iter()
        .map(|(name, id)| (artist_key(name, id), name.clone(), id.clone()))
        .collect()
}

fn song_keys(s: &Song) -> Vec<(String, String, Option<String>)> {
    vec![(s.id.clone(), s.title.clone(), Some(s.id.clone()))]
}

fn albums(s: &Song) -> Vec<(String, String, Option<String>)> {
    s.album
        .iter()
        .map(|(name, id)| {
            let by = s.artists.first().map(|a| a.0.as_str()).unwrap_or_default();
            let key = id
                .clone()
                .unwrap_or_else(|| format!("name:{}|{}", name.to_lowercase(), by.to_lowercase()));
            (key, name.clone(), id.clone())
        })
        .collect()
}

/// Plays from `since` until (not including) `until`, added up by `keys`, most heard first.
fn sum(
    log: &Log,
    since: u64,
    until: u64,
    keys: impl Fn(&Song) -> Vec<(String, String, Option<String>)>,
) -> Vec<Total> {
    let mut totals: HashMap<String, Total> = HashMap::new();
    // Per total, seconds per song, for its top song.
    let mut by_song: HashMap<String, HashMap<usize, u64>> = HashMap::new();
    for p in log.plays.iter().filter(|p| p.at >= since && p.at < until) {
        for (key, name, id) in keys(&log.songs[p.song]) {
            let t = totals.entry(key.clone()).or_insert_with(|| Total {
                key: key.clone(),
                name: name.clone(),
                id: id.clone(),
                plays: 0,
                secs: 0,
                top_song: p.song,
                first: p.at,
                last: p.at,
            });
            t.plays += 1;
            t.secs += u64::from(p.secs);
            t.first = t.first.min(p.at);
            t.last = t.last.max(p.at);
            // The name as last seen.
            if p.at >= t.last {
                t.name = name;
            }
            *by_song.entry(key).or_default().entry(p.song).or_default() += u64::from(p.secs);
        }
    }
    let mut out: Vec<Total> = totals
        .into_values()
        .map(|mut t| {
            if let Some(s) = by_song.get(&t.key) {
                t.top_song = s
                    .iter()
                    .max_by_key(|&(&i, &secs)| (secs, std::cmp::Reverse(i)))
                    .map_or(t.top_song, |(&i, _)| i);
            }
            t
        })
        .collect();
    out.sort_by(|a, b| {
        (b.secs, b.plays)
            .cmp(&(a.secs, a.plays))
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

pub fn top_artists(log: &Log, since: u64, until: u64) -> Vec<Total> {
    sum(log, since, until, artists)
}

pub fn top_songs(log: &Log, since: u64, until: u64) -> Vec<Total> {
    sum(log, since, until, song_keys)
}

pub fn top_albums(log: &Log, since: u64, until: u64) -> Vec<Total> {
    sum(log, since, until, albums)
}

/// An artist's most heard songs from `since` on.
pub fn songs_by(log: &Log, artist: &str, since: u64, until: u64) -> Vec<Total> {
    sum(log, since, until, |s| {
        if artists(s).iter().any(|a| a.0 == artist) {
            song_keys(s)
        } else {
            Vec::new()
        }
    })
}

/// When each artist was first heard, by key.
pub fn first_heard(log: &Log) -> HashMap<String, u64> {
    let mut out: HashMap<String, u64> = HashMap::new();
    for p in &log.plays {
        for (key, _, _) in artists(&log.songs[p.song]) {
            let at = out.entry(key).or_insert(p.at);
            *at = (*at).min(p.at);
        }
    }
    out
}

/// An artist this month next to how they were before.
#[derive(Debug, Clone, PartialEq)]
pub struct Trend {
    /// This month (for rising) or the months before (for fading).
    pub total: Total,
    /// Seconds a month this month, and over the three months before.
    pub now: u64,
    pub before: u64,
}

/// Each artist's seconds this month and per month over the three before.
fn months(log: &Log, now: u64) -> (Vec<Total>, HashMap<String, Total>) {
    let this = top_artists(log, now.saturating_sub(MONTH), u64::MAX);
    let before = top_artists(
        log,
        now.saturating_sub(MONTH + BEFORE),
        now.saturating_sub(MONTH),
    )
    .into_iter()
    .map(|t| (t.key.clone(), t))
    .collect();
    (this, before)
}

/// Artists heard much more this month than in the three before: at least twice as much a month,
/// and some real listening (3 plays, 10 minutes). Ones new this month are `new_artists`.
pub fn rising(log: &Log, now: u64) -> Vec<Trend> {
    let (this, before) = months(log, now);
    let first = first_heard(log);
    let fresh = now.saturating_sub(MONTH);
    let mut out: Vec<Trend> = this
        .into_iter()
        .filter(|t| t.plays >= 3 && t.secs >= 600)
        .filter(|t| first.get(&t.key).is_some_and(|&f| f < fresh))
        .map(|t| {
            let was = before.get(&t.key).map_or(0, |b| b.secs) / 3;
            Trend {
                now: t.secs,
                before: was,
                total: t,
            }
        })
        .filter(|t| t.now >= 2 * t.before)
        .collect();
    out.sort_by_key(|t| std::cmp::Reverse(t.now - t.before));
    out
}

/// Artists heard first this month, with some real listening (2 plays).
pub fn new_artists(log: &Log, now: u64) -> Vec<Total> {
    let first = first_heard(log);
    let fresh = now.saturating_sub(MONTH);
    top_artists(log, fresh, u64::MAX)
        .into_iter()
        .filter(|t| t.plays >= 2 && first.get(&t.key).is_some_and(|&f| f >= fresh))
        .collect()
}

/// Artists played a lot in the three months before this one (5 plays, half an hour a month)
/// and a third as much or less this month.
pub fn fading(log: &Log, now: u64) -> Vec<Trend> {
    let (this, before) = months(log, now);
    let this: HashMap<String, u64> = this.into_iter().map(|t| (t.key, t.secs)).collect();
    let mut out: Vec<Trend> = before
        .into_values()
        .map(|t| Trend {
            now: this.get(&t.key).copied().unwrap_or(0),
            before: t.secs / 3,
            total: t,
        })
        .filter(|t| t.total.plays >= 5 && t.before >= 1800 && t.now * 3 <= t.before)
        .collect();
    out.sort_by(|a, b| {
        (b.before - b.now)
            .cmp(&(a.before - a.now))
            .then_with(|| a.total.name.cmp(&b.total.name))
    });
    out
}

/// "45 min", "3 h 20 min", "120 h".
pub fn duration(secs: u64) -> String {
    let mins = secs / 60;
    match mins {
        0 => "<1 min".into(),
        1..60 => format!("{mins} min"),
        60..6000 if !mins.is_multiple_of(60) => format!("{} h {} min", mins / 60, mins % 60),
        _ => format!("{} h", mins / 60),
    }
}

/// "1 play", "12 plays".
pub fn plays(n: u32) -> String {
    if n == 1 {
        "1 play".into()
    } else {
        format!("{n} plays")
    }
}

/// Unix time as "3 Mar 2026" (UTC).
pub fn date(at: u64) -> String {
    let (y, m, d) = civil(at / DAY);
    format!("{d} {} {y}", MONTHS[m as usize - 1])
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Days since 1970-01-01 as year, month (1–12), day (Howard Hinnant's `civil_from_days`).
fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000;

    fn track(id: &str, artists: &[(&str, &str)], album: Option<&str>) -> Track {
        Track {
            video_id: id.into(),
            title: format!("Song {id}"),
            artists: artists
                .iter()
                .map(|(n, i)| Named {
                    name: (*n).into(),
                    id: (!i.is_empty()).then(|| (*i).into()),
                })
                .collect(),
            album: album.map(|a| Named {
                name: a.into(),
                id: Some(format!("MPRE{a}")),
            }),
            duration_ms: Some(200_000),
            thumbs: vec![
                Thumb {
                    url: "small".into(),
                    width: 60,
                    height: 60,
                },
                Thumb {
                    url: "big".into(),
                    width: 544,
                    height: 544,
                },
            ],
            ..Default::default()
        }
    }

    /// `n` plays of `secs` each, `days_ago` days back.
    fn play(log: &mut Log, t: &Track, days_ago: u64, n: u32, secs: u32) {
        for i in 0..n {
            log.add(t, NOW - days_ago * DAY + u64::from(i), secs);
        }
    }

    #[test]
    fn the_file_reads_back_as_written() {
        let mut log = Log::default();
        let a = track("a", &[("Ada", "UCada")], Some("First"));
        let mut text = log.add(&a, 100, 180);
        text += &log.add(&a, 200, 190);
        // A song's details go in once, and again only when they change.
        assert_eq!(text.lines().count(), 3);
        let mut renamed = a.clone();
        renamed.title = "Song a (Remastered)".into();
        text += &log.add(&renamed, 300, 30);
        assert_eq!(text.lines().count(), 5);
        // A torn last line is skipped.
        text += "[400,\"a\",";
        let back = Log::parse(&text);
        assert_eq!(back.plays, log.plays);
        assert_eq!(back.songs, log.songs);
        assert_eq!(back.songs[0].title, "Song a (Remastered)");
        assert_eq!(back.songs[0].thumb.as_ref().unwrap().0, "big");
        let t = back.songs[0].track();
        assert_eq!(t.artists[0].id.as_deref(), Some("UCada"));
        assert_eq!(t.album.unwrap().name, "First");
        // A play line is short.
        assert!(text.lines().nth(1).unwrap().len() < 20);
    }

    #[test]
    fn totals_add_up_by_artist_song_and_album() {
        let mut log = Log::default();
        let a = track("a", &[("Ada", "UCada")], Some("First"));
        let b = track("b", &[("Ada", "UCada"), ("Bo", "UCbo")], Some("First"));
        let c = track("c", &[("Cy", "")], None);
        play(&mut log, &a, 2, 3, 200);
        play(&mut log, &b, 3, 1, 100);
        play(&mut log, &c, 1, 5, 100);
        // Long ago: outside the last 4 weeks.
        play(&mut log, &c, 100, 10, 200);

        let since = Period::Month.since(NOW);
        let top = top_artists(&log, since, u64::MAX);
        let names: Vec<&str> = top.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Ada", "Cy", "Bo"]);
        assert_eq!((top[0].plays, top[0].secs), (4, 700));
        assert_eq!(top[0].id.as_deref(), Some("UCada"));
        assert_eq!(log.songs[top[0].top_song].id, "a");
        // An artist without an id is told apart by name, and has no page to open.
        assert_eq!(top[1].key, "name:cy");
        assert_eq!(top[1].id, None);

        let all = top_artists(&log, Period::All.since(NOW), u64::MAX);
        assert_eq!(all[0].name, "Cy");
        assert_eq!(all[0].plays, 15);

        let songs = top_songs(&log, since, u64::MAX);
        assert_eq!(songs[0].id.as_deref(), Some("a"));
        assert_eq!(songs.len(), 3);

        let albums = top_albums(&log, since, u64::MAX);
        assert_eq!(albums.len(), 1);
        assert_eq!((albums[0].name.as_str(), albums[0].plays), ("First", 4));

        let by = songs_by(&log, "UCada", since, u64::MAX);
        let ids: Vec<_> = by.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn first_heard_is_the_earliest_play() {
        let mut log = Log::default();
        let a = track("a", &[("Ada", "UCada")], None);
        play(&mut log, &a, 3, 1, 100);
        play(&mut log, &a, 40, 1, 100);
        play(&mut log, &a, 1, 1, 100);
        assert_eq!(first_heard(&log)["UCada"], NOW - 40 * DAY);
    }

    #[test]
    fn rising_new_and_fading() {
        let mut log = Log::default();
        let up = track("up", &[("Up", "UCup")], None);
        let steady = track("steady", &[("Steady", "UCsteady")], None);
        let down = track("down", &[("Down", "UCdown")], None);
        let new = track("new", &[("New", "UCnew")], None);
        // Up: a little before, a lot now.
        play(&mut log, &up, 60, 2, 200);
        play(&mut log, &up, 5, 10, 200);
        // Steady: the same each month.
        for m in 0..4 {
            play(&mut log, &steady, 5 + 30 * m, 6, 200);
        }
        // Down: a lot before, once now.
        play(&mut log, &down, 50, 20, 200);
        play(&mut log, &down, 80, 20, 200);
        play(&mut log, &down, 5, 1, 200);
        // New: first heard this month.
        play(&mut log, &new, 3, 4, 200);

        let r: Vec<String> = rising(&log, NOW)
            .into_iter()
            .map(|t| t.total.name)
            .collect();
        assert_eq!(r, ["Up"]);
        let n: Vec<String> = new_artists(&log, NOW).into_iter().map(|t| t.name).collect();
        assert_eq!(n, ["New"]);
        let f = fading(&log, NOW);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].total.name, "Down");
        assert_eq!(f[0].now, 200);
        assert_eq!(f[0].total.last, NOW - 50 * DAY + 19);
    }

    #[test]
    fn periods_go_through_the_route() {
        for p in Period::EVERY {
            assert_eq!(Period::from_id(p.id()), p);
        }
        assert_eq!(Period::from_id(""), Period::Month);
        assert_eq!(Period::All.since(NOW), 0);
    }

    #[test]
    fn durations_and_dates_read_well() {
        assert_eq!(duration(30), "<1 min");
        assert_eq!(duration(45 * 60), "45 min");
        assert_eq!(duration(200 * 60), "3 h 20 min");
        assert_eq!(duration(180 * 60), "3 h");
        assert_eq!(duration(120 * 3600 + 300), "120 h");
        assert_eq!(plays(1), "1 play");
        assert_eq!(plays(3), "3 plays");
        assert_eq!(date(0), "1 Jan 1970");
        // 2026-10-25 12:00 UTC.
        assert_eq!(date(1_792_929_600), "25 Oct 2026");
        assert_eq!(date(1_709_251_200), "1 Mar 2024"); // after a leap day
        assert_eq!(date(1_709_164_800), "29 Feb 2024");
    }
}
