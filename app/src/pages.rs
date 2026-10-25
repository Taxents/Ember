//! What each page shows. Pages are fetched on a worker into a banner or title, the page's own
//! tracks, blocks (headed track lists and card shelves) and a card grid, which the app then lays
//! out as rows.

use crate::{Card, PageKind, Route};
use music_core::Result;
use music_core::lyrics::{self, Lyrics};
use music_core::yt::Client;
use music_core::yt::model::*;

/// How the current page continues.
#[derive(Clone)]
pub enum More {
    /// More of the page's own tracks, from a feed (Songs).
    Tracks(Continuation),
    Playlist(Continuation),
    /// More cards for the grid.
    Grid(Continuation),
    /// More shelves (Home).
    Shelves(Continuation),
}

/// What the banner's Play and Shuffle play, when it isn't the page's own tracks.
#[derive(Clone)]
pub enum HeroPlay {
    Item(Item),
    Artist {
        radio: Option<WatchRef>,
        shuffle: Option<WatchRef>,
    },
}

/// The banner: albums, playlists, artists and Home's hero.
#[derive(Clone)]
pub struct Header {
    pub label: String,
    pub title: String,
    pub artist: String,
    /// The artists behind `artist`, which link to their pages.
    pub artists: Vec<Named>,
    pub thumbs: Vec<Thumb>,
    /// Rows leave out the artwork, the album column and the album's own artist.
    pub album: bool,
    /// Your own playlist: its tracks can be reordered.
    pub editable: bool,
    /// Artists: a wide picture, no cover.
    pub wide: bool,
    /// Artists: the YouTube channel, which finds their logo.
    pub channel: Option<String>,
    /// Albums and others' playlists: the playlist id that saves it, and whether it's saved.
    pub save: Option<(String, bool)>,
    /// Artists: the channel id that follows them, and whether you do.
    pub follow: Option<(String, bool)>,
    pub play: Option<HeroPlay>,
}

#[derive(Clone)]
pub enum Block {
    /// A heading, then tracks that play together.
    Tracks {
        title: String,
        more: Option<BrowseRef>,
        tracks: Vec<Track>,
    },
    Shelf(Shelf),
    /// A song's lyrics beside its release date, credits and the artist's bio.
    Details {
        lyrics: Option<Lyrics>,
        /// "Released": "28 November 2019", "Written by": "Max Martin, …".
        info: Vec<(String, String)>,
        about: Option<String>,
    },
}

/// A page's first load.
#[derive(Default, Clone)]
pub struct Loaded {
    pub header: Option<Header>,
    /// A title row, for pages without a banner.
    pub title: Option<String>,
    /// The page's own tracks, after the banner or title.
    pub tracks: Vec<Track>,
    /// The tracks end with "237 tracks, 14 hours 3 minutes".
    pub summary: bool,
    pub blocks: Vec<Block>,
    pub grid: Vec<Item>,
    pub more: Option<More>,
    /// Playlists: where the songs recommended under it come from (fetched after the page).
    pub suggest: Option<SuggestionsFrom>,
}

/// What a page shows, in short: a page fetched again is only laid out again when this differs.
pub fn signature(l: &Loaded) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    l.title.hash(&mut h);
    if let Some(x) = &l.header {
        (&x.title, &x.artist, &x.save, &x.follow, x.editable).hash(&mut h);
    }
    let track = |t: &Track, h: &mut std::collections::hash_map::DefaultHasher| {
        (&t.video_id, &t.title, &t.set_video_id, t.unavailable).hash(h);
    };
    for t in &l.tracks {
        track(t, &mut h);
    }
    for b in &l.blocks {
        match b {
            Block::Tracks { title, tracks, .. } => {
                title.hash(&mut h);
                for t in tracks {
                    track(t, &mut h);
                }
            }
            Block::Shelf(s) => {
                s.title.hash(&mut h);
                for i in &s.items {
                    item_id(i).hash(&mut h);
                }
            }
            Block::Details { info, about, .. } => (info, about).hash(&mut h),
        }
    }
    for i in &l.grid {
        item_id(i).hash(&mut h);
    }
    l.more.is_some().hash(&mut h);
    h.finish()
}

/// What a continuation brought.
pub enum Extra {
    Tracks(Vec<Track>),
    Grid(Vec<Item>),
    Blocks(Vec<Block>),
}

/// A feed route's id: "browse id|params".
pub fn feed_id(r: &BrowseRef) -> String {
    match &r.params {
        Some(p) => format!("{}|{p}", r.browse_id),
        None => r.browse_id.clone(),
    }
}

fn feed_ref(id: &str) -> BrowseRef {
    match id.split_once('|') {
        Some((b, p)) => BrowseRef {
            browse_id: b.into(),
            params: Some(p.into()),
        },
        None => BrowseRef {
            browse_id: id.into(),
            params: None,
        },
    }
}

/// Where a browse link leads in the app.
pub fn route_for(r: &BrowseRef, title: &str) -> Route {
    if let Some(radio) = crate::radio::route(&r.browse_id, title) {
        return radio;
    }
    let id = &r.browse_id;
    let (kind, id) = if id.starts_with("VL") {
        (PageKind::Playlist, id.clone())
    } else if id.starts_with("MPRE") {
        (PageKind::Album, id.clone())
    } else if id.starts_with("UC") {
        (PageKind::Artist, id.clone())
    } else {
        (PageKind::Feed, feed_id(r))
    };
    Route {
        kind,
        id,
        title: title.into(),
    }
}

/// Fetches a page (on a worker). `hidden` playlists are left out of All playlists.
pub fn load(client: &Client, r: &Route, hidden: &[String]) -> Result<Loaded> {
    Ok(match r.kind {
        PageKind::Home => home(client.home()?),
        PageKind::Explore => {
            let f = client.explore()?;
            Loaded {
                title: Some(r.title.clone()),
                blocks: blocks(f.shelves, false),
                more: f.continuation.map(More::Shelves),
                ..Default::default()
            }
        }
        PageKind::Feed => {
            let f = client.feed(&feed_ref(&r.id))?;
            let title = f.title.clone().filter(|t| !t.is_empty());
            // A single shelf (new releases, a discography) is the whole page: a grid.
            if f.shelves.len() == 1 {
                let s = f.shelves.into_iter().next().unwrap_or_default();
                let title = title
                    .or(Some(s.title).filter(|t| !t.is_empty()))
                    .unwrap_or_else(|| r.title.clone());
                Loaded {
                    title: Some(title),
                    grid: s.items,
                    more: f.continuation.map(More::Grid),
                    ..Default::default()
                }
            } else {
                Loaded {
                    title: Some(title.unwrap_or_else(|| r.title.clone())),
                    blocks: blocks(f.shelves, false),
                    more: f.continuation.map(More::Shelves),
                    ..Default::default()
                }
            }
        }
        PageKind::Song => song(client, &r.id)?,
        PageKind::Search => {
            // YouTube itself alongside, for the uploads YouTube Music leaves out; a failure
            // there only loses those.
            let (music, uploads) = std::thread::scope(|s| {
                let uploads = s.spawn(|| {
                    client.uploads(&r.id).unwrap_or_else(|e| {
                        log::warn!("uploads search: {e}");
                        Vec::new()
                    })
                });
                (
                    client.search(&r.id, None),
                    uploads.join().unwrap_or_default(),
                )
            });
            Loaded {
                title: Some(r.title.clone()),
                ..search(music?, uploads)
            }
        }
        PageKind::History => Loaded {
            title: Some(r.title.clone()),
            tracks: tracks_of(client.history()?.shelves.into_iter().flat_map(|s| s.items)),
            ..Default::default()
        },
        PageKind::Songs => {
            let f = client.library_songs()?;
            Loaded {
                title: Some(r.title.clone()),
                more: f.continuation.clone().map(More::Tracks),
                tracks: tracks_of(f.shelves.into_iter().flat_map(|s| s.items)),
                ..Default::default()
            }
        }
        PageKind::Albums | PageKind::Artists | PageKind::AllPlaylists => {
            let f = match r.kind {
                PageKind::Albums => client.library_albums()?,
                PageKind::Artists => client.library_artists()?,
                _ => client.library_playlists()?,
            };
            Loaded {
                title: Some(r.title.clone()),
                more: f.continuation.clone().map(More::Grid),
                grid: visible(f.shelves.into_iter().flat_map(|s| s.items), hidden),
                ..Default::default()
            }
        }
        PageKind::Album => {
            let a = client.album(&r.id)?;
            Loaded {
                header: Some(Header {
                    label: String::new(),
                    title: a.title,
                    artist: names(&a.artists),
                    artists: a.artists,
                    thumbs: a.thumbs,
                    album: true,
                    editable: false,
                    wide: false,
                    channel: None,
                    save: a.playlist_id.zip(a.saved),
                    follow: None,
                    play: None,
                }),
                tracks: a.tracks,
                summary: true,
                blocks: blocks(a.shelves, false),
                ..Default::default()
            }
        }
        PageKind::Artist => {
            let a = client.artist(&r.id)?;
            Loaded {
                header: Some(Header {
                    label: String::new(),
                    title: a.name,
                    artist: a.audience.unwrap_or_default(),
                    artists: Vec::new(),
                    thumbs: a.thumbs,
                    album: false,
                    editable: false,
                    wide: true,
                    channel: a.channel_id.clone().or(Some(a.id)),
                    save: None,
                    follow: a.channel_id.zip(a.subscribed),
                    play: Some(HeroPlay::Artist {
                        radio: a.radio,
                        shuffle: a.shuffle,
                    }),
                }),
                blocks: blocks(a.shelves, true),
                ..Default::default()
            }
        }
        _ => {
            let p = client.playlist(&r.id)?;
            // Without a cover of its own (Liked songs), the first track's stands in.
            let thumbs = if p.thumbs.is_empty() {
                p.tracks
                    .first()
                    .map(|t| t.thumbs.clone())
                    .unwrap_or_default()
            } else {
                p.thumbs
            };
            let editable = p.owned && r.kind == PageKind::Playlist;
            let suggest = (r.kind == PageKind::Playlist).then(|| match p.suggestions {
                Some(c) if editable => SuggestionsFrom::Shelf(c),
                _ => SuggestionsFrom::Radio {
                    playlist_id: p.id.clone(),
                    continuation: None,
                },
            });
            Loaded {
                header: Some(Header {
                    label: "Playlist".into(),
                    title: if r.kind == PageKind::Liked {
                        r.title.clone()
                    } else {
                        p.title
                    },
                    artist: p.author.map(|a| a.name).unwrap_or_default(),
                    artists: Vec::new(),
                    thumbs,
                    album: false,
                    editable,
                    wide: false,
                    channel: None,
                    save: p.saved.map(|s| (p.id.clone(), s)),
                    follow: None,
                    play: None,
                }),
                tracks: p.tracks,
                more: p.continuation.map(More::Playlist),
                summary: true,
                suggest,
                ..Default::default()
            }
        }
    })
}

pub fn load_more(client: &Client, m: More, hidden: &[String]) -> Result<(Extra, Option<More>)> {
    Ok(match m {
        More::Tracks(c) => {
            let f = client.feed_more(&c)?;
            (
                Extra::Tracks(tracks_of(f.shelves.into_iter().flat_map(|s| s.items))),
                f.continuation.map(More::Tracks),
            )
        }
        More::Playlist(c) => {
            let (t, c) = client.playlist_more(&c)?;
            (Extra::Tracks(t), c.map(More::Playlist))
        }
        More::Grid(c) => {
            let f = client.feed_more(&c)?;
            (
                Extra::Grid(visible(f.shelves.into_iter().flat_map(|s| s.items), hidden)),
                f.continuation.map(More::Grid),
            )
        }
        More::Shelves(c) => {
            let f = client.feed_more(&c)?;
            (
                Extra::Blocks(blocks(f.shelves, false)),
                f.continuation.map(More::Shelves),
            )
        }
    })
}

/// Home: YouTube's first pick as the hero, then every shelf as cards.
fn home(f: Feed) -> Loaded {
    let mut l = Loaded {
        more: f.continuation.clone().map(More::Shelves),
        ..Default::default()
    };
    // A mix or album from a recommendation shelf; else the first song (its radio); else anything.
    let pick = |want: fn(&Item) -> bool| {
        f.shelves.iter().find_map(|s| {
            let ours = s.title.to_lowercase().contains("library");
            s.items
                .iter()
                .find(|i| !ours && want(i))
                .map(|i| (s.title.clone(), i.clone()))
        })
    };
    let hero = pick(|i| matches!(i, Item::Album(_) | Item::Playlist(_)))
        .or_else(|| pick(|i| matches!(i, Item::Track(_))))
        .or_else(|| {
            f.shelves
                .iter()
                .find_map(|s| s.items.first().map(|i| (s.title.clone(), i.clone())))
        });
    // The shelf the hero comes from doesn't show it again.
    let mut shelves = f.shelves;
    if let Some((title, item)) = &hero {
        let id = item_id(item);
        if let Some(s) = shelves.iter_mut().find(|s| s.title == *title) {
            s.items.retain(|i| item_id(i) != id);
        }
    }
    match hero {
        Some((shelf, item)) => {
            let (title, artist, thumbs) = match &item {
                Item::Track(t) => (t.title.clone(), t.artist_line(), t.thumbs.clone()),
                Item::Album(a) => (a.title.clone(), names(&a.artists), a.thumbs.clone()),
                Item::Playlist(p) => (p.title.clone(), p.subtitle.clone(), p.thumbs.clone()),
                Item::Artist(a) => (a.name.clone(), a.subtitle.clone(), a.thumbs.clone()),
                Item::Link(l) => (l.title.clone(), String::new(), Vec::new()),
            };
            let artists = artists_of(&item);
            l.header = Some(Header {
                label: if shelf.is_empty() {
                    "For you".into()
                } else {
                    shelf
                },
                title,
                artist,
                artists,
                thumbs,
                album: false,
                editable: false,
                wide: false,
                channel: None,
                save: None,
                follow: None,
                play: Some(HeroPlay::Item(item)),
            });
        }
        None => l.title = Some("Home".into()),
    }
    l.blocks = blocks(shelves, false);
    l
}

/// Search: the top results as cards, the songs as a list, then the uploads that score as songs
/// (YouTube's own, and YouTube Music's), then artists, albums, playlists and videos.
fn search(s: SearchResults, uploads: Vec<Track>) -> Loaded {
    let mut top: Vec<Item> = s.top.into_iter().collect();
    let (mut songs, mut uploaded) = (Vec::new(), Vec::new());
    let (mut artists, mut albums, mut playlists, mut videos) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for i in s.top_extra.into_iter().chain(s.items) {
        match i {
            Item::Track(t) if t.kind == TrackKind::Song && t.upload => uploaded.push(t),
            Item::Track(t) if t.kind == TrackKind::Song => songs.push(t),
            Item::Track(_) => videos.push(i),
            Item::Artist(_) => artists.push(i),
            Item::Album(_) => albums.push(i),
            Item::Playlist(_) => playlists.push(i),
            Item::Link(_) => {}
        }
    }
    // YouTube's uploads after YouTube Music's own, each video once.
    let mut seen: std::collections::HashSet<String> = top
        .iter()
        .chain(&videos)
        .filter_map(|i| match i {
            Item::Track(t) => Some(t.video_id.clone()),
            _ => None,
        })
        .chain(songs.iter().chain(&uploaded).map(|t| t.video_id.clone()))
        .collect();
    // The artists YouTube Music found: which side of an upload's dash names the artist.
    let mut known: Vec<String> = Vec::new();
    for i in top.iter().chain(&artists).chain(&albums).chain(&videos) {
        match i {
            Item::Artist(a) => known.push(a.name.clone()),
            Item::Album(a) => known.extend(a.artists.iter().map(|n| n.name.clone())),
            Item::Track(t) if !t.upload => known.extend(t.artists.iter().map(|n| n.name.clone())),
            _ => {}
        }
    }
    known.extend(
        songs
            .iter()
            .flat_map(|t| t.artists.iter().map(|n| n.name.clone())),
    );
    known.sort();
    known.dedup();
    for mut t in uploads {
        if !seen.insert(t.video_id.clone()) {
            continue;
        }
        match t.kind {
            TrackKind::Song => {
                music_core::yt::uploads::tidy(&mut t, &known);
                uploaded.push(t);
            }
            TrackKind::Video => videos.push(Item::Track(t)),
        }
    }
    // The best of each kind leads, after the top result.
    for v in [&albums, &playlists] {
        if let Some(i) = v.first()
            && !top.contains(i)
        {
            top.push(i.clone());
        }
    }
    let shelf = |title: &str, items: Vec<Item>| {
        Block::Shelf(Shelf {
            title: title.into(),
            items,
            more: None,
        })
    };
    let mut b = Vec::new();
    if !top.is_empty() {
        b.push(shelf("Top results", top));
    }
    if !songs.is_empty() {
        songs.truncate(12);
        b.push(Block::Tracks {
            title: "Songs".into(),
            more: None,
            tracks: songs,
        });
    }
    if !uploaded.is_empty() {
        uploaded.truncate(12);
        b.push(Block::Tracks {
            title: "Songs from YouTube".into(),
            more: None,
            tracks: uploaded,
        });
    }
    for (title, items) in [
        ("Artists", artists),
        ("Albums", albums),
        ("Playlists", playlists),
        ("Videos", videos),
    ] {
        if !items.is_empty() {
            b.push(shelf(title, items));
        }
    }
    Loaded {
        blocks: b,
        ..Default::default()
    }
}

/// Shelves as blocks. With `lists`, a shelf of songs (an artist's top songs) is a track list.
fn blocks(shelves: Vec<Shelf>, lists: bool) -> Vec<Block> {
    shelves
        .into_iter()
        .filter(|s| !s.items.is_empty())
        .map(|s| {
            let songs = s
                .items
                .iter()
                .all(|i| matches!(i, Item::Track(t) if t.kind == TrackKind::Song));
            if lists && songs {
                Block::Tracks {
                    title: s.title,
                    more: s.more,
                    tracks: tracks_of(s.items),
                }
            } else {
                Block::Shelf(s)
            }
        })
        .collect()
}

fn visible(items: impl Iterator<Item = Item>, hidden: &[String]) -> Vec<Item> {
    items
        .filter(|i| !matches!(i, Item::Playlist(p) if hidden.contains(&p.id)))
        .collect()
}

pub fn tracks_of(items: impl IntoIterator<Item = Item>) -> Vec<Track> {
    items
        .into_iter()
        .filter_map(|i| match i {
            Item::Track(t) => Some(t),
            _ => None,
        })
        .collect()
}

/// Its address on music.youtube.com, for sharing.
pub fn link(item: &Item) -> String {
    const SITE: &str = "https://music.youtube.com";
    match item {
        Item::Track(t) => format!("{SITE}/watch?v={}", t.video_id),
        Item::Album(a) => format!("{SITE}/browse/{}", a.id),
        Item::Artist(a) => format!("{SITE}/channel/{}", a.id),
        Item::Playlist(p) => format!("{SITE}/playlist?list={}", p.id.trim_start_matches("VL")),
        Item::Link(l) => format!("{SITE}/browse/{}", l.browse_id),
    }
}

/// A song's page: the song on the banner, its other versions (live, remixes, acoustic…) and
/// YouTube's Related tab (similar songs, other performances, similar artists, more from them).
fn song(client: &Client, id: &str) -> Result<Loaded> {
    let w = client.watch_next(Some(id), None, None)?;
    let Some(t) = w.tracks.iter().find(|t| t.video_id == id).cloned() else {
        return Err(music_core::Error::Unavailable("song not found".into()));
    };
    let artist = t
        .artists
        .first()
        .map(|a| a.name.clone())
        .unwrap_or_default();
    let query = format!("{} {artist}", base_title(&t.title));
    // Everything else at once: each is its own request.
    let (related, versions, videos, credits, stats, lyrics) = std::thread::scope(|s| {
        let related = s.spawn(|| match &w.related_id {
            Some(r) => client.related(r).unwrap_or_default(),
            None => Default::default(),
        });
        let versions = s.spawn(|| {
            client
                .search(&query, Some(SearchFilter::Songs))
                .map(|r| versions_of(&t, r.top.into_iter().chain(r.items)))
                .unwrap_or_default()
        });
        let videos = s.spawn(|| {
            client
                .search(&query, Some(SearchFilter::Videos))
                .map(|r| videos_of(&t, &artist, r.top.into_iter().chain(r.items)))
                .unwrap_or_default()
        });
        let credits = s.spawn(|| client.credits(id).unwrap_or_default());
        let stats = s.spawn(|| client.video_stats(id).unwrap_or_default());
        let lyrics = s.spawn(|| lyrics_for(client, &t, w.lyrics_id.as_deref()));
        (
            related.join().unwrap_or_default(),
            versions.join().unwrap_or_default(),
            videos.join().unwrap_or_default(),
            credits.join().unwrap_or_default(),
            stats.join().unwrap_or_default(),
            lyrics.join().ok().flatten(),
        )
    });
    let (related, about) = related;
    let mut label = match t.kind {
        TrackKind::Video => "Music video",
        TrackKind::Song => "Song",
    }
    .to_string();
    if let Some(a) = t.album.as_ref().filter(|a| !a.name.is_empty()) {
        label += &format!(" · {}", a.name);
    }
    if let Some(y) = stats.published.as_deref().and_then(|d| d.get(..4)) {
        label += &format!(" · {y}");
    }
    let mut info: Vec<(String, String)> = stats
        .published
        .as_deref()
        .and_then(long_date)
        .map(|d| ("Released".to_string(), d))
        .into_iter()
        .collect();
    info.extend(
        credits
            .sections
            .into_iter()
            .map(|s| (s.role, s.names.join(", "))),
    );
    let about = about.map(|a| first_sentences(&a, 420));
    let mut blocks = Vec::new();
    if lyrics.is_some() || !info.is_empty() || about.is_some() {
        blocks.push(Block::Details {
            lyrics,
            info,
            about,
        });
    }
    if !versions.is_empty() {
        blocks.push(Block::Tracks {
            title: "Other versions".into(),
            more: None,
            tracks: versions,
        });
    }
    if !videos.is_empty() {
        blocks.push(Block::Shelf(Shelf {
            title: "Music videos".into(),
            items: videos.into_iter().map(Item::Track).collect(),
            more: None,
        }));
    }
    blocks.extend(self::blocks(related.shelves, false));
    Ok(Loaded {
        header: Some(Header {
            label,
            title: t.title.clone(),
            artist: names(&t.artists),
            artists: t.artists.clone(),
            thumbs: t.thumbs.clone(),
            album: false,
            editable: false,
            wide: false,
            channel: None,
            save: None,
            follow: None,
            play: Some(HeroPlay::Item(Item::Track(t))),
        }),
        blocks,
        ..Default::default()
    })
}

/// Whether lyrics may come from Unison and LRCLIB too (Settings > Lyrics).
pub static THIRD_PARTY_LYRICS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// A song's lyrics (`lyrics::find`: Unison, YouTube's timed ones, LRCLIB, then plain; cached).
/// `lyrics_id` is the watch page's lyrics tab when the caller has it.
pub fn lyrics_for(client: &Client, t: &Track, lyrics_id: Option<&str>) -> Option<Lyrics> {
    let artist = t
        .artists
        .first()
        .map(|a| a.name.as_str())
        .unwrap_or_default();
    let q = lyrics::Query {
        video_id: &t.video_id,
        title: &t.title,
        artist,
        album: t.album.as_ref().map(|a| a.name.as_str()),
        duration_ms: t.duration_ms,
        lyrics_id,
        third_party: THIRD_PARTY_LYRICS.load(std::sync::atomic::Ordering::Relaxed),
    };
    let l = lyrics::find(client, &q);
    log::info!(
        "lyrics {}: {}",
        t.video_id,
        l.as_ref()
            .map_or("none".into(), |l| format!("{} {:?}", l.source, l.timing))
    );
    l
}

/// "2019-11-28" -> "28 November 2019".
fn long_date(d: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let mut p = d.splitn(3, '-');
    let (y, m, day) = (p.next()?, p.next()?, p.next()?);
    let m: usize = m.parse().ok()?;
    let day: u32 = day.parse().ok()?;
    Some(format!("{day} {} {y}", MONTHS.get(m.checked_sub(1)?)?))
}

/// The opening of a long text, cut at a sentence end near `max` characters.
fn first_sentences(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    match cut.rfind(". ") {
        Some(i) if i > max / 3 => cut[..=i].to_string(),
        _ => format!("{}…", cut.trim_end()),
    }
}

/// Search results that are videos of this song by the same artist (its music video, live
/// performances).
fn videos_of(t: &Track, artist: &str, found: impl Iterator<Item = Item>) -> Vec<Track> {
    let base = base_title(&t.title);
    let artist = artist.to_lowercase();
    found
        .filter_map(|i| match i {
            Item::Track(x) => Some(x),
            _ => None,
        })
        .filter(|x| x.video_id != t.video_id && !x.unavailable)
        .filter(|x| {
            let title = x.title.to_lowercase();
            title.contains(&base)
                && (title.contains(&artist)
                    || x.artists.iter().any(|a| a.name.to_lowercase() == artist))
        })
        .take(8)
        .collect()
}

/// A title without its "(Live…)", "[Remix]" or " - Acoustic" part, lowercased.
fn base_title(title: &str) -> String {
    let end = [" (", " [", " - "]
        .iter()
        .filter_map(|p| title.find(p))
        .min()
        .unwrap_or(title.len());
    title[..end].trim().to_lowercase()
}

/// Search results that are this song in another form: the same base title, but not the same
/// title again (the same recording on another release).
fn versions_of(t: &Track, found: impl Iterator<Item = Item>) -> Vec<Track> {
    let base = base_title(&t.title);
    let same = t.title.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    found
        .filter_map(|i| match i {
            Item::Track(x) => Some(x),
            _ => None,
        })
        .filter(|x| x.video_id != t.video_id && !x.unavailable)
        .filter(|x| {
            let title = x.title.to_lowercase();
            base_title(&x.title) == base && title != same && seen.insert(title)
        })
        .take(12)
        .collect()
}

/// What tells two items apart.
pub fn item_id(item: &Item) -> &str {
    match item {
        Item::Track(t) => &t.video_id,
        Item::Album(a) => &a.id,
        Item::Artist(a) => &a.id,
        Item::Playlist(p) => &p.id,
        Item::Link(l) => &l.browse_id,
    }
}

/// The token "Not interested" sends, on Home recommendations.
pub fn not_interested(item: &Item) -> Option<&str> {
    match item {
        Item::Track(t) => t.not_interested.as_deref(),
        Item::Album(a) => a.not_interested.as_deref(),
        Item::Playlist(p) => p.not_interested.as_deref(),
        _ => None,
    }
}

/// The artists of a song or album.
pub fn artists_of(item: &Item) -> Vec<Named> {
    match item {
        Item::Track(t) => t.artists.clone(),
        Item::Album(a) => a.artists.clone(),
        _ => Vec::new(),
    }
}

pub fn names(n: &[Named]) -> String {
    n.iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A card for an item, without its artwork (that's asked for when it's drawn).
pub fn card_for(item: &Item) -> Card {
    let (title, subtitle) = match item {
        Item::Track(t) => (t.title.clone(), t.artist_line()),
        Item::Album(a) => {
            let kind = match a.kind {
                AlbumKind::Album => "Album",
                AlbumKind::Single => "Single",
                AlbumKind::Ep => "EP",
            };
            // On an artist's page the artist goes without saying.
            let sub = match (names(&a.artists), a.year) {
                (n, _) if !n.is_empty() => n,
                (_, Some(y)) if a.kind == AlbumKind::Album => y.to_string(),
                (_, Some(y)) => format!("{kind} · {y}"),
                _ => kind.to_string(),
            };
            (a.title.clone(), sub)
        }
        Item::Artist(a) => (a.name.clone(), a.subtitle.clone()),
        Item::Playlist(p) => (p.title.clone(), p.subtitle.clone()),
        Item::Link(l) => (l.title.clone(), String::new()),
    };
    let color = match item {
        Item::Link(l) => match l.color {
            Some(c) => slint::Color::from_argb_encoded(c),
            None => slint::Color::from_rgb_u8(0x76, 0x29, 0x1E),
        },
        _ => slint::Color::default(),
    };
    Card {
        title: title.into(),
        subtitle: subtitle.into(),
        art: Default::default(),
        round: matches!(item, Item::Artist(_)),
        tile: matches!(item, Item::Link(_)),
        color,
        playable: !matches!(item, Item::Link(_)),
        video: matches!(item, Item::Track(t) if t.kind == TrackKind::Video),
        at: 0.0,
    }
}

/// The picture to load for a card shown `px` wide. A music video shows its 16:9 YouTube
/// thumbnail: the one it came with when that's wide, else YouTube's own.
pub fn card_url(item: &Item, px: u32) -> Option<String> {
    if let Item::Track(t) = item
        && t.kind == TrackKind::Video
    {
        let wide: Vec<Thumb> = t
            .thumbs
            .iter()
            .filter(|th| {
                th.url.contains("ytimg.com") && th.height > 0 && th.width * 9 >= th.height * 15
            })
            .cloned()
            .collect();
        return crate::images::url_for(&wide, px).or_else(|| {
            Some(format!(
                "https://i.ytimg.com/vi/{}/mqdefault.jpg",
                t.video_id
            ))
        });
    }
    crate::images::url_for(thumbs(item), px)
}

/// The artwork a card shows.
pub fn thumbs(item: &Item) -> &[Thumb] {
    match item {
        Item::Track(t) => &t.thumbs,
        Item::Album(a) => &a.thumbs,
        Item::Artist(a) => &a.thumbs,
        Item::Playlist(p) => &p.thumbs,
        Item::Link(_) => &[],
    }
}
