//! Your library and account: likes, saves, follows, playlists, signing in and out.

use crate::*;

impl App {
    /// Moves a track in your own playlist from row `from` to before row `to`: here at once,
    /// then on YouTube (reloading the page if that fails).
    pub(crate) fn track_move(&mut self, from: usize, to: usize) {
        self.forget_page();
        if !self.header.as_ref().is_some_and(|h| h.editable) {
            return;
        }
        let off = self.tracks_at;
        let (Some(from), Some(to)) = (from.checked_sub(off), to.checked_sub(off)) else {
            return;
        };
        let n = self.main_len;
        if from >= n || to > n || to == from || to == from + 1 {
            return;
        }
        // Moving to the end needs the whole list here, to know what's last.
        if to == n && self.more.is_some() {
            return;
        }
        let Some(set_id) = self.page_tracks[from].set_video_id.clone() else {
            return;
        };
        let before = self
            .page_tracks
            .get(to)
            .and_then(|t| t.set_video_id.clone());
        let at = if to > from { to - 1 } else { to };
        let t = self.page_tracks.remove(from);
        self.page_tracks.insert(at, t);
        self.art_index.clear();
        let row = self.page_rows.remove(from + off);
        self.page_rows.insert(at + off, row);
        for i in from.min(at)..=from.max(at) {
            if let Some(mut r) = self.page_rows.row_data(i + off) {
                r.number = i as i32 + 1;
                self.page_rows.set_row_data(i + off, r);
            }
        }
        let playlist = self.history[self.at].id.clone();
        let client = self.client.clone();
        let generation = self.generation;
        workers::run_ordered(
            move || client.move_in_playlist(&playlist, &set_id, before.as_deref()),
            move |r| {
                if let Err(e) = r {
                    log::warn!("move: {e}");
                    with(|a| {
                        if a.generation == generation {
                            a.reload();
                        }
                    });
                }
            },
        );
    }

    pub(crate) fn toggle_like(&mut self) {
        // On a station the heart is "favourite station".
        if self.station_like() {
            return;
        }
        let Some(id) = self.player.current().map(str::to_string) else {
            return;
        };
        let Some(t) = self.known.get(&id) else {
            return;
        };
        let like = if t.like == Some(Like::Like) {
            Like::Indifferent
        } else {
            Like::Like
        };
        // The same way as a row's heart, so the rows showing this song follow too.
        self.set_like(&id, like);
    }

    // ---- account ---------------------------------------------------------------------------

    pub(crate) fn sign_in(&mut self) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_signing_in(true);
        let client = self.client.clone();
        // The sign-in window runs its own message loop on its own thread.
        std::thread::spawn(move || {
            let r = signin::sign_in(&webview_dir())
                .map_err(|e| e.to_string())
                .and_then(|c| Session::new(c).map_err(|e| e.to_string()))
                .and_then(|s| {
                    s.save().map_err(|e| e.to_string())?;
                    client.set_session(Some(s));
                    Ok(())
                });
            let _ = slint::invoke_from_event_loop(move || {
                with(|a| {
                    a.ui().global::<Ui>().set_signing_in(false);
                    if let Err(e) = r {
                        log::warn!("sign-in: {e}");
                        a.notify_with("Sign-in didn't finish", "Try again", |a| a.sign_in());
                    }
                    a.account_changed();
                    a.reload();
                });
            });
        });
    }

    pub(crate) fn sign_out(&mut self) {
        Session::delete();
        self.client.set_session(None);
        self.account_changed();
        self.navigate(Route {
            kind: PageKind::Home,
            id: String::new(),
            title: "Home".into(),
        });
    }

    /// Refreshes the account name, avatar and sidebar playlists.
    pub(crate) fn account_changed(&mut self) {
        let signed_in = self.client.signed_in();
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_signed_in(signed_in);
        // Pages seen were someone else's (or nobody's).
        self.page_cache.clear();
        self.account = None;
        self.playlists.clear();
        self.playlist_rows.set_vec(Vec::new());
        g.set_account_name(SharedString::new());
        g.set_account_photo(Image::default());
        if !signed_in {
            return;
        }
        let client = self.client.clone();
        workers::run(
            move || client.account(),
            |r| {
                with(|a| match r {
                    _ if !a.client.signed_in() => {}
                    Ok(acc) => {
                        a.ui()
                            .global::<Ui>()
                            .set_account_name(acc.name.clone().into());
                        a.account = Some(acc);
                        a.account_images();
                    }
                    Err(e) => log::warn!("account: {e}"),
                });
            },
        );
        self.load_playlists();
    }

    /// The sidebar's playlists (again after one is saved or removed).
    pub(crate) fn load_playlists(&mut self) {
        if !self.client.signed_in() {
            return;
        }
        self.playlists_seq += 1;
        let seq = self.playlists_seq;
        let client = self.client.clone();
        // After the changes asked for before it, so it sees them.
        workers::run_ordered(
            move || client.library_playlists(),
            move |r| {
                with(|a| match r {
                    // Signed out meanwhile (they aren't yours to show any more), or asked for
                    // again since: that answer is the one to show.
                    _ if !a.client.signed_in() || seq != a.playlists_seq => {}
                    Ok(f) => {
                        let all: Vec<PlaylistCard> = f
                            .shelves
                            .into_iter()
                            .flat_map(|s| s.items)
                            .filter_map(|i| match i {
                                Item::Playlist(p) if p.id != "VLLM" => Some(p),
                                _ => None,
                            })
                            .collect();
                        // What YouTube now agrees with needn't be remembered.
                        a.library_wish
                            .retain(|(id, wish)| all.iter().any(|p| p.id == *id) != wish.is_some());
                        a.set_playlists(all);
                    }
                    Err(e) => log::warn!("playlists: {e}"),
                });
            },
        );
    }

    /// Shows your playlists in the sidebar: what you just saved or removed as such, hidden ones
    /// apart, in the order you put them in.
    pub(crate) fn set_playlists(&mut self, mut all: Vec<PlaylistCard>) {
        for (id, wish) in &self.library_wish {
            match wish {
                Some(card) if !all.iter().any(|p| p.id == *id) => all.insert(0, card.clone()),
                None => all.retain(|p| p.id != *id),
                _ => {}
            }
        }
        let order = &self.settings.playlist_order;
        // Stable: ones not in the order (new since) keep YouTube's order, first.
        all.sort_by_key(|p| order.iter().position(|o| *o == p.id).map_or(0, |i| i + 1));
        let (hidden, shown): (Vec<_>, Vec<_>) = all
            .into_iter()
            .partition(|p| self.settings.hidden_playlists.contains(&p.id));
        self.playlists = shown;
        self.hidden_lists = hidden;
        self.show_hidden_lists();
        let rows: Vec<SidebarPlaylist> = self
            .playlists
            .iter()
            .map(|p| SidebarPlaylist {
                id: p.id.clone().into(),
                title: p.title.clone().into(),
                cover: Image::default(),
            })
            .collect();
        self.playlist_rows.set_vec(rows);
        for i in 0..self.playlists.len() {
            self.sidebar_cover(i);
        }
    }

    /// A sidebar playlist was dragged from `from` to before `to`.
    pub(crate) fn move_playlist(&mut self, from: usize, to: usize) {
        if from >= self.playlists.len() || to > self.playlists.len() || to == from || to == from + 1
        {
            return;
        }
        let p = self.playlists.remove(from);
        self.playlists
            .insert(if to > from { to - 1 } else { to }, p);
        self.settings.playlist_order = self
            .playlists
            .iter()
            .chain(&self.hidden_lists)
            .map(|p| p.id.clone())
            .collect();
        self.save_later();
        let all = self
            .playlists
            .iter()
            .chain(&self.hidden_lists)
            .cloned()
            .collect();
        self.set_playlists(all);
    }

    /// Whether a song is liked, by what we last heard or did.
    pub(crate) fn liked(&self, t: &Track) -> bool {
        self.known.get(&t.video_id).and_then(|k| k.like).or(t.like) == Some(Like::Like)
    }

    /// Likes a song or takes the like away: here everywhere it shows, then on YouTube (and back
    /// here if YouTube refuses).
    pub(crate) fn set_like(&mut self, id: &str, like: Like) {
        if self.needs_account("like songs") {
            return;
        }
        let before = self
            .known
            .get(id)
            .and_then(|k| k.like)
            .or_else(|| {
                self.page_tracks
                    .iter()
                    .find(|t| t.video_id == id)
                    .and_then(|t| t.like)
            })
            .unwrap_or(Like::Indifferent);
        self.mark_like(id, like);
        let (client, id) = (self.client.clone(), id.to_string());
        workers::run_ordered(
            {
                let id = id.clone();
                move || client.rate(&id, like)
            },
            move |r| {
                with(|a| {
                    if let Err(e) = r {
                        a.mark_like(&id, before);
                        let what = if like == Like::Like {
                            "like this song"
                        } else {
                            "remove the like"
                        };
                        a.account_failed(what, &e, Some(Box::new(move |a| a.set_like(&id, like))));
                    }
                });
            },
        );
    }

    /// Shows a song liked (or not) everywhere it is here.
    pub(crate) fn mark_like(&mut self, id: &str, like: Like) {
        self.forget_pages(PageKind::Liked);
        for t in self.page_tracks.iter_mut().filter(|t| t.video_id == id) {
            t.like = Some(like);
        }
        for r in self.rows.iter_mut() {
            if let RowRef::Shelf(items, _) = r {
                for i in items.iter_mut() {
                    if let Item::Track(t) = i
                        && t.video_id == id
                    {
                        t.like = Some(like);
                    }
                }
            }
        }
        if let Some(k) = self.known.get_mut(id) {
            k.like = Some(like);
        }
        if self.player.current() == Some(id) {
            let ui = self.ui();
            let g = ui.global::<Ui>();
            let mut np = g.get_np();
            np.liked = like == Like::Like;
            g.set_np(np);
        }
    }

    /// Account actions need an account: without one, a toast offers to sign in.
    pub(crate) fn needs_account(&mut self, to: &str) -> bool {
        if self.client.signed_in() {
            return false;
        }
        self.notify_with(format!("Sign in to {to}"), "Sign in", |a| a.sign_in());
        true
    }

    /// An account change YouTube refused (already undone here): says so, with Retry when
    /// `retry` can do it again.
    pub(crate) fn account_failed(
        &mut self,
        what: &str,
        e: &music_core::Error,
        retry: Option<toast::Then>,
    ) {
        log::warn!("{what}: {e}");
        if self.session_check() {
            return;
        }
        let msg = match e {
            music_core::Error::Net(_) => {
                self.went_offline();
                format!("Couldn't {what}: check your connection")
            }
            _ => format!("Couldn't {what}"),
        };
        match retry {
            Some(retry) => self.notify_with(msg, "Retry", retry),
            None => self.notify(msg),
        }
    }

    /// Adds songs to a playlist. Without `force`, ones it already has are left out and a toast
    /// offers "Add anyway".
    pub(crate) fn add_to(
        &mut self,
        playlist: String,
        name: String,
        ids: Vec<String>,
        showing: bool,
        force: bool,
    ) {
        self.page_cache
            .remove(&format!("{:?}|{}", PageKind::Playlist, playlist));
        let client = self.client.clone();
        let generation = self.generation;
        workers::run_ordered(
            {
                let (playlist, ids) = (playlist.clone(), ids.clone());
                move || client.add_to_playlist_checked(&playlist, &ids, force)
            },
            move |r| {
                with(|a| match r {
                    Ok(added) => {
                        // Showing that playlist: show it with the new songs.
                        if showing && a.generation == generation {
                            a.reload();
                        }
                        let skipped = added.skipped;
                        if skipped.is_empty() {
                            a.notify(format!("Added to {name}"));
                            return;
                        }
                        let msg = match (ids.len(), skipped.len()) {
                            (1, _) => format!("Already in {name}"),
                            (n, k) if n == k => format!("All {n} are already in {name}"),
                            (n, k) => format!("Added {} to {name}; {k} were already there", n - k),
                        };
                        a.notify_with(msg, "Add anyway", move |a| {
                            a.add_to(playlist, name, skipped, showing, true)
                        });
                    }
                    Err(e) => a.account_failed(&format!("add to {name}"), &e, None),
                });
            },
        );
    }

    /// YouTube has signed this session out (cookies expired or revoked): say so once, with a
    /// way to sign in again. True when it has.
    pub(crate) fn session_check(&mut self) -> bool {
        if !self.client.session_expired() {
            self.told_expired = false;
            return false;
        }
        if !self.told_expired {
            self.told_expired = true;
            self.notify_with("YouTube signed you out", "Sign in", |a| a.sign_in());
        }
        true
    }

    /// One of the sidebar's playlists that you made (its byline is your name).
    pub(crate) fn mine(&self, p: &PlaylistCard) -> bool {
        self.account
            .as_ref()
            .is_some_and(|a| !a.name.is_empty() && p.subtitle.starts_with(&a.name))
    }

    /// Renames the playlist on screen: here at once, then on YouTube, then the sidebar.
    pub(crate) fn rename_playlist(&mut self, name: String) {
        self.forget_page();
        self.forget_pages(PageKind::AllPlaylists);
        let r = &mut self.history[self.at];
        if r.kind != PageKind::Playlist || !self.header.as_ref().is_some_and(|h| h.editable) {
            return;
        }
        r.title = name.clone();
        let id = r.id.clone();
        if let Some(h) = &mut self.header {
            h.title = name.clone();
        }
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut c = g.get_collection();
        c.title = name.clone().into();
        g.set_collection(c);
        let client = self.client.clone();
        let generation = self.generation;
        workers::run_ordered(
            move || client.rename_playlist(&id, &name),
            move |r| {
                with(|a| {
                    a.load_playlists();
                    if let Err(e) = r {
                        // The old name back, from YouTube.
                        if a.generation == generation {
                            a.reload();
                        }
                        a.account_failed("rename the playlist", &e, None);
                    }
                });
            },
        );
    }

    /// Saves a song to the library or takes it out: everywhere it shows, then on YouTube.
    pub(crate) fn save_song(&mut self, t: &Track, save: bool) {
        if self.needs_account("save songs") {
            return;
        }
        let Some(lib) = &t.library else { return };
        let token = if save {
            lib.add.clone()
        } else {
            lib.remove.clone()
        };
        self.mark_saved(&t.video_id, save);
        let (client, id, t) = (self.client.clone(), t.video_id.clone(), t.clone());
        workers::run_ordered(
            move || client.feedback(&token),
            move |r| {
                with(|a| {
                    if let Err(e) = r {
                        a.mark_saved(&id, !save);
                        let what = if save {
                            "add the song to your library"
                        } else {
                            "remove the song from your library"
                        };
                        a.account_failed(what, &e, Some(Box::new(move |a| a.save_song(&t, save))));
                    }
                });
            },
        );
    }

    /// Shows a song saved to the library (or not) everywhere it is here.
    pub(crate) fn mark_saved(&mut self, id: &str, save: bool) {
        let mark = |x: &mut Track| {
            if x.video_id == id
                && let Some(l) = &mut x.library
            {
                l.saved = save;
            }
        };
        self.page_tracks.iter_mut().for_each(mark);
        self.known.values_mut().for_each(mark);
        for r in self.rows.iter_mut() {
            if let RowRef::Shelf(items, _) = r {
                for i in items.iter_mut() {
                    if let Item::Track(x) = i {
                        mark(x);
                    }
                }
            }
        }
    }

    /// Adds an album or playlist to the library, or takes it out (off the Albums wall too).
    pub(crate) fn save_item(&mut self, item: Item, save: bool) {
        if self.needs_account("save to your library") {
            return;
        }
        self.forget_pages(PageKind::Albums);
        self.forget_pages(PageKind::AllPlaylists);
        if !save {
            let id = pages::item_id(&item).to_string();
            self.drop_from_page(&|i| pages::item_id(i) == id);
        }
        let client = self.client.clone();
        let (again, generation) = (item.clone(), self.generation);
        workers::run_ordered(
            move || -> music_core::Result<()> {
                let id = match &item {
                    Item::Album(a) => client.album(&a.id)?.playlist_id.unwrap_or_default(),
                    Item::Playlist(p) => p.id.clone(),
                    _ => return Ok(()),
                };
                client.save_playlist(&id, save)
            },
            move |r| {
                with(|a| {
                    a.load_playlists();
                    if let Err(e) = r {
                        // A card taken off the page comes back.
                        if !save && a.generation == generation {
                            a.reload();
                        }
                        let what = if save {
                            "add it to your library"
                        } else {
                            "remove it from your library"
                        };
                        a.account_failed(
                            what,
                            &e,
                            Some(Box::new(move |a| a.save_item(again, save))),
                        );
                    }
                });
            },
        );
    }

    /// The banner's Add to library.
    pub(crate) fn toggle_saved(&mut self) {
        if self.needs_account("save to your library") {
            return;
        }
        self.forget_page();
        self.forget_pages(PageKind::Albums);
        self.forget_pages(PageKind::AllPlaylists);
        let Some((id, saved)) = self.header.as_mut().and_then(|h| h.save.as_mut()) else {
            return;
        };
        *saved = !*saved;
        let (id, save) = (id.clone(), *saved);
        // A playlist: in the sidebar (or out of it) at once, not when YouTube's list catches up.
        let r = &self.history[self.at];
        if r.kind == PageKind::Playlist
            && let Some(h) = &self.header
        {
            let card = save.then(|| PlaylistCard {
                id: r.id.clone(),
                title: h.title.clone(),
                thumbs: h.thumbs.clone(),
                ..Default::default()
            });
            let rid = r.id.clone();
            self.library_wish.retain(|(w, _)| *w != rid);
            self.library_wish.push((rid, card));
            let all = self
                .playlists
                .iter()
                .chain(&self.hidden_lists)
                .cloned()
                .collect();
            self.set_playlists(all);
        }
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut c = g.get_collection();
        c.saved = save;
        g.set_collection(c);
        let client = self.client.clone();
        let (page_id, generation) = (self.history[self.at].id.clone(), self.generation);
        workers::run_ordered(
            move || client.save_playlist(&id, save),
            move |r| {
                with(|a| {
                    if let Err(e) = &r {
                        // Back as it was: the sidebar, and the banner if it's still showing.
                        a.library_wish.retain(|(w, _)| *w != page_id);
                        if a.generation == generation {
                            if let Some((_, saved)) =
                                a.header.as_mut().and_then(|h| h.save.as_mut())
                            {
                                *saved = !save;
                            }
                            let ui = a.ui();
                            let g = ui.global::<Ui>();
                            let mut c = g.get_collection();
                            c.saved = !save;
                            g.set_collection(c);
                        }
                        let what = if save {
                            "add it to your library"
                        } else {
                            "remove it from your library"
                        };
                        a.account_failed(what, e, None);
                    }
                    a.load_playlists();
                });
            },
        );
    }

    /// The banner's Follow.
    pub(crate) fn toggle_follow(&mut self) {
        if self.needs_account("follow artists") {
            return;
        }
        self.forget_page();
        self.forget_pages(PageKind::Artists);
        let Some((channel, on)) = self.header.as_mut().and_then(|h| h.follow.as_mut()) else {
            return;
        };
        *on = !*on;
        let (channel, follow) = (channel.clone(), *on);
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut c = g.get_collection();
        c.following = follow;
        g.set_collection(c);
        let client = self.client.clone();
        let generation = self.generation;
        workers::run_ordered(
            move || client.subscribe(&channel, follow),
            move |r| {
                with(|a| {
                    if let Err(e) = r {
                        if a.generation == generation {
                            if let Some((_, on)) = a.header.as_mut().and_then(|h| h.follow.as_mut())
                            {
                                *on = !follow;
                            }
                            let ui = a.ui();
                            let g = ui.global::<Ui>();
                            let mut c = g.get_collection();
                            c.following = !follow;
                            g.set_collection(c);
                        }
                        let what = if follow { "follow" } else { "unfollow" };
                        a.account_failed(what, &e, None);
                    }
                });
            },
        );
    }

    /// "Don't recommend this song": off this page's recommendations and the queue at once,
    /// skipping it if it's playing (the page's own list, a playlist or an album, keeps it); told
    /// to YouTube when its toast goes, so Undo can still take it back.
    pub(crate) fn dislike(&mut self, t: Track) {
        if self.needs_account("tune your recommendations") {
            return;
        }
        let id = t.video_id.clone();
        let before = self.known.get(&id).and_then(|k| k.like);
        if let Some(k) = self.known.get_mut(&id) {
            k.like = Some(Like::Dislike);
        }
        for p in self.page_tracks.iter_mut().filter(|p| p.video_id == id) {
            p.like = Some(Like::Dislike);
        }
        self.drop_from_page(&|i| matches!(i, Item::Track(x) if x.video_id == id));
        // Out of the queue: skipped if it's playing, else taken out (listening along: by the
        // host, who is asked to skip it if it plays).
        if self.listening() {
            if self.player.current() == Some(id.as_str()) {
                self.ask_host(listen::Req::Next);
            }
        } else if self.player.current() == Some(id.as_str()) {
            self.player.next();
        }
        let at = self.player.up_next().1;
        let gone: Vec<usize> = self
            .queue_ids
            .iter()
            .enumerate()
            .filter(|(i, q)| **q == id && *i != at)
            .map(|(i, _)| i)
            .collect();
        for pos in gone.into_iter().rev() {
            if !self.ask_host(listen::Req::Remove(pos)) {
                self.player.remove(pos);
            }
        }
        self.queue_changed();
        let generation = self.generation;
        let title = t.title.clone();
        self.notify_undoable(
            format!("Won't recommend '{title}'"),
            move |a| {
                if let Some(k) = a.known.get_mut(&t.video_id) {
                    k.like = before;
                }
                for p in a
                    .page_tracks
                    .iter_mut()
                    .filter(|p| p.video_id == t.video_id)
                {
                    p.like = before;
                }
                // YouTube wasn't told, so the page fetched again has it back.
                if a.generation == generation {
                    a.reload();
                }
            },
            move |a| {
                let client = a.client.clone();
                workers::run_ordered(
                    move || client.rate(&id, Like::Dislike),
                    |r| {
                        if let Err(e) = r {
                            with(|a| {
                                a.account_failed("tell YouTube not to recommend it", &e, None)
                            });
                        }
                    },
                );
            },
        );
    }

    /// "Hide this suggestion" (YouTube's "Not interested"): off the page at once, told to
    /// YouTube when its toast goes (YouTube has no undo for it), so Undo can still take it back.
    pub(crate) fn not_interested(&mut self, item: Item) {
        let Some(token) = pages::not_interested(&item).map(str::to_string) else {
            return;
        };
        let id = pages::item_id(&item).to_string();
        self.drop_from_page(&|i| pages::item_id(i) == id);
        let generation = self.generation;
        self.notify_undoable(
            "Suggestion hidden",
            move |a| {
                if a.generation == generation {
                    a.reload();
                }
            },
            move |a| {
                let client = a.client.clone();
                workers::run_ordered(
                    move || client.feedback(&token),
                    |r| {
                        if let Err(e) = r {
                            with(|a| a.account_failed("hide the suggestion", &e, None));
                        }
                    },
                );
            },
        );
    }

    /// Takes a track out of your own playlist: here at once, then on YouTube (reloading the
    /// page if that fails).
    pub(crate) fn remove_from_playlist(&mut self, i: usize) {
        self.forget_page();
        if i >= self.main_len {
            return;
        }
        let at = self.tracks_at + i;
        let t = self.page_tracks.remove(i);
        self.page_groups.remove(i);
        self.rows.remove(at);
        self.art_index.clear();
        self.page_rows.remove(at);
        self.main_len -= 1;
        for r in self.rows.iter_mut() {
            if let RowRef::Track(j) = r
                && *j > i
            {
                *j -= 1;
            }
        }
        for j in i..self.main_len {
            if let Some(mut r) = self.page_rows.row_data(self.tracks_at + j) {
                r.number = j as i32 + 1;
                self.page_rows.set_row_data(self.tracks_at + j, r);
            }
        }
        // The footer's totals.
        let footer = self.tracks_at + self.main_len;
        if matches!(self.rows.get(footer), Some(RowRef::Footer))
            && let Some(mut r) = self.page_rows.row_data(footer)
        {
            r.title = summary_line(&self.page_tracks[..self.main_len]).into();
            self.page_rows.set_row_data(footer, r);
        }
        let playlist = self.history[self.at].id.clone();
        let client = self.client.clone();
        let generation = self.generation;
        workers::run_ordered(
            move || client.remove_from_playlist(&playlist, &[t]),
            move |r| {
                if let Err(e) = r {
                    with(|a| {
                        if a.generation == generation {
                            a.reload();
                        }
                        a.account_failed("remove the song from the playlist", &e, None);
                    });
                }
            },
        );
    }

    /// Asks for a new playlist's name in the sidebar (it gets these songs, if any).
    pub(crate) fn new_playlist(&mut self, ids: Vec<String>) {
        self.naming_ids = ids;
        self.ui().global::<Ui>().set_naming_playlist(true);
    }

    /// The name typed for a new playlist ("" when cancelled): makes it, and it shows up in the
    /// sidebar.
    pub(crate) fn create_playlist(&mut self, name: String) {
        self.forget_pages(PageKind::AllPlaylists);
        self.ui().global::<Ui>().set_naming_playlist(false);
        let ids = std::mem::take(&mut self.naming_ids);
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let client = self.client.clone();
        let made = name.clone();
        workers::run_ordered(
            move || client.create_playlist(&name, "", &ids),
            move |r| {
                with(|a| match r {
                    Ok(_) => {
                        a.load_playlists();
                        a.notify(format!("Created '{made}'"));
                    }
                    Err(e) => a.account_failed("create the playlist", &e, None),
                });
            },
        );
    }

    /// Deletes one of your playlists on YouTube, then everywhere here.
    pub(crate) fn delete_playlist(&mut self, id: String) {
        self.forget_pages(PageKind::AllPlaylists);
        let client = self.client.clone();
        let gone = id.clone();
        workers::run_ordered(
            move || client.delete_playlist(&gone),
            move |r| {
                with(|a| match r {
                    Ok(()) => {
                        a.hide_playlist(id.clone());
                        // Not a hidden one: it's gone, so nothing to remember.
                        a.settings.hidden_playlists.retain(|h| *h != id);
                        a.settings.save();
                        if a.history[a.at].id == id {
                            a.navigate(Route {
                                kind: PageKind::AllPlaylists,
                                id: String::new(),
                                title: "All playlists".into(),
                            });
                        }
                    }
                    Err(e) => a.account_failed("delete the playlist", &e, None),
                });
            },
        );
    }

    pub(crate) fn hide_playlist(&mut self, id: String) {
        self.forget_pages(PageKind::AllPlaylists);
        if let Some(i) = self.playlists.iter().position(|p| p.id == id) {
            let p = self.playlists.remove(i);
            self.playlist_rows.remove(i);
            self.hidden_lists.push(p);
        }
        // Off All playlists too.
        let before = self.grid.len();
        self.grid
            .retain(|i| !matches!(i, Item::Playlist(p) if p.id == id));
        if self.grid.len() != before {
            self.grid_laid = (0, 0);
            self.layout_grid();
        }
        if !self.settings.hidden_playlists.contains(&id) {
            self.settings.hidden_playlists.push(id);
            self.settings.save();
        }
        self.show_hidden_lists();
    }

    /// Settings' list of hidden playlists (covers the sidebar already loaded, if any).
    pub(crate) fn show_hidden_lists(&self) {
        let px = self.px(SIDEBAR_ART);
        let rows: Vec<SidebarPlaylist> = self
            .hidden_lists
            .iter()
            .map(|p| SidebarPlaylist {
                id: p.id.clone().into(),
                title: p.title.clone().into(),
                cover: images::url_for(&p.thumbs, px)
                    .and_then(|u| self.images.peek(&u))
                    .unwrap_or_default(),
            })
            .collect();
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_hidden_count(rows.len() as i32);
        g.set_hidden_playlists(Rc::new(VecModel::from(rows)).into());
    }

    pub(crate) fn account_images(&mut self) {
        let Some(acc) = &self.account else { return };
        let px = self.px(ACCOUNT_ART);
        if let Some(url) = images::url_for(&acc.photo, px)
            && let Some(img) = self.images.get(&url, px, image_loaded)
        {
            self.ui().global::<Ui>().set_account_photo(img);
        }
    }

    pub(crate) fn sidebar_cover(&mut self, i: usize) {
        let px = self.px(SIDEBAR_ART);
        let Some(url) = self
            .playlists
            .get(i)
            .and_then(|p| images::url_for(&p.thumbs, px))
        else {
            return;
        };
        if let Some(img) = self.images.get(&url, px, image_loaded)
            && let Some(mut r) = self.playlist_rows.row_data(i)
        {
            r.cover = img;
            self.playlist_rows.set_row_data(i, r);
        }
    }
}
