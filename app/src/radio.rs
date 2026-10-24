//! The Radio page: live stations from Radio Garden.
//!
//! * Pages are `PageKind::Radio` routes: "" (home), `search:<q>`, `page:<id>` (a place or a
//!   country), `country:<name>` and `countries`. They load on a worker into the usual
//!   [`Loaded`] blocks: station shelves, place tiles, and a grid under a heading.
//! * A station is a [`Track`] whose video id is `radio:<channel>` (so the queue, the bar, the
//!   media flyout and the saved queue work unchanged); places, countries and searches are
//!   [`Link`] tiles whose browse id starts with `radio:`.
//! * Playing a station queues the stations beside it, so next and previous step through them.
//! * Favourites, recently played stations, searches and places visited live in `radio.json`
//!   ([`music_core::radio::store`]), on this machine only.

use crate::pages::{Block, Loaded};
use crate::{App, Card, MenuEntry, MenuTarget, PageKind, Route, RowRef, Ui};
use music_core::radio::{self, PlaceRef, Section, Station, store::Store};
use music_core::yt::model::{BrowseRef, Item, Link, Named, Shelf, Track};
use music_core::{Error, Result};
use slint::{ComponentHandle, Image, Model, Rgba8Pixel, SharedPixelBuffer, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Big cities whose popular stations the home page shows.
const WORLD: [&str; 5] = ["London", "New York NY", "Paris", "Tokyo", "Berlin"];
/// Stations per shelf.
const SHELF: usize = 20;
/// Countries on the home page's shelf ("See all" has the rest).
const COUNTRIES: usize = 24;

/// Warm tile colours (ARGB), picked by name so a place keeps its colour.
const TILES: [u32; 10] = [
    0xFF76291E, 0xFF8A4B1C, 0xFF5B3A29, 0xFF3F4A3C, 0xFF2F4858, 0xFF6B3E5E, 0xFF7A5C1E, 0xFF4E3B6B,
    0xFF365E5B, 0xFF8C3B2E,
];

fn store() -> MutexGuard<'static, Store> {
    static S: OnceLock<Mutex<Store>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Store::load()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Stations seen in lists, by channel id: what a station in the queue is.
fn seen() -> MutexGuard<'static, HashMap<String, Station>> {
    static S: OnceLock<Mutex<HashMap<String, Station>>> = OnceLock::new();
    S.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

thread_local! {
    /// What each station last said it plays (its stream title), by play id.
    static LIVE: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    /// Generated station artwork, by play id.
    static ART: RefCell<HashMap<String, Image>> = RefCell::new(HashMap::new());
    /// The page row and card the open radio menu is for.
    static MENU_AT: RefCell<Option<(usize, usize)>> = const { RefCell::new(None) };
}

pub fn is_station(id: &str) -> bool {
    radio::is_station(id)
}

fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}

fn tile_color(name: &str) -> u32 {
    TILES[(hash(name) % TILES.len() as u64) as usize]
}

// ---- items ------------------------------------------------------------------------------------

/// A station as a track: its place is the "artist".
pub fn track(st: &Station) -> Track {
    seen().insert(st.id.clone(), st.clone());
    Track {
        video_id: st.play_id(),
        title: st.title.clone(),
        artists: vec![Named {
            name: st.subtitle(),
            id: None,
        }],
        ..Default::default()
    }
}

/// A station's track from what's known of it here (lists seen, favourites, recents).
pub fn known_station(id: &str) -> Option<Track> {
    let c = radio::channel_of(id)?;
    let st = seen().get(c).cloned();
    let st = st.or_else(|| store().station(c).cloned())?;
    Some(track(&st))
}

/// The station behind a track (from the lists seen, else from the track itself).
pub fn station_of(t: &Track) -> Option<Station> {
    let channel = radio::channel_of(&t.video_id)?;
    if let Some(s) = seen().get(channel) {
        return Some(s.clone());
    }
    if let Some(s) = store().station(channel) {
        return Some(s.clone());
    }
    let line = t.artist_line();
    let (place, country) = line.rsplit_once(", ").unwrap_or((line.as_str(), ""));
    Some(Station {
        id: channel.to_string(),
        title: t.title.clone(),
        place: place.to_string(),
        country: country.to_string(),
        place_id: None,
    })
}

fn station_item(st: &Station) -> Item {
    Item::Track(track(st))
}

fn link(title: &str, target: String) -> Item {
    Item::Link(Link {
        title: title.to_string(),
        browse_id: format!("radio:{target}"),
        params: None,
        color: Some(tile_color(title)),
    })
}

fn place_item(p: &PlaceRef) -> Item {
    link(&p.title, format!("page:{}", p.id))
}

fn stations_shelf(title: &str, stations: &[Station]) -> Option<Block> {
    (!stations.is_empty()).then(|| {
        Block::Shelf(Shelf {
            title: title.to_string(),
            items: stations.iter().take(SHELF).map(station_item).collect(),
            more: None,
        })
    })
}

fn tiles_shelf(title: &str, items: Vec<Item>, more: Option<&str>) -> Option<Block> {
    (!items.is_empty()).then(|| {
        Block::Shelf(Shelf {
            title: title.to_string(),
            items,
            more: more.map(|m| BrowseRef {
                browse_id: format!("radio:{m}"),
                params: None,
            }),
        })
    })
}

/// A heading over the grid that follows.
fn heading(title: String) -> Block {
    Block::Tracks {
        title,
        more: None,
        tracks: Vec::new(),
    }
}

/// Where a `radio:` browse id leads.
pub fn route(browse_id: &str, title: &str) -> Option<Route> {
    let id = browse_id.strip_prefix("radio:")?;
    // A station's play id isn't a page.
    if radio::valid_id(id) && id != "countries" {
        return None;
    }
    Some(Route {
        kind: PageKind::Radio,
        id: id.to_string(),
        title: title.to_string(),
    })
}

// ---- pages (on a worker) ----------------------------------------------------------------------

pub fn load(r: &Route) -> Result<Loaded> {
    let id = r.id.as_str();
    if id.is_empty() {
        return home();
    }
    if id == "countries" {
        return all_countries();
    }
    if let Some(q) = id.strip_prefix("search:") {
        return search(q);
    }
    if let Some(name) = id.strip_prefix("country:") {
        let pid = radio::country_id(name)?
            .ok_or_else(|| Error::Unavailable(format!("no stations found in {name}")))?;
        return page(&pid);
    }
    if let Some(pid) = id.strip_prefix("page:") {
        return page(pid);
    }
    Err(Error::Unavailable(format!("radio page {id}")))
}

fn home() -> Result<Loaded> {
    let data = store().data.clone();
    let mut blocks: Vec<Block> = Vec::new();
    blocks.extend(stations_shelf("Favourites", &data.favourites));
    blocks.extend(stations_shelf("Recently played", &data.recent));

    // Where you are and the world's big cities, fetched side by side.
    let (places, geo) = std::thread::scope(|s| {
        let geo = s.spawn(radio::geo);
        (radio::places(), geo.join().ok().and_then(|g| g.ok()))
    });
    let places = places.ok();
    let near = places
        .as_deref()
        .zip(geo.as_ref())
        .and_then(|(p, g)| radio::nearest(p, g.lat, g.lon))
        .map(|p| p.id.clone());
    let mut cities: Vec<String> = Vec::new();
    if let Some(p) = places.as_deref() {
        for name in WORLD {
            if let Some(c) = p.iter().filter(|x| x.title == name).max_by_key(|x| x.size)
                && Some(&c.id) != near.as_ref()
            {
                cities.push(c.id.clone());
            }
        }
        cities.truncate(4);
    }
    let ids: Vec<String> = near.iter().cloned().chain(cities).collect();
    let pages: Vec<Option<radio::Page>> = std::thread::scope(|s| {
        let handles: Vec<_> = ids
            .iter()
            .map(|id| s.spawn(move || radio::page(id)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .ok()
                    .and_then(|r| r.map_err(|e| log::warn!("radio: {e}")).ok())
            })
            .collect()
    });
    let mut pages = pages.into_iter();
    let mut nearby: Option<Block> = None;
    if near.is_some()
        && let Some(Some(p)) = pages.next()
    {
        for s in &p.sections {
            match s {
                Section::Stations { title, stations } => {
                    blocks.extend(stations_shelf(title, stations))
                }
                Section::Places { title, places } if nearby.is_none() => {
                    nearby = tiles_shelf(title, places.iter().map(place_item).collect(), None);
                }
                Section::Places { .. } => {}
            }
        }
    }
    blocks.extend(tiles_shelf(
        "Places you visited",
        data.visited.iter().map(place_item).collect(),
        None,
    ));
    blocks.extend(tiles_shelf(
        "Your searches",
        data.searches
            .iter()
            .map(|q| link(q, format!("search:{q}")))
            .collect(),
        None,
    ));
    for p in pages.flatten() {
        // Each city's own popular list.
        let popular = p.sections.iter().find_map(|s| match s {
            Section::Stations { title, stations } if title.starts_with("Popular in") => {
                Some((title, stations))
            }
            _ => None,
        });
        if let Some((title, stations)) = popular {
            blocks.extend(stations_shelf(title, stations));
        }
    }
    if let Some(p) = places.as_deref() {
        let countries: Vec<Item> = radio::countries(p)
            .iter()
            .take(COUNTRIES)
            .map(|c| link(&c.name, format!("country:{}", c.name)))
            .collect();
        blocks.extend(tiles_shelf(
            "Browse by country",
            countries,
            Some("countries"),
        ));
    }
    blocks.extend(nearby);
    if blocks.is_empty() {
        return Err(Error::Net("couldn't reach Radio Garden".into()));
    }
    Ok(Loaded {
        title: Some("Radio".into()),
        blocks,
        ..Default::default()
    })
}

fn all_countries() -> Result<Loaded> {
    let places = radio::places()?;
    Ok(Loaded {
        title: Some("Countries".into()),
        grid: radio::countries(&places)
            .iter()
            .map(|c| link(&c.name, format!("country:{}", c.name)))
            .collect(),
        ..Default::default()
    })
}

fn search(q: &str) -> Result<Loaded> {
    let q = q.trim();
    let (stations, places) = radio::search(q)?;
    if !stations.is_empty() || !places.is_empty() {
        store().searched(q);
    }
    let mut blocks: Vec<Block> = Vec::new();
    blocks.extend(tiles_shelf(
        "Places",
        places.iter().map(place_item).collect(),
        None,
    ));
    if !stations.is_empty() {
        blocks.push(heading("Stations".into()));
    } else if places.is_empty() {
        // Keep the page (and its search field) up with a note.
        blocks.push(heading(format!(
            "No stations or places match \u{201c}{q}\u{201d}"
        )));
    }
    Ok(Loaded {
        title: Some(format!("\u{201c}{q}\u{201d}")),
        blocks,
        grid: stations.iter().map(station_item).collect(),
        ..Default::default()
    })
}

/// A place (its lists, then every station) or a country (its lists, then every place).
fn page(id: &str) -> Result<Loaded> {
    let p = radio::page(id)?;
    store().visited(&PlaceRef {
        id: p.id.clone(),
        title: p.title.clone(),
        subtitle: p.subtitle.clone(),
        count: p.count,
        country: !p.place,
    });
    let mut blocks: Vec<Block> = Vec::new();
    for s in &p.sections {
        match s {
            // The full list follows as the grid.
            Section::Stations { title, .. } if p.place && title.starts_with("Stations in") => {}
            Section::Places { title, .. } if !p.place && title.starts_with("Places in") => {}
            Section::Stations { title, stations } => blocks.extend(stations_shelf(title, stations)),
            Section::Places { title, places } => blocks.extend(tiles_shelf(
                title,
                places.iter().map(place_item).collect(),
                None,
            )),
        }
    }
    let grid: Vec<Item> = if p.place {
        let all = radio::place_stations(&p.id).unwrap_or_else(|e| {
            log::warn!("radio stations of {}: {e}", p.id);
            Vec::new()
        });
        if !all.is_empty() {
            blocks.push(heading(format!("All stations in {}", p.title)));
        }
        all.iter().map(station_item).collect()
    } else {
        let places = radio::places()?;
        let mine = radio::places_in(&places, &p.title);
        if !mine.is_empty() {
            blocks.push(heading(format!("Places in {}", p.title)));
        }
        mine.iter()
            .map(|x| {
                place_item(&PlaceRef {
                    id: x.id.clone(),
                    title: x.title.clone(),
                    ..Default::default()
                })
            })
            .collect()
    };
    Ok(Loaded {
        title: Some(p.title.clone()),
        blocks,
        grid,
        ..Default::default()
    })
}

// ---- artwork ----------------------------------------------------------------------------------

/// A station's artwork: its logo once found, till then a warm gradient with faint rings.
pub fn art(id: &str, title: &str) -> Image {
    if let Some(img) = ART.with(|a| a.borrow().get(id).cloned()) {
        return img;
    }
    let img = make_art(title);
    ART.with(|a| a.borrow_mut().insert(id.to_string(), img.clone()));
    ask_logo(id);
    img
}

/// Looks for the station's logo on a worker and swaps it in where the station shows.
fn ask_logo(id: &str) {
    let Some(st) = radio::channel_of(id).and_then(|c| {
        seen()
            .get(c)
            .cloned()
            .or_else(|| store().station(c).cloned())
    }) else {
        return;
    };
    let id = id.to_string();
    // Asked for every station a page lists: kept out of the way of page loads.
    crate::workers::run_background(
        move || radio::logo::logo(&st).and_then(|b| logo_tile(&b)),
        move |tile| {
            if let Some(tile) = tile {
                let img = Image::from_rgba8(tile);
                ART.with(|a| a.borrow_mut().insert(id.clone(), img.clone()));
                crate::with(|a| a.station_art(&id, img));
            }
        },
    );
}

/// A logo on a square tile: an opaque square logo fills it; others sit in the middle, on their
/// own edge colour, or (transparent) on ivory when dark and on smoke when light.
fn logo_tile(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    const N: u32 = 256;
    let logo = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = logo.dimensions();
    if w < 16 || h < 16 {
        return None;
    }
    let corner = logo.get_pixel(0, 0).0;
    let opaque = corner[3] > 240;
    let bg = if opaque {
        [corner[0], corner[1], corner[2]]
    } else {
        let (mut sum, mut n) = (0.0f32, 0.0f32);
        for p in logo.pixels().filter(|p| p.0[3] > 128) {
            sum += 0.2126 * p.0[0] as f32 + 0.7152 * p.0[1] as f32 + 0.0722 * p.0[2] as f32;
            n += 1.0;
        }
        if n > 0.0 && sum / n < 110.0 {
            [0xF1, 0xE5, 0xD2]
        } else {
            [0x24, 0x1A, 0x16]
        }
    };
    let square = (0.9..1.1).contains(&(w as f32 / h as f32));
    let fill = if opaque && square {
        1.0
    } else if opaque {
        0.86
    } else {
        0.72
    };
    let scale = N as f32 * fill / w.max(h) as f32;
    let sw = ((w as f32 * scale).round() as u32).clamp(1, N);
    let sh = ((h as f32 * scale).round() as u32).clamp(1, N);
    let small = image::imageops::resize(&logo, sw, sh, image::imageops::FilterType::Lanczos3);
    let (ox, oy) = ((N - sw) / 2, (N - sh) / 2);
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(N, N);
    for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i as u32 % N, i as u32 / N);
        let mut c = bg.map(f32::from);
        if (ox..ox + sw).contains(&x) && (oy..oy + sh).contains(&y) {
            let p = small.get_pixel(x - ox, y - oy).0;
            let a = p[3] as f32 / 255.0;
            for k in 0..3 {
                c[k] = c[k] * (1.0 - a) + p[k] as f32 * a;
            }
        }
        *px = Rgba8Pixel {
            r: c[0] as u8,
            g: c[1] as u8,
            b: c[2] as u8,
            a: 255,
        };
    }
    Some(buf)
}

fn make_art(title: &str) -> Image {
    const N: u32 = 64;
    let h = hash(title);
    let c1 = TILES[(h % TILES.len() as u64) as usize];
    let c2 = TILES[((h / 11) % TILES.len() as u64) as usize];
    let rgb = |c: u32| {
        [
            (c >> 16) as u8 as f32,
            (c >> 8) as u8 as f32,
            c as u8 as f32,
        ]
    };
    let (a, b) = (rgb(c1), rgb(c2));
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(N, N);
    for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i as u32 % N) as f32, (i as u32 / N) as f32);
        let t = (x + (N as f32 - y)) / (2.0 * N as f32);
        // Rings spreading from the lower left, like a broadcast.
        let d = (x * x + (N as f32 - y).powi(2)).sqrt();
        let ring = if (d / 10.0).fract() < 0.14 { 1.12 } else { 1.0 };
        let shade = 0.55 + 0.45 * (1.0 - t);
        let mix = |k: usize| ((a[k] * (1.0 - t) + b[k] * t * 0.6) * shade * ring).min(255.0) as u8;
        *px = Rgba8Pixel {
            r: mix(0),
            g: mix(1),
            b: mix(2),
            a: 255,
        };
    }
    Image::from_rgba8(buf)
}

/// Gives a station's card its artwork.
pub fn dress(c: &mut Card, item: &Item) {
    if let Item::Track(t) = item
        && is_station(&t.video_id)
    {
        c.art = art(&t.video_id, &t.title);
    }
}

// ---- titles -----------------------------------------------------------------------------------

/// A stream title worth showing ("Artist - Title"), not "-" or the station's own name.
fn useful_title(raw: &str, station: &str) -> Option<String> {
    let t = raw.trim();
    let squash = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let core = squash(t);
    (!core.is_empty() && core != squash(station)).then(|| t.to_string())
}

// ---- the app side -----------------------------------------------------------------------------

impl App {
    /// A card on a radio page was clicked: plays the station (with the others beside it) or
    /// opens the place. False when it isn't radio's.
    pub(crate) fn radio_card_clicked(&mut self, row: usize, i: usize) -> bool {
        let Some(item) = self.card_item(row, i).cloned() else {
            return false;
        };
        match item {
            Item::Link(l) => match route(&l.browse_id, &l.title) {
                Some(r) => {
                    self.navigate(r);
                    true
                }
                None => false,
            },
            Item::Track(t) if is_station(&t.video_id) => {
                let items: Vec<Item> = match &self.rows[row] {
                    RowRef::Shelf(items, _) => items.clone(),
                    _ => self.grid.clone(),
                };
                let tracks: Vec<Track> = items
                    .into_iter()
                    .filter_map(|x| match x {
                        Item::Track(t) if is_station(&t.video_id) => Some(t),
                        _ => None,
                    })
                    .collect();
                let start = tracks
                    .iter()
                    .position(|x| x.video_id == t.video_id)
                    .unwrap_or(0);
                self.play_stations(tracks, start);
                true
            }
            _ => false,
        }
    }

    /// Replaces the queue with these stations and plays `start`.
    fn play_stations(&mut self, tracks: Vec<Track>, start: usize) {
        LIVE.with(|l| {
            let mut l = l.borrow_mut();
            for t in &tracks {
                l.remove(&t.video_id);
            }
        });
        self.play_list(tracks, Some(start), Some(false));
    }

    /// The right-click menu for a radio card.
    pub(crate) fn radio_card_menu(
        &self,
        row: usize,
        j: usize,
    ) -> Option<(MenuTarget, Vec<MenuEntry>)> {
        let item = self.card_item(row, j)?.clone();
        let on_home =
            self.history[self.at].kind == PageKind::Radio && self.history[self.at].id.is_empty();
        let shelf = self
            .page_rows
            .row_data(row)
            .map(|r| r.title.to_string())
            .unwrap_or_default();
        let mut items = Vec::new();
        match &item {
            Item::Track(t) if is_station(&t.video_id) => {
                let st = station_of(t)?;
                let s = store();
                items.push(crate::menu_entry("radio-play", "Play", "play", false));
                items.push(crate::menu_entry("-", "", "", false));
                items.push(if s.is_favourite(&st.id) {
                    crate::menu_entry(
                        "radio-unfav",
                        "Remove from favourites",
                        "heart-filled",
                        false,
                    )
                } else {
                    crate::menu_entry("radio-fav", "Add to favourites", "heart", false)
                });
                if st.place_id.is_some() {
                    items.push(crate::menu_entry(
                        "radio-place",
                        &format!("Go to {}", st.place),
                        "radio",
                        false,
                    ));
                }
                if on_home && shelf == "Recently played" {
                    items.push(crate::menu_entry("-", "", "", false));
                    items.push(crate::menu_entry(
                        "radio-forget",
                        "Remove from recently played",
                        "close",
                        true,
                    ));
                }
            }
            Item::Link(l) if l.browse_id.starts_with("radio:") => {
                items.push(crate::menu_entry("radio-open", "Open", "grid", false));
                if on_home && shelf == "Places you visited" {
                    items.push(crate::menu_entry("-", "", "", false));
                    items.push(crate::menu_entry(
                        "radio-forget",
                        "Remove from places you visited",
                        "close",
                        true,
                    ));
                } else if on_home && shelf == "Your searches" {
                    items.push(crate::menu_entry("-", "", "", false));
                    items.push(crate::menu_entry(
                        "radio-forget",
                        "Remove from your searches",
                        "close",
                        true,
                    ));
                }
            }
            _ => return None,
        }
        MENU_AT.with(|m| *m.borrow_mut() = Some((row, j)));
        Some((MenuTarget::Card(Box::new(item)), items))
    }

    /// The right-click menu for a station in the queue or the now-playing bar.
    pub(crate) fn radio_queue_menu(
        &self,
        what: &str,
        row: usize,
    ) -> Option<(MenuTarget, Vec<MenuEntry>)> {
        let pos = if what == "np" {
            self.player.up_next().1
        } else {
            self.queue_from + row
        };
        let t = self.queue_ids.get(pos).and_then(|id| self.known.get(id))?;
        let st = station_of(t)?;
        MENU_AT.with(|m| *m.borrow_mut() = None);
        let mut items = vec![crate::menu_entry("radio-jump", "Play", "play", false)];
        items.push(if store().is_favourite(&st.id) {
            crate::menu_entry(
                "radio-unfav",
                "Remove from favourites",
                "heart-filled",
                false,
            )
        } else {
            crate::menu_entry("radio-fav", "Add to favourites", "heart", false)
        });
        if st.place_id.is_some() {
            items.push(crate::menu_entry(
                "radio-place",
                &format!("Go to {}", st.place),
                "radio",
                false,
            ));
        }
        if pos != self.player.up_next().1 {
            items.push(crate::menu_entry("-", "", "", false));
            items.push(crate::menu_entry(
                "radio-remove",
                "Remove from queue",
                "close",
                true,
            ));
        }
        Some((MenuTarget::Queue(pos), items))
    }

    /// A radio menu entry was chosen.
    pub(crate) fn radio_menu_chosen(&mut self, choice: &str, target: MenuTarget) {
        let at = MENU_AT.with(|m| m.borrow_mut().take());
        let (station, item) = match &target {
            MenuTarget::Card(item) => (
                match item.as_ref() {
                    Item::Track(t) => station_of(t),
                    _ => None,
                },
                Some(item.as_ref().clone()),
            ),
            MenuTarget::Queue(pos) => (
                self.queue_ids
                    .get(*pos)
                    .and_then(|id| self.known.get(id))
                    .and_then(station_of),
                None,
            ),
            _ => (None, None),
        };
        let on_home =
            self.history[self.at].kind == PageKind::Radio && self.history[self.at].id.is_empty();
        match choice {
            "radio-play" | "radio-open" => {
                if let Some((row, i)) = at {
                    self.radio_card_clicked(row, i);
                }
            }
            "radio-jump" => {
                if let MenuTarget::Queue(pos) = target {
                    self.player.jump(pos);
                }
            }
            "radio-remove" => {
                if let MenuTarget::Queue(pos) = target {
                    self.player.remove(pos);
                    self.queue_changed();
                }
            }
            "radio-fav" | "radio-unfav" => {
                let Some(st) = station else { return };
                let on = choice == "radio-fav";
                store().set_favourite(&st, on);
                self.favourite_changed(&st.play_id(), on);
                if on_home {
                    // The Favourites shelf changed: show it as it is now.
                    if !on
                        && let Some((row, i)) =
                            at.filter(|&(row, _)| self.shelf_title(row) == "Favourites")
                    {
                        self.remove_card(row, i);
                    } else {
                        self.reload();
                    }
                }
            }
            "radio-place" => {
                if let Some(st) = station
                    && let Some(pid) = st.place_id
                {
                    self.navigate(Route {
                        kind: PageKind::Radio,
                        id: format!("page:{pid}"),
                        title: st.place,
                    });
                }
            }
            "radio-forget" => {
                let Some((row, i)) = at else { return };
                match item {
                    Some(Item::Track(t)) => {
                        if let Some(c) = radio::channel_of(&t.video_id) {
                            store().forget_played(c);
                        }
                    }
                    Some(Item::Link(l)) => {
                        let target = l.browse_id.strip_prefix("radio:").unwrap_or("");
                        if let Some(q) = target.strip_prefix("search:") {
                            store().forget_search(q);
                        } else if let Some(id) = target.strip_prefix("page:") {
                            store().forget_visited(id);
                        }
                    }
                    _ => return,
                }
                self.remove_card(row, i);
            }
            _ => {}
        }
    }

    fn shelf_title(&self, row: usize) -> String {
        self.page_rows
            .row_data(row)
            .map(|r| r.title.to_string())
            .unwrap_or_default()
    }

    /// Takes one card off a shelf; an emptied shelf goes with a reload.
    fn remove_card(&mut self, row: usize, i: usize) {
        let empty = match self.rows.get_mut(row) {
            Some(RowRef::Shelf(items, _)) if i < items.len() => {
                items.remove(i);
                if let Some(r) = self.page_rows.row_data(row)
                    && let Some(cards) = r.cards.as_any().downcast_ref::<VecModel<Card>>()
                    && i < cards.row_count()
                {
                    cards.remove(i);
                    // The cards after it move up.
                    let mut all: Vec<Card> = cards.iter().collect();
                    crate::place_cards(&mut all);
                    for (j, c) in all.into_iter().enumerate().skip(i) {
                        cards.set_row_data(j, c);
                    }
                }
                items.is_empty()
            }
            _ => return,
        };
        if empty {
            self.reload();
        }
    }

    /// The heart in the bar on a station: favourite or not.
    fn favourite_changed(&mut self, play_id: &str, on: bool) {
        if self.player.current() == Some(play_id) {
            let ui = self.ui();
            let g = ui.global::<Ui>();
            let mut np = g.get_np();
            np.liked = on;
            g.set_np(np);
        }
    }

    /// The bar's heart while a station plays: adds or removes the favourite. False when no
    /// station plays.
    pub(crate) fn station_like(&mut self) -> bool {
        let Some(t) = self
            .player
            .current()
            .filter(|id| is_station(id))
            .and_then(|id| self.known.get(id))
        else {
            return false;
        };
        let Some(st) = station_of(t) else {
            return false;
        };
        let on = !store().is_favourite(&st.id);
        store().set_favourite(&st, on);
        self.favourite_changed(&st.play_id(), on);
        if self.history[self.at].kind == PageKind::Radio && self.history[self.at].id.is_empty() {
            self.reload();
        }
        true
    }

    /// A station became audible: it goes to the front of the recents.
    pub(crate) fn station_started(&mut self, id: &str) {
        if let Some(st) = self.known.get(id).and_then(station_of) {
            store().played(&st);
        }
    }

    /// A station said what it plays now.
    pub(crate) fn station_title(&mut self, id: &str, title: &str) {
        LIVE.with(|l| l.borrow_mut().insert(id.to_string(), title.to_string()));
        if self.player.current() == Some(id)
            && let Some(t) = self.known.get(id).cloned()
        {
            self.station_now_playing(&t);
        }
    }

    /// A station's logo arrived: swap it in on the page's cards, in the queue and in the bar.
    pub(crate) fn station_art(&mut self, id: &str, img: Image) {
        for ri in 0..self.rows.len() {
            let n = match &self.rows[ri] {
                RowRef::Shelf(items, _) => items.len(),
                RowRef::Grid(_) => self
                    .page_rows
                    .row_data(ri)
                    .map_or(0, |r| r.cards.row_count()),
                _ => continue,
            };
            for j in 0..n {
                if matches!(self.card_item(ri, j), Some(Item::Track(t)) if t.video_id == id)
                    && let Some(row) = self.page_rows.row_data(ri)
                    && let Some(mut c) = row.cards.row_data(j)
                {
                    c.art = img.clone();
                    row.cards.set_row_data(j, c);
                }
            }
        }
        let at: Vec<usize> = (self.queue_ids.iter().skip(self.queue_from).enumerate())
            .filter(|(_, q)| q.as_str() == id)
            .map(|(i, _)| i)
            .collect();
        for i in at {
            if let Some(mut r) = self.queue_rows.row_data(i) {
                r.art = img.clone();
                self.queue_rows.set_row_data(i, r);
            }
        }
        if self.player.current() == Some(id) {
            let ui = self.ui();
            let g = ui.global::<Ui>();
            let mut np = g.get_np();
            np.art = img;
            g.set_np(np);
        }
    }

    /// The bar and the media flyout for a station: its name over what it plays now (else its
    /// place), its artwork, and the heart as "favourite".
    pub(crate) fn station_now_playing(&mut self, t: &Track) {
        let station = is_station(&t.video_id);
        self.ui().global::<Ui>().set_np_live(station);
        if !station {
            return;
        }
        let place = t.artist_line();
        let live = LIVE
            .with(|l| l.borrow().get(&t.video_id).cloned())
            .and_then(|raw| useful_title(&raw, &t.title));
        let sub = live.clone().unwrap_or_else(|| place.clone());
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut np = g.get_np();
        np.artist = sub.clone().into();
        np.art = art(&t.video_id, &t.title);
        np.duration_ms = 0;
        np.liked = radio::channel_of(&t.video_id).is_some_and(|c| store().is_favourite(c));
        g.set_np(np);
        if let Some(m) = &self.media {
            let album = if live.is_some() {
                place.as_str()
            } else {
                "Live radio"
            };
            m.track(&t.title, &sub, album, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_and_titles() {
        let r = route("radio:page:0eZoYyEW", "London").unwrap();
        assert_eq!((r.kind, r.id.as_str()), (PageKind::Radio, "page:0eZoYyEW"));
        assert_eq!(
            route("radio:countries", "Countries").unwrap().id,
            "countries"
        );
        assert_eq!(
            route("radio:search:jazz fm", "x").unwrap().id,
            "search:jazz fm"
        );
        assert!(
            route("radio:hcx4T119", "Kiss FM").is_none(),
            "a station isn't a page"
        );
        assert!(route("VLPL123", "x").is_none());
        assert_eq!(useful_title(" - ", "Kiss FM"), None);
        assert_eq!(useful_title("KISS FM", "Kiss FM"), None);
        assert_eq!(
            useful_title("Daft Punk - One More Time", "Kiss FM").as_deref(),
            Some("Daft Punk - One More Time")
        );
    }

    #[test]
    fn stations_round_trip_through_tracks() {
        let st = Station {
            id: "abcDEF12".into(),
            title: "Test FM".into(),
            place: "Ghent".into(),
            country: "Belgium".into(),
            place_id: Some("p1".into()),
        };
        let t = track(&st);
        assert_eq!(t.video_id, "radio:abcDEF12");
        assert_eq!(t.artist_line(), "Ghent, Belgium");
        assert_eq!(station_of(&t).unwrap(), st);
        // Unknown to this run (a restored queue): rebuilt from the track.
        let other = Track {
            video_id: "radio:zzz999".into(),
            title: "Other".into(),
            artists: vec![Named {
                name: "Lyon, France".into(),
                id: None,
            }],
            ..Default::default()
        };
        let s = station_of(&other).unwrap();
        assert_eq!((s.place.as_str(), s.country.as_str()), ("Lyon", "France"));
    }
}
