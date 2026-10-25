//! Right-click menus: building them and acting on what's chosen.

use crate::*;

impl App {
    // ---- right-click menu ------------------------------------------------------------------

    /// Opens the menu for what was right-clicked at `at` (window coordinates).
    pub(crate) fn context(
        &mut self,
        what: &str,
        row: usize,
        id: String,
        at: slint::LogicalPosition,
    ) {
        let entry = |id: &str, label: &str, icon: &str, danger: bool| MenuEntry {
            id: id.into(),
            label: label.into(),
            icon: icon.into(),
            danger,
        };
        let divider = || entry("-", "", "", false);
        let (target, items) = match what {
            "track" => {
                let Some(i) = self.track_at(row) else {
                    return;
                };
                let t = &self.page_tracks[i];
                let mut items = vec![
                    entry("play", "Play", "play", false),
                    entry("next", "Play next", "next", false),
                    entry("queue", "Add to queue", "queue", false),
                    divider(),
                ];
                items.extend(song_entries(t));
                items.push(divider());
                items.push(like_entry(self.liked(t)));
                items.push(entry("dislike", "Don't recommend this song", "hide", false));
                items.push(divider());
                items.push(entry("song", "Go to song", "song", false));
                let on_album = self.header.as_ref().is_some_and(|h| h.album);
                if !on_album && t.album.as_ref().is_some_and(|a| a.id.is_some()) {
                    items.push(entry("album", "Go to album", "album", false));
                }
                items.extend(artist_entries(&t.artists));
                items.push(entry("link", "Copy link", "link", false));
                if self.header.as_ref().is_some_and(|h| h.editable) && t.set_video_id.is_some() {
                    items.push(divider());
                    items.push(entry("remove", "Remove from this playlist", "close", true));
                }
                self.menu_artists = linked(&t.artists);
                (MenuTarget::Track(i, row), items)
            }
            "card" => {
                if let Some((target, items)) =
                    id.parse().ok().and_then(|j| self.radio_card_menu(row, j))
                {
                    return self.show_menu(target, items, at);
                }
                let Some(item) = id.parse().ok().and_then(|j| self.card_item(row, j)) else {
                    return;
                };
                let mut items = vec![entry("play", "Play", "play", false)];
                if !matches!(item, Item::Track(_)) {
                    items.push(entry("shuffle", "Shuffle", "shuffle", false));
                }
                items.push(entry("next", "Play next", "next", false));
                items.push(entry("queue", "Add to queue", "queue", false));
                if matches!(item, Item::Link(_)) {
                    return;
                }
                let page = self.history[self.at].kind;
                let library = match item {
                    Item::Track(t) => song_entries(t),
                    Item::Album(_) if page == PageKind::Albums => {
                        vec![entry("unsave", "Remove from library", "close", false)]
                    }
                    Item::Album(_) => vec![entry("save", "Add to library", "plus", false)],
                    Item::Playlist(_) if page != PageKind::AllPlaylists => {
                        vec![entry("save", "Add to library", "plus", false)]
                    }
                    _ => Vec::new(),
                };
                if !library.is_empty() {
                    items.push(divider());
                    items.extend(library);
                }
                let has_feedback =
                    matches!(item, Item::Track(_)) || pages::not_interested(item).is_some();
                if has_feedback {
                    items.push(divider());
                }
                if let Item::Track(t) = item {
                    items.push(like_entry(self.liked(t)));
                    items.push(entry("dislike", "Don't recommend this song", "hide", false));
                }
                if pages::not_interested(item).is_some() {
                    items.push(entry("hide", "Hide this suggestion", "hide", false));
                }
                items.push(divider());
                let artists = pages::artists_of(item);
                if matches!(item, Item::Track(_)) {
                    items.push(entry("song", "Go to song", "song", false));
                }
                if matches!(item, Item::Track(t) if t.album.as_ref().is_some_and(|a| a.id.is_some()))
                {
                    items.push(entry("album", "Go to album", "album", false));
                }
                items.extend(artist_entries(&artists));
                items.push(entry("link", "Copy link", "link", false));
                let item = item.clone();
                self.menu_artists = linked(&artists);
                (MenuTarget::Card(Box::new(item)), items)
            }
            "queue" | "np" => {
                if let Some((target, items)) = self.radio_queue_menu(what, row) {
                    if let MenuTarget::Queue(pos) = target {
                        self.menu_queue_id = self.queue_ids.get(pos).cloned();
                    }
                    return self.show_menu(target, items, at);
                }
                // Queue rows start after the playing song: make both a play-order position.
                let row = if what == "np" {
                    self.player.up_next().1
                } else {
                    self.queue_from + row
                };
                let liked = self
                    .queue_ids
                    .get(row)
                    .and_then(|id| self.known.get(id))
                    .is_some_and(|t| self.liked(t));
                let mut items = vec![entry("play", "Play", "play", false)];
                let song = self.queue_ids.get(row).and_then(|id| self.known.get(id));
                let artists = song.map(|t| t.artists.clone()).unwrap_or_default();
                if let Some(t) = song {
                    items.push(divider());
                    items.extend(song_entries(t));
                }
                items.push(divider());
                items.push(like_entry(liked));
                items.push(entry("dislike", "Don't recommend this song", "hide", false));
                items.push(divider());
                if let Some(t) = song {
                    items.push(entry("song", "Go to song", "song", false));
                    if t.album.as_ref().is_some_and(|a| a.id.is_some()) {
                        items.push(entry("album", "Go to album", "album", false));
                    }
                }
                items.extend(artist_entries(&artists));
                if song.is_some() {
                    items.push(entry("link", "Copy link", "link", false));
                }
                self.menu_artists = linked(&artists);
                if row != self.player.up_next().1 {
                    items.push(divider());
                    items.push(entry("remove", "Remove from queue", "close", true));
                }
                self.menu_queue_id = self.queue_ids.get(row).cloned();
                (MenuTarget::Queue(row), items)
            }
            "playlist" => {
                let mut items = vec![entry("open", "Open", "grid", false)];
                let mine = self.playlists.iter().any(|p| p.id == id && self.mine(p));
                if mine {
                    items.push(entry("rename", "Rename", "edit", false));
                }
                items.push(entry("link", "Copy link", "link", false));
                items.push(divider());
                items.push(entry("hide", "Hide from sidebar", "close", true));
                if mine {
                    items.push(entry("delete", "Delete playlist", "close", true));
                }
                (MenuTarget::Playlist(id), items)
            }
            _ => return,
        };
        self.show_menu(target, items, at);
    }

    pub(crate) fn show_menu(
        &mut self,
        target: MenuTarget,
        items: Vec<MenuEntry>,
        at: slint::LogicalPosition,
    ) {
        self.menu = Some(target);
        self.menu_at = at;
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_menu_items(ModelRc::new(VecModel::from(items)));
        g.set_menu_at(at);
        g.set_menu_open(true);
    }

    pub(crate) fn menu_chosen(&mut self, choice: &str) {
        let Some(target) = self.menu.take() else {
            return;
        };
        if choice.starts_with("radio-") {
            return self.radio_menu_chosen(choice, target);
        }
        if let Some(n) = choice.strip_prefix("artist:") {
            if let Some(a) = n
                .parse::<usize>()
                .ok()
                .and_then(|n| self.menu_artists.get(n))
            {
                let r = Route {
                    kind: PageKind::Artist,
                    id: a.id.clone().unwrap_or_default(),
                    title: a.name.clone(),
                };
                self.navigate(r);
            }
            return;
        }
        match target {
            MenuTarget::Track(i, row) => {
                let Some(t) = self.page_tracks.get(i).cloned() else {
                    return;
                };
                match choice {
                    "play" => self.play_page(row as i32),
                    "next" | "queue" => {
                        let req = listen::Req::Add {
                            tracks: vec![t.clone()],
                            next: choice == "next",
                        };
                        if self.ask_host(req) {
                            return;
                        }
                        let id = t.video_id.clone();
                        self.known.insert(id.clone(), crate::edits::applied(t));
                        if choice == "next" {
                            self.player.insert_next(vec![id]);
                        } else {
                            self.player.append(vec![id]);
                        }
                        self.queue_changed();
                        self.show_now_playing(false);
                    }
                    "like" => self.set_like(&t.video_id, Like::Like),
                    "unlike" => self.set_like(&t.video_id, Like::Indifferent),
                    "album" => self.open_album(row),
                    "remove" => self.remove_from_playlist(i),
                    "dislike" => self.dislike(t),
                    "song" => self.open_song(&t),
                    _ => self.song_chosen(choice, t),
                }
            }
            MenuTarget::Card(item) => match choice {
                "play" => self.play_item(*item, Mode::Play),
                "shuffle" => self.play_item(*item, Mode::Shuffle),
                "next" => self.play_item(*item, Mode::Next),
                "queue" => self.play_item(*item, Mode::Queue),
                "dislike" => {
                    if let Item::Track(t) = *item {
                        self.dislike(t);
                    }
                }
                "like" | "unlike" => {
                    if let Item::Track(t) = *item {
                        let like = if choice == "like" {
                            Like::Like
                        } else {
                            Like::Indifferent
                        };
                        self.set_like(&t.video_id, like);
                    }
                }
                "hide" => self.not_interested(*item),
                "save" | "unsave" => self.save_item(*item, choice == "save"),
                "link" => self.copy_link(&item),
                "song" => {
                    if let Item::Track(t) = *item {
                        self.open_song(&t);
                    }
                }
                "album" => {
                    if let Item::Track(t) = *item
                        && let Some(a) = t.album
                        && let Some(id) = a.id
                    {
                        self.navigate(Route {
                            kind: PageKind::Album,
                            id,
                            title: a.name,
                        });
                    }
                }
                _ => {
                    if let Item::Track(t) = *item {
                        self.song_chosen(choice, t);
                    }
                }
            },
            MenuTarget::Artists => {}
            MenuTarget::DeletePlaylist(id) if choice == "delete" => self.delete_playlist(id),
            MenuTarget::DeletePlaylist(_) => {}
            MenuTarget::AddTo(ids) if choice == "new" => self.new_playlist(ids),
            MenuTarget::AddTo(ids) => {
                let Some(p) = choice
                    .strip_prefix("pl:")
                    .and_then(|n| n.parse::<usize>().ok())
                    .and_then(|n| self.playlists.get(n))
                else {
                    return;
                };
                let (playlist, name) = (p.id.clone(), p.title.clone());
                let showing = self.history[self.at].id == playlist;
                self.add_to(playlist, name, ids, showing, false);
            }
            MenuTarget::Queue(pos) => {
                // The queue may have moved on since the menu opened (a song ended, the radio
                // added more): act on the song that was clicked, wherever it is now.
                let Some(id) = self.menu_queue_id.take() else {
                    return;
                };
                let pos = if self.queue_ids.get(pos) == Some(&id) {
                    pos
                } else {
                    let at = self.player.up_next().1;
                    match self
                        .queue_ids
                        .iter()
                        .enumerate()
                        .position(|(i, q)| i > at && *q == id)
                    {
                        Some(p) => p,
                        None => return,
                    }
                };
                self.queue_chosen(pos, choice);
            }
            MenuTarget::Playlist(id) => match choice {
                "open" => {
                    let title = self
                        .playlists
                        .iter()
                        .find(|p| p.id == id)
                        .map(|p| p.title.clone())
                        .unwrap_or_default();
                    self.navigate(Route {
                        kind: PageKind::Playlist,
                        id,
                        title,
                    });
                }
                "hide" => self.hide_playlist(id),
                "delete" => {
                    // YouTube can't bring it back, so ask once more.
                    let title = self
                        .playlists
                        .iter()
                        .find(|p| p.id == id)
                        .map(|p| p.title.clone())
                        .unwrap_or_default();
                    let items = vec![
                        menu_entry(
                            "ask",
                            &format!("Delete \u{201c}{title}\u{201d}? It can't be undone."),
                            "",
                            false,
                        ),
                        menu_entry("delete", "Delete for good", "close", true),
                        menu_entry("cancel", "Cancel", "back", false),
                    ];
                    self.show_menu(MenuTarget::DeletePlaylist(id), items, self.menu_at);
                }
                "rename" => {
                    let title = self
                        .playlists
                        .iter()
                        .find(|p| p.id == id)
                        .map(|p| p.title.clone())
                        .unwrap_or_default();
                    self.navigate(Route {
                        kind: PageKind::Playlist,
                        id,
                        title,
                    });
                    self.ui().global::<Ui>().set_renaming(true);
                }
                "link" => {
                    let p = PlaylistCard {
                        id,
                        ..Default::default()
                    };
                    self.copy_link(&Item::Playlist(p));
                }
                _ => {}
            },
        }
    }

    /// A queue song's menu entry was chosen; `pos` is where it is now.
    pub(crate) fn queue_chosen(&mut self, pos: usize, choice: &str) {
        match choice {
            "play" => {
                if !self.ask_host(listen::Req::Jump(pos)) {
                    self.cancel_skip();
                    self.player.jump(pos);
                }
            }
            "dislike" => {
                if let Some(t) = self.queue_ids.get(pos).and_then(|id| self.known.get(id)) {
                    self.dislike(t.clone());
                }
            }
            "song" | "album" => {
                let t = self
                    .queue_ids
                    .get(pos)
                    .and_then(|id| self.known.get(id))
                    .cloned();
                match (choice, t) {
                    ("song", Some(t)) => self.open_song(&t),
                    (_, Some(t)) => {
                        if let Some(a) = t.album
                            && let Some(id) = a.id
                        {
                            self.navigate(Route {
                                kind: PageKind::Album,
                                id,
                                title: a.name,
                            });
                        }
                    }
                    _ => {}
                }
            }
            "like" | "unlike" => {
                if let Some(id) = self.queue_ids.get(pos).cloned() {
                    let like = if choice == "like" {
                        Like::Like
                    } else {
                        Like::Indifferent
                    };
                    self.set_like(&id, like);
                }
            }
            "remove" => {
                if !self.ask_host(listen::Req::Remove(pos)) {
                    self.player.remove(pos);
                    self.queue_changed();
                }
            }
            _ => {
                if let Some(t) = self.queue_ids.get(pos).and_then(|id| self.known.get(id)) {
                    self.song_chosen(choice, t.clone());
                }
            }
        }
    }

    /// The song entries every song menu shares (track rows, cards, the queue).
    pub(crate) fn song_chosen(&mut self, choice: &str, t: Track) {
        match choice {
            "radio" => self.play_item(Item::Track(t), Mode::Play),
            "link" => self.copy_link(&Item::Track(t)),
            "libadd" | "libremove" => self.save_song(&t, choice == "libadd"),
            "edit" => self.edit_song(t),
            "add-to" => {
                // Your own playlists, else every one.
                let mut mine: Vec<usize> = (0..self.playlists.len())
                    .filter(|&i| self.mine(&self.playlists[i]))
                    .collect();
                if mine.is_empty() {
                    mine = (0..self.playlists.len()).collect();
                }
                let mut items = vec![
                    menu_entry("new", "New playlist", "plus", false),
                    menu_entry("-", "", "", false),
                ];
                items.extend(mine.into_iter().map(|i| MenuEntry {
                    id: format!("pl:{i}").into(),
                    label: self.playlists[i].title.clone().into(),
                    icon: "song".into(),
                    danger: false,
                }));
                self.show_menu(MenuTarget::AddTo(vec![t.video_id]), items, self.menu_at);
            }
            _ => {}
        }
    }

    pub(crate) fn copy_link(&self, item: &Item) {
        if let Some(hwnd) = self.hwnd {
            win::copy_text(hwnd, &pages::link(item));
        }
    }

    /// A click on an artist name: on a page row, the banner (-1) or the now-playing bar (-2).
    /// One artist opens their page; several open a small menu to pick from.
    pub(crate) fn open_artist(&mut self, row: i32, at: slint::LogicalPosition) {
        let artists = match row {
            -1 => self.header.as_ref().map(|h| h.artists.clone()),
            -2 => self
                .player
                .current()
                .and_then(|id| self.known.get(id))
                .map(|t| t.artists.clone()),
            r => self
                .track_at(r.max(0) as usize)
                .map(|i| self.page_tracks[i].artists.clone()),
        };
        let artists = linked(&artists.unwrap_or_default());
        match artists.as_slice() {
            [] => {}
            [a] => {
                let r = Route {
                    kind: PageKind::Artist,
                    id: a.id.clone().unwrap_or_default(),
                    title: a.name.clone(),
                };
                self.navigate(r);
            }
            _ => {
                let items: Vec<MenuEntry> = artists
                    .iter()
                    .enumerate()
                    .map(|(i, a)| MenuEntry {
                        id: format!("artist:{i}").into(),
                        label: a.name.clone().into(),
                        icon: "artist".into(),
                        danger: false,
                    })
                    .collect();
                self.menu = Some(MenuTarget::Artists);
                self.menu_artists = artists;
                let ui = self.ui();
                let g = ui.global::<Ui>();
                g.set_menu_items(ModelRc::new(VecModel::from(items)));
                g.set_menu_at(at);
                g.set_menu_open(true);
            }
        }
    }
}

fn like_entry(liked: bool) -> MenuEntry {
    let (id, label, icon) = if liked {
        ("unlike", "Remove from liked", "heart-filled")
    } else {
        ("like", "Like", "heart")
    };
    MenuEntry {
        id: id.into(),
        label: label.into(),
        icon: icon.into(),
        danger: false,
    }
}

/// Remove from library (when it's in it), Add to playlist and radio entries for a song.
fn song_entries(t: &Track) -> Vec<MenuEntry> {
    let entry = |id: &str, label: &str, icon: &str| MenuEntry {
        id: id.into(),
        label: label.into(),
        icon: icon.into(),
        danger: false,
    };
    let mut v = Vec::new();
    if t.library.as_ref().is_some_and(|l| l.saved) {
        v.push(entry("libremove", "Remove from library", "check"));
    }
    v.push(entry("add-to", "Add to playlist…", "queue"));
    v.push(entry("radio", "Start radio", "radio"));
    v.push(entry("edit", "Edit song info…", "edit"));
    v
}

/// The artists that have a page.
fn linked(artists: &[Named]) -> Vec<Named> {
    artists.iter().filter(|a| a.id.is_some()).cloned().collect()
}

/// "Go to artist", or one "Go to …" per artist when there are several.
fn artist_entries(artists: &[Named]) -> Vec<MenuEntry> {
    let artists = linked(artists);
    let entry = |i: usize, label: String| MenuEntry {
        id: format!("artist:{i}").into(),
        label: label.into(),
        icon: "artist".into(),
        danger: false,
    };
    match artists.as_slice() {
        [_] => vec![entry(0, "Go to artist".into())],
        _ => artists
            .iter()
            .enumerate()
            .map(|(i, a)| entry(i, format!("Go to {}", a.name)))
            .collect(),
    }
}
