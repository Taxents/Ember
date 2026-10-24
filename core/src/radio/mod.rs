//! Live internet radio from Radio Garden (radio.garden): places, countries, stations, search,
//! and the stream behind each station.
//!
//! * A station plays under the id `radio:<channel id>`. The engine sends ids with that prefix
//!   to [`live`], which streams it endlessly (no duration, no seeking, reconnects on drops).
//! * Every Radio Garden page (a place or a country) is addressed by the last segment of its
//!   URL, and `ara/content/page/<id>` describes it as titled lists of stations or places.
//! * The API sits behind Cloudflare, which wants a browser user agent and a radio.garden
//!   referer.
//! * Favourites, recently played stations, searches and visited places: [`store`].
//! * Listen Along: a hosting station's exact bytes are tapped and replayed on listeners
//!   through a [`relay`].

pub mod icy;
pub mod live;
pub mod logo;
pub mod relay;
pub mod store;

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const API: &str = "https://radio.garden/api";
pub const REFERER: &str = "https://radio.garden/";
/// Play-id prefix of a station.
pub const PREFIX: &str = "radio:";
/// Pages are kept this long in memory.
const PAGE_TTL: Duration = Duration::from_secs(10 * 60);
/// The places list (1.9 MB) is kept on disk this long.
const PLACES_TTL: u64 = 7 * 24 * 3600;

// ---- ids --------------------------------------------------------------------------------------

/// Radio Garden ids are short `[A-Za-z0-9_-]` tokens (they go into URL paths).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn play_id(channel: &str) -> String {
    format!("{PREFIX}{channel}")
}

/// The channel id of a station's play id.
pub fn channel_of(id: &str) -> Option<&str> {
    id.strip_prefix(PREFIX).filter(|c| valid_id(c))
}

/// Is this play id a live station?
pub fn is_station(id: &str) -> bool {
    channel_of(id).is_some()
}

/// The id at the end of a Radio Garden URL (`/listen/kiss-fm/hcx4T119` → `hcx4T119`).
pub fn id_of_url(url: &str) -> Option<&str> {
    let id = url.trim_end_matches('/').rsplit('/').next()?;
    valid_id(id).then_some(id)
}

/// "Bucharest, Romania".
pub fn place_label(place: &str, country: &str) -> String {
    match (place.trim(), country.trim()) {
        ("", c) => c.to_string(),
        (p, "") => p.to_string(),
        (p, c) if p == c => p.to_string(),
        (p, c) => format!("{p}, {c}"),
    }
}

// ---- model ------------------------------------------------------------------------------------

/// A station: what it takes to show it and play it again.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Station {
    /// Radio Garden channel id.
    pub id: String,
    pub title: String,
    pub place: String,
    pub country: String,
    pub place_id: Option<String>,
}

impl Station {
    pub fn play_id(&self) -> String {
        play_id(&self.id)
    }

    /// "London, United Kingdom".
    pub fn subtitle(&self) -> String {
        place_label(&self.place, &self.country)
    }
}

/// A link to a place or a country page.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaceRef {
    pub id: String,
    pub title: String,
    /// The country of a place; empty for a country.
    pub subtitle: String,
    /// How many stations, when known.
    pub count: u32,
    pub country: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Section {
    Stations {
        title: String,
        stations: Vec<Station>,
    },
    Places {
        title: String,
        places: Vec<PlaceRef>,
    },
}

/// A place or country page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Page {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    /// A place (it has its own stations); else a country.
    pub place: bool,
    /// Stations in the place, when known.
    pub count: u32,
    pub sections: Vec<Section>,
}

/// A place on the globe, from the full places list.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Place {
    pub id: String,
    pub title: String,
    pub country: String,
    pub lat: f64,
    pub lon: f64,
    /// Stations there.
    pub size: u32,
}

/// A country, summed up from the places list.
#[derive(Debug, Clone, PartialEq)]
pub struct Country {
    pub name: String,
    pub stations: u32,
    pub places: u32,
}

/// Where the listener is, as Radio Garden guesses from the IP address.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Geo {
    pub lat: f64,
    pub lon: f64,
    pub city: String,
    pub country_code: String,
}

// ---- parsing ----------------------------------------------------------------------------------

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("").trim()
}

/// A channel's `page` object → station.
pub fn parse_station(p: &Value) -> Option<Station> {
    if p["type"].as_str().is_some_and(|t| t != "channel") {
        return None;
    }
    let id = p["id"]
        .as_str()
        .filter(|i| valid_id(i))
        .or_else(|| p["url"].as_str().and_then(id_of_url))?;
    let title = s(&p["title"]);
    if title.is_empty() {
        return None;
    }
    Some(Station {
        id: id.to_string(),
        title: title.to_string(),
        place: s(&p["place"]["title"]).to_string(),
        country: s(&p["country"]["title"]).to_string(),
        place_id: p["place"]["id"]
            .as_str()
            .filter(|i| valid_id(i))
            .map(str::to_string),
    })
}

/// A place or country `page` object (`type: page`) → link.
fn parse_place_ref(p: &Value) -> Option<PlaceRef> {
    if p["type"].as_str() != Some("page") {
        return None;
    }
    let id = p["url"].as_str().and_then(id_of_url)?;
    let title = s(&p["title"]);
    if title.is_empty() {
        return None;
    }
    // Places carry their map id (their own id) and a station count; countries don't.
    let country = p["map"].as_str().is_none_or(|m| m != id) && p["count"].is_null();
    Some(PlaceRef {
        id: id.to_string(),
        title: title.to_string(),
        subtitle: s(&p["subtitle"]).to_string(),
        count: p["count"].as_u64().unwrap_or(0) as u32,
        country,
    })
}

/// `ara/content/page/<id>` → page. Lists are stations or places; anything else is skipped.
pub fn parse_page(id: &str, v: &Value) -> Option<Page> {
    let d = &v["data"];
    let title = s(&d["title"]);
    if title.is_empty() {
        return None;
    }
    let own = d["url"].as_str().and_then(id_of_url).unwrap_or(id);
    let mut sections = Vec::new();
    for c in d["content"].as_array().into_iter().flatten() {
        let Some(items) = c["items"].as_array() else {
            continue;
        };
        let title = s(&c["title"]).to_string();
        let stations: Vec<Station> = dedup(
            items.iter().filter_map(|i| parse_station(&i["page"])),
            |s| s.id.clone(),
        );
        if !stations.is_empty() {
            sections.push(Section::Stations { title, stations });
            continue;
        }
        let places: Vec<PlaceRef> = dedup(
            items.iter().filter_map(|i| parse_place_ref(&i["page"])),
            |p| p.id.clone(),
        );
        if !places.is_empty() {
            sections.push(Section::Places { title, places });
        }
    }
    Some(Page {
        id: own.to_string(),
        title: title.to_string(),
        subtitle: s(&d["subtitle"]).to_string(),
        place: d["map"].as_str() == Some(own),
        count: d["count"].as_u64().unwrap_or(0) as u32,
        sections,
    })
}

/// Search response → (stations, places and countries).
pub fn parse_search(v: &Value) -> (Vec<Station>, Vec<PlaceRef>) {
    let hits: Vec<&Value> = v["hits"]["hits"]
        .as_array()
        .map(|h| h.iter().map(|h| &h["_source"]).collect())
        .unwrap_or_default();
    let stations = dedup(
        hits.iter()
            .filter(|h| h["type"].as_str() == Some("channel"))
            .filter_map(|h| parse_station(&h["page"])),
        |s| s.id.clone(),
    );
    let places = dedup(
        hits.iter()
            .filter(|h| matches!(h["type"].as_str(), Some("place" | "country")))
            .filter_map(|h| {
                let mut p = parse_place_ref(&h["page"])?;
                p.country = h["type"].as_str() == Some("country");
                Some(p)
            }),
        |p| p.id.clone(),
    );
    (stations, places)
}

/// `ara/content/places` → every place.
pub fn parse_places(v: &Value) -> Vec<Place> {
    v["data"]["list"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let id = p["id"].as_str().filter(|i| valid_id(i))?;
            let geo = p["geo"].as_array()?;
            Some(Place {
                id: id.to_string(),
                title: s(&p["title"]).to_string(),
                country: s(&p["country"]).to_string(),
                lon: geo.first()?.as_f64()?,
                lat: geo.get(1)?.as_f64()?,
                size: p["size"].as_u64().unwrap_or(0) as u32,
            })
        })
        .collect()
}

fn parse_geo(v: &Value) -> Option<Geo> {
    Some(Geo {
        lat: v["latitude"].as_f64()?,
        lon: v["longitude"].as_f64()?,
        city: s(&v["city"]).to_string(),
        country_code: s(&v["country_code"]).to_string(),
    })
}

fn dedup<T>(it: impl Iterator<Item = T>, key: impl Fn(&T) -> String) -> Vec<T> {
    let mut seen = std::collections::HashSet::new();
    it.filter(|x| seen.insert(key(x))).collect()
}

/// Countries by how many stations they have, most first.
pub fn countries(places: &[Place]) -> Vec<Country> {
    let mut m: HashMap<&str, (u32, u32)> = HashMap::new();
    for p in places.iter().filter(|p| !p.country.is_empty()) {
        let e = m.entry(&p.country).or_default();
        e.0 += p.size;
        e.1 += 1;
    }
    let mut out: Vec<Country> = m
        .into_iter()
        .map(|(name, (stations, places))| Country {
            name: name.to_string(),
            stations,
            places,
        })
        .collect();
    out.sort_by(|a, b| b.stations.cmp(&a.stations).then(a.name.cmp(&b.name)));
    out
}

/// The places of a country, biggest first.
pub fn places_in<'a>(places: &'a [Place], country: &str) -> Vec<&'a Place> {
    let mut v: Vec<&Place> = places.iter().filter(|p| p.country == country).collect();
    v.sort_by(|a, b| b.size.cmp(&a.size).then(a.title.cmp(&b.title)));
    v
}

/// Distance in km (equirectangular; plenty for "nearest").
fn km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let x = (lon2 - lon1).to_radians() * ((lat1 + lat2) / 2.0).to_radians().cos();
    let y = (lat2 - lat1).to_radians();
    (x * x + y * y).sqrt() * 6371.0
}

/// The place that stands for "near you": the biggest within 40 km, else the nearest.
pub fn nearest(places: &[Place], lat: f64, lon: f64) -> Option<&Place> {
    let close = places
        .iter()
        .filter(|p| km(lat, lon, p.lat, p.lon) <= 40.0)
        .max_by_key(|p| p.size);
    close.or_else(|| {
        places
            .iter()
            .min_by(|a, b| km(lat, lon, a.lat, a.lon).total_cmp(&km(lat, lon, b.lat, b.lon)))
    })
}

// ---- requests ---------------------------------------------------------------------------------

fn get(path: &str) -> Result<Value> {
    crate::net::get_json(
        &format!("{API}{path}"),
        &[
            ("User-Agent", crate::net::BROWSER_UA),
            ("Referer", REFERER),
            ("Accept", "application/json"),
        ],
    )
}

/// Percent-encodes a query value.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Stations, places and countries matching `q`.
pub fn search(q: &str) -> Result<(Vec<Station>, Vec<PlaceRef>)> {
    get(&format!("/search?q={}", encode(q.trim()))).map(|v| parse_search(&v))
}

/// A place or country page (kept in memory for a few minutes).
pub fn page(id: &str) -> Result<Page> {
    if !valid_id(id) {
        return Err(Error::Unavailable(format!("bad radio page id {id}")));
    }
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Page)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some((t, p)) = cache.lock().unwrap().get(id)
        && t.elapsed() < PAGE_TTL
    {
        return Ok(p.clone());
    }
    let v = get(&format!("/ara/content/page/{id}"))?;
    let p = parse_page(id, &v).ok_or_else(|| Error::Unavailable(format!("radio page {id}")))?;
    let mut c = cache.lock().unwrap();
    if c.len() > 200 {
        c.clear();
    }
    c.insert(id.to_string(), (Instant::now(), p.clone()));
    Ok(p)
}

/// Every station of a place.
pub fn place_stations(id: &str) -> Result<Vec<Station>> {
    if !valid_id(id) {
        return Err(Error::Unavailable(format!("bad radio place id {id}")));
    }
    let v = get(&format!("/ara/content/page/{id}/channels"))?;
    Ok(parse_page(id, &v)
        .map(|p| {
            p.sections
                .into_iter()
                .flat_map(|s| match s {
                    Section::Stations { stations, .. } => stations,
                    Section::Places { .. } => Vec::new(),
                })
                .collect()
        })
        .unwrap_or_default())
}

/// One station's details.
pub fn station(channel: &str) -> Result<Station> {
    if !valid_id(channel) {
        return Err(Error::Unavailable(format!("bad station id {channel}")));
    }
    let v = get(&format!("/ara/content/channel/{channel}"))?;
    parse_station(&v["data"]).ok_or_else(|| Error::Unavailable(format!("station {channel}")))
}

/// Every place with stations, from disk when fetched in the last week.
pub fn places() -> Result<Arc<Vec<Place>>> {
    static MEM: OnceLock<Mutex<Option<Arc<Vec<Place>>>>> = OnceLock::new();
    let mem = MEM.get_or_init(Default::default);
    if let Some(p) = mem.lock().unwrap().clone() {
        return Ok(p);
    }
    let path = crate::paths::cache_dir().join("radio_places.json");
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs() < PLACES_TTL);
    let cached: Option<Vec<Place>> = fresh
        .then(|| std::fs::read(&path).ok())
        .flatten()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|v: &Vec<Place>| !v.is_empty());
    let list = match cached {
        Some(v) => v,
        None => {
            let v = parse_places(&get("/ara/content/places")?);
            if v.is_empty() {
                return Err(Error::Unavailable("radio places".into()));
            }
            if let Ok(json) = serde_json::to_vec(&v) {
                let _ = crate::paths::write_atomic(&path, &json);
            }
            v
        }
    };
    let list = Arc::new(list);
    *mem.lock().unwrap() = Some(list.clone());
    Ok(list)
}

/// Where the listener is.
pub fn geo() -> Result<Geo> {
    parse_geo(&get("/geo")?).ok_or_else(|| Error::Unavailable("radio geo".into()))
}

/// A country's page id, found through search.
pub fn country_id(name: &str) -> Result<Option<String>> {
    let (_, places) = search(name)?;
    Ok(places
        .into_iter()
        .find(|p| p.country && p.title.eq_ignore_ascii_case(name))
        .map(|p| p.id))
}

/// No redirects followed: the listen endpoint's redirect is the answer.
fn no_redirects() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .into()
    })
}

/// The station's real stream URL (Radio Garden's listen endpoint redirects to it).
pub fn stream_url(channel: &str) -> Result<String> {
    if !valid_id(channel) {
        return Err(Error::Unavailable(format!("bad station id {channel}")));
    }
    let url = format!("{API}/ara/content/listen/{channel}/channel.mp3");
    let resp = no_redirects()
        .get(&url)
        .header("User-Agent", crate::net::BROWSER_UA)
        .header("Referer", REFERER)
        .call()
        .map_err(|e| Error::Net(e.to_string()))?;
    match resp.status().as_u16() {
        300..=399 => resp
            .headers()
            .get("location")
            .and_then(|l| l.to_str().ok())
            .map(|l| absolute(&url, l))
            .ok_or_else(|| Error::Net("redirect without a location".into())),
        // Served directly: the listen URL is the stream.
        200..=299 => Ok(url),
        s => Err(Error::Unavailable(format!(
            "station unavailable (HTTP {s})"
        ))),
    }
}

/// Resolves a (possibly relative) redirect target against `base`.
fn absolute(base: &str, loc: &str) -> String {
    if loc.contains("://") {
        return loc.to_string();
    }
    let origin_end = base
        .find("://")
        .map(|i| i + 3)
        .and_then(|s| base[s..].find('/').map(|j| s + j))
        .unwrap_or(base.len());
    if loc.starts_with('/') {
        format!("{}{loc}", &base[..origin_end])
    } else {
        format!("{}/{loc}", base.rsplit_once('/').map_or(base, |(a, _)| a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids() {
        assert_eq!(play_id("hcx4T119"), "radio:hcx4T119");
        assert_eq!(channel_of("radio:hcx4T119"), Some("hcx4T119"));
        assert!(is_station("radio:a_b-C9"));
        for bad in ["radio:", "radio:../x", "radio:a b", "dQw4w9WgXcQ", ""] {
            assert!(!is_station(bad), "{bad}");
        }
        assert_eq!(id_of_url("/listen/kiss-fm/hcx4T119/"), Some("hcx4T119"));
        assert_eq!(id_of_url("/listen/bad/.."), None);
    }

    #[test]
    fn helpers() {
        assert_eq!(place_label("Bucharest", "Romania"), "Bucharest, Romania");
        assert_eq!(place_label("", "Romania"), "Romania");
        assert_eq!(place_label("Singapore", "Singapore"), "Singapore");
        assert_eq!(encode("kiss fm/\u{e4}"), "kiss%20fm%2F%C3%A4");
        let base = "https://radio.garden/api/x/channel.mp3";
        assert_eq!(absolute(base, "https://a.b/s"), "https://a.b/s");
        assert_eq!(absolute(base, "/stream"), "https://radio.garden/stream");
        assert_eq!(absolute(base, "s.mp3"), "https://radio.garden/api/x/s.mp3");
    }

    #[test]
    fn stations_skip_bad_entries() {
        let ok = json!({"url": "/listen/kiss-fm/hcx4T119", "type": "channel", "title": "Kiss FM",
            "place": {"id": "p1", "title": "Bucharest"}, "country": {"id": "c1", "title": "Romania"}});
        let st = parse_station(&ok).unwrap();
        assert_eq!(st.play_id(), "radio:hcx4T119");
        assert_eq!(st.subtitle(), "Bucharest, Romania");
        assert_eq!(st.place_id.as_deref(), Some("p1"));
        assert!(parse_station(&json!({"url": "/listen/x/..", "title": "Bad"})).is_none());
        assert!(parse_station(&json!({"url": "/listen/x/abc", "title": ""})).is_none());
        assert!(
            parse_station(&json!({"url": "/visit/x/abc", "type": "page", "title": "X"})).is_none()
        );
    }

    #[test]
    fn nearest_prefers_big_places_close_by() {
        let p = |id: &str, lat, lon, size| Place {
            id: id.into(),
            lat,
            lon,
            size,
            ..Default::default()
        };
        let places = vec![
            p("village", 51.06, 3.72, 1),
            p("city", 51.2, 3.5, 40),
            p("far", 48.8, 2.3, 700),
        ];
        assert_eq!(nearest(&places, 51.05, 3.71).unwrap().id, "city");
        assert_eq!(nearest(&places, 40.0, 0.0).unwrap().id, "far");
        assert!(nearest(&[], 0.0, 0.0).is_none());
    }
}
