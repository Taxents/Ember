//! YouTube Music's own web API (InnerTube, client `WEB_REMIX`), signed in when a session exists.
//!
//! Endpoint logic follows ytmusicapi (MIT). Every response is parsed tolerantly in `parse`:
//! anything unexpected is skipped, never fatal, and only known renderers are read, so ads and
//! promotions never reach the app.

use super::model::*;
use super::parse;
use super::session::{ORIGIN, Session};
use crate::{Error, Result, net};
use serde_json::{Value, json};
use std::sync::RwLock;

const API: &str = "https://music.youtube.com/youtubei/v1";
const CLIENT_NAME: &str = "WEB_REMIX";
const CLIENT_ID: &str = "67";
pub const CLIENT_VERSION: &str = "1.20260707.12.00";

pub struct Client {
    session: RwLock<Option<Session>>,
}

impl Client {
    pub fn new(session: Option<Session>) -> Self {
        Client {
            session: RwLock::new(session),
        }
    }

    /// Uses the stored session, if any.
    pub fn from_disk() -> Self {
        Self::new(Session::load())
    }

    pub fn signed_in(&self) -> bool {
        self.session.read().unwrap().is_some()
    }

    pub fn set_session(&self, s: Option<Session>) {
        *self.session.write().unwrap() = s;
    }

    /// POST to `endpoint` with the client context added. `query` is appended to the URL
    /// (continuations use it).
    pub fn call(&self, endpoint: &str, mut body: Value, query: &str) -> Result<Value> {
        body["context"] = json!({
            "client": {"clientName": CLIENT_NAME, "clientVersion": CLIENT_VERSION, "hl": "en"},
            "user": {},
        });
        let url = format!("{API}/{endpoint}?prettyPrint=false{query}");
        let session = self.session.read().unwrap().clone();
        let (cookie, auth);
        let mut headers: Vec<(&str, &str)> = vec![
            ("User-Agent", net::BROWSER_UA),
            ("Origin", ORIGIN),
            ("X-Origin", ORIGIN),
            ("X-YouTube-Client-Name", CLIENT_ID),
            ("X-YouTube-Client-Version", CLIENT_VERSION),
        ];
        if let Some(s) = &session {
            cookie = s.cookie_header();
            auth = s.authorization();
            headers.push(("Cookie", &cookie));
            headers.push(("Authorization", &auth));
            headers.push(("X-Goog-AuthUser", "0"));
        }
        net::post_json(&url, &headers, &body)
    }

    /// Like `call`, but fails clearly when signed out (for library and account actions).
    pub fn call_signed_in(&self, endpoint: &str, body: Value, query: &str) -> Result<Value> {
        if !self.signed_in() {
            return Err(Error::Unavailable("sign in required".into()));
        }
        self.call(endpoint, body, query)
    }

    pub fn browse(&self, browse_id: &str, params: Option<&str>) -> Result<Value> {
        let mut body = json!({ "browseId": browse_id });
        if let Some(p) = params {
            body["params"] = json!(p);
        }
        self.call("browse", body, "")
    }

    /// Follows a continuation token from a browse response.
    pub fn browse_continuation(&self, token: &str) -> Result<Value> {
        self.continue_on("browse", json!({}), &Continuation::Query(token.to_string()))
    }

    /// Fetches the next part of a list from `endpoint`, `body` being the original request body
    /// (the old scheme repeats it).
    fn continue_on(&self, endpoint: &str, mut body: Value, c: &Continuation) -> Result<Value> {
        match c {
            Continuation::Query(t) => {
                let t = encode(t);
                self.call(
                    endpoint,
                    body,
                    &format!("&ctoken={t}&continuation={t}&type=next"),
                )
            }
            Continuation::Body(t) => {
                body["continuation"] = json!(t);
                self.call(endpoint, body, "")
            }
        }
    }

    // ---- browsing --------------------------------------------------------------------------

    fn feed_page(&self, id: &str, params: Option<&str>) -> Result<Feed> {
        Ok(parse::feed(&self.browse(id, params)?))
    }

    pub fn home(&self) -> Result<Feed> {
        self.feed_page("FEmusic_home", None)
    }

    pub fn explore(&self) -> Result<Feed> {
        self.feed_page("FEmusic_explore", None)
    }

    pub fn new_releases(&self) -> Result<Feed> {
        self.feed_page("FEmusic_new_releases", None)
    }

    pub fn charts(&self) -> Result<Feed> {
        self.feed_page("FEmusic_charts", None)
    }

    pub fn moods(&self) -> Result<Feed> {
        self.feed_page("FEmusic_moods_and_genres", None)
    }

    /// Any page of shelves: a mood/genre `Link`, a shelf's "See all", a library page.
    pub fn feed(&self, r: &BrowseRef) -> Result<Feed> {
        self.feed_page(&r.browse_id, r.params.as_deref())
    }

    /// More shelves of a feed (Home scrolls this way).
    pub fn feed_more(&self, c: &Continuation) -> Result<Feed> {
        Ok(parse::feed(&self.continue_on("browse", json!({}), c)?))
    }

    pub fn search(&self, query: &str, filter: Option<SearchFilter>) -> Result<SearchResults> {
        Ok(parse::search(&self.call(
            "search",
            search_body(query, filter),
            "",
        )?))
    }

    pub fn search_more(
        &self,
        query: &str,
        filter: Option<SearchFilter>,
        c: &Continuation,
    ) -> Result<SearchResults> {
        Ok(parse::search(&self.continue_on(
            "search",
            search_body(query, filter),
            c,
        )?))
    }

    /// Completions for a partial query, plus quick results.
    pub fn suggestions(&self, partial: &str) -> Result<(Vec<String>, Vec<Item>)> {
        let v = self.call(
            "music/get_search_suggestions",
            json!({ "input": partial }),
            "",
        )?;
        Ok(parse::suggestions(&v))
    }

    pub fn album(&self, browse_id: &str) -> Result<AlbumPage> {
        Ok(parse::album(browse_id, &self.browse(browse_id, None)?))
    }

    pub fn artist(&self, browse_id: &str) -> Result<ArtistPage> {
        Ok(parse::artist(browse_id, &self.browse(browse_id, None)?))
    }

    /// A playlist by id, with or without the `VL` prefix. Long playlists continue with
    /// `playlist_more`.
    pub fn playlist(&self, id: &str) -> Result<PlaylistPage> {
        let browse_id = if id.starts_with("VL") {
            id.to_string()
        } else {
            format!("VL{id}")
        };
        Ok(parse::playlist(&browse_id, &self.browse(&browse_id, None)?))
    }

    pub fn playlist_more(&self, c: &Continuation) -> Result<(Vec<Track>, Option<Continuation>)> {
        Ok(parse::playlist_continuation(&self.continue_on(
            "browse",
            json!({}),
            c,
        )?))
    }

    /// The whole playlist, following continuations (bounded, for very long lists).
    pub fn playlist_all(&self, id: &str, max_tracks: usize) -> Result<PlaylistPage> {
        let mut p = self.playlist(id)?;
        while let Some(c) = p.continuation.take() {
            if p.tracks.len() >= max_tracks {
                p.continuation = Some(c);
                break;
            }
            let (more, next) = self.playlist_more(&c)?;
            if more.is_empty() {
                break;
            }
            p.tracks.extend(more);
            p.continuation = next;
        }
        Ok(p)
    }

    /// Radio for a track ("start radio"), or the up-next list of a playlist when
    /// `playlist_id` is given.
    pub fn watch_next(
        &self,
        video_id: Option<&str>,
        playlist_id: Option<&str>,
        params: Option<&str>,
    ) -> Result<WatchNext> {
        Ok(parse::watch_next(&self.call(
            "next",
            watch_body(video_id, playlist_id, params),
            "",
        )?))
    }

    pub fn radio(&self, video_id: &str) -> Result<WatchNext> {
        self.watch_next(Some(video_id), Some(&format!("RDAMVM{video_id}")), None)
    }

    pub fn watch_next_more(
        &self,
        video_id: Option<&str>,
        playlist_id: Option<&str>,
        params: Option<&str>,
        c: &Continuation,
    ) -> Result<WatchNext> {
        let v = self.continue_on("next", watch_body(video_id, playlist_id, params), c)?;
        Ok(parse::watch_next(&v))
    }

    // ---- library (signed in) ---------------------------------------------------------------

    fn library(&self, id: &str) -> Result<Feed> {
        Ok(parse::feed(&self.call_signed_in(
            "browse",
            json!({ "browseId": id }),
            "",
        )?))
    }

    /// Follows a feed's continuations until it ends or holds `max_items`; parts that continue
    /// a shelf (untitled) are appended to the last shelf.
    pub fn feed_all(&self, mut f: Feed, max_items: usize) -> Result<Feed> {
        let count = |f: &Feed| f.shelves.iter().map(|s| s.items.len()).sum::<usize>();
        while let Some(c) = f.continuation.take() {
            if count(&f) >= max_items {
                f.continuation = Some(c);
                break;
            }
            let more = self.feed_more(&c)?;
            if more.shelves.is_empty() {
                break;
            }
            for s in more.shelves {
                match f.shelves.last_mut() {
                    Some(last) if s.title.is_empty() => last.items.extend(s.items),
                    _ => f.shelves.push(s),
                }
            }
            f.continuation = more.continuation;
        }
        Ok(f)
    }

    pub fn library_playlists(&self) -> Result<Feed> {
        self.library("FEmusic_liked_playlists")
    }

    /// Songs saved to the library.
    pub fn library_songs(&self) -> Result<Feed> {
        self.library("FEmusic_liked_videos")
    }

    pub fn library_albums(&self) -> Result<Feed> {
        self.library("FEmusic_liked_albums")
    }

    /// Artists of songs in the library.
    pub fn library_artists(&self) -> Result<Feed> {
        self.library("FEmusic_library_corpus_track_artists")
    }

    /// Subscribed artists.
    pub fn subscriptions(&self) -> Result<Feed> {
        self.library("FEmusic_library_corpus_artists")
    }

    /// Recently played, grouped by day.
    pub fn history(&self) -> Result<Feed> {
        self.library("FEmusic_history")
    }

    pub fn liked_songs(&self, max_tracks: usize) -> Result<PlaylistPage> {
        if !self.signed_in() {
            return Err(Error::Unavailable("sign in required".into()));
        }
        self.playlist_all("VLLM", max_tracks)
    }

    pub fn account(&self) -> Result<Account> {
        let v = self.call_signed_in("account/account_menu", json!({}), "")?;
        parse::account(&v).ok_or_else(|| Error::Unavailable("no account in response".into()))
    }

    // ---- actions (signed in) ---------------------------------------------------------------

    pub fn rate(&self, video_id: &str, like: Like) -> Result<()> {
        let endpoint = match like {
            Like::Like => "like/like",
            Like::Dislike => "like/dislike",
            Like::Indifferent => "like/removelike",
        };
        self.call_signed_in(endpoint, json!({ "target": { "videoId": video_id } }), "")?;
        Ok(())
    }

    /// Adds or removes an album/playlist from the library (`playlist_id` of an album is its
    /// `OLAK5uy_…` id).
    pub fn save_playlist(&self, playlist_id: &str, save: bool) -> Result<()> {
        let id = playlist_id.trim_start_matches("VL");
        let endpoint = if save { "like/like" } else { "like/removelike" };
        self.call_signed_in(endpoint, json!({ "target": { "playlistId": id } }), "")?;
        Ok(())
    }

    /// Creates a private playlist; returns its id.
    pub fn create_playlist(
        &self,
        title: &str,
        description: &str,
        video_ids: &[String],
    ) -> Result<String> {
        let mut body =
            json!({ "title": title, "description": description, "privacyStatus": "PRIVATE" });
        if !video_ids.is_empty() {
            body["videoIds"] = json!(video_ids);
        }
        let v = self.call_signed_in("playlist/create", body, "")?;
        v["playlistId"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| status_error(&v))
    }

    pub fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        let id = playlist_id.trim_start_matches("VL");
        self.call_signed_in("playlist/delete", json!({ "playlistId": id }), "")?;
        Ok(())
    }

    /// Appends tracks (duplicates allowed, like the website's "add anyway").
    pub fn add_to_playlist(&self, playlist_id: &str, video_ids: &[String]) -> Result<()> {
        let actions: Vec<Value> = video_ids
            .iter()
            .map(|v| json!({ "action": "ACTION_ADD_VIDEO", "addedVideoId": v, "dedupeOption": "DEDUPE_OPTION_SKIP" }))
            .collect();
        self.edit_playlist(playlist_id, actions)
    }

    /// Removes tracks; they need the `set_video_id` from the playlist's own listing.
    pub fn remove_from_playlist(&self, playlist_id: &str, tracks: &[Track]) -> Result<()> {
        let actions: Vec<Value> = tracks
            .iter()
            .filter_map(|t| {
                Some(json!({
                    "action": "ACTION_REMOVE_VIDEO",
                    "setVideoId": t.set_video_id.as_deref()?,
                    "removedVideoId": t.video_id,
                }))
            })
            .collect();
        if actions.is_empty() {
            return Err(Error::Unavailable(
                "tracks have no playlist position ids".into(),
            ));
        }
        self.edit_playlist(playlist_id, actions)
    }

    pub fn rename_playlist(&self, playlist_id: &str, title: &str) -> Result<()> {
        self.edit_playlist(
            playlist_id,
            vec![json!({ "action": "ACTION_SET_PLAYLIST_NAME", "playlistName": title })],
        )
    }

    fn edit_playlist(&self, playlist_id: &str, actions: Vec<Value>) -> Result<()> {
        let id = playlist_id.trim_start_matches("VL");
        let v = self.call_signed_in(
            "browse/edit_playlist",
            json!({ "playlistId": id, "actions": actions }),
            "",
        )?;
        match v["status"].as_str() {
            Some("STATUS_SUCCEEDED") | None if v["error"].is_null() => Ok(()),
            _ => Err(status_error(&v)),
        }
    }

    pub fn subscribe(&self, channel_id: &str, subscribe: bool) -> Result<()> {
        let endpoint = if subscribe {
            "subscription/subscribe"
        } else {
            "subscription/unsubscribe"
        };
        self.call_signed_in(endpoint, json!({ "channelIds": [channel_id] }), "")?;
        Ok(())
    }

    // ---- history ---------------------------------------------------------------------------

    /// Records a play in the account's YouTube Music history, as the website does when a track
    /// starts. Signed out this does nothing.
    pub fn report_play(&self, video_id: &str) -> Result<()> {
        if !self.signed_in() {
            return Ok(());
        }
        let v = self.call("player", json!({ "videoId": video_id }), "")?;
        let base = parse::playback_url(&v)
            .ok_or_else(|| Error::Unavailable("no playback tracking url".into()))?;
        let sep = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{sep}ver=2&c={CLIENT_NAME}&cpn={}", cpn());
        let session = self.session.read().unwrap().clone();
        let mut req = net::agent()
            .get(&url)
            .header("User-Agent", net::BROWSER_UA)
            .header("Origin", ORIGIN);
        if let Some(s) = &session {
            req = req
                .header("Cookie", s.cookie_header())
                .header("Authorization", s.authorization());
        }
        let resp = req.call().map_err(|e| Error::Net(e.to_string()))?;
        match resp.status().as_u16() {
            200..=299 => Ok(()),
            s => Err(Error::Net(format!("history report: HTTP {s}"))),
        }
    }
}

fn search_body(query: &str, filter: Option<SearchFilter>) -> Value {
    let mut body = json!({ "query": query });
    if let Some(f) = filter {
        body["params"] = json!(match f {
            SearchFilter::Songs => "EgWKAQIIAWoSEAUQCRAKEAMQBBAOEBAQFRAR",
            SearchFilter::Videos => "EgWKAQIQAWoSEAUQCRAKEAMQBBAOEBAQFRAR",
            SearchFilter::Albums => "EgWKAQIYAWoSEAUQCRAKEAMQBBAOEBAQFRAR",
            SearchFilter::Artists => "EgWKAQIgAWoSEAUQCRAKEAMQBBAOEBAQFRAR",
            SearchFilter::Playlists => "EgeKAQQoAEABahIQBRAJEAoQAxAEEA4QEBAVEBE%3D",
        });
    }
    body
}

fn watch_body(video_id: Option<&str>, playlist_id: Option<&str>, params: Option<&str>) -> Value {
    let mut body = json!({ "enablePersistentPlaylistPanel": true, "isAudioOnly": true, "tunerSettingValue": "AUTOMIX_SETTING_NORMAL" });
    if let Some(v) = video_id {
        body["videoId"] = json!(v);
    }
    if let Some(p) = playlist_id {
        body["playlistId"] = json!(p);
    }
    if let Some(p) = params {
        body["params"] = json!(p);
    }
    body
}

fn status_error(v: &Value) -> Error {
    let msg = v["error"]["message"]
        .as_str()
        .or(v["status"].as_str())
        .unwrap_or("unexpected response");
    Error::Unavailable(msg.to_string())
}

/// Percent-encodes a continuation token for a query string.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Client playback nonce: 16 characters of the URL-safe base64 alphabet.
fn cpn() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        ^ (std::process::id() as u64) << 32;
    (0..16)
        .map(|_| {
            // xorshift64*
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            ALPHABET[(x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 58) as usize] as char
        })
        .collect()
}
