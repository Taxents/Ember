//! Turns InnerTube JSON into the model. Tolerant by design: missing fields become `None` or
//! empty, unknown renderers are skipped (which is also what drops ads, promos and upsells),
//! and nothing here can panic on unexpected input: `serde_json::Value` indexing yields `Null`.

use super::model::*;
use serde_json::Value;

// ---- primitives ------------------------------------------------------------------------------

/// Concatenated text of a `{runs: [...]}` or `{simpleText}` object.
pub fn text(v: &Value) -> String {
    if let Some(s) = v["simpleText"].as_str() {
        return s.to_string();
    }
    runs(v).iter().filter_map(|r| r["text"].as_str()).collect()
}

fn runs(v: &Value) -> &[Value] {
    v["runs"].as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn opt_text(v: &Value) -> Option<String> {
    Some(text(v)).filter(|s| !s.is_empty())
}

fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn str_of(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// The single `{ "xRenderer": {...} }` key of a wrapper object.
fn renderer(v: &Value) -> Option<(&str, &Value)> {
    v.as_object()?.iter().next().map(|(k, v)| (k.as_str(), v))
}

fn thumbs(v: &Value) -> Vec<Thumb> {
    // Accepts a musicThumbnailRenderer wrapper, a thumbnail object or the list itself.
    let list = if v["musicThumbnailRenderer"].is_object() {
        &v["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]
    } else if v["thumbnail"]["thumbnails"].is_array() {
        &v["thumbnail"]["thumbnails"]
    } else if v["thumbnails"].is_array() {
        &v["thumbnails"]
    } else {
        v
    };
    arr(list)
        .iter()
        .filter_map(|t| {
            Some(Thumb {
                url: t["url"].as_str()?.to_string(),
                width: t["width"].as_u64().unwrap_or(0) as u32,
                height: t["height"].as_u64().unwrap_or(0) as u32,
            })
        })
        .collect()
}

fn page_type(endpoint: &Value) -> Option<&str> {
    endpoint["browseEndpoint"]["browseEndpointContextSupportedConfigs"]["browseEndpointContextMusicConfig"]
        ["pageType"]
        .as_str()
}

fn video_type(watch: &Value) -> Option<&str> {
    watch["watchEndpointMusicSupportedConfigs"]["watchEndpointMusicConfig"]["musicVideoType"]
        .as_str()
}

fn track_kind(watch: &Value) -> Option<TrackKind> {
    match video_type(watch) {
        Some("MUSIC_VIDEO_TYPE_ATV") | None => Some(TrackKind::Song),
        Some("MUSIC_VIDEO_TYPE_PODCAST_EPISODE") => None,
        Some(_) => Some(TrackKind::Video),
    }
}

fn is_artist_page(pt: Option<&str>) -> bool {
    matches!(
        pt,
        Some(
            "MUSIC_PAGE_TYPE_ARTIST"
                | "MUSIC_PAGE_TYPE_USER_CHANNEL"
                | "MUSIC_PAGE_TYPE_LIBRARY_ARTIST"
        )
    )
}

fn explicit(badges: &Value) -> bool {
    arr(badges)
        .iter()
        .any(|b| b["musicInlineBadgeRenderer"]["icon"]["iconType"] == "MUSIC_EXPLICIT_BADGE")
}

/// "3:45" / "1:02:03" → ms.
pub fn parse_duration(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if !(2..=3).contains(&parts.len())
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some(
        parts
            .iter()
            .fold(0u64, |acc, p| acc * 60 + p.parse::<u64>().unwrap_or(0))
            * 1000,
    )
}

/// "149M plays", "48M views", "5.8K", "1.2B plays" → an approximate count.
pub fn parse_count(s: &str) -> Option<u64> {
    let s = s.trim();
    let num = s.split_whitespace().next()?.replace(',', "");
    let (digits, mult) = match num.chars().last()? {
        'K' => (&num[..num.len() - 1], 1e3),
        'M' => (&num[..num.len() - 1], 1e6),
        'B' => (&num[..num.len() - 1], 1e9),
        _ => (num.as_str(), 1.0),
    };
    let rest = s[s.find(' ').map(|i| i + 1).unwrap_or(s.len())..].trim();
    if !(rest.is_empty() || rest.starts_with("play") || rest.starts_with("view")) {
        return None;
    }
    if rest.is_empty() && mult == 1.0 {
        return None; // a bare number isn't a count
    }
    Some((digits.parse::<f64>().ok()? * mult).round() as u64)
}

/// The play or view count among subtitle segments.
fn plays_in<'a>(parts: impl IntoIterator<Item = &'a String>) -> Option<u64> {
    parts
        .into_iter()
        .flat_map(|s| s.split(SEP))
        .filter(|p| {
            p.ends_with("plays")
                || p.ends_with("play")
                || p.ends_with("views")
                || p.ends_with("view")
        })
        .find_map(parse_count)
}

fn year_in(s: &str) -> Option<u32> {
    s.split(|c: char| !c.is_ascii_digit())
        .find(|w| w.len() == 4)
        .and_then(|w| w.parse().ok())
        .filter(|y| (1900..2100).contains(y))
}

/// Linked artists among `runs`.
fn artists_in(rs: &[Value]) -> Vec<Named> {
    rs.iter()
        .filter(|r| is_artist_page(page_type(&r["navigationEndpoint"])))
        .map(|r| Named {
            name: r["text"].as_str().unwrap_or("").to_string(),
            id: str_of(&r["navigationEndpoint"]["browseEndpoint"]["browseId"]),
        })
        .collect()
}

fn album_in(rs: &[Value]) -> Option<Named> {
    rs.iter()
        .find(|r| page_type(&r["navigationEndpoint"]) == Some("MUSIC_PAGE_TYPE_ALBUM"))
        .map(|r| Named {
            name: r["text"].as_str().unwrap_or("").to_string(),
            id: str_of(&r["navigationEndpoint"]["browseEndpoint"]["browseId"]),
        })
}

const SEP: &str = " • ";
/// Leading labels YouTube puts in subtitles ("Song • Artist • 3:45").
const LABELS: &[&str] = &[
    "Song", "Video", "Album", "Single", "EP", "Playlist", "Artist", "Episode", "Podcast", "Profile",
];

/// Subtitle segments that aren't type labels, counts or durations: unlinked artist names.
fn plain_names(s: &str) -> Vec<Named> {
    s.split(SEP)
        .map(str::trim)
        .filter(|p| {
            !p.is_empty()
                && !LABELS.contains(p)
                && parse_duration(p).is_none()
                && year_in(p).is_none_or(|_| p.len() != 4)
                && !p.ends_with("plays")
                && !p.ends_with("views")
                && !p.ends_with("songs")
                && !p.ends_with("likes")
        })
        .take(1)
        .flat_map(|p| {
            p.split(" & ")
                .map(|n| Named {
                    name: n.trim().to_string(),
                    id: None,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn strip_label(s: &str) -> String {
    match s.split_once(SEP) {
        Some((first, rest)) if LABELS.contains(&first) => rest.to_string(),
        _ => s.to_string(),
    }
}

/// Artist subtitles keep the monthly audience only; subscriber counts are dropped.
fn audience(s: &str) -> String {
    s.split(SEP)
        .filter(|p| !p.contains("subscriber"))
        .collect::<Vec<_>>()
        .join(SEP)
}

fn album_kind(s: &str) -> AlbumKind {
    match s.split(SEP).next().unwrap_or("") {
        "Single" => AlbumKind::Single,
        "EP" => AlbumKind::Ep,
        _ => AlbumKind::Album,
    }
}

fn like_status(menu: &Value) -> Option<Like> {
    arr(&menu["menuRenderer"]["topLevelButtons"])
        .iter()
        .find_map(|b| match b["likeButtonRenderer"]["likeStatus"].as_str()? {
            "LIKE" => Some(Like::Like),
            "DISLIKE" => Some(Like::Dislike),
            _ => Some(Like::Indifferent),
        })
}

/// A song menu's "Save to library" toggle.
fn library(menu: &Value) -> Option<Library> {
    arr(&menu["menuRenderer"]["items"]).iter().find_map(|i| {
        let t = &i["toggleMenuServiceItemRenderer"];
        if t["defaultIcon"]["iconType"].as_str() != Some("BOOKMARK_BORDER") {
            return None;
        }
        let token = |k: &str| str_of(&t[k]["feedbackEndpoint"]["feedbackToken"]);
        Some(Library {
            saved: t["isToggled"].as_bool().unwrap_or(false),
            add: token("defaultServiceEndpoint")?,
            remove: token("toggledServiceEndpoint")?,
        })
    })
}

/// Whether a header's "Save to library" button is on (`None` without one).
fn saved(h: &Value) -> Option<bool> {
    arr(&h["buttons"]).iter().find_map(|b| {
        let t = &b["toggleButtonRenderer"];
        (t["defaultIcon"]["iconType"].as_str() == Some("BOOKMARK_BORDER"))
            .then(|| t["isToggled"].as_bool().unwrap_or(false))
    })
}

/// The token a menu's "Not interested" sends (Home recommendations).
fn not_interested(menu: &Value) -> Option<String> {
    arr(&menu["menuRenderer"]["items"]).iter().find_map(|i| {
        let s = &i["menuServiceItemRenderer"];
        if s["icon"]["iconType"].as_str() != Some("NOT_INTERESTED") {
            return None;
        }
        str_of(&s["serviceEndpoint"]["feedbackEndpoint"]["feedbackToken"])
    })
}

fn browse_ref(endpoint: &Value) -> Option<BrowseRef> {
    let b = &endpoint["browseEndpoint"];
    Some(BrowseRef {
        browse_id: str_of(&b["browseId"])?,
        params: str_of(&b["params"]),
    })
}

// ---- items -----------------------------------------------------------------------------------

/// Any list/grid item. `None` for things the app doesn't show (podcasts, ads, unknowns).
pub fn item(v: &Value) -> Option<Item> {
    let (k, r) = renderer(v)?;
    let mut i = match k {
        "musicResponsiveListItemRenderer" => responsive(r),
        "musicTwoRowItemRenderer" => two_row(r),
        "musicNavigationButtonRenderer" => nav_button(r),
        "playlistPanelVideoRenderer" => panel_track(r).map(Item::Track),
        "playlistPanelVideoWrapperRenderer" => item(&r["primaryRenderer"]),
        _ => None,
    };
    let token = not_interested(&r["menu"]);
    match &mut i {
        Some(Item::Track(t)) => {
            t.not_interested = token;
            t.library = library(&r["menu"]);
        }
        Some(Item::Album(a)) => a.not_interested = token,
        Some(Item::Playlist(p)) => p.not_interested = token,
        _ => {}
    }
    // Library artist rows link a library-only page (`MPLA` + channel id); the app opens the
    // artist's own page instead.
    if let Some(Item::Artist(a)) = &mut i
        && let Some(id) = a.id.strip_prefix("MPLA")
    {
        a.id = id.to_string();
    }
    i
}

fn two_row(r: &Value) -> Option<Item> {
    let ep = &r["navigationEndpoint"];
    let title = text(&r["title"]);
    let sub = &r["subtitle"];
    let sub_text = text(sub);
    let th = thumbs(&r["thumbnailRenderer"]);
    if let Some(vid) = ep["watchEndpoint"]["videoId"].as_str() {
        let mut artists = artists_in(runs(sub));
        if artists.is_empty() {
            artists = plain_names(&sub_text);
        }
        return Some(Item::Track(Track {
            video_id: vid.to_string(),
            title,
            artists,
            album: album_in(runs(sub)),
            thumbs: th,
            explicit: explicit(&r["subtitleBadges"]),
            kind: track_kind(&ep["watchEndpoint"])?,
            ..Default::default()
        }));
    }
    let id = str_of(&ep["browseEndpoint"]["browseId"])?;
    match page_type(ep) {
        Some("MUSIC_PAGE_TYPE_ALBUM") | Some("MUSIC_PAGE_TYPE_AUDIOBOOK") => {
            Some(Item::Album(AlbumCard {
                id,
                title,
                artists: artists_in(runs(sub)),
                year: year_in(&sub_text),
                kind: album_kind(&sub_text),
                thumbs: th,
                explicit: explicit(&r["subtitleBadges"]),
                not_interested: None,
            }))
        }
        pt if is_artist_page(pt) => Some(Item::Artist(ArtistCard {
            id,
            name: title,
            subtitle: audience(&sub_text),
            thumbs: th,
        })),
        Some("MUSIC_PAGE_TYPE_PLAYLIST") => Some(Item::Playlist(PlaylistCard {
            id,
            title,
            subtitle: strip_label(&sub_text),
            thumbs: th,
            not_interested: None,
        })),
        _ => None,
    }
}

fn responsive(r: &Value) -> Option<Item> {
    let cols: Vec<&Value> = arr(&r["flexColumns"])
        .iter()
        .map(|c| &c["musicResponsiveListItemFlexColumnRenderer"]["text"])
        .collect();
    let first = cols.first().copied().unwrap_or(&Value::Null);
    let title = text(first);
    let th = thumbs(&r["thumbnail"]);
    let rest_runs: Vec<Value> = cols
        .iter()
        .skip(1)
        .flat_map(|c| runs(c).iter().cloned())
        .collect();
    let rest_text: Vec<String> = cols
        .iter()
        .skip(1)
        .map(|c| text(c))
        .filter(|s| !s.is_empty())
        .collect();

    // Browse items (albums, artists, playlists) carry their endpoint on the row itself.
    let ep = &r["navigationEndpoint"];
    if ep["browseEndpoint"].is_object() {
        let id = str_of(&ep["browseEndpoint"]["browseId"])?;
        let sub = rest_text.join(SEP);
        return match page_type(ep) {
            Some("MUSIC_PAGE_TYPE_ALBUM") => Some(Item::Album(AlbumCard {
                id,
                title,
                artists: artists_in(&rest_runs),
                year: year_in(&sub),
                kind: album_kind(&sub),
                thumbs: th,
                explicit: explicit(&r["badges"]),
                not_interested: None,
            })),
            pt if is_artist_page(pt) => Some(Item::Artist(ArtistCard {
                id,
                name: title,
                subtitle: audience(&strip_label(&sub)),
                thumbs: th,
            })),
            Some("MUSIC_PAGE_TYPE_PLAYLIST") => Some(Item::Playlist(PlaylistCard {
                id,
                title,
                subtitle: strip_label(&sub),
                thumbs: th,
                not_interested: None,
            })),
            _ => None,
        };
    }

    // Tracks.
    let watch = runs(first)
        .iter()
        .map(|x| &x["navigationEndpoint"]["watchEndpoint"])
        .find(|w| w.is_object())
        .unwrap_or(
            &r["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]["musicPlayButtonRenderer"]
                ["playNavigationEndpoint"]["watchEndpoint"],
        );
    let video_id =
        str_of(&r["playlistItemData"]["videoId"]).or_else(|| str_of(&watch["videoId"]))?;
    let mut artists = artists_in(&rest_runs);
    if artists.is_empty()
        && let Some(s) = rest_text.first()
    {
        artists = plain_names(s);
    }
    let fixed = arr(&r["fixedColumns"])
        .iter()
        .map(|c| text(&c["musicResponsiveListItemFixedColumnRenderer"]["text"]))
        .collect::<Vec<_>>();
    let duration_ms = fixed
        .iter()
        .chain(rest_text.iter())
        .flat_map(|s| s.split(SEP).map(str::to_string).collect::<Vec<_>>())
        .find_map(|p| parse_duration(&p));
    Some(Item::Track(Track {
        video_id,
        title,
        artists,
        album: album_in(&rest_runs),
        duration_ms,
        thumbs: th,
        explicit: explicit(&r["badges"]),
        kind: track_kind(watch)?,
        set_video_id: str_of(&r["playlistItemData"]["playlistSetVideoId"]),
        like: like_status(&r["menu"]),
        unavailable: r["musicItemRendererDisplayPolicy"]
            == "MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT",
        plays: plays_in(rest_text.iter()),
        not_interested: None,
        library: None,
    }))
}

fn nav_button(r: &Value) -> Option<Item> {
    let b = &r["clickCommand"]["browseEndpoint"];
    Some(Item::Link(Link {
        title: text(&r["buttonText"]),
        browse_id: str_of(&b["browseId"])?,
        params: str_of(&b["params"]),
        color: r["solid"]["leftStripeColor"].as_u64().map(|c| c as u32),
    }))
}

fn panel_track(r: &Value) -> Option<Track> {
    let watch = &r["navigationEndpoint"]["watchEndpoint"];
    let byline = runs(&r["longBylineText"]);
    let mut artists = artists_in(byline);
    if artists.is_empty() {
        artists = plain_names(&text(&r["shortBylineText"]));
    }
    Some(Track {
        video_id: str_of(&r["videoId"])?,
        title: text(&r["title"]),
        artists,
        album: album_in(byline),
        duration_ms: parse_duration(&text(&r["lengthText"])),
        thumbs: thumbs(&r["thumbnail"]),
        explicit: explicit(&r["badges"]),
        kind: track_kind(watch)?,
        set_video_id: str_of(&r["playlistSetVideoId"]),
        like: None,
        unavailable: false,
        plays: plays_in([text(&r["longBylineText"])].iter()),
        not_interested: None,
        library: None,
    })
}

// ---- shelves and pages -----------------------------------------------------------------------

/// One section of a section list. `None` for sections the app doesn't show.
fn shelf(v: &Value) -> Option<Shelf> {
    let (k, r) = renderer(v)?;
    let (title, list, more) = match k {
        "musicCarouselShelfRenderer" => {
            let h = &r["header"]["musicCarouselShelfBasicHeaderRenderer"];
            let more = browse_ref(&h["moreContentButton"]["buttonRenderer"]["navigationEndpoint"])
                .or_else(|| browse_ref(&h["title"]["runs"][0]["navigationEndpoint"]));
            (text(&h["title"]), &r["contents"], more)
        }
        "musicShelfRenderer" => (
            text(&r["title"]),
            &r["contents"],
            browse_ref(&r["bottomEndpoint"]),
        ),
        "musicPlaylistShelfRenderer" => (String::new(), &r["contents"], None),
        "gridRenderer" => (
            text(&r["header"]["gridHeaderRenderer"]["title"]),
            &r["items"],
            None,
        ),
        "musicImmersiveCarouselShelfRenderer" => (
            text(&r["header"]["musicImmersiveCarouselShelfBasicHeaderRenderer"]["title"]),
            &r["contents"],
            None,
        ),
        "itemSectionRenderer" => {
            // Wrapper: one shelf inside, or plain rows (search).
            let inner = arr(&r["contents"]);
            if let Some(s) = inner.iter().find_map(|c| {
                renderer(c)
                    .filter(|(k, _)| k.ends_with("ShelfRenderer") || *k == "gridRenderer")
                    .and_then(|_| shelf(c))
            }) {
                return Some(s);
            }
            (String::new(), &r["contents"], None)
        }
        _ => return None,
    };
    let items: Vec<Item> = arr(list).iter().filter_map(item).collect();
    if items.is_empty() {
        return None;
    }
    Some(Shelf { title, items, more })
}

/// All `sectionListRenderer`s of a browse/search response, in page order.
fn section_lists(v: &Value) -> Vec<&Value> {
    let c = &v["contents"];
    let mut out = Vec::new();
    for root in [
        &c["singleColumnBrowseResultsRenderer"],
        &c["twoColumnBrowseResultsRenderer"],
        &c["tabbedSearchResultsRenderer"],
    ] {
        for tab in arr(&root["tabs"]) {
            let s = &tab["tabRenderer"]["content"]["sectionListRenderer"];
            if s.is_object() {
                out.push(s);
            }
        }
        let s = &root["secondaryContents"]["sectionListRenderer"];
        if s.is_object() {
            out.push(s);
        }
    }
    if c["sectionListRenderer"].is_object() {
        out.push(&c["sectionListRenderer"]);
    }
    out
}

/// Continuation of a list's `continuations` array or a trailing `continuationItemRenderer`.
fn continuation(container: &Value, items: &Value) -> Option<Continuation> {
    for c in arr(&container["continuations"]) {
        for k in ["nextContinuationData", "nextRadioContinuationData"] {
            if let Some(t) = c[k]["continuation"].as_str() {
                return Some(Continuation::Query(t.to_string()));
            }
        }
    }
    arr(items).last().and_then(|last| {
        let cmd = &last["continuationItemRenderer"]["continuationEndpoint"]["continuationCommand"]
            ["token"];
        cmd.as_str().map(|t| Continuation::Body(t.to_string()))
    })
}

/// A page of shelves (Home, Explore, moods, library pages) or its continuation.
pub fn feed(v: &Value) -> Feed {
    let mut f = Feed {
        title: opt_text(&v["header"]["musicHeaderRenderer"]["title"]),
        ..Default::default()
    };
    let lists: Vec<&Value> = section_lists(v);
    let cont = &v["continuationContents"]["sectionListContinuation"];
    for s in lists
        .iter()
        .copied()
        .chain(cont.is_object().then_some(cont))
    {
        for sec in arr(&s["contents"]) {
            f.shelves.extend(shelf(sec));
            // Library pages are one long shelf or grid that continues by itself.
            if let Some((_, r)) = renderer(sec)
                && let Some(c) = continuation(r, inner_items(r))
            {
                f.continuation = Some(c);
            }
        }
        if let Some(c) = continuation(s, &s["contents"]) {
            f.continuation = Some(c);
        }
    }
    // The next part of a long shelf or grid: one untitled shelf to append to the last one.
    for k in ["musicShelfContinuation", "gridContinuation"] {
        let c = &v["continuationContents"][k];
        if c.is_object() {
            let items = inner_items(c);
            f.shelves.extend(untitled(items));
            f.continuation = continuation(c, items);
        }
    }
    // New-style continuation responses: more shelves, or more items of one.
    for a in arr(&v["onResponseReceivedActions"]) {
        let items = &a["appendContinuationItemsAction"]["continuationItems"];
        let shelves: Vec<Shelf> = arr(items).iter().filter_map(shelf).collect();
        if shelves.is_empty() {
            f.shelves.extend(untitled(items));
        } else {
            f.shelves.extend(shelves);
        }
        if let Some(c) = continuation(&Value::Null, items) {
            f.continuation = Some(c);
        }
    }
    f
}

fn inner_items(r: &Value) -> &Value {
    if r["items"].is_array() {
        &r["items"]
    } else {
        &r["contents"]
    }
}

fn untitled(items: &Value) -> Option<Shelf> {
    let items: Vec<Item> = arr(items).iter().filter_map(item).collect();
    (!items.is_empty()).then(|| Shelf {
        title: String::new(),
        items,
        more: None,
    })
}

pub fn search(v: &Value) -> SearchResults {
    let mut out = SearchResults::default();
    let take_list = |list: &Value, out: &mut SearchResults| {
        for sec in arr(list) {
            let Some((k, r)) = renderer(sec) else {
                continue;
            };
            match k {
                "musicCardShelfRenderer" => {
                    out.top = top_card(r);
                    out.top_extra = arr(&r["contents"]).iter().filter_map(item).collect();
                    // An artist card's songs leave the artist out.
                    if let Some(Item::Artist(a)) = &out.top {
                        for i in &mut out.top_extra {
                            if let Item::Track(t) = i
                                && t.artists.is_empty()
                            {
                                t.artists.push(Named {
                                    name: a.name.clone(),
                                    id: Some(a.id.clone()),
                                });
                            }
                        }
                    }
                }
                "musicShelfRenderer" | "itemSectionRenderer" => {
                    out.items
                        .extend(arr(&r["contents"]).iter().filter_map(item));
                    if let Some(c) = continuation(r, &r["contents"]) {
                        out.continuation = Some(c);
                    }
                }
                _ => {}
            }
        }
    };
    for s in section_lists(v) {
        take_list(&s["contents"], &mut out);
    }
    // Continuation of a filtered search.
    let c = &v["continuationContents"]["musicShelfContinuation"];
    if c.is_object() {
        out.items
            .extend(arr(&c["contents"]).iter().filter_map(item));
        out.continuation = continuation(c, &c["contents"]);
    }
    out
}

/// The "top result" card of a search.
fn top_card(r: &Value) -> Option<Item> {
    let title = text(&r["title"]);
    let sub = text(&r["subtitle"]);
    let th = thumbs(&r["thumbnail"]);
    let ep = &r["title"]["runs"][0]["navigationEndpoint"];
    let tap = if ep.is_object() { ep } else { &r["onTap"] };
    if let Some(vid) = tap["watchEndpoint"]["videoId"].as_str() {
        let mut artists = artists_in(runs(&r["subtitle"]));
        if artists.is_empty() {
            artists = plain_names(&sub);
        }
        let duration_ms = sub.split(SEP).find_map(parse_duration);
        return Some(Item::Track(Track {
            video_id: vid.to_string(),
            title,
            artists,
            album: album_in(runs(&r["subtitle"])),
            duration_ms,
            thumbs: th,
            kind: track_kind(&tap["watchEndpoint"])?,
            ..Default::default()
        }));
    }
    let id = str_of(&tap["browseEndpoint"]["browseId"])?;
    match page_type(tap) {
        pt if is_artist_page(pt) => Some(Item::Artist(ArtistCard {
            id,
            name: title,
            subtitle: audience(&strip_label(&sub)),
            thumbs: th,
        })),
        Some("MUSIC_PAGE_TYPE_ALBUM") => Some(Item::Album(AlbumCard {
            id,
            title,
            artists: artists_in(runs(&r["subtitle"])),
            year: year_in(&sub),
            kind: album_kind(&sub),
            thumbs: th,
            explicit: false,
            not_interested: None,
        })),
        Some("MUSIC_PAGE_TYPE_PLAYLIST") => Some(Item::Playlist(PlaylistCard {
            id,
            title,
            subtitle: strip_label(&sub),
            thumbs: th,
            not_interested: None,
        })),
        _ => None,
    }
}

pub fn suggestions(v: &Value) -> (Vec<String>, Vec<Item>) {
    let mut words = Vec::new();
    let mut items = Vec::new();
    for sec in arr(&v["contents"]) {
        for c in arr(&sec["searchSuggestionsSectionRenderer"]["contents"]) {
            if let Some(s) = opt_text(&c["searchSuggestionRenderer"]["suggestion"]) {
                words.push(s);
            } else if let Some(i) = item(c) {
                items.push(i);
            }
        }
    }
    (words, items)
}

/// Header of album and playlist pages.
fn responsive_header(v: &Value) -> &Value {
    for s in section_lists(v) {
        for c in arr(&s["contents"]) {
            for k in [
                "musicResponsiveHeaderRenderer",
                "musicEditablePlaylistDetailHeaderRenderer",
            ] {
                if c[k].is_object() {
                    let h = &c[k];
                    // The editable variant wraps the real header.
                    let inner = &h["header"]["musicResponsiveHeaderRenderer"];
                    return if inner.is_object() { inner } else { h };
                }
            }
        }
    }
    &Value::Null
}

fn description(h: &Value) -> Option<String> {
    opt_text(&h["description"]["musicDescriptionShelfRenderer"]["description"])
}

pub fn album(id: &str, v: &Value) -> AlbumPage {
    let h = responsive_header(v);
    let sub = text(&h["subtitle"]);
    let artists = artists_in(runs(&h["straplineTextOne"]));
    let th = thumbs(&h["thumbnail"]);
    let playlist_id = arr(&h["buttons"]).iter().find_map(|b| {
        str_of(&b["musicPlayButtonRenderer"]["playNavigationEndpoint"]["watchPlaylistEndpoint"]["playlistId"])
    });
    let mut page = AlbumPage {
        id: id.to_string(),
        title: text(&h["title"]),
        kind: album_kind(&sub),
        year: year_in(&sub),
        artists: artists.clone(),
        thumbs: th.clone(),
        explicit: explicit(&h["subtitleBadge"]),
        description: description(h),
        summary: text(&h["secondSubtitle"]),
        playlist_id,
        saved: saved(h),
        ..Default::default()
    };
    let me = Named {
        name: page.title.clone(),
        id: Some(id.to_string()),
    };
    for s in section_lists(v) {
        for sec in arr(&s["contents"]) {
            if sec["musicShelfRenderer"].is_object() && page.tracks.is_empty() {
                // Album rows leave out what the header already says: fill it in.
                for row in arr(&sec["musicShelfRenderer"]["contents"]) {
                    if let Some(Item::Track(mut t)) = item(row) {
                        // Some albums list the music videos; the credits entry names the audio
                        // track, which is what an album should play.
                        if t.kind == TrackKind::Video
                            && let Some(audio) =
                                credits_id(&row["musicResponsiveListItemRenderer"]["menu"])
                        {
                            t.video_id = audio;
                            t.kind = TrackKind::Song;
                        }
                        if t.artists.is_empty() {
                            t.artists = artists.clone();
                        }
                        t.album.get_or_insert_with(|| me.clone());
                        if t.thumbs.is_empty() {
                            t.thumbs = th.clone();
                        }
                        page.tracks.push(t);
                    }
                }
            } else if sec["musicCarouselShelfRenderer"].is_object() {
                page.shelves.extend(shelf(sec));
            }
        }
    }
    page
}

/// Video id from a row menu's "View song credits" entry (`MPTC<videoId>`).
fn credits_id(menu: &Value) -> Option<String> {
    arr(&menu["menuRenderer"]["items"]).iter().find_map(|i| {
        let id =
            i["menuNavigationItemRenderer"]["navigationEndpoint"]["browseEndpoint"]["browseId"]
                .as_str()?;
        id.strip_prefix("MPTC")
            .filter(|v| v.len() == 11)
            .map(str::to_string)
    })
}

pub fn playlist(id: &str, v: &Value) -> PlaylistPage {
    let h = responsive_header(v);
    let author_run = runs(&h["straplineTextOne"]).first();
    let mut page = PlaylistPage {
        id: id.trim_start_matches("VL").to_string(),
        title: text(&h["title"]),
        author: author_run
            .map(|r| Named {
                name: r["text"].as_str().unwrap_or("").to_string(),
                id: str_of(&r["navigationEndpoint"]["browseEndpoint"]["browseId"]),
            })
            .or_else(|| {
                // Newer headers show the author as an avatar "facepile".
                let f = &h["facepile"]["avatarStackViewModel"];
                Some(Named {
                    name: str_of(&f["text"]["content"])?,
                    id: str_of(
                        &f["rendererContext"]["commandContext"]["onTap"]["innertubeCommand"]["browseEndpoint"]
                            ["browseId"],
                    ),
                })
            }),
        subtitle: text(&h["subtitle"]),
        summary: text(&h["secondSubtitle"]),
        description: description(h),
        thumbs: thumbs(&h["thumbnail"]),
        owned: arr(&h["buttons"]).iter().any(|b| {
            b["buttonRenderer"]["icon"]["iconType"] == "EDIT"
                || b["buttonRenderer"]["navigationEndpoint"]["playlistEditorEndpoint"].is_object()
        }) || !v["contents"]["twoColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]
            ["sectionListRenderer"]["contents"][0]["musicEditablePlaylistDetailHeaderRenderer"]
            .is_null(),
        saved: saved(h),
        ..Default::default()
    };
    if page.thumbs.is_empty() {
        page.thumbs = thumbs(&h["thumbnail"]["croppedSquareThumbnailRenderer"]);
    }
    for s in section_lists(v) {
        for sec in arr(&s["contents"]) {
            let shelf = &sec["musicPlaylistShelfRenderer"];
            if shelf.is_object() {
                page.tracks.extend(tracks(&shelf["contents"]));
                page.continuation = continuation(shelf, &shelf["contents"]);
            }
        }
    }
    page
}

fn tracks(list: &Value) -> Vec<Track> {
    arr(list)
        .iter()
        .filter_map(item)
        .filter_map(|i| match i {
            Item::Track(t) => Some(t),
            _ => None,
        })
        .collect()
}

/// The next page of a playlist (either continuation scheme).
pub fn playlist_continuation(v: &Value) -> (Vec<Track>, Option<Continuation>) {
    let c = &v["continuationContents"]["musicPlaylistShelfContinuation"];
    if c.is_object() {
        return (tracks(&c["contents"]), continuation(c, &c["contents"]));
    }
    for a in arr(&v["onResponseReceivedActions"]) {
        let items = &a["appendContinuationItemsAction"]["continuationItems"];
        if items.is_array() {
            return (tracks(items), continuation(&Value::Null, items));
        }
    }
    (Vec::new(), None)
}

pub fn artist(id: &str, v: &Value) -> ArtistPage {
    let h = if v["header"]["musicImmersiveHeaderRenderer"].is_object() {
        &v["header"]["musicImmersiveHeaderRenderer"]
    } else {
        &v["header"]["musicVisualHeaderRenderer"]
    };
    let watch_ref = |b: &Value| {
        let w = &b["buttonRenderer"]["navigationEndpoint"]["watchEndpoint"];
        Some(WatchRef {
            video_id: str_of(&w["videoId"]),
            playlist_id: str_of(&w["playlistId"])?,
            params: str_of(&w["params"]),
        })
    };
    let sub = &h["subscriptionButton"]["subscribeButtonRenderer"];
    let mut page = ArtistPage {
        id: id.to_string(),
        name: text(&h["title"]),
        audience: opt_text(&h["monthlyListenerCount"]),
        description: opt_text(&h["description"]),
        thumbs: thumbs(&h["thumbnail"]),
        shuffle: watch_ref(&h["playButton"]),
        radio: watch_ref(&h["startRadioButton"]),
        subscribed: sub["subscribed"].as_bool(),
        channel_id: str_of(&sub["channelId"]),
        shelves: Vec::new(),
    };
    for s in section_lists(v) {
        for sec in arr(&s["contents"]) {
            if let Some(d) = opt_text(&sec["musicDescriptionShelfRenderer"]["description"]) {
                page.description.get_or_insert(d);
            } else if let Some(sh) = shelf(sec) {
                page.shelves.push(sh);
            }
        }
    }
    // Headers are styled in capitals ("PINK FLOYD"); the page's own rows have the real casing.
    if let Some(n) = page
        .shelves
        .iter()
        .flat_map(|s| &s.items)
        .find_map(|i| match i {
            Item::Track(t) => t
                .artists
                .iter()
                .find(|a| a.name.eq_ignore_ascii_case(&page.name)),
            _ => None,
        })
    {
        page.name = n.name.clone();
    }
    page
}

/// Radio / up-next queue from `next`, or its continuation.
pub fn watch_next(v: &Value) -> WatchNext {
    let tabs = &v["contents"]["singleColumnMusicWatchNextResultsRenderer"]["tabbedRenderer"]["watchNextTabbedResultsRenderer"]
        ["tabs"];
    let panel = &tabs[0]["tabRenderer"]["content"]["musicQueueRenderer"]["content"]["playlistPanelRenderer"];
    let cont = &v["continuationContents"]["playlistPanelContinuation"];
    let p = if panel.is_object() { panel } else { cont };
    let chips = arr(
        &tabs[0]["tabRenderer"]["content"]["musicQueueRenderer"]["subHeaderChipCloud"]["chipCloudRenderer"]["chips"],
    )
    .iter()
    .filter_map(|c| {
        let c = &c["chipCloudChipRenderer"];
        let w = &c["navigationEndpoint"]["queueUpdateCommand"]["fetchContentsCommand"]["watchEndpoint"];
        Some(Chip {
            label: opt_text(&c["text"])?,
            selected: c["isSelected"].as_bool().unwrap_or(false),
            watch: WatchRef {
                video_id: str_of(&w["videoId"]),
                playlist_id: str_of(&w["playlistId"])?,
                params: str_of(&w["params"]),
            },
        })
    })
    .collect();
    let tab = |prefix: &str| {
        arr(tabs).iter().find_map(|t| {
            let id = t["tabRenderer"]["endpoint"]["browseEndpoint"]["browseId"].as_str()?;
            id.starts_with(prefix).then(|| id.to_string())
        })
    };
    let (lyrics_id, related_id) = (tab("MPLY"), tab("MPTR"));
    WatchNext {
        tracks: tracks(&p["contents"]),
        playlist_id: str_of(&p["playlistId"]),
        continuation: continuation(p, &p["contents"]),
        lyrics_id,
        related_id,
        chips,
    }
}

pub fn account(v: &Value) -> Option<Account> {
    let h = &v["actions"][0]["openPopupAction"]["popup"]["multiPageMenuRenderer"]["header"]["activeAccountHeaderRenderer"];
    Some(Account {
        name: opt_text(&h["accountName"])?,
        handle: opt_text(&h["channelHandle"]),
        photo: thumbs(&h["accountPhoto"]),
    })
}

/// `playbackTracking.videostatsPlaybackUrl.baseUrl` of a signed-in `/player` response.
pub fn playback_url(v: &Value) -> Option<String> {
    str_of(&v["playbackTracking"]["videostatsPlaybackUrl"]["baseUrl"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts() {
        assert_eq!(parse_count("149M plays"), Some(149_000_000));
        assert_eq!(parse_count("5.8K views"), Some(5_800));
        assert_eq!(parse_count("1.2B plays"), Some(1_200_000_000));
        assert_eq!(parse_count("1,234 views"), Some(1_234));
        assert_eq!(parse_count("2013"), None);
        assert_eq!(parse_count("10 songs"), None);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("3:45"), Some(225_000));
        assert_eq!(parse_duration("1:02:03"), Some(3_723_000));
        assert_eq!(parse_duration("149M plays"), None);
        assert_eq!(parse_duration("3:4x"), None);
    }

    #[test]
    fn names_from_subtitles() {
        let n = plain_names("Song • Pink Floyd • 3:45");
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].name, "Pink Floyd");
        assert_eq!(
            strip_label("Playlist • YouTube Music • 40 songs"),
            "YouTube Music • 40 songs"
        );
        assert_eq!(album_kind("Single • 2024"), AlbumKind::Single);
        assert_eq!(year_in("Album • 1973"), Some(1973));
    }
}

/// The lyrics tab: the words and their source ("Source: Musixmatch" → "Musixmatch").
pub fn lyrics(v: &Value) -> Option<(String, String)> {
    section_lists(v)
        .into_iter()
        .chain(Some(&v["contents"]["sectionListRenderer"]))
        .flat_map(|s| arr(&s["contents"]))
        .find_map(|sec| {
            let d = &sec["musicDescriptionShelfRenderer"];
            let words = opt_text(&d["description"])?;
            let source = text(&d["footer"]);
            let source = source.trim_start_matches("Source:").trim().to_string();
            Some((words, source))
        })
}

/// A timed lyric line: start and end in milliseconds, and the words.
pub type TimedLine = (u64, u64, String);

/// The Android app's timed lyrics, in order, and their source ("Musixmatch").
pub fn timed_lyrics(v: &Value) -> Option<(Vec<TimedLine>, String)> {
    let d = &v["contents"]["elementRenderer"]["newElement"]["type"]["componentType"]["model"]["timedLyricsModel"]
        ["lyricsData"];
    let ms = |x: &Value| {
        x.as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| x.as_u64())
    };
    let mut lines: Vec<TimedLine> = arr(&d["timedLyricsData"])
        .iter()
        .filter_map(|l| {
            let c = &l["cueRange"];
            let text = l["lyricLine"].as_str()?.trim();
            (!text.is_empty()).then(|| {
                let start = ms(&c["startTimeMilliseconds"]).unwrap_or(0);
                let end = ms(&c["endTimeMilliseconds"]).unwrap_or(start);
                (start, end, text.to_string())
            })
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    lines.sort_by_key(|l| l.0);
    let source = d["sourceMessage"].as_str().unwrap_or("");
    let source = source.trim_start_matches("Source:").trim().to_string();
    Some((lines, source))
}

/// "View song credits": a dialog of roles and names.
pub fn credits(v: &Value) -> Credits {
    let d =
        &v["onResponseReceivedActions"][0]["openPopupAction"]["popup"]["dismissableDialogRenderer"];
    Credits {
        sections: arr(&d["sections"])
            .iter()
            .filter_map(|s| {
                let s = &s["dismissableDialogContentSectionRenderer"];
                let names: Vec<String> = runs(&s["subtitle"])
                    .iter()
                    .filter_map(|r| r["text"].as_str())
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string)
                    .collect();
                Some(CreditSection {
                    role: opt_text(&s["title"])?,
                    names,
                })
            })
            .filter(|s| !s.names.is_empty())
            .collect(),
    }
}

/// The Related tab's "About the artist" text.
pub fn about(v: &Value) -> Option<String> {
    section_lists(v)
        .into_iter()
        .chain(Some(&v["contents"]["sectionListRenderer"]))
        .flat_map(|s| arr(&s["contents"]))
        .find_map(|sec| opt_text(&sec["musicDescriptionShelfRenderer"]["description"]))
}
