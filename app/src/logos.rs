//! Artist logos, shown in place of the name on artist pages. YouTube Music has none, so they come
//! from fanart.tv (our project key, compiled in from `secrets.toml`), by the artist's MusicBrainz
//! id: found through MusicBrainz's link to their YouTube channel, else an exact-name search.
//! Never a namesake's guess: no logo beats a wrong one.

use music_core::paths::{cache_dir, unix_now, write_atomic};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const FANART_KEY: &str = env!("FANART_KEY");
const UA: &str = concat!(
    "Ember/",
    env!("CARGO_PKG_VERSION"),
    " ( https://github.com/Taxents/Ember )"
);
/// Answers (logo or none) are kept this long.
const TTL: u64 = 30 * 86400;

type Cache = HashMap<String, (Option<String>, u64)>;

/// The answers so far, read from disk once.
fn cache() -> std::sync::MutexGuard<'static, Option<Cache>> {
    static CACHE: Mutex<Option<Cache>> = Mutex::new(None);
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if c.is_none() {
        *c = Some(load());
    }
    c
}

/// The answer already known for an artist (`Some(None)`: known to have no logo), without
/// asking anyone. Quick enough for the UI thread.
pub fn known(name: &str, channel: Option<&str>) -> Option<Option<String>> {
    if FANART_KEY.is_empty() {
        return Some(None);
    }
    let key = channel.unwrap_or(name);
    let c = cache();
    let (url, at) = c.as_ref()?.get(key)?;
    (unix_now().saturating_sub(*at) < TTL).then(|| url.clone())
}

/// The logo's URL for an artist, if there is one. Blocks (network); call on a worker.
pub fn find(name: &str, channel: Option<&str>) -> Option<String> {
    if let Some(answer) = known(name, channel) {
        return answer;
    }
    let key = channel.unwrap_or(name).to_string();
    let now = unix_now();
    let mut failed = false;
    let mut get =
        |url: &str, query: &[(&str, &str)], key: Option<&str>| match fetch(url, query, key) {
            Ok(v) => v,
            Err(()) => {
                failed = true;
                None
            }
        };
    let by_channel = channel.and_then(|c| {
        let resource = format!("https://www.youtube.com/channel/{c}");
        get(
            "https://musicbrainz.org/ws/2/url",
            &[
                ("resource", &resource),
                ("inc", "artist-rels"),
                ("fmt", "json"),
            ],
            None,
        )
        .and_then(|v| {
            v["relations"]
                .as_array()?
                .iter()
                .find_map(|r| r["artist"]["id"].as_str().map(str::to_string))
        })
    });
    let mbid = by_channel.or_else(|| {
        let q = format!("artist:\"{}\"", name.replace('"', ""));
        get(
            "https://musicbrainz.org/ws/2/artist",
            &[("query", &q), ("fmt", "json"), ("limit", "5")],
            None,
        )
        .and_then(|v| by_name(&v, name))
    });
    let url = mbid.and_then(|m| {
        get(
            &format!("https://webservice.fanart.tv/v3/music/{m}"),
            &[],
            Some(FANART_KEY),
        )
        .and_then(|v| best_logo(&v))
    });
    // A failed request is asked again next time; a real "none" is remembered.
    if !failed || url.is_some() {
        let mut c = cache();
        let c = c.get_or_insert_with(HashMap::new);
        c.insert(key, (url.clone(), now));
        save(c);
    }
    url
}

/// The search's top match, when it's a sure one with exactly this name.
fn by_name(v: &Value, name: &str) -> Option<String> {
    let a = v["artists"].as_array()?.first()?;
    let sure = a["score"].as_u64() == Some(100)
        && a["name"].as_str().is_some_and(|n| loose(n) == loose(name));
    sure.then(|| a["id"].as_str().map(str::to_string))?
}

/// The newest HD logo, else the newest standard one: the one fanart.tv shows on the artist's
/// page (likes barely differ, and the newest is the current look).
fn best_logo(v: &Value) -> Option<String> {
    let pick = |k: &str| {
        v[k].as_array()?
            .iter()
            .filter(|l| l["lang"].as_str().is_none_or(|x| x == "en" || x.is_empty()))
            .max_by_key(|l| {
                l["id"]
                    .as_str()
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0)
            })
            .and_then(|l| l["url"].as_str())
            // Listed as http://; the same file is served over https, the only scheme shown.
            .map(|u| match u.strip_prefix("http://") {
                Some(rest) => format!("https://{rest}"),
                None => u.to_string(),
            })
    };
    pick("hdmusiclogo").or_else(|| pick("musiclogo"))
}

fn loose(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// GET JSON, at most one request a second to each host (MusicBrainz's rule). `Ok(None)` is a
/// plain "not found". `key` goes in fanart.tv's `api-key` header, not the URL, so it stays out
/// of logs and proxies.
pub(crate) fn fetch(
    url: &str,
    query: &[(&str, &str)],
    key: Option<&str>,
) -> Result<Option<Value>, ()> {
    static LAST: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());
    let host = url.split('/').nth(2).unwrap_or_default().to_string();
    // One request a second per host: this one's turn is booked, then waited for unlocked, so
    // other hosts (and the next request here) don't wait behind the sleep.
    let wait = {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let at = match last.iter().find(|(h, _)| *h == host) {
            Some((_, t)) => (*t + Duration::from_millis(1100)).max(now),
            None => now,
        };
        last.retain(|(h, _)| *h != host);
        last.push((host, at));
        at - now
    };
    std::thread::sleep(wait);
    let mut req = music_core::net::agent()
        .get(url)
        .header("User-Agent", UA)
        .header("Accept", "application/json");
    for (k, v) in query {
        req = req.query(*k, *v);
    }
    if let Some(key) = key {
        req = req.header("api-key", key);
    }
    let mut r = req.call().map_err(|_| ())?;
    match r.status().as_u16() {
        200 => Ok(r.body_mut().read_json::<Value>().ok()),
        404 => Ok(None),
        c => {
            log::debug!("MusicBrainz/fanart.tv lookup: HTTP {c}");
            Err(())
        }
    }
}

fn path() -> std::path::PathBuf {
    // v2: picked newest-first (v1 picked the most liked).
    cache_dir().join("logos-v2.json")
}

fn load() -> Cache {
    std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(m: &Cache) {
    if let Ok(b) = serde_json::to_vec(m) {
        let _ = write_atomic(&path(), &b);
    }
}
