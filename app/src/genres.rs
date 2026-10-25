//! Albums' genres, for sorting the library's Albums by genre. YouTube Music has none, so they
//! come from MusicBrainz: the release group's tags, else its artist's, mapped onto a short list
//! of broad genres so the groups don't splinter. Looked up one album at a time on a thread of
//! their own (MusicBrainz allows a request a second) and kept on disk.

use music_core::paths::{cache_dir, unix_now, write_atomic};
use music_core::yt::model::AlbumCard;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// The broad genres, in the order a tie between them goes.
pub const GENRES: [&str; 15] = [
    "Hip hop",
    "R&B",
    "Pop",
    "Rock",
    "Metal",
    "Electronic",
    "Jazz",
    "Classical",
    "Country",
    "Folk",
    "Latin",
    "Reggae",
    "Blues",
    "Soundtrack",
    "World",
];

/// Words in a tag that place it, with the genre they mean. A tag can hold several ("pop rap",
/// "indie rock"): the one that ends it wins, as the last word names the kind ("jazz rap" is
/// rap). Tags are lowercase, with "r&b" as "rnb" and other punctuation as spaces.
const WORDS: &[(&str, &str)] = &[
    ("hip hop", "Hip hop"),
    ("hiphop", "Hip hop"),
    ("rap", "Hip hop"),
    ("trap", "Hip hop"),
    ("drill", "Hip hop"),
    ("grime", "Hip hop"),
    ("boom bap", "Hip hop"),
    ("g funk", "Hip hop"),
    ("crunk", "Hip hop"),
    ("rnb", "R&B"),
    ("soul", "R&B"),
    ("funk", "R&B"),
    ("motown", "R&B"),
    ("gospel", "R&B"),
    ("pop", "Pop"),
    ("kpop", "Pop"),
    ("jpop", "Pop"),
    ("synthpop", "Pop"),
    ("electropop", "Pop"),
    ("europop", "Pop"),
    ("hyperpop", "Pop"),
    ("disco", "Pop"),
    ("rock", "Rock"),
    ("punk", "Rock"),
    ("grunge", "Rock"),
    ("emo", "Rock"),
    ("shoegaze", "Rock"),
    ("indie", "Rock"),
    ("alternative", "Rock"),
    ("new wave", "Rock"),
    ("britpop", "Rock"),
    ("metal", "Metal"),
    ("metalcore", "Metal"),
    ("deathcore", "Metal"),
    ("grindcore", "Metal"),
    ("djent", "Metal"),
    ("electronic", "Electronic"),
    ("electronica", "Electronic"),
    ("edm", "Electronic"),
    ("house", "Electronic"),
    ("techno", "Electronic"),
    ("trance", "Electronic"),
    ("dubstep", "Electronic"),
    ("drum and bass", "Electronic"),
    ("dnb", "Electronic"),
    ("ambient", "Electronic"),
    ("idm", "Electronic"),
    ("synthwave", "Electronic"),
    ("electro", "Electronic"),
    ("garage", "Electronic"),
    ("breakbeat", "Electronic"),
    ("jungle", "Electronic"),
    ("downtempo", "Electronic"),
    ("trip hop", "Electronic"),
    ("dance", "Electronic"),
    ("jazz", "Jazz"),
    ("bebop", "Jazz"),
    ("swing", "Jazz"),
    ("classical", "Classical"),
    ("baroque", "Classical"),
    ("opera", "Classical"),
    ("orchestral", "Classical"),
    ("symphony", "Classical"),
    ("chamber music", "Classical"),
    ("country", "Country"),
    ("bluegrass", "Country"),
    ("americana", "Country"),
    ("folk", "Folk"),
    ("singer songwriter", "Folk"),
    ("latin", "Latin"),
    ("reggaeton", "Latin"),
    ("salsa", "Latin"),
    ("bachata", "Latin"),
    ("cumbia", "Latin"),
    ("bossa nova", "Latin"),
    ("samba", "Latin"),
    ("mpb", "Latin"),
    ("corrido", "Latin"),
    ("corridos tumbados", "Latin"),
    ("dembow", "Latin"),
    ("reggae", "Reggae"),
    ("dancehall", "Reggae"),
    ("ska", "Reggae"),
    ("dub", "Reggae"),
    ("blues", "Blues"),
    ("soundtrack", "Soundtrack"),
    ("score", "Soundtrack"),
    ("video game", "Soundtrack"),
    ("musical", "Soundtrack"),
    ("afrobeats", "World"),
    ("afrobeat", "World"),
    ("amapiano", "World"),
    ("world", "World"),
];

/// The broad genre a single tag names, if any.
pub fn genre_of_tag(tag: &str) -> Option<&'static str> {
    let t = tag
        .to_lowercase()
        .replace("r&b", "rnb")
        .replace("r'n'b", "rnb")
        .replace("d&b", "dnb");
    let t: String = t
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let t = format!(" {} ", t.split_whitespace().collect::<Vec<_>>().join(" "));
    WORDS
        .iter()
        .filter_map(|&(w, g)| {
            let end = t.rfind(&format!(" {w} "))? + w.len();
            Some((end, w.len(), g))
        })
        .max_by_key(|&(end, len, _)| (end, len))
        .map(|(_, _, g)| g)
}

/// The broad genre most of the tags (name, votes) agree on. A tag nobody voted for still
/// counts a little; one voted down doesn't count.
pub fn genre_of_tags<'a>(tags: impl IntoIterator<Item = (&'a str, i64)>) -> Option<&'static str> {
    let mut votes: HashMap<&'static str, i64> = HashMap::new();
    for (name, count) in tags {
        if count < 0 {
            continue;
        }
        if let Some(g) = genre_of_tag(name) {
            *votes.entry(g).or_default() += count + 1;
        }
    }
    GENRES
        .iter()
        .filter_map(|g| votes.get(g).map(|&v| (*g, v)))
        // The first of equals, as `max_by_key` keeps the last.
        .rev()
        .max_by_key(|&(_, v)| v)
        .map(|(g, _)| g)
}

/// A genre found is kept half a year; "none found" a month, as tags get added.
const FOUND_TTL: u64 = 180 * 86400;
const NONE_TTL: u64 = 30 * 86400;

/// Album id (or "artist:" and a MusicBrainz artist id) → genre and when it was looked up.
type Cache = HashMap<String, (Option<String>, u64)>;

fn cache() -> std::sync::MutexGuard<'static, Option<Cache>> {
    static CACHE: Mutex<Option<Cache>> = Mutex::new(None);
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if c.is_none() {
        *c = Some(load());
    }
    c
}

fn fresh(entry: &(Option<String>, u64)) -> bool {
    let ttl = if entry.0.is_some() {
        FOUND_TTL
    } else {
        NONE_TTL
    };
    unix_now().saturating_sub(entry.1) < ttl
}

/// What's known of an album's genre without asking anyone: `Some(None)` is known to have none
/// (it goes under "Other"), `None` not looked up yet.
pub fn known(album_id: &str) -> Option<Option<String>> {
    cache()
        .as_ref()?
        .get(album_id)
        .filter(|e| fresh(e))
        .map(|e| e.0.clone())
}

fn remember(key: String, genre: Option<String>) {
    let mut c = cache();
    let c = c.get_or_insert_with(HashMap::new);
    c.insert(key, (genre, unix_now()));
}

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Looks up the albums not known yet, on a thread of its own, handing each batch found
/// (album id, genre) to `found` on the UI thread. Does nothing while a lookup is running: the
/// page asks again when it's next shown.
pub fn look_up(albums: Vec<AlbumCard>, found: fn(Vec<(String, Option<String>)>)) {
    let albums: Vec<AlbumCard> = albums
        .into_iter()
        .filter(|a| known(&a.id).is_none())
        .collect();
    if albums.is_empty() || RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("genres".into())
        .spawn(move || {
            crate::win::below_normal_priority();
            let mut batch = Vec::new();
            let mut sent = std::time::Instant::now();
            for a in &albums {
                match find(a) {
                    Ok(g) => {
                        remember(a.id.clone(), g.clone());
                        batch.push((a.id.clone(), g));
                    }
                    // Offline or refused: the rest can wait for next time.
                    Err(()) => {
                        log::warn!("genres: MusicBrainz couldn't be asked");
                        break;
                    }
                }
                if sent.elapsed().as_secs() >= 3 {
                    save();
                    let b = std::mem::take(&mut batch);
                    let _ = slint::invoke_from_event_loop(move || found(b));
                    sent = std::time::Instant::now();
                }
            }
            save();
            let _ = slint::invoke_from_event_loop(move || found(batch));
            RUNNING.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        RUNNING.store(false, Ordering::Release);
    }
}

/// An album's genre: its release group's tags, else its artist's. `Err` when MusicBrainz
/// couldn't be asked.
fn find(a: &AlbumCard) -> Result<Option<String>, ()> {
    let artist = a.artists.first().map(|n| n.name.as_str()).unwrap_or("");
    let title = bare_title(&a.title);
    let mut q = format!("releasegroup:\"{}\"", lucene(&title));
    if !artist.is_empty() {
        q += &format!(" AND artist:\"{}\"", lucene(artist));
    }
    let v = fetch(
        "https://musicbrainz.org/ws/2/release-group",
        &[("query", &q), ("fmt", "json"), ("limit", "3")],
        None,
    )?;
    let best = v.as_ref().and_then(|v| {
        v["release-groups"]
            .as_array()?
            .iter()
            .find(|r| r["score"].as_u64().unwrap_or(0) >= 90)
            .cloned()
    });
    if let Some(g) = best.as_ref().and_then(|r| tags_genre(&r["tags"])) {
        return Ok(Some(g.to_string()));
    }
    // The artist's genre stands in: theirs by the release group's credit, else a sure match
    // by name.
    let mbid = match best
        .as_ref()
        .and_then(|r| r["artist-credit"][0]["artist"]["id"].as_str())
    {
        Some(id) => Some(id.to_string()),
        None if !artist.is_empty() => {
            let q = format!("artist:\"{}\"", lucene(artist));
            fetch(
                "https://musicbrainz.org/ws/2/artist",
                &[("query", &q), ("fmt", "json"), ("limit", "3")],
                None,
            )?
            .and_then(|v| artist_by_name(&v, artist))
        }
        None => None,
    };
    let Some(mbid) = mbid else {
        return Ok(None);
    };
    let key = format!("artist:{mbid}");
    if let Some(e) = cache().as_ref().and_then(|c| c.get(&key).cloned())
        && fresh(&e)
    {
        return Ok(e.0);
    }
    let v = fetch(
        &format!("https://musicbrainz.org/ws/2/artist/{mbid}"),
        &[("inc", "tags"), ("fmt", "json")],
        None,
    )?;
    let g = v
        .as_ref()
        .and_then(|v| tags_genre(&v["tags"]))
        .map(str::to_string);
    remember(key, g.clone());
    Ok(g)
}

/// MusicBrainz, asked again a few times when it's busy (it answers 503 past its rate).
fn fetch(url: &str, query: &[(&str, &str)], key: Option<&str>) -> Result<Option<Value>, ()> {
    let mut wait = 2;
    loop {
        match crate::logos::fetch(url, query, key) {
            Err(()) if wait <= 16 => {
                std::thread::sleep(std::time::Duration::from_secs(wait));
                wait *= 2;
            }
            r => return r,
        }
    }
}

fn tags_genre(tags: &Value) -> Option<&'static str> {
    genre_of_tags(
        tags.as_array()?
            .iter()
            .filter_map(|t| Some((t["name"].as_str()?, t["count"].as_i64().unwrap_or(0)))),
    )
}

/// The search's top artist, when it's a sure match for exactly this name.
fn artist_by_name(v: &Value, name: &str) -> Option<String> {
    let loose = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let a = v["artists"].as_array()?.first()?;
    let sure = a["score"].as_u64() == Some(100)
        && a["name"].as_str().is_some_and(|n| loose(n) == loose(name));
    sure.then(|| a["id"].as_str().map(str::to_string))?
}

/// A title without what editions add: "Album (Deluxe Edition) [Remastered]" → "Album".
pub fn bare_title(t: &str) -> String {
    let mut s = t.trim().to_string();
    while let Some(open) = s.rfind(['(', '[']) {
        let close = if s[open..].starts_with('(') { ')' } else { ']' };
        if open == 0 || !s.trim_end().ends_with(close) {
            break;
        }
        s.truncate(open);
        s = s.trim_end().to_string();
    }
    for sep in [" - ", " – "] {
        if let Some(i) = s.find(sep) {
            let tail = s[i + sep.len()..].to_lowercase();
            if [
                "deluxe",
                "edition",
                "remaster",
                "version",
                "expanded",
                "anniversary",
            ]
            .iter()
            .any(|w| tail.contains(w))
            {
                s.truncate(i);
            }
        }
    }
    s
}

/// Text safe inside a quoted Lucene phrase.
fn lucene(s: &str) -> String {
    s.replace(['"', '\\'], " ")
}

fn path() -> std::path::PathBuf {
    cache_dir().join("genres-v1.json")
}

fn load() -> Cache {
    std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save() {
    let c = cache();
    if let Some(c) = c.as_ref()
        && let Ok(b) = serde_json::to_vec(c)
    {
        let _ = write_atomic(&path(), &b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_map_onto_broad_genres() {
        for (tag, want) in [
            ("hip hop", Some("Hip hop")),
            ("west coast hip hop", Some("Hip hop")),
            ("Hip-Hop", Some("Hip hop")),
            ("jazz rap", Some("Hip hop")),
            ("pop rap", Some("Hip hop")),
            ("trip hop", Some("Electronic")),
            ("contemporary R&B", Some("R&B")),
            ("neo soul", Some("R&B")),
            ("indie pop", Some("Pop")),
            ("k-pop", Some("Pop")),
            ("synthpop", Some("Pop")),
            ("alternative rock", Some("Rock")),
            ("pop punk", Some("Rock")),
            ("death metal", Some("Metal")),
            ("drum and bass", Some("Electronic")),
            ("deep house", Some("Electronic")),
            ("latin pop", Some("Pop")),
            ("reggaeton", Some("Latin")),
            ("singer-songwriter", Some("Folk")),
            ("film soundtrack", Some("Soundtrack")),
            ("conscious", None),
            ("rapture", None),
            ("male vocalist", None),
        ] {
            assert_eq!(genre_of_tag(tag), want, "{tag}");
        }
    }

    #[test]
    fn votes_decide_and_ties_go_by_list_order() {
        // To Pimp a Butterfly, roughly.
        let tags = [
            ("west coast hip hop", 14),
            ("conscious hip hop", 14),
            ("jazz rap", 11),
            ("neo soul", 3),
            ("funk", 0),
            ("political", 1),
        ];
        assert_eq!(genre_of_tags(tags), Some("Hip hop"));
        assert_eq!(genre_of_tags([("rock", 2), ("pop", 2)]), Some("Pop"));
        assert_eq!(genre_of_tags([("rock", 0), ("pop", -3)]), Some("Rock"));
        assert_eq!(genre_of_tags([("english", 5)]), None);
    }

    #[test]
    fn edition_words_come_off_titles() {
        assert_eq!(bare_title("Album (Deluxe Edition)"), "Album");
        assert_eq!(bare_title("Album (Deluxe) [Remastered 2011]"), "Album");
        assert_eq!(bare_title("Album - 20th Anniversary Edition"), "Album");
        assert_eq!(
            bare_title("(What's the Story) Morning Glory?"),
            "(What's the Story) Morning Glory?"
        );
        assert_eq!(
            bare_title("Good Kid, M.A.A.D City"),
            "Good Kid, M.A.A.D City"
        );
    }
}
