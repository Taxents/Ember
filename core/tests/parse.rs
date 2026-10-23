//! Parsers against recorded InnerTube responses (signed out, trimmed by tools/trim_fixture.py).
//! Signed-in responses hold personal data, so they live in the gitignored `fixtures/private/`
//! and their tests skip when it's absent.

use music_core::yt::model::*;
use music_core::yt::parse;
use serde_json::Value;
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load(name: &str) -> Value {
    let s = std::fs::read_to_string(dir().join(name)).unwrap();
    serde_json::from_str(&s).unwrap()
}

fn private(name: &str) -> Option<Value> {
    let s = std::fs::read_to_string(dir().join("private").join(name)).ok()?;
    Some(serde_json::from_str(&s).unwrap())
}

fn tracks(items: &[Item]) -> impl Iterator<Item = &Track> {
    items.iter().filter_map(|i| match i {
        Item::Track(t) => Some(t),
        _ => None,
    })
}

fn check_item(i: &Item) {
    assert!(!i.title().is_empty(), "untitled item: {i:?}");
    match i {
        Item::Track(t) => assert_eq!(t.video_id.len(), 11, "{t:?}"),
        Item::Album(a) => assert!(a.id.starts_with("MPRE"), "{a:?}"),
        Item::Artist(a) => assert!(a.id.starts_with("UC") || a.id.starts_with("MPLA"), "{a:?}"),
        Item::Playlist(p) => assert!(!p.id.is_empty(), "{p:?}"),
        Item::Link(l) => assert!(!l.browse_id.is_empty(), "{l:?}"),
    }
}

fn check_feed(f: &Feed, min_shelves: usize) {
    assert!(
        f.shelves.len() >= min_shelves,
        "{} shelves",
        f.shelves.len()
    );
    for s in &f.shelves {
        assert!(!s.items.is_empty());
        s.items.iter().for_each(check_item);
    }
}

#[test]
fn home() {
    let f = parse::feed(&load("home.json"));
    check_feed(&f, 3);
    assert!(f.shelves.iter().all(|s| !s.title.is_empty()));
    assert!(matches!(f.continuation, Some(Continuation::Query(_))));
}

#[test]
fn explore_charts_releases() {
    check_feed(&parse::feed(&load("explore.json")), 3);
    let charts = parse::feed(&load("charts.json"));
    check_feed(&charts, 2);
    assert!(
        charts
            .shelves
            .iter()
            .flat_map(|s| &s.items)
            .any(|i| matches!(i, Item::Playlist(_)))
    );
    // Monthly audience only: subscriber counts never reach the app.
    for i in charts.shelves.iter().flat_map(|s| &s.items) {
        if let Item::Artist(a) = i {
            assert!(!a.subtitle.contains("subscriber"), "{a:?}");
        }
    }
    let nr = parse::feed(&load("new_releases.json"));
    check_feed(&nr, 1);
    assert!(
        nr.shelves
            .iter()
            .flat_map(|s| &s.items)
            .any(|i| matches!(i, Item::Album(_)))
    );
}

#[test]
fn moods() {
    let f = parse::feed(&load("moods.json"));
    check_feed(&f, 1);
    let links: Vec<&Link> = f
        .shelves
        .iter()
        .flat_map(|s| &s.items)
        .filter_map(|i| match i {
            Item::Link(l) => Some(l),
            _ => None,
        })
        .collect();
    assert!(links.len() > 10);
    assert!(links.iter().all(|l| l.params.is_some()));
}

#[test]
fn search_all() {
    let r = parse::search(&load("search.json"));
    let top = r.top.as_ref().expect("top result");
    check_item(top);
    assert!(r.items.len() > 5);
    r.items.iter().for_each(check_item);
    // Several kinds mixed.
    assert!(r.items.iter().any(|i| matches!(i, Item::Track(_))));
    assert!(r.items.iter().any(|i| !matches!(i, Item::Track(_))));
}

#[test]
fn search_songs() {
    let r = parse::search(&load("search_songs.json"));
    assert!(r.items.len() >= 10);
    for t in tracks(&r.items) {
        assert!(!t.artists.is_empty(), "{t:?}");
        assert!(t.duration_ms.is_some(), "{t:?}");
        assert_eq!(t.kind, TrackKind::Song);
    }
    assert_eq!(tracks(&r.items).count(), r.items.len());
    assert!(r.continuation.is_some());
}

#[test]
fn suggestions() {
    let (words, items) = parse::suggestions(&load("suggestions.json"));
    assert!(!words.is_empty());
    items.iter().for_each(check_item);
}

#[test]
fn album() {
    let a = parse::album("MPREb_nHbCAGX6uUL", &load("album.json"));
    assert_eq!(a.title, "The Dark Side of the Moon");
    assert_eq!(a.year, Some(1973));
    assert_eq!(a.kind, AlbumKind::Album);
    assert_eq!(a.artists[0].name, "Pink Floyd");
    assert!(
        a.artists[0]
            .id
            .as_deref()
            .is_some_and(|i| i.starts_with("UC"))
    );
    assert_eq!(a.tracks.len(), 10);
    assert!(
        a.playlist_id
            .as_deref()
            .is_some_and(|p| p.starts_with("OLAK5uy_"))
    );
    assert!(!a.thumbs.is_empty());
    for t in &a.tracks {
        assert_eq!(t.artists[0].name, "Pink Floyd");
        assert!(t.duration_ms.is_some(), "{t:?}");
        assert_eq!(
            t.album.as_ref().unwrap().id.as_deref(),
            Some("MPREb_nHbCAGX6uUL")
        );
    }
}

#[test]
fn artist() {
    let a = parse::artist("UCO6LS_5W7vqG9mALDNzSFug", &load("artist.json"));
    assert_eq!(a.name, "Pink Floyd");
    assert!(a.audience.as_deref().is_some_and(|s| s.contains("monthly")));
    assert!(a.description.is_some());
    assert!(a.radio.is_some() && a.shuffle.is_some());
    assert!(a.channel_id.is_some());
    let titles: Vec<&str> = a.shelves.iter().map(|s| s.title.as_str()).collect();
    assert!(titles.contains(&"Top songs"), "{titles:?}");
    assert!(titles.contains(&"Albums"), "{titles:?}");
    let top = &a.shelves.iter().find(|s| s.title == "Top songs").unwrap();
    assert!(top.more.is_some());
    assert!(tracks(&top.items).count() >= 5);
    for s in &a.shelves {
        s.items.iter().for_each(check_item);
    }
}

#[test]
fn playlist() {
    let p = parse::playlist(
        "VLPL4fGSI1pDJn4wJiy11zqxdQZfrFdiKnC5",
        &load("playlist.json"),
    );
    assert_eq!(p.id, "PL4fGSI1pDJn4wJiy11zqxdQZfrFdiKnC5");
    assert!(!p.title.is_empty());
    assert!(!p.thumbs.is_empty());
    assert!(!p.owned);
    assert_eq!(
        p.author.as_ref().map(|a| a.name.as_str()),
        Some("YouTube Charts")
    );
    assert!(p.tracks.len() >= 50, "{} tracks", p.tracks.len());
    for t in &p.tracks {
        assert_eq!(t.video_id.len(), 11);
        assert!(!t.artists.is_empty(), "{t:?}");
        assert!(t.duration_ms.is_some(), "{t:?}");
    }
}

#[test]
fn radio() {
    let w = parse::watch_next(&load("radio.json"));
    assert!(w.tracks.len() >= 20, "{} tracks", w.tracks.len());
    assert_eq!(w.tracks[0].video_id, "jcz0YxYl6Ac");
    assert!(
        w.playlist_id
            .as_deref()
            .is_some_and(|p| p.starts_with("RD"))
    );
    assert!(w.continuation.is_some());
    assert!(w.lyrics_id.is_some());
    assert!(
        w.related_id
            .as_deref()
            .is_some_and(|r| r.starts_with("MPTR"))
    );
    assert!(w.chips.len() >= 4, "{} chips", w.chips.len());
    assert_eq!(w.chips[0].label, "All");
    assert!(w.chips[0].selected && w.chips.iter().filter(|c| c.selected).count() == 1);
    assert!(
        w.chips
            .iter()
            .all(|c| c.watch.playlist_id.starts_with("RD"))
    );
    for t in &w.tracks {
        assert!(!t.artists.is_empty(), "{t:?}");
        assert!(t.duration_ms.is_some(), "{t:?}");
    }
}

#[test]
fn garbage_never_panics() {
    for v in [
        Value::Null,
        serde_json::json!({"contents": 5}),
        serde_json::json!([1, {"a": []}]),
    ] {
        parse::feed(&v);
        parse::search(&v);
        parse::album("x", &v);
        parse::artist("x", &v);
        parse::playlist("x", &v);
        parse::watch_next(&v);
        parse::suggestions(&v);
        assert!(parse::account(&v).is_none());
    }
}

// ---- signed in (private fixtures) ------------------------------------------------------------

#[test]
fn library_pages() {
    for name in [
        "library_playlists.json",
        "library_songs.json",
        "library_albums.json",
        "library_artists.json",
        "history.json",
    ] {
        let Some(v) = private(name) else { continue };
        let f = parse::feed(&v);
        check_feed(&f, 1);
    }
}

#[test]
fn liked_songs() {
    let Some(v) = private("liked.json") else {
        return;
    };
    let p = parse::playlist("VLLM", &v);
    // The list may be empty; whatever is in it is liked.
    assert_eq!(p.title, "Liked Music");
    assert!(p.tracks.iter().all(|t| t.like == Some(Like::Like)));
}

#[test]
fn home_not_interested() {
    let Some(v) = private("home_signed_in.json") else {
        return;
    };
    let f = parse::feed(&v);
    let tokens = f
        .shelves
        .iter()
        .flat_map(|s| &s.items)
        .filter(|i| match i {
            Item::Track(t) => t.not_interested.is_some(),
            Item::Album(a) => a.not_interested.is_some(),
            Item::Playlist(p) => p.not_interested.is_some(),
            _ => false,
        })
        .count();
    assert!(tokens > 0, "recommendations carry a Not interested token");
}

#[test]
fn related() {
    let Some(v) = private("related.json") else {
        return;
    };
    let f = parse::feed(&v);
    assert!(f.shelves.len() >= 4, "{} shelves", f.shelves.len());
    assert!(
        f.shelves
            .iter()
            .all(|s| !s.items.is_empty() && !s.title.is_empty())
    );
    assert!(parse::about(&v).is_some_and(|a| a.len() > 40));
}

#[test]
fn library_state() {
    let Some(v) = private("album_signed_in.json") else {
        return;
    };
    let a = parse::album("MPREb_FWIMEPTHFsY", &v);
    assert!(
        a.saved.is_some(),
        "signed-in albums say whether they're saved"
    );
    assert!(
        a.tracks.iter().all(|t| t.library.is_some()),
        "songs can be saved"
    );
    if let Some(v) = private("playlist_other.json") {
        let p = parse::playlist("VLRDCLAK5uy_ls6pkxsVNi5b3e1B3qZnRDamY6siJ-nj8", &v);
        assert!(!p.owned && p.saved.is_some());
    }
}

#[test]
fn account() {
    let Some(v) = private("account.json") else {
        return;
    };
    assert!(parse::account(&v).is_some());
}

#[test]
fn lyrics() {
    let Some(v) = private("lyrics.json") else {
        return;
    };
    let (words, source) = parse::lyrics(&v).unwrap();
    assert!(words.lines().count() > 10);
    assert_eq!(source, "Musixmatch");
}

#[test]
fn timed_lyrics() {
    let Some(v) = private("timed_lyrics.json") else {
        return;
    };
    let (lines, source) = parse::timed_lyrics(&v).unwrap();
    assert!(lines.len() > 10);
    assert!(lines.windows(2).all(|w| w[0].0 <= w[1].0 && w[0].0 <= w[0].1));
    assert_eq!(source, "Musixmatch");
}

/// A real Unison TTML file (word and syllable timed, with background vocals).
#[test]
fn unison_ttml() {
    let Ok(s) = std::fs::read_to_string(dir().join("private").join("unison.ttml")) else {
        return;
    };
    let l = music_core::lyrics::parse_ttml(&s);
    assert_eq!(l.timing, music_core::lyrics::Timing::Word);
    assert!(l.lines.len() > 20);
    assert!(l.lines.iter().any(|x| x.background));
    for x in &l.lines {
        assert!(!x.text().is_empty() && x.start <= x.end, "{x:?}");
        assert!(x.words.iter().all(|w| w.start <= w.end && !w.text.contains("  ")));
    }
}

#[test]
fn credits() {
    let Some(v) = private("credits.json") else {
        return;
    };
    let c = parse::credits(&v);
    let roles: Vec<_> = c.sections.iter().map(|s| s.role.as_str()).collect();
    assert!(roles.contains(&"Written by"), "{roles:?}");
    assert!(c.sections.iter().all(|s| s.names.iter().all(|n| !n.trim().is_empty())));
}
