//! Your own names for songs: a title, and artists for one song or for everything a channel
//! uploads (someone who posts one artist's leaks). Kept in `edits.json` beside the settings,
//! never sent anywhere: YouTube still has its own names.
//!
//! Edits are put on tracks as they come in (pages, the queue), so everything that shows a
//! song, looks up its lyrics or reports it sees your names. The names a track came with are
//! remembered for this run, so undoing an edit puts them back without fetching anything.

use crate::pages::{Block, Extra, Loaded};
use music_core::yt::model::{Item, Named, Track};
use serde::{Deserialize, Serialize};
use slint::ComponentHandle;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

const FILE: &str = "edits.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Edits {
    /// Video id → what you changed on that song.
    #[serde(default)]
    pub songs: HashMap<String, SongEdit>,
    /// Channel id → the artists of everything it uploads.
    #[serde(default)]
    pub channels: HashMap<String, Vec<Named>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SongEdit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artists: Option<Vec<Named>>,
}

impl Edits {
    /// The track with your names on it: the channel's artists, then the song's own edit.
    pub fn apply(&self, t: &mut Track) {
        if let Some(a) = channel_id(t).and_then(|id| self.channels.get(id)) {
            t.artists = a.clone();
        }
        if let Some(e) = self.songs.get(&t.video_id) {
            if let Some(title) = &e.title {
                t.title = title.clone();
            }
            if let Some(a) = &e.artists {
                t.artists = a.clone();
            }
        }
    }

    fn touches(&self, t: &Track) -> bool {
        self.songs.contains_key(&t.video_id)
            || channel_id(t).is_some_and(|id| self.channels.contains_key(id))
    }
}

fn channel_id(t: &Track) -> Option<&str> {
    t.channel.as_ref()?.id.as_deref()
}

struct State {
    edits: Edits,
    /// The title and artists a track came with, before any edit: for undoing one.
    original: HashMap<String, (String, Vec<Named>)>,
}

fn state() -> MutexGuard<'static, State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            edits: crate::settings::load(FILE),
            original: HashMap::new(),
        })
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

/// Puts your names on a track that just came in.
pub fn apply(t: &mut Track) {
    let mut s = state();
    if !s.edits.touches(t) {
        return;
    }
    s.original
        .entry(t.video_id.clone())
        .or_insert_with(|| (t.title.clone(), t.artists.clone()));
    s.edits.apply(t);
}

pub fn applied(mut t: Track) -> Track {
    apply(&mut t);
    t
}

/// A track already shown, after an edit: its own names back, then whatever edits are left.
pub fn reapply(t: &mut Track) {
    let mut s = state();
    if let Some((title, artists)) = s.original.get(&t.video_id) {
        (t.title, t.artists) = (title.clone(), artists.clone());
    }
    if s.edits.touches(t) {
        s.original
            .entry(t.video_id.clone())
            .or_insert_with(|| (t.title.clone(), t.artists.clone()));
        s.edits.apply(t);
    }
}

/// The names a track came with (as YouTube has them).
pub fn original(t: &Track) -> (String, Vec<Named>) {
    state()
        .original
        .get(&t.video_id)
        .cloned()
        .unwrap_or_else(|| (t.title.clone(), t.artists.clone()))
}

/// Whether everything from this track's channel goes under the artists you gave it.
pub fn whole_channel(t: &Track) -> bool {
    channel_id(t).is_some_and(|id| state().edits.channels.contains_key(id))
}

/// Whether the track has any edit of yours on it.
pub fn edited(t: &Track) -> bool {
    state().edits.touches(t)
}

/// Saves your names for a song. A title or artists equal to the song's own drop that part of
/// the edit. With `channel`, the artists go on everything its channel uploads instead.
pub fn set(t: &Track, title: &str, artists: Vec<Named>, channel: bool) {
    let (own_title, own_artists) = original(t);
    let mut s = state();
    let title = title.trim();
    let same_artists = names(&artists) == names(&own_artists);
    let mut e = SongEdit {
        title: (!title.is_empty() && title != own_title).then(|| title.to_string()),
        artists: None,
    };
    match channel_id(t).filter(|_| channel) {
        Some(id) if !artists.is_empty() => {
            s.edits.channels.insert(id.to_string(), artists);
        }
        _ => {
            if let Some(id) = channel_id(t) {
                s.edits.channels.remove(id);
            }
            if !artists.is_empty() && !same_artists {
                e.artists = Some(artists);
            }
        }
    }
    if e == SongEdit::default() {
        s.edits.songs.remove(&t.video_id);
    } else {
        s.edits.songs.insert(t.video_id.clone(), e);
    }
    crate::settings::save(FILE, &s.edits);
}

/// Forgets your names for a song, and for its channel.
pub fn reset(t: &Track) {
    let mut s = state();
    s.edits.songs.remove(&t.video_id);
    if let Some(id) = channel_id(t) {
        s.edits.channels.remove(id);
    }
    crate::settings::save(FILE, &s.edits);
}

fn names(a: &[Named]) -> Vec<&str> {
    a.iter().map(|n| n.name.as_str()).collect()
}

/// "A, B & C" → three artists (unlinked: [`crate::App`] links them when it can).
pub fn parse_artists(s: &str) -> Vec<Named> {
    s.split([',', '&'])
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| Named {
            name: n.to_string(),
            id: None,
        })
        .collect()
}

/// Your names on everything a page shows.
pub fn apply_loaded(l: &mut Loaded) {
    l.tracks.iter_mut().for_each(apply);
    l.grid.iter_mut().for_each(apply_item);
    l.blocks.iter_mut().for_each(apply_block);
}

pub fn apply_extra(e: &mut Extra) {
    match e {
        Extra::Tracks(t) => t.iter_mut().for_each(apply),
        Extra::Grid(items) => items.iter_mut().for_each(apply_item),
        Extra::Blocks(b) => b.iter_mut().for_each(apply_block),
    }
}

fn apply_block(b: &mut Block) {
    match b {
        Block::Tracks { tracks, .. } => tracks.iter_mut().for_each(apply),
        Block::Shelf(s) => s.items.iter_mut().for_each(apply_item),
        _ => {}
    }
}

fn apply_item(i: &mut Item) {
    if let Item::Track(t) = i {
        apply(t);
    }
}

impl crate::App {
    /// Opens "Edit song info" for a song.
    pub(crate) fn edit_song(&mut self, t: Track) {
        let ui = self.ui();
        let g = ui.global::<crate::Ui>();
        g.set_edit_title(t.title.clone().into());
        g.set_edit_artist(join(&t.artists).into());
        let channel = t.channel.as_ref().filter(|c| t.upload && c.id.is_some());
        g.set_edit_channel(channel.map(|c| c.name.clone()).unwrap_or_default().into());
        g.set_edit_whole_channel(whole_channel(&t));
        g.set_edit_edited(edited(&t));
        g.set_editing(true);
        self.editing = Some(t);
    }

    /// Saves what was typed. Artists get their pages when YouTube Music has one by exactly
    /// that name (looked up first, so the edit lands once).
    pub(crate) fn edit_saved(&mut self, title: String, artists: String, channel: bool) {
        let Some(t) = self.editing.take() else {
            return;
        };
        let mut wanted = parse_artists(&artists);
        let (_, own) = original(&t);
        for a in &mut wanted {
            a.id = own
                .iter()
                .chain(&t.artists)
                .find(|o| o.name.eq_ignore_ascii_case(&a.name))
                .and_then(|o| o.id.clone());
        }
        let client = self.client.clone();
        crate::workers::run(
            move || {
                for a in wanted.iter_mut().filter(|a| a.id.is_none()) {
                    if let Some(found) = artist(&client, &a.name) {
                        *a = found;
                    }
                }
                wanted
            },
            move |artists| {
                let _ = crate::with(|a| {
                    set(&t, &title, artists, channel);
                    a.edits_changed();
                });
            },
        );
    }

    pub(crate) fn edit_reset(&mut self) {
        if let Some(t) = self.editing.take() {
            reset(&t);
            self.edits_changed();
        }
    }

    /// Everything shown again with the edits as they are now.
    fn edits_changed(&mut self) {
        for t in self.known.values_mut() {
            reapply(t);
        }
        self.page_cache.clear();
        self.reload();
        self.queue_changed();
        self.show_now_playing(false);
    }
}

fn join(a: &[Named]) -> String {
    names(a).join(", ")
}

/// The artist YouTube Music has by exactly this name (any case), as it spells it.
fn artist(client: &music_core::yt::Client, name: &str) -> Option<Named> {
    use music_core::yt::model::SearchFilter;
    let r = client.search(name, Some(SearchFilter::Artists)).ok()?;
    r.top.into_iter().chain(r.items).find_map(|i| match i {
        Item::Artist(a) if a.name.eq_ignore_ascii_case(name.trim()) => Some(Named {
            name: a.name,
            id: Some(a.id),
        }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(n: &str, id: Option<&str>) -> Named {
        Named {
            name: n.into(),
            id: id.map(str::to_string),
        }
    }

    fn upload() -> Track {
        Track {
            video_id: "abcdefghijk".into(),
            title: "Never See Me Again (Clear Vocals)".into(),
            artists: vec![named("uploader", Some("UCx"))],
            channel: Some(named("uploader", Some("UCx"))),
            ..Default::default()
        }
    }

    #[test]
    fn a_channel_names_its_uploads_and_a_song_edit_wins() {
        let mut e = Edits::default();
        e.channels
            .insert("UCx".into(), vec![named("Kanye West", None)]);
        let mut t = upload();
        e.apply(&mut t);
        assert_eq!(t.artists[0].name, "Kanye West");
        e.songs.insert(
            t.video_id.clone(),
            SongEdit {
                title: Some("Never See Me Again".into()),
                artists: Some(vec![named("Ye", None)]),
            },
        );
        let mut t = upload();
        e.apply(&mut t);
        assert_eq!(
            (t.title.as_str(), t.artists[0].name.as_str()),
            ("Never See Me Again", "Ye")
        );
    }

    #[test]
    fn other_songs_are_left_alone() {
        let e = Edits {
            channels: [("UCother".to_string(), vec![named("X", None)])].into(),
            ..Default::default()
        };
        let mut t = upload();
        e.apply(&mut t);
        assert_eq!(t, upload());
        assert!(!e.touches(&t));
    }

    #[test]
    fn artists_split_on_commas_and_ampersands() {
        let a = parse_artists(" Kanye West, JAY-Z & Frank Ocean ,");
        assert_eq!(names(&a), ["Kanye West", "JAY-Z", "Frank Ocean"]);
    }

    #[test]
    fn edits_read_back() {
        let mut e = Edits::default();
        e.songs.insert(
            "id".into(),
            SongEdit {
                title: Some("T".into()),
                artists: None,
            },
        );
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("artists"));
        assert_eq!(serde_json::from_str::<Edits>(&json).unwrap(), e);
    }
}
