//! Pages: history, loading and showing a page, its rows and cards, artwork and play counts.

use crate::*;

impl App {
    // ---- navigation ------------------------------------------------------------------------

    pub(crate) fn navigate(&mut self, r: Route) {
        self.ui().global::<Ui>().set_renaming(false);
        if self.history.get(self.at) == Some(&r) {
            return self.reload();
        }
        if !self.history.is_empty() {
            self.history.truncate(self.at + 1);
            self.scrolls.retain(|&k, _| k < self.at);
            self.scrolls.insert(self.at, self.page_rows.scroll());
        }
        self.scroll_wanted = None;
        self.history.push(r);
        self.at = self.history.len() - 1;
        self.load_page();
    }

    /// The page again, fetched afresh (after a change to it, or asked for again).
    pub(crate) fn reload(&mut self) {
        let key = page_key(&self.history[self.at]);
        self.page_cache.remove(&key);
        self.load_page();
    }

    /// The kept copy of the page shown is out of date (it was changed here).
    pub(crate) fn forget_page(&mut self) {
        let key = page_key(&self.history[self.at]);
        self.page_cache.remove(&key);
    }

    /// Kept pages of `kind` are out of date (a change made from elsewhere).
    pub(crate) fn forget_pages(&mut self, kind: PageKind) {
        let prefix = format!("{kind:?}|");
        self.page_cache.retain(|k, _| !k.starts_with(&prefix));
    }

    pub(crate) fn go(&mut self, d: i32) {
        let to = self.at as i32 + d;
        if to >= 0 && (to as usize) < self.history.len() {
            self.scrolls.insert(self.at, self.page_rows.scroll());
            self.at = to as usize;
            self.scroll_wanted = self.scrolls.get(&self.at).copied();
            self.load_page();
        }
    }

    pub(crate) fn load_page(&mut self) {
        self.trim_caches();
        let r = self.history[self.at].clone();
        self.generation += 1;
        let generation = self.generation;
        PAGE.store(generation, Ordering::Relaxed);
        // A menu holds rows of the page it was opened on.
        self.menu = None;
        self.ui().global::<Ui>().set_menu_open(false);
        let from = self.opened_from.take();
        let scroll = self.scroll_wanted.take();
        self.clear_rows();
        self.ui().global::<Ui>().set_page_scroll(0.0);
        g_serial_bump(&self.ui());
        self.video_sync();
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_collection(Collection::default());
        g.set_can_back(self.at > 0);
        g.set_can_forward(self.at + 1 < self.history.len());

        let library = matches!(
            r.kind,
            PageKind::History
                | PageKind::Songs
                | PageKind::Liked
                | PageKind::Artists
                | PageKind::Albums
                | PageKind::AllPlaylists
        );
        let needs_sign_in = library && !self.client.signed_in();
        if r.kind == PageKind::Settings {
            self.settings_opened();
        }
        let fetches = !needs_sign_in && r.kind != PageKind::Settings;
        g.set_page(PageState {
            kind: r.kind,
            id: r.id.clone().into(),
            title: r.title.clone().into(),
            subtitle: SharedString::new(),
            loading: fetches,
            error: if needs_sign_in {
                "Sign in to see your library.".into()
            } else {
                SharedString::new()
            },
            playable: false,
        });
        if !fetches {
            return;
        }
        // Seen lately: as it was, at once, while it's fetched again. Else its shape, with
        // what's already known (the banner of what was clicked), until it comes.
        let key = page_key(&r);
        let cached = self
            .page_cache
            .get(&key)
            .filter(|(_, at)| at.elapsed() < PAGE_KEPT)
            .map(|(l, _)| l.clone());
        let again = cached.is_some();
        match cached {
            Some(l) => {
                self.page_loaded(generation, Ok(l));
                // The new page's first layout reads it (see PageView's `changed serial`).
                if let Some(y) = scroll {
                    self.ui().global::<Ui>().set_page_scroll(y);
                    self.page_rows.scrolled(y);
                }
            }
            None => {
                self.show_skeleton(&r, from);
                self.scroll_wanted = scroll;
            }
        }
        let client = self.client.clone();
        let hidden = self.settings.hidden_playlists.clone();
        workers::run_urgent(
            move || {
                let t = std::time::Instant::now();
                let res = if r.kind == PageKind::Radio {
                    radio::load(&r)
                } else {
                    pages::load(&client, &r, &hidden)
                };
                log::info!("page {:?} in {} ms", r.kind, t.elapsed().as_millis());
                res
            },
            move |res| {
                with(|a| a.page_arrived(generation, key, res, again));
            },
        );
    }

    /// Forgets what the page list shows (not where it's scrolled to).
    pub(crate) fn clear_rows(&mut self) {
        self.page_tracks.clear();
        self.page_groups.clear();
        self.rows.clear();
        self.art_index.clear();
        self.page_rows.set_vec(Vec::new());
        self.page_rows.set_playable(false);
        self.tracks_at = 0;
        self.main_len = 0;
        self.grid.clear();
        self.grid_laid = (0, 0);
        self.more = None;
        self.loading_more = false;
        self.summary = false;
        self.header = None;
        self.logo_url = None;
        self.extended = false;
    }

    /// Song details pile up over a long session: past 5,000, keep only what the queue needs.
    pub(crate) fn trim_caches(&mut self) {
        const MOST: usize = 5000;
        if self.known.len() > MOST {
            let keep: std::collections::HashSet<&str> =
                self.player.queue().iter().map(String::as_str).collect();
            self.known.retain(|id, _| keep.contains(id.as_str()));
        }
        if self.lengths.len() > MOST {
            self.lengths.clear();
        }
        if self.plays_asked.len() > MOST {
            self.plays_asked.clear();
        }
    }

    /// A page was fetched: kept for next time, and shown, unless what's shown is it already
    /// (`again`: it was shown from what was kept).
    pub(crate) fn page_arrived(
        &mut self,
        generation: u64,
        key: String,
        res: music_core::Result<Loaded>,
        again: bool,
    ) {
        let before = self.page_cache.get(&key).map(|(l, _)| pages::signature(l));
        if let Ok(l) = &res {
            self.page_cache
                .insert(key, (l.clone(), std::time::Instant::now()));
            if self.page_cache.len() > PAGES_KEPT {
                let oldest = self
                    .page_cache
                    .iter()
                    .min_by_key(|(_, (_, at))| *at)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    self.page_cache.remove(&k);
                }
            }
        }
        if generation != self.generation {
            return;
        }
        if again {
            let Ok(l) = &res else {
                return;
            };
            // Shown again where it's scrolled to, not from the top under a view still down the
            // page (R4).
            let keep = self.page_rows.scroll();
            // Feeds differ every time they're asked for: what's shown stays until next time.
            // Your own pages are shown as they are now, where you are on them.
            let feed = matches!(
                self.history[self.at].kind,
                PageKind::Home
                    | PageKind::Explore
                    | PageKind::Feed
                    | PageKind::Search
                    | PageKind::Radio
                    | PageKind::Song
            );
            if feed || self.extended || before == Some(pages::signature(l)) {
                return;
            }
            self.show_arrived(generation, res);
            self.page_rows.scrolled(keep);
            return;
        }
        let ok = res.is_ok();
        self.show_arrived(generation, res);
        // Back/Forward to a page that had to be fetched: where it was left, now its rows are in.
        if let Some(y) = self.scroll_wanted.take().filter(|_| ok) {
            let ui = self.ui();
            let g = ui.global::<Ui>();
            g.set_page_scroll(y);
            g.set_scroll_serial(g.get_scroll_serial().wrapping_add(1));
            self.page_rows.scrolled(y);
        }
    }

    pub(crate) fn show_arrived(&mut self, generation: u64, res: music_core::Result<Loaded>) {
        self.clear_rows();
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut page = g.get_page();
        page.subtitle = SharedString::new();
        page.error = SharedString::new();
        g.set_page(page);
        self.page_loaded(generation, res);
    }

    /// Placeholder rows shaped like the page that's coming, under its real banner or title.
    pub(crate) fn show_skeleton(&mut self, r: &Route, from: Option<Item>) {
        use PageKind as K;
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let tracks = matches!(
            r.kind,
            K::Album | K::Playlist | K::Liked | K::Songs | K::History
        );
        let grid = matches!(r.kind, K::Albums | K::Artists | K::AllPlaylists);
        if matches!(
            r.kind,
            K::Album | K::Playlist | K::Liked | K::Artist | K::Song
        ) {
            // A playlist opened from the sidebar is one of yours, known already.
            let from = from.or_else(|| {
                (r.kind == K::Playlist)
                    .then(|| {
                        self.playlists
                            .iter()
                            .chain(&self.hidden_lists)
                            .find(|p| p.id == r.id)
                    })
                    .flatten()
                    .cloned()
                    .map(Item::Playlist)
            });
            let (title, artist, thumbs) = match from {
                Some(Item::Album(a)) => (a.title, pages::names(&a.artists), a.thumbs),
                // An artist's square picture isn't their banner.
                Some(Item::Artist(a)) => (a.name, String::new(), Vec::new()),
                Some(Item::Playlist(p)) => (p.title, String::new(), p.thumbs),
                _ => (r.title.clone(), String::new(), Vec::new()),
            };
            let h = pages::Header {
                label: if matches!(r.kind, K::Playlist | K::Liked) {
                    "Playlist".into()
                } else {
                    String::new()
                },
                title,
                artist,
                artists: Vec::new(),
                thumbs,
                album: r.kind == K::Album,
                editable: false,
                wide: r.kind == K::Artist,
                channel: None,
                save: None,
                follow: None,
                play: None,
            };
            g.set_collection(Collection {
                label: h.label.clone().into(),
                title: h.title.clone().into(),
                artist: h.artist.clone().into(),
                album: h.album,
                wide: h.wide,
                ..Default::default()
            });
            self.header = Some(h);
            self.push_row(RowRef::Header, RowKind::Header, TrackRow::default());
            self.header_images();
        } else if r.kind != K::Home {
            if tracks {
                // Its Play and Shuffle come with the tracks: room for them already.
                let mut page = g.get_page();
                page.playable = true;
                g.set_page(page);
                self.page_rows.set_playable(true);
            }
            self.push_row(RowRef::Title, RowKind::Title, TrackRow::default());
        }
        let (style, n) = if tracks {
            self.push_row(RowRef::Columns, RowKind::Columns, TrackRow::default());
            (0, 14)
        } else if grid {
            (2, 4)
        } else {
            (1, 4)
        };
        for _ in 0..n {
            let row = TrackRow {
                number: style,
                ..Default::default()
            };
            self.push_row(RowRef::Skeleton, RowKind::Skeleton, row);
        }
    }

    pub(crate) fn page_loaded(&mut self, generation: u64, res: music_core::Result<Loaded>) {
        if generation != self.generation {
            return;
        }
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut page = g.get_page();
        page.loading = false;
        match res {
            Ok(l) => {
                let empty = l.tracks.is_empty() && l.blocks.is_empty() && l.grid.is_empty();
                if empty {
                    page.subtitle = "Nothing here yet.".into();
                }
                if let Some(t) = &l.title {
                    page.title = t.into();
                }
                page.playable = !l.tracks.is_empty();
                self.page_rows.set_playable(page.playable);
                g.set_page(page.clone());
                self.summary = l.summary;
                self.more = l.more;
                if empty {
                } else if let Some(h) = l.header {
                    g.set_collection(Collection {
                        label: h.label.clone().into(),
                        title: h.title.clone().into(),
                        artist: h.artist.clone().into(),
                        album: h.album,
                        editable: h.editable,
                        wide: h.wide,
                        linked: h.artists.iter().any(|a| a.id.is_some()),
                        can_save: h.save.is_some(),
                        saved: h.save.as_ref().is_some_and(|s| s.1),
                        can_follow: h.follow.is_some(),
                        following: h.follow.as_ref().is_some_and(|f| f.1),
                        ..Default::default()
                    });
                    if h.wide {
                        self.find_logo(h.title.clone(), h.channel.clone());
                    }
                    self.header = Some(h);
                    self.push_row(RowRef::Header, RowKind::Header, TrackRow::default());
                    self.header_images();
                } else if l.title.is_some() {
                    self.push_row(RowRef::Title, RowKind::Title, TrackRow::default());
                }
                if !l.tracks.is_empty() {
                    self.push_row(RowRef::Columns, RowKind::Columns, TrackRow::default());
                }
                self.tracks_at = self.rows.len();
                self.add_page_tracks(l.tracks);
                self.add_blocks(l.blocks);
                self.add_grid(l.grid);
            }
            Err(e) => {
                log::warn!("page: {e}");
                page.error = match &e {
                    music_core::Error::Net(_) => {
                        self.went_offline();
                        "Couldn't load this page. Check your connection."
                    }
                    _ if self.session_check() => {
                        "YouTube signed you out. Sign in to see this page."
                    }
                    _ => "Couldn't load this page.",
                }
                .into();
            }
        }
        g.set_page(page);
        self.video_sync();
    }

    pub(crate) fn push_row(&mut self, r: RowRef, kind: RowKind, row: TrackRow) {
        self.rows.push(r);
        self.art_index.clear();
        self.page_rows.push(TrackRow { kind, ..row });
    }

    /// Adds to the page's own tracks, keeping the footer after them.
    pub(crate) fn add_page_tracks(&mut self, tracks: Vec<Track>) {
        let mut at = self.tracks_at + self.main_len;
        if matches!(self.rows.get(at), Some(RowRef::Footer)) {
            self.rows.remove(at);
            self.art_index.clear();
            self.page_rows.remove(at);
        }
        let current = self.player.current().map(str::to_string);
        // On an album, the album's own artist goes without saying.
        let album_artist = self
            .header
            .as_ref()
            .filter(|h| h.album)
            .map(|h| h.artist.clone());
        for t in tracks {
            let mut row = self.row_for(&t, current.as_deref() == Some(t.video_id.as_str()), false);
            self.main_len += 1;
            row.number = self.main_len as i32;
            if album_artist.as_deref() == Some(row.artist.as_str()) {
                row.artist = SharedString::new();
            }
            self.rows.insert(at, RowRef::Track(self.page_tracks.len()));
            self.art_index.clear();
            self.page_rows.insert(at, row);
            self.page_tracks.push(t);
            self.page_groups.push(0);
            at += 1;
        }
        // Only once the whole list is here, so the totals are right.
        if self.summary && self.more.is_none() && self.main_len > 0 {
            self.rows.insert(at, RowRef::Footer);
            self.art_index.clear();
            self.page_rows.insert(
                at,
                TrackRow {
                    title: summary_line(&self.page_tracks).into(),
                    kind: RowKind::Footer,
                    ..Default::default()
                },
            );
        }
    }

    pub(crate) fn add_blocks(&mut self, blocks: Vec<Block>) {
        let current = self.player.current().map(str::to_string);
        for b in blocks {
            match b {
                Block::Tracks {
                    title,
                    more,
                    tracks,
                } => {
                    let group = self.page_groups.iter().copied().max().unwrap_or(0) + 1;
                    let row = TrackRow {
                        title: title.into(),
                        link: more.is_some(),
                        ..Default::default()
                    };
                    self.push_row(RowRef::Heading(more), RowKind::Heading, row);
                    // Uploads have no album, and their titles run long: the title takes it all.
                    let wide = tracks.iter().all(|t| t.album.is_none());
                    // No tracks: a heading over the grid that follows (Radio).
                    if !tracks.is_empty() {
                        let row = TrackRow {
                            wide,
                            ..Default::default()
                        };
                        self.push_row(RowRef::Columns, RowKind::Columns, row);
                    }
                    for (n, t) in tracks.into_iter().enumerate() {
                        let mut row = self.row_for(
                            &t,
                            current.as_deref() == Some(t.video_id.as_str()),
                            false,
                        );
                        row.number = n as i32 + 1;
                        row.wide = wide;
                        self.push_row(RowRef::Track(self.page_tracks.len()), RowKind::Track, row);
                        self.page_tracks.push(t);
                        self.page_groups.push(group);
                    }
                }
                Block::Details {
                    lyrics,
                    info,
                    about,
                } => {
                    let id = self.history[self.at].id.clone();
                    let row = TrackRow {
                        lines: lyric_lines(lyrics.as_ref()),
                        note: lyrics.as_ref().map(lyrics_note).unwrap_or_default().into(),
                        info: ModelRc::new(VecModel::from(
                            info.into_iter()
                                .map(|(label, text)| InfoLine {
                                    label: label.into(),
                                    text: text.into(),
                                })
                                .collect::<Vec<_>>(),
                        )),
                        about: about.unwrap_or_default().into(),
                        live: current.as_deref() == Some(id.as_str()),
                        ..Default::default()
                    };
                    self.push_row(RowRef::Details(id), RowKind::Details, row);
                }
                Block::Shelf(s) => {
                    let mut cards: Vec<Card> = s.items.iter().map(|i| self.card(i)).collect();
                    place_cards(&mut cards);
                    let row = TrackRow {
                        title: s.title.into(),
                        link: s.more.is_some(),
                        cards: ModelRc::new(VecModel::from(cards)),
                        ..Default::default()
                    };
                    self.push_row(RowRef::Shelf(s.items, s.more), RowKind::Shelf, row);
                }
            }
        }
    }

    pub(crate) fn add_grid(&mut self, items: Vec<Item>) {
        if items.is_empty() {
            return;
        }
        if self.grid.is_empty() {
            self.grid_at = self.rows.len();
        }
        self.grid.extend(items);
        self.layout_grid();
    }

    /// Lays the grid out in lines of `columns` cards: what's new, or all of it when the width
    /// changed.
    pub(crate) fn layout_grid(&mut self) {
        if self.grid.is_empty() {
            return;
        }
        let cols = self.columns.max(1);
        let (laid_cols, laid) = self.grid_laid;
        // Lines as wide as `cols` square cards; a music video takes 16/9 of one.
        let mut lines: Vec<(usize, usize)> = Vec::new();
        let (mut start, mut used) = (0, 0.0);
        for (i, item) in self.grid.iter().enumerate() {
            let w = card_units(matches!(item, Item::Track(t) if t.kind == TrackKind::Video));
            if i > start && used + w > cols as f32 + 0.001 {
                lines.push((start, i));
                (start, used) = (i, 0.0);
            }
            used += w;
        }
        lines.push((start, self.grid.len()));
        // Lines laid out at this width that were already followed by another stay as they are.
        let keep = if laid_cols == cols {
            lines.iter().take_while(|l| l.1 < laid).count()
        } else {
            0
        };
        while self.rows.len() > self.grid_at + keep {
            self.rows.pop();
            self.art_index.clear();
            self.page_rows.remove(self.page_rows.row_count() - 1);
        }
        for &(start, end) in &lines[keep..] {
            let mut cards: Vec<Card> = self.grid[start..end].iter().map(|i| self.card(i)).collect();
            place_cards(&mut cards);
            let row = TrackRow {
                cards: ModelRc::new(VecModel::from(cards)),
                ..Default::default()
            };
            self.push_row(RowRef::Grid(start), RowKind::Grid, row);
        }
        self.grid_laid = (cols, self.grid.len());
    }

    pub(crate) fn columns_changed(&mut self, n: usize) {
        if n != self.columns {
            self.columns = n;
            self.layout_grid();
        }
    }

    pub(crate) fn load_more(&mut self) {
        if self.loading_more {
            return;
        }
        let Some(more) = self.more.take() else { return };
        let again = more.clone();
        self.loading_more = true;
        self.extended = true;
        let generation = self.generation;
        let client = self.client.clone();
        let hidden = self.settings.hidden_playlists.clone();
        workers::run(
            move || pages::load_more(&client, more, &hidden),
            move |res| {
                with(|a| {
                    if generation != a.generation {
                        return;
                    }
                    a.loading_more = false;
                    match res {
                        Ok((extra, more)) => {
                            a.more = more;
                            match extra {
                                Extra::Tracks(t) => a.add_page_tracks(t),
                                Extra::Grid(items) => a.add_grid(items),
                                Extra::Blocks(b) => a.add_blocks(b),
                            }
                        }
                        Err(e) => {
                            log::warn!("more: {e}");
                            // Kept, so reaching the end again tries again.
                            a.more = Some(again);
                            a.notify_with("Couldn't load more", "Retry", |a| a.load_more());
                        }
                    }
                });
            },
        );
    }

    // ---- rows and artwork -------------------------------------------------------------------

    pub(crate) fn row_for(&self, t: &Track, current: bool, _queue: bool) -> TrackRow {
        TrackRow {
            title: t.title.clone().into(),
            artist: t.artist_line().into(),
            album: t
                .album
                .as_ref()
                .map(|a| a.name.clone())
                .unwrap_or_default()
                .into(),
            duration: t
                .duration_ms
                .or_else(|| self.lengths.get(&t.video_id).copied())
                .map(fmt_ms)
                .unwrap_or_default()
                .into(),
            // The exact count from the player beats the list's rounded "1.6B plays".
            plays: self
                .plays
                .get(&t.video_id)
                .map(|p| p.0)
                .or(t.plays)
                .map(fmt_count)
                .unwrap_or_default()
                .into(),
            explicit: t.explicit,
            upload: t.upload,
            current,
            unavailable: t.unavailable,
            ..Default::default()
        }
    }

    /// A card, with its artwork if that's already in memory.
    pub(crate) fn card(&self, item: &Item) -> Card {
        let mut c = pages::card_for(item);
        radio::dress(&mut c, item);
        if let Some(url) = pages::card_url(item, self.px(CARD_ART))
            && let Some(img) = self.images.peek(&url)
        {
            c.art = img;
        }
        c
    }

    /// The track on a row.
    pub(crate) fn track_at(&self, row: usize) -> Option<usize> {
        match self.rows.get(row) {
            Some(RowRef::Track(i)) => Some(*i),
            _ => None,
        }
    }

    /// The item behind a card.
    pub(crate) fn card_item(&self, row: usize, i: usize) -> Option<&Item> {
        match self.rows.get(row)? {
            RowRef::Shelf(items, _) => items.get(i),
            RowRef::Grid(start) => self.grid.get(start + i),
            _ => None,
        }
    }

    /// The size page rows show artwork at, if they show it.
    pub(crate) fn page_art(&self) -> Option<f32> {
        match &self.header {
            Some(h) if h.album => None,
            _ => Some(COLLECTION_ART),
        }
    }

    pub(crate) fn open_album(&mut self, row: usize) {
        let Some(album) = self
            .track_at(row)
            .and_then(|i| self.page_tracks.get(i))
            .and_then(|t| t.album.clone())
        else {
            return;
        };
        if let Some(id) = album.id {
            self.navigate(Route {
                kind: PageKind::Album,
                id,
                title: album.name,
            });
        }
    }

    pub(crate) fn card_clicked(&mut self, row: usize, i: usize) {
        if self.radio_card_clicked(row, i) {
            return;
        }
        let Some(item) = self.card_item(row, i).cloned() else {
            return;
        };
        let route = |kind, id: &str, title: &str| Route {
            kind,
            id: id.into(),
            title: title.into(),
        };
        match item {
            // A music video plays (unless it is already) and opens its song page, the
            // picture centred.
            Item::Track(ref t) if t.kind == TrackKind::Video => {
                let t = t.clone();
                if self.player.current() != Some(t.video_id.as_str()) {
                    self.play_shelf_track(row, i, item);
                }
                self.open_video(&t);
            }
            Item::Track(_) => self.play_shelf_track(row, i, item),
            Item::Album(ref a) => {
                let r = route(PageKind::Album, &a.id, &a.title);
                self.opened_from = Some(item);
                self.navigate(r);
            }
            Item::Playlist(ref p) => {
                let r = route(PageKind::Playlist, &p.id, &p.title);
                self.opened_from = Some(item);
                self.navigate(r);
            }
            Item::Artist(ref a) => {
                let r = route(PageKind::Artist, &a.id, &a.name);
                self.opened_from = Some(item);
                self.navigate(r);
            }
            Item::Link(l) => {
                let id = pages::feed_id(&BrowseRef {
                    browse_id: l.browse_id,
                    params: l.params,
                });
                self.navigate(route(PageKind::Feed, &id, &l.title));
            }
        }
    }

    pub(crate) fn see_all(&mut self, row: usize) {
        let more = match self.rows.get(row) {
            Some(RowRef::Heading(Some(m)) | RowRef::Shelf(_, Some(m))) => m.clone(),
            _ => return,
        };
        let title = self
            .page_rows
            .row_data(row)
            .map(|r| r.title.to_string())
            .unwrap_or_default();
        self.navigate(pages::route_for(&more, &title));
    }

    /// Shows an artist's logo in place of the name: at once when it's known (the name waits
    /// for the picture), else after asking on a worker.
    pub(crate) fn find_logo(&mut self, name: String, channel: Option<String>) {
        if let Some(url) = logos::known(&name, channel.as_deref()) {
            if url.is_some() {
                self.logo_url = url;
                let ui = self.ui();
                let g = ui.global::<Ui>();
                let mut c = g.get_collection();
                c.logo_pending = true;
                g.set_collection(c);
                self.header_images();
            }
            return;
        }
        let generation = self.generation;
        // Rate-limited (it may wait its turn a while): kept off the threads pages load on.
        workers::run_background(
            move || logos::find(&name, channel.as_deref()),
            move |url| {
                with(|a| {
                    if a.generation == generation && url.is_some() {
                        a.logo_url = url;
                        a.header_images();
                    }
                });
            },
        );
    }

    /// The banner's cover and backdrop: from memory, or asked for.
    pub(crate) fn header_images(&mut self) {
        let Some((thumbs, wide)) = self.header.as_ref().map(|h| (h.thumbs.clone(), h.wide)) else {
            return;
        };
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut c = g.get_collection();
        if let Some(url) = self.logo_url.clone()
            && let Some(img) = self.images.get_urgent(&url, LOGO_ART, image_loaded)
        {
            c.logo = img;
            c.logo_pending = false;
        }
        if wide {
            if let Some(t) = thumbs.iter().max_by_key(|t| t.width)
                && let Some(img) = self.images.get_wide(&t.url, image_loaded)
            {
                c.banner = img;
            }
        } else {
            let px = self.px(HEADER_ART);
            if let Some(url) = images::url_for(&thumbs, px)
                && let Some(img) = self.images.get(&url, px, image_loaded)
            {
                c.cover = img;
            }
            if let Some(url) = images::url_for(&thumbs, BANNER_SOURCE)
                && let Some(img) = self.images.get_blurred(&url, image_loaded)
            {
                c.banner = img;
            }
        }
        g.set_collection(c);
    }

    /// A row became visible: load its artwork. `list` 0 = page, 1 = queue.
    pub(crate) fn row_shown(&mut self, list: i32, i: usize) {
        let (thumbs, rows, size) = if list == 0 {
            let Some(t) = self.track_at(i).and_then(|ti| self.page_tracks.get(ti)) else {
                return;
            };
            // Lists give plays rounded ("1.6B") or not at all, and some leave out lengths: ask
            // the player for the exact count, again once a week.
            let fresh = self
                .plays
                .get(&t.video_id)
                .is_some_and(|p| music_core::paths::unix_now().saturating_sub(p.1) < 7 * 24 * 3600);
            if !fresh || (t.duration_ms.is_none() && !self.lengths.contains_key(&t.video_id)) {
                self.want_stats(t.video_id.clone());
            }
            let Some(size) = self.page_art() else {
                return;
            };
            let Some(t) = self.track_at(i).and_then(|ti| self.page_tracks.get(ti)) else {
                return;
            };
            let rows: Box<dyn rows::RowStore> = Box::new(self.page_rows.clone());
            (t.thumbs.clone(), rows, size)
        } else {
            let Some(t) = self
                .queue_ids
                .get(self.queue_from + i)
                .and_then(|id| self.known.get(id))
            else {
                return;
            };
            let rows: Box<dyn rows::RowStore> = Box::new(self.queue_rows.clone());
            (t.thumbs.clone(), rows, QUEUE_ART)
        };
        let px = self.px(size);
        let Some(url) = images::url_for(&thumbs, px) else {
            return;
        };
        // Only when the picture changes: rewriting a row makes the list measure it again, which
        // shows as flicker mid-scroll.
        if let Some(img) = self.images.get(&url, px, image_loaded)
            && let Some(mut r) = rows.get(i)
            && r.art != img
        {
            r.art = img;
            rows.put(i, r);
        }
    }

    /// A card was drawn: load its artwork.
    pub(crate) fn card_shown(&mut self, row: usize, i: usize) {
        // Stations draw their own artwork (again after a minimize dropped it).
        if let Some(Item::Track(t)) = self.card_item(row, i)
            && radio::is_station(&t.video_id)
        {
            if let Some(r) = self.page_rows.row_data(row)
                && let Some(mut c) = r.cards.row_data(i)
                && c.art.size().width == 0
            {
                c.art = radio::art(&t.video_id, &t.title);
                r.cards.set_row_data(i, c);
            }
            return;
        }
        let px = self.px(CARD_ART);
        let Some(url) = self
            .card_item(row, i)
            .and_then(|item| pages::card_url(item, px))
        else {
            return;
        };
        if let Some(img) = self.images.get(&url, px, image_loaded)
            && let Some(r) = self.page_rows.row_data(row)
            && let Some(mut c) = r.cards.row_data(i)
            && c.art.size().width == 0
        {
            c.art = img;
            r.cards.set_row_data(i, c);
        }
    }

    /// A row showing `id` came into view: its stats are asked for once the list has been still
    /// for 300 ms, the rows seen last first.
    pub(crate) fn want_stats(&mut self, id: String) {
        if self.plays_asked.contains(&id) {
            return;
        }
        self.stats_queue.retain(|q| *q != id);
        self.stats_queue.push_back(id);
        // Rows scrolled past long ago aren't on screen any more.
        while self.stats_queue.len() > 40 {
            self.stats_queue.pop_front();
        }
        self.stats_timer
            .start(TimerMode::SingleShot, Duration::from_millis(300), || {
                with(|a| a.send_stats());
            });
    }

    /// Sends what's waiting in `stats_queue`, at most 4 at a time.
    pub(crate) fn send_stats(&mut self) {
        if self.stats_timer.running() {
            return;
        }
        while self.stats_in_flight < 4
            && let Some(id) = self.stats_queue.pop_back()
        {
            self.ask_stats(id);
        }
    }

    /// Asks for a song's play count and length (once per run), then fills them in wherever it's
    /// listed.
    pub(crate) fn ask_stats(&mut self, id: String) {
        if !self.plays_asked.insert(id.clone()) {
            return;
        }
        self.stats_in_flight += 1;
        let client = self.client.clone();
        let page = PAGE.load(Ordering::Relaxed);
        workers::run_background(
            // Rows of a page that's since been left aren't worth the round trip.
            move || {
                let r = (PAGE.load(Ordering::Relaxed) == page).then(|| client.video_stats(&id));
                (r, id)
            },
            |(r, id)| {
                with(|a| {
                    a.stats_in_flight -= 1;
                    a.got_stats(r, id);
                    a.send_stats();
                });
            },
        );
    }

    pub(crate) fn got_stats(&mut self, r: Option<music_core::Result<VideoStats>>, id: String) {
        let a = self;
        match r {
            None => {
                a.plays_asked.remove(&id);
            }
            Some(r) => match r {
                Ok(VideoStats {
                    plays,
                    duration_ms: ms,
                    ..
                }) => {
                    if let Some(n) = plays {
                        a.plays
                            .insert(id.clone(), (n, music_core::paths::unix_now()));
                        a.save_plays_later();
                    }
                    if let Some(ms) = ms {
                        a.lengths.insert(id.clone(), ms);
                    }
                    for (ri, r) in a.rows.iter().enumerate() {
                        let RowRef::Track(ti) = r else { continue };
                        let t = &mut a.page_tracks[*ti];
                        if t.video_id != id {
                            continue;
                        }
                        let Some(mut row) = a.page_rows.row_data(ri) else {
                            continue;
                        };
                        if let Some(n) = plays {
                            t.plays = Some(n);
                            row.plays = fmt_count(n).into();
                        }
                        if let Some(ms) = ms.filter(|_| t.duration_ms.is_none()) {
                            t.duration_ms = Some(ms);
                            row.duration = fmt_ms(ms).into();
                        }
                        a.page_rows.set_row_data(ri, row);
                    }
                }
                Err(e) => log::warn!("stats {id}: {e}"),
            },
        }
    }

    /// The page's sizes changed. While the sidebar or panel slides they change every frame and
    /// each change lays out every row, so it's done at most every 80 ms (and once at the end).
    pub(crate) fn page_metrics(&mut self, m: rows::Metrics) {
        if self.metrics_timer.running() {
            self.metrics = Some(m);
            return;
        }
        self.page_rows.set_metrics(m);
        self.metrics_timer
            .start(TimerMode::SingleShot, Duration::from_millis(80), || {
                with(|a| {
                    if let Some(m) = a.metrics.take() {
                        // What changed since it was asked for by other means stays.
                        let ui = a.ui();
                        let g = ui.global::<Ui>();
                        a.page_rows.set_metrics(rows::Metrics {
                            playable: g.get_page().playable,
                            video: g.get_video_on(),
                            ..m
                        });
                    }
                });
            });
    }

    /// An image arrived: put it everywhere it's wanted.
    /// Brings `art_index` up to date with the page's rows and artwork sizes.
    pub(crate) fn index_art(&mut self) {
        let row_px = self.page_art().map(|s| self.px(s));
        let card_px = self.px(CARD_ART);
        let cards: usize = (0..self.page_rows.row_count())
            .filter_map(|ri| self.page_rows.row_data(ri))
            .map(|r| r.cards.row_count())
            .sum();
        let key = (
            self.generation,
            self.rows.len(),
            self.grid.len(),
            cards,
            row_px,
            card_px,
        );
        if key == self.art_index_key && !self.art_index.is_empty() {
            return;
        }
        self.art_index_key = key;
        self.art_index.clear();
        for (ri, r) in self.rows.iter().enumerate() {
            let items: &[Item] = match r {
                RowRef::Track(ti) => {
                    if let Some(px) = row_px
                        && let Some(url) = images::url_for(&self.page_tracks[*ti].thumbs, px)
                    {
                        self.art_index.entry(url).or_default().push((ri, None));
                    }
                    continue;
                }
                RowRef::Shelf(items, _) => items,
                RowRef::Grid(start) => {
                    let n = self
                        .page_rows
                        .row_data(ri)
                        .map_or(0, |r| r.cards.row_count());
                    let end = (start + n).min(self.grid.len());
                    &self.grid[*start..end]
                }
                _ => continue,
            };
            for (j, item) in items.iter().enumerate() {
                if let Some(url) = pages::card_url(item, card_px) {
                    self.art_index.entry(url).or_default().push((ri, Some(j)));
                }
            }
        }
    }

    pub(crate) fn image_arrived(&mut self, url: String, img: Image) {
        self.images.insert(&url, img.clone());
        if self.minimized || img.size().width == 0 {
            return;
        }
        self.index_art();
        for &(ri, card) in self.art_index.get(&url).into_iter().flatten() {
            let Some(mut row) = self.page_rows.row_data(ri) else {
                continue;
            };
            match card {
                None => {
                    row.art = img.clone();
                    self.page_rows.set_row_data(ri, row);
                }
                Some(j) => {
                    if let Some(mut c) = row.cards.row_data(j) {
                        c.art = img.clone();
                        row.cards.set_row_data(j, c);
                    }
                }
            }
        }
        let q_px = self.px(QUEUE_ART);
        for (i, id) in self.queue_ids.iter().skip(self.queue_from).enumerate() {
            if let Some(t) = self.known.get(id)
                && images::url_for(&t.thumbs, q_px).as_deref() == Some(url.as_str())
                && let Some(mut r) = self.queue_rows.row_data(i)
            {
                r.art = img.clone();
                self.queue_rows.set_row_data(i, r);
            }
        }
        let s_px = self.px(SIDEBAR_ART);
        for (i, p) in self.playlists.iter().enumerate() {
            if images::url_for(&p.thumbs, s_px).as_deref() == Some(url.as_str())
                && let Some(mut r) = self.playlist_rows.row_data(i)
            {
                r.cover = img.clone();
                self.playlist_rows.set_row_data(i, r);
            }
        }
        let ui = self.ui();
        let g = ui.global::<Ui>();
        if let Some(t) = self.player.current().and_then(|id| self.known.get(id))
            && images::url_for(&t.thumbs, self.px(BAR_ART)).as_deref() == Some(url.as_str())
        {
            let mut np = g.get_np();
            np.art = img.clone();
            g.set_np(np);
        }
        if let Some(h) = &self.header {
            let mut c = g.get_collection();
            let (cover, banner) = if h.wide {
                let wide = h.thumbs.iter().max_by_key(|t| t.width);
                (None, wide.map(|t| images::wide_key(&t.url)))
            } else {
                (
                    images::url_for(&h.thumbs, self.px(HEADER_ART)),
                    images::url_for(&h.thumbs, BANNER_SOURCE).map(|u| images::blurred_key(&u)),
                )
            };
            if self.logo_url.as_deref() == Some(url.as_str()) {
                // A logo that failed to load leaves the name.
                c.logo = img.clone();
                c.logo_pending = false;
                g.set_collection(c);
            } else if cover.as_deref() == Some(url.as_str()) {
                c.cover = img.clone();
                g.set_collection(c);
            } else if banner.as_deref() == Some(url.as_str()) {
                c.banner = img.clone();
                g.set_collection(c);
            }
        }
        if let Some(acc) = &self.account
            && images::url_for(&acc.photo, self.px(ACCOUNT_ART)).as_deref() == Some(url.as_str())
        {
            g.set_account_photo(img);
        }
    }

    pub(crate) fn open_song(&mut self, t: &Track) {
        self.navigate(Route {
            kind: PageKind::Song,
            id: t.video_id.clone(),
            title: t.title.clone(),
        });
    }

    /// A music video's song page, which scrolls to centre the picture once it shows.
    pub(crate) fn open_video(&mut self, t: &Track) {
        self.open_song(t);
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_centre_video_serial(g.get_centre_video_serial().wrapping_add(1));
    }

    /// Takes matching cards, and songs in headed lists (search results, top songs), off the
    /// page. A song taken off stays in `page_tracks`, marked so it isn't played.
    pub(crate) fn drop_from_page(&mut self, gone: &dyn Fn(&Item) -> bool) {
        self.forget_page();
        for ri in (0..self.rows.len()).rev() {
            let remove = match &mut self.rows[ri] {
                RowRef::Shelf(items, _) => {
                    let row = self.page_rows.row_data(ri);
                    let cards = row
                        .as_ref()
                        .and_then(|r| r.cards.as_any().downcast_ref::<VecModel<Card>>());
                    let mut dropped = false;
                    for j in (0..items.len()).rev() {
                        if gone(&items[j]) {
                            items.remove(j);
                            if let Some(c) = cards {
                                c.remove(j);
                                dropped = true;
                            }
                        }
                    }
                    // The cards after a gap move up into it.
                    if let Some(c) = cards.filter(|_| dropped) {
                        let mut all: Vec<Card> = c.iter().collect();
                        place_cards(&mut all);
                        c.set_vec(all);
                    }
                    items.is_empty()
                }
                RowRef::Track(ti) if self.page_groups[*ti] != 0 => {
                    let t = &mut self.page_tracks[*ti];
                    let hit = gone(&Item::Track(t.clone()));
                    if hit {
                        t.unavailable = true;
                    }
                    hit
                }
                _ => false,
            };
            if remove {
                self.rows.remove(ri);
                self.art_index.clear();
                self.page_rows.remove(ri);
                if self.tracks_at > ri {
                    self.tracks_at -= 1;
                }
                if self.grid_at > ri {
                    self.grid_at -= 1;
                }
            }
        }
        let before = self.grid.len();
        self.grid.retain(|i| !gone(i));
        if self.grid.is_empty() && before > 0 {
            // The grid ends the page: its last card gone, so are its rows.
            while self.rows.len() > self.grid_at {
                self.rows.pop();
                self.art_index.clear();
                self.page_rows.remove(self.page_rows.row_count() - 1);
            }
        } else if self.grid.len() != before {
            self.grid_laid = (0, 0);
            self.layout_grid();
        }
    }

    /// Hides a playlist from the app for good (kept in settings).
    /// The now-playing bar's cover opens the album, its title the song. Songs without an album
    /// (videos) open their song page either way.
    pub(crate) fn np_open(&mut self, what: &str) {
        let Some(t) = self
            .player
            .current()
            .and_then(|id| self.known.get(id))
            .cloned()
        else {
            return;
        };
        match t
            .album
            .as_ref()
            .and_then(|a| Some((a.id.clone()?, a.name.clone())))
        {
            Some((id, title)) if what == "album" => self.navigate(Route {
                kind: PageKind::Album,
                id,
                title,
            }),
            _ => self.open_song(&t),
        }
    }
}

/// The whole count with thousands separators, as Apple Music's Plays column: "1,234,567".
fn fmt_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Lyrics as the UI's lines: unsynced ones have no time.
fn lyric_lines(l: Option<&lyrics::Lyrics>) -> ModelRc<LyricLine> {
    ModelRc::new(VecModel::from(l.map(lyric_view::rows).unwrap_or_default()))
}

/// Nothing: the app names no lyric source (PLAN R6).
fn lyrics_note(_: &lyrics::Lyrics) -> String {
    String::new()
}

fn g_serial_bump(ui: &AppWindow) {
    let g = ui.global::<Ui>();
    g.set_page_serial(g.get_page_serial().wrapping_add(1));
}

fn fmt_ms(ms: u64) -> String {
    format!("{}:{:02}", ms / 60000, ms / 1000 % 60)
}
