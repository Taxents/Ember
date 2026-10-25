//! What the app knows about YouTube Music content. Plain data, shared by the core and the UI.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thumb {
    pub url: String,
    pub width: u32,
    pub height: u32,
}

/// Picks the smallest thumbnail at least `px` wide (else the largest). Google image URLs also
/// take a size suffix, so `sized` can ask for exactly what's needed.
pub fn best_thumb(thumbs: &[Thumb], px: u32) -> Option<&Thumb> {
    thumbs
        .iter()
        .filter(|t| t.width >= px)
        .min_by_key(|t| t.width)
        .or_else(|| thumbs.iter().max_by_key(|t| t.width))
}

/// Rewrites a googleusercontent URL's `=wN-hN…` suffix to request `w`×`h`.
pub fn sized(url: &str, w: u32, h: u32) -> String {
    match url.rfind('=') {
        Some(i) if url.contains("googleusercontent.com") => {
            format!("{}=w{w}-h{h}-l90-rj", &url[..i])
        }
        _ => url.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Named {
    pub name: String,
    /// Browse id, when it links somewhere.
    pub id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TrackKind {
    /// An audio track ("Art Track").
    #[default]
    Song,
    /// A music video or user upload.
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Like {
    Like,
    Dislike,
    #[default]
    Indifferent,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    pub artists: Vec<Named>,
    pub album: Option<Named>,
    pub duration_ms: Option<u64>,
    pub thumbs: Vec<Thumb>,
    pub explicit: bool,
    pub kind: TrackKind,
    /// Position-specific id inside a playlist (needed to remove it).
    pub set_video_id: Option<String>,
    pub like: Option<Like>,
    /// Greyed out (region-locked, removed).
    pub unavailable: bool,
    /// How many times it was played ("149M plays", or a video's views), when known.
    pub plays: Option<u64>,
    /// The token "Not interested" sends, on Home recommendations.
    #[serde(default)]
    pub not_interested: Option<String>,
    /// Saving the song to the library, when it can be (signed in).
    #[serde(default)]
    pub library: Option<Library>,
    /// Someone's upload rather than a release: its kind is a guess from the title
    /// ([`super::uploads`]).
    #[serde(default)]
    pub upload: bool,
}

/// A song's place in the library, and the tokens that change it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Library {
    pub saved: bool,
    pub add: String,
    pub remove: String,
}

impl Track {
    pub fn artist_line(&self) -> String {
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AlbumKind {
    #[default]
    Album,
    Single,
    Ep,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AlbumCard {
    pub id: String,
    pub title: String,
    pub artists: Vec<Named>,
    pub year: Option<u32>,
    pub kind: AlbumKind,
    pub thumbs: Vec<Thumb>,
    pub explicit: bool,
    /// The token "Not interested" sends, on Home recommendations.
    #[serde(default)]
    pub not_interested: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ArtistCard {
    pub id: String,
    pub name: String,
    /// e.g. "2.3M monthly audience".
    pub subtitle: String,
    pub thumbs: Vec<Thumb>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PlaylistCard {
    /// Browse id (`VL…`).
    pub id: String,
    pub title: String,
    /// e.g. "YouTube Music • 40 songs".
    pub subtitle: String,
    pub thumbs: Vec<Thumb>,
    /// The token "Not interested" sends, on Home recommendations.
    #[serde(default)]
    pub not_interested: Option<String>,
}

/// A tile leading to another browse page (moods, genres).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Link {
    pub title: String,
    pub browse_id: String,
    pub params: Option<String>,
    /// ARGB.
    pub color: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Item {
    Track(Track),
    Album(AlbumCard),
    Artist(ArtistCard),
    Playlist(PlaylistCard),
    Link(Link),
}

impl Item {
    pub fn title(&self) -> &str {
        match self {
            Item::Track(t) => &t.title,
            Item::Album(a) => &a.title,
            Item::Artist(a) => &a.name,
            Item::Playlist(p) => &p.title,
            Item::Link(l) => &l.title,
        }
    }
}

/// Where a shelf's "See all" leads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowseRef {
    pub browse_id: String,
    pub params: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Shelf {
    pub title: String,
    pub items: Vec<Item>,
    pub more: Option<BrowseRef>,
}

/// A page of shelves (Home, Explore, library pages, mood pages), possibly with more to load.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Feed {
    pub title: Option<String>,
    pub shelves: Vec<Shelf>,
    pub continuation: Option<Continuation>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AlbumPage {
    pub id: String,
    pub title: String,
    pub kind: AlbumKind,
    pub year: Option<u32>,
    pub artists: Vec<Named>,
    pub thumbs: Vec<Thumb>,
    pub explicit: bool,
    pub description: Option<String>,
    /// e.g. "10 songs • 42 minutes".
    pub summary: String,
    pub tracks: Vec<Track>,
    /// The album as a playlist (`OLAK5uy_…`): what "play all" and "save" act on.
    pub playlist_id: Option<String>,
    /// In the library; `None` when it can't be saved (signed out).
    pub saved: Option<bool>,
    pub shelves: Vec<Shelf>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ArtistPage {
    pub id: String,
    pub name: String,
    /// "25.9M monthly audience".
    pub audience: Option<String>,
    pub description: Option<String>,
    /// Wide header images.
    pub thumbs: Vec<Thumb>,
    pub shuffle: Option<WatchRef>,
    pub radio: Option<WatchRef>,
    pub subscribed: Option<bool>,
    /// The channel id subscribe actions use (can differ from `id`).
    pub channel_id: Option<String>,
    pub shelves: Vec<Shelf>,
}

/// Something to start playing: a video in a playlist (artist shuffle, radio).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchRef {
    pub video_id: Option<String>,
    pub playlist_id: String,
    pub params: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PlaylistPage {
    /// Without the `VL` prefix.
    pub id: String,
    pub title: String,
    pub author: Option<Named>,
    pub subtitle: String,
    pub summary: String,
    pub description: Option<String>,
    pub thumbs: Vec<Thumb>,
    pub tracks: Vec<Track>,
    pub continuation: Option<Continuation>,
    /// The signed-in user can edit it.
    pub owned: bool,
    /// In the library; `None` when it can't be saved (yours, or signed out).
    pub saved: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SearchResults {
    pub top: Option<Item>,
    /// Items shown with the top result (e.g. an artist's top songs).
    pub top_extra: Vec<Item>,
    pub items: Vec<Item>,
    pub continuation: Option<Continuation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchFilter {
    Songs,
    Videos,
    Albums,
    Artists,
    Playlists,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WatchNext {
    pub tracks: Vec<Track>,
    pub playlist_id: Option<String>,
    pub continuation: Option<Continuation>,
    /// Browse id of the lyrics tab, for the YouTube lyrics source.
    pub lyrics_id: Option<String>,
    /// Browse id of the Related tab (similar songs, other performances, similar artists).
    pub related_id: Option<String>,
    /// The autoplay choices above the queue (All, Popular, Discover, Deep cuts…).
    pub chips: Vec<Chip>,
}

/// One autoplay choice: what the rest of the queue is drawn from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chip {
    pub label: String,
    pub selected: bool,
    pub watch: WatchRef,
}

/// How to fetch the next part of a long list. YouTube uses two schemes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Continuation {
    /// Old: `?ctoken=…&continuation=…` on the same endpoint and body.
    Query(String),
    /// New: `{"continuation": token}` in the body.
    Body(String),
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub handle: Option<String>,
    pub photo: Vec<Thumb>,
}

/// A song's credits ("Written by": Max Martin, …), as YouTube Music's "View song credits".
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Credits {
    pub sections: Vec<CreditSection>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CreditSection {
    pub role: String,
    pub names: Vec<String>,
}

/// What the player says about a song that lists leave out.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct VideoStats {
    pub plays: Option<u64>,
    pub duration_ms: Option<u64>,
    /// When it came out, "2019-11-28".
    pub published: Option<String>,
}
