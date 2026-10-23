//! The Music app: one window, the audio engine, and the YouTube Music client.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod images;
mod settings;
mod win;
mod workers;

use images::Images;
use music_core::audio::{Engine, Event};
use music_core::player::{Player, Repeat};
use music_core::yt::model::*;
use music_core::yt::{Client, Resolver, Session};
use settings::{SavedQueue, Settings};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use win::Remote;
use windows::Win32::Foundation::HWND;

slint::include_modules!();

/// Sizes artwork is shown at, in logical pixels.
const ROW_ART: f32 = 38.0;
const BAR_ART: f32 = 52.0;
const SIDEBAR_ART: f32 = 18.0;
const ACCOUNT_ART: f32 = 28.0;
const QUEUE_ART: f32 = 36.0;

#[derive(Clone, PartialEq)]
struct Route {
    kind: PageKind,
    id: String,
    title: String,
}

/// How the current page continues.
enum More {
    Feed(Continuation),
    Playlist(Continuation),
    Search(String, Continuation),
}

struct App {
    ui: slint::Weak<AppWindow>,
    hwnd: Option<HWND>,
    client: Arc<Client>,
    player: Player,
    media: Option<win::Media>,
    settings: Settings,
    images: Images,
    /// Everything we know about tracks in the queue, by video id.
    known: HashMap<String, Track>,

    history: Vec<Route>,
    at: usize,
    /// Bumped on every navigation, so late results for an old page are dropped.
    generation: u64,
    page_tracks: Vec<Track>,
    page_rows: Rc<VecModel<TrackRow>>,
    more: Option<More>,
    loading_more: bool,
    /// The line after the last track ("237 tracks, 6+ hours").
    summary: bool,
    /// Play counts looked up for rows that don't carry one: video id → (count, unix time).
    plays: HashMap<String, (u64, u64)>,
    plays_asked: std::collections::HashSet<String>,

    queue_rows: Rc<VecModel<TrackRow>>,
    queue_ids: Vec<String>,
    playlists: Vec<PlaylistCard>,
    playlist_rows: Rc<VecModel<SidebarPlaylist>>,
    account: Option<Account>,

    /// Seek to this once the restored track starts.
    resume_at: Option<u64>,
    minimized: bool,
    position_timer: Timer,
    save_timer: Timer,
    plays_timer: Timer,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` with the app state (UI thread only).
fn with<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.borrow_mut().as_mut().map(f))
}

fn main() {
    logger::init();
    win::dpi_aware();
    log::info!("starting {}", env!("CARGO_PKG_VERSION"));
    let settings = Settings::load();
    let resolver = Arc::new(Resolver::new());
    let engine = match Engine::new(resolver) {
        Ok(e) => e,
        Err(e) => {
            log::error!("audio: {e}");
            return;
        }
    };
    let ui = AppWindow::new().expect("window");
    ui.window().set_size(slint::LogicalSize::new(
        settings.window.0,
        settings.window.1,
    ));

    let app = App {
        ui: ui.as_weak(),
        hwnd: None,
        client: Arc::new(Client::from_disk()),
        player: Player::new(engine),
        media: None,
        settings,
        images: Images::new(),
        known: HashMap::new(),
        history: Vec::new(),
        at: 0,
        generation: 0,
        page_tracks: Vec::new(),
        page_rows: Rc::new(VecModel::default()),
        more: None,
        loading_more: false,
        summary: false,
        plays: settings::load_plays(),
        plays_asked: Default::default(),
        queue_rows: Rc::new(VecModel::default()),
        queue_ids: Vec::new(),
        playlists: Vec::new(),
        playlist_rows: Rc::new(VecModel::default()),
        account: None,
        resume_at: None,
        minimized: false,
        position_timer: Timer::default(),
        save_timer: Timer::default(),
        plays_timer: Timer::default(),
    };
    APP.with(|a| *a.borrow_mut() = Some(app));

    wire(&ui);
    with(|a| a.start());
    forward_engine_events();
    workers::spawn(images::prune_disk);

    // Window handle work needs the native window, which exists once the event loop runs.
    let _ = slint::invoke_from_event_loop(|| {
        with(|a| a.attach_window());
    });
    ui.run().expect("event loop");
    with(|a| a.shutdown());
    // Drop the app (and the engine's threads) here, not during thread-local teardown.
    let app = APP.with(|a| a.borrow_mut().take());
    drop(app);
}

fn forward_engine_events() {
    let rx = with(|a| a.player.engine.events().clone()).expect("app");
    std::thread::Builder::new()
        .name("engine-events".into())
        .spawn(move || {
            for e in rx {
                let _ = slint::invoke_from_event_loop(move || {
                    with(|a| a.on_engine(e));
                });
            }
        })
        .expect("spawn");
}

/// Connects every UI callback to the app.
fn wire(ui: &AppWindow) {
    let g = ui.global::<Ui>();
    g.on_navigate(|kind, id, title| {
        with(|a| {
            a.navigate(Route {
                kind,
                id: id.into(),
                title: title.into(),
            })
        });
    });
    g.on_back(|| {
        with(|a| a.go(-1));
    });
    g.on_forward(|| {
        with(|a| a.go(1));
    });
    g.on_search(|q| {
        with(|a| {
            a.navigate(Route {
                kind: PageKind::Search,
                id: q.to_string(),
                title: q.to_string(),
            })
        });
    });
    g.on_sign_in(|| {
        with(|a| a.sign_in());
    });
    g.on_sign_out(|| {
        with(|a| a.sign_out());
    });
    g.on_play_tracks(|i| {
        with(|a| a.play_page(i));
    });
    g.on_near_end(|| {
        with(|a| a.load_more());
    });
    g.on_row_shown(|list, i| {
        with(|a| a.row_shown(list, i as usize));
    });
    g.on_play_pause(|| {
        with(|a| a.play_pause());
    });
    g.on_next(|| {
        with(|a| a.player.next());
    });
    g.on_previous(|| {
        with(|a| a.player.previous());
    });
    g.on_seek(|ms| {
        with(|a| {
            a.player.engine.seek(ms.max(0) as u64);
            a.tick();
        });
    });
    g.on_set_volume(|v| {
        with(|a| a.set_volume(v));
    });
    g.on_toggle_shuffle(|| {
        with(|a| {
            let on = !a.player.shuffle();
            a.player.set_shuffle(on);
            a.queue_changed();
        });
    });
    g.on_cycle_repeat(|| {
        with(|a| {
            a.player.set_repeat(match a.player.repeat {
                Repeat::Off => Repeat::All,
                Repeat::All => Repeat::One,
                Repeat::One => Repeat::Off,
            });
            a.queue_changed();
        });
    });
    g.on_toggle_like(|| {
        with(|a| a.toggle_like());
    });
    g.on_panel_changed(|| {
        with(|a| a.panel_changed());
    });
    g.on_queue_jump(|i| {
        with(|a| a.player.jump(i as usize));
    });
    g.on_queue_remove(|i| {
        with(|a| {
            a.player.remove(i as usize);
            a.queue_changed();
        });
    });
    g.on_queue_move(|from, to| {
        with(|a| {
            a.player.move_item(from as usize, to as usize);
            a.queue_changed();
        });
    });
    g.on_eq_toggle(|| {
        with(|a| {
            a.settings.eq.enabled = !a.settings.eq.enabled;
            a.eq_changed();
        });
    });
    g.on_eq_set(|band, gain| {
        with(|a| {
            if let Some(g) = a.settings.eq.gains.get_mut(band as usize) {
                *g = gain;
            }
            a.settings.eq.enabled = true;
            a.settings.eq_preset.clear();
            a.eq_changed();
        });
    });
    g.on_eq_choose(|name| {
        with(|a| {
            if let Some(gains) = music_core::audio::EqSettings::preset(&name.to_lowercase()) {
                a.settings.eq.gains = gains;
                a.settings.eq.enabled = name != "Flat";
                a.settings.eq_preset = name.to_string();
                a.eq_changed();
            }
        });
    });
    g.on_sidebar_changed(|| {
        with(|a| {
            a.settings.sidebar_collapsed = a.ui().global::<Ui>().get_sidebar_collapsed();
            a.save_later();
        });
    });
}

impl App {
    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("window")
    }

    fn scale(&self) -> f32 {
        self.ui().window().scale_factor()
    }

    fn px(&self, logical: f32) -> u32 {
        (logical * self.scale()).ceil() as u32
    }

    fn start(&mut self) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_tracks(ModelRc::from(self.page_rows.clone()));
        g.set_queue(ModelRc::from(self.queue_rows.clone()));
        g.set_playlists(ModelRc::from(self.playlist_rows.clone()));
        g.set_volume(self.settings.volume);
        g.set_sidebar_collapsed(self.settings.sidebar_collapsed);
        g.set_panel_open(self.settings.panel_open);
        g.set_panel_tab(self.settings.panel_tab);
        self.set_volume(self.settings.volume);
        self.eq_changed();

        let q = SavedQueue::load();
        if !q.tracks.is_empty() {
            let ids: Vec<String> = q.tracks.iter().map(|t| t.video_id.clone()).collect();
            for t in q.tracks {
                self.known.insert(t.video_id.clone(), t);
            }
            self.player.restore(ids, q.at);
            self.player.set_repeat(q.repeat);
            if q.shuffle {
                self.player.set_shuffle(true);
            }
            self.resume_at = Some(q.position_ms);
            self.queue_changed();
            self.show_now_playing(false);
        }

        self.account_changed();
        self.navigate(Route {
            kind: PageKind::Home,
            id: String::new(),
            title: "Home".into(),
        });
    }

    fn attach_window(&mut self) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let ui = self.ui();
        let handle = ui.window().window_handle();
        let Ok(h) = handle.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(h) = h.as_raw() else {
            return;
        };
        let hwnd = HWND(h.hwnd.get() as _);
        self.hwnd = Some(hwnd);
        log::info!("window {:#x}", hwnd.0 as usize);
        win::dark_title_bar(hwnd);
        win::install(hwnd, |r| {
            with(|a| a.remote(r));
        });
        match win::Media::new(hwnd) {
            Ok(m) => self.media = Some(m),
            Err(e) => log::warn!("media controls unavailable: {e}"),
        }
        self.show_now_playing(false);
    }

    fn shutdown(&mut self) {
        let size = self.ui().window().size().to_logical(self.scale());
        self.settings.window = (size.width, size.height);
        self.settings.save();
        self.save_queue();
        self.player.engine.stop();
    }

    // ---- navigation ------------------------------------------------------------------------

    fn navigate(&mut self, r: Route) {
        if self.history.get(self.at) == Some(&r) {
            return self.load_page();
        }
        if !self.history.is_empty() {
            self.history.truncate(self.at + 1);
        }
        self.history.push(r);
        self.at = self.history.len() - 1;
        self.load_page();
    }

    fn go(&mut self, d: i32) {
        let to = self.at as i32 + d;
        if to >= 0 && (to as usize) < self.history.len() {
            self.at = to as usize;
            self.load_page();
        }
    }

    fn load_page(&mut self) {
        let r = self.history[self.at].clone();
        self.generation += 1;
        let generation = self.generation;
        self.page_tracks.clear();
        self.page_rows.set_vec(Vec::new());
        self.more = None;
        self.loading_more = false;
        self.summary = false;
        let ui = self.ui();
        let g = ui.global::<Ui>();
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
        let note = match r.kind {
            PageKind::Home
            | PageKind::Explore
            | PageKind::Artists
            | PageKind::Albums
            | PageKind::AllPlaylists => {
                "This page arrives in the next build. Recently played, Songs, Liked songs, playlists and search already play."
            }
            PageKind::Radio => "Radio is coming later.",
            _ => "",
        };
        let needs_sign_in = library && !self.client.signed_in();
        let fetches = !needs_sign_in
            && matches!(
                r.kind,
                PageKind::History
                    | PageKind::Songs
                    | PageKind::Liked
                    | PageKind::Playlist
                    | PageKind::Search
            );
        g.set_page(PageState {
            kind: r.kind,
            id: r.id.clone().into(),
            title: r.title.clone().into(),
            subtitle: note.into(),
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
        let client = self.client.clone();
        workers::run(
            move || -> music_core::Result<(Vec<Track>, Option<More>, bool)> {
                Ok(match r.kind {
                    PageKind::Search => {
                        let s = client.search(&r.id, Some(SearchFilter::Songs))?;
                        let cont = s.continuation.map(|c| More::Search(r.id.clone(), c));
                        (tracks_of(s.items), cont, false)
                    }
                    PageKind::History => {
                        let f = client.history()?;
                        (
                            tracks_of(f.shelves.into_iter().flat_map(|s| s.items).collect()),
                            None,
                            false,
                        )
                    }
                    PageKind::Songs => {
                        let f = client.library_songs()?;
                        let cont = f.continuation.map(More::Feed);
                        (
                            tracks_of(f.shelves.into_iter().flat_map(|s| s.items).collect()),
                            cont,
                            false,
                        )
                    }
                    _ => {
                        let p = client.playlist(&r.id)?;
                        (p.tracks, p.continuation.map(More::Playlist), true)
                    }
                })
            },
            move |res| {
                with(|a| a.page_loaded(generation, res));
            },
        );
    }

    fn page_loaded(
        &mut self,
        generation: u64,
        res: music_core::Result<(Vec<Track>, Option<More>, bool)>,
    ) {
        if generation != self.generation {
            return;
        }
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut page = g.get_page();
        page.loading = false;
        match res {
            Ok((tracks, more, summary)) => {
                self.summary = summary;
                if page.subtitle.is_empty() {
                    page.subtitle = if tracks.is_empty() {
                        "Nothing here yet.".into()
                    } else {
                        SharedString::new()
                    };
                }
                page.playable = !tracks.is_empty();
                self.more = more;
                self.add_page_tracks(tracks);
            }
            Err(e) => {
                log::warn!("page: {e}");
                page.error = format!("Couldn't load this page ({e}).").into();
            }
        }
        g.set_page(page);
    }

    fn add_page_tracks(&mut self, tracks: Vec<Track>) {
        // The footer row stays last: take it off, append, put it back.
        if self.page_rows.row_count() > self.page_tracks.len() {
            self.page_rows.remove(self.page_rows.row_count() - 1);
        }
        let current = self.player.current().map(str::to_string);
        for t in tracks {
            self.page_rows.push(self.row_for(
                &t,
                current.as_deref() == Some(t.video_id.as_str()),
                false,
            ));
            self.page_tracks.push(t);
        }
        // Only once the whole list is here, so the totals are right.
        if self.summary && self.more.is_none() && !self.page_tracks.is_empty() {
            self.page_rows.push(TrackRow {
                title: summary_line(&self.page_tracks).into(),
                is_footer: true,
                ..Default::default()
            });
        }
    }

    fn load_more(&mut self) {
        if self.loading_more {
            return;
        }
        let Some(more) = self.more.take() else { return };
        self.loading_more = true;
        let generation = self.generation;
        let client = self.client.clone();
        workers::run(
            move || -> music_core::Result<(Vec<Track>, Option<More>)> {
                Ok(match more {
                    More::Feed(c) => {
                        let f = client.feed_more(&c)?;
                        (
                            tracks_of(f.shelves.into_iter().flat_map(|s| s.items).collect()),
                            f.continuation.map(More::Feed),
                        )
                    }
                    More::Playlist(c) => {
                        let (t, c) = client.playlist_more(&c)?;
                        (t, c.map(More::Playlist))
                    }
                    More::Search(q, c) => {
                        let s = client.search_more(&q, Some(SearchFilter::Songs), &c)?;
                        let next = s.continuation.map(|c| More::Search(q.clone(), c));
                        (tracks_of(s.items), next)
                    }
                })
            },
            move |res| {
                with(|a| {
                    if generation != a.generation {
                        return;
                    }
                    a.loading_more = false;
                    match res {
                        Ok((tracks, more)) => {
                            a.more = more;
                            a.add_page_tracks(tracks);
                        }
                        Err(e) => log::warn!("more: {e}"),
                    }
                });
            },
        );
    }

    // ---- rows and artwork -------------------------------------------------------------------

    fn row_for(&self, t: &Track, current: bool, _queue: bool) -> TrackRow {
        TrackRow {
            title: t.title.clone().into(),
            artist: t.artist_line().into(),
            album: t
                .album
                .as_ref()
                .map(|a| a.name.clone())
                .unwrap_or_default()
                .into(),
            duration: t.duration_ms.map(fmt_ms).unwrap_or_default().into(),
            plays: t
                .plays
                .or_else(|| self.plays.get(&t.video_id).map(|p| p.0))
                .map(fmt_count)
                .unwrap_or_default()
                .into(),
            art: Image::default(),
            explicit: t.explicit,
            current,
            unavailable: t.unavailable,
            is_footer: false,
        }
    }

    /// A row became visible: load its artwork. `list` 0 = page, 1 = queue.
    fn row_shown(&mut self, list: i32, i: usize) {
        let (thumbs, rows, size) = if list == 0 {
            let Some(t) = self.page_tracks.get(i) else {
                return;
            };
            if t.plays.is_none() && !self.plays.contains_key(&t.video_id) {
                self.ask_plays(t.video_id.clone());
            }
            let t = &self.page_tracks[i];
            (t.thumbs.clone(), self.page_rows.clone(), ROW_ART)
        } else {
            let Some(t) = self.queue_ids.get(i).and_then(|id| self.known.get(id)) else {
                return;
            };
            (t.thumbs.clone(), self.queue_rows.clone(), QUEUE_ART)
        };
        let px = self.px(size);
        let Some(url) = images::url_for(&thumbs, px) else {
            return;
        };
        if let Some(img) = self.images.get(&url, px, image_loaded)
            && let Some(mut r) = rows.row_data(i)
        {
            r.art = img;
            rows.set_row_data(i, r);
        }
    }

    /// Looks up a track's play count (once per run), then fills it into the page.
    fn ask_plays(&mut self, id: String) {
        if !self.plays_asked.insert(id.clone()) {
            return;
        }
        let client = self.client.clone();
        workers::run(
            move || (client.play_count(&id), id),
            |(r, id)| {
                with(|a| match r {
                    Ok(Some(n)) => {
                        a.plays
                            .insert(id.clone(), (n, music_core::paths::unix_now()));
                        a.save_plays_later();
                        for (i, t) in a.page_tracks.iter_mut().enumerate() {
                            if t.video_id == id {
                                t.plays = Some(n);
                                if let Some(mut r) = a.page_rows.row_data(i) {
                                    r.plays = fmt_count(n).into();
                                    a.page_rows.set_row_data(i, r);
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => log::warn!("plays {id}: {e}"),
                });
            },
        );
    }

    fn save_plays_later(&mut self) {
        self.plays_timer
            .start(TimerMode::SingleShot, Duration::from_secs(5), || {
                with(|a| settings::save_plays(&a.plays));
            });
    }

    /// An image arrived: put it everywhere it's wanted.
    fn image_arrived(&mut self, url: String, img: Image) {
        self.images.insert(&url, img.clone());
        if self.minimized || img.size().width == 0 {
            return;
        }
        let row_px = self.px(ROW_ART);
        for (i, t) in self.page_tracks.iter().enumerate() {
            if images::url_for(&t.thumbs, row_px).as_deref() == Some(url.as_str())
                && let Some(mut r) = self.page_rows.row_data(i)
            {
                r.art = img.clone();
                self.page_rows.set_row_data(i, r);
            }
        }
        let q_px = self.px(QUEUE_ART);
        for (i, id) in self.queue_ids.iter().enumerate() {
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
        if let Some(acc) = &self.account
            && images::url_for(&acc.photo, self.px(ACCOUNT_ART)).as_deref() == Some(url.as_str())
        {
            g.set_account_photo(img);
        }
    }

    // ---- playback --------------------------------------------------------------------------

    fn play_page(&mut self, i: i32) {
        let playable: Vec<&Track> = self.page_tracks.iter().filter(|t| !t.unavailable).collect();
        if playable.is_empty() {
            return;
        }
        let start = if i < 0 {
            self.player.set_shuffle(true);
            (music_core::paths::unix_now() as usize) % playable.len()
        } else {
            let id = &self.page_tracks[i as usize].video_id;
            playable.iter().position(|t| &t.video_id == id).unwrap_or(0)
        };
        let ids: Vec<String> = playable.iter().map(|t| t.video_id.clone()).collect();
        for t in playable {
            self.known.insert(t.video_id.clone(), t.clone());
        }
        self.resume_at = None;
        self.player.play_queue(ids, start);
        self.queue_changed();
        self.show_now_playing(false);
    }

    fn play_pause(&mut self) {
        match self.player.engine.position() {
            Some(p) if p.paused => self.player.engine.resume(),
            Some(_) => self.player.engine.pause(),
            None => self.player.play_current(),
        }
        self.tick();
    }

    fn set_volume(&mut self, v: f32) {
        self.settings.volume = v;
        // Perceptual curve; the top stays bit-exact.
        self.player.engine.set_volume(v * v);
        self.save_later();
    }

    fn eq_changed(&mut self) {
        self.player.engine.set_eq(&self.settings.eq);
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_eq_enabled(self.settings.eq.enabled);
        g.set_eq_gains(ModelRc::new(VecModel::from(
            self.settings.eq.gains.to_vec(),
        )));
        g.set_eq_preset(self.settings.eq_preset.clone().into());
        self.save_later();
    }

    fn on_engine(&mut self, e: Event) {
        self.player.on_event(&e);
        match &e {
            Event::Started { id, .. } => {
                if let Some(ms) = self.resume_at.take().filter(|&ms| ms > 5000) {
                    self.player.engine.seek(ms);
                }
                self.queue_changed();
                self.show_now_playing(true);
                if self.client.signed_in() {
                    let (client, id) = (self.client.clone(), id.clone());
                    workers::spawn(move || {
                        if let Err(e) = client.report_play(&id) {
                            log::warn!("history: {e}");
                        }
                    });
                }
            }
            Event::Finished => self.tick(),
            Event::Failed { id, error } => log::warn!("{id}: {error}"),
            Event::Device(d) => log::info!("device: {d}"),
        }
    }

    /// Updates the bar, media flyout and taskbar for the current track.
    fn show_now_playing(&mut self, started: bool) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let Some(t) = self
            .player
            .current()
            .and_then(|id| self.known.get(id))
            .cloned()
        else {
            g.set_np(NowPlaying::default());
            return;
        };
        let px = self.px(BAR_ART);
        let art = images::url_for(&t.thumbs, px)
            .and_then(|u| self.images.get(&u, px, image_loaded))
            .unwrap_or_default();
        let np = NowPlaying {
            active: true,
            title: t.title.clone().into(),
            artist: t.artist_line().into(),
            art,
            duration_ms: t.duration_ms.unwrap_or(0) as i32,
            position_ms: if started {
                0
            } else {
                self.resume_at.unwrap_or(0) as i32
            },
            playing: started,
            loading: false,
            liked: t.like == Some(Like::Like),
        };
        g.set_np(np);
        if let Some(m) = &self.media {
            let art_url = images::url_for(&t.thumbs, 300);
            let album = t.album.as_ref().map(|a| a.name.as_str()).unwrap_or("");
            m.track(&t.title, &t.artist_line(), album, art_url.as_deref());
        }
        self.tick();
    }

    /// Reads the engine position; keeps a 4 Hz timer running only while playing and visible.
    fn tick(&mut self) {
        let pos = self.player.engine.position();
        let playing = pos.as_ref().is_some_and(|p| !p.paused);
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut np = g.get_np();
        if let Some(p) = &pos {
            np.position_ms = p.ms as i32;
            if let Some(d) = p.duration_ms {
                np.duration_ms = d as i32;
            }
        }
        if np.playing != playing {
            np.playing = playing;
            if let Some(m) = &self.media {
                m.status(if np.active { Some(playing) } else { None });
            }
            if let Some(h) = self.hwnd {
                win::taskbar_playing(h, playing);
            }
            if !playing {
                self.save_queue();
            }
        }
        g.set_np(np);
        let want = playing && !self.minimized;
        if want && !self.position_timer.running() {
            self.position_timer
                .start(TimerMode::Repeated, Duration::from_millis(250), || {
                    with(|a| a.tick());
                });
        } else if !want && self.position_timer.running() {
            self.position_timer.stop();
        }
    }

    /// Rebuilds the queue panel and marks the playing row on the page.
    fn queue_changed(&mut self) {
        let (order, at) = self.player.up_next();
        self.queue_ids = order.iter().map(|s| s.to_string()).collect();
        let rows: Vec<TrackRow> = self
            .queue_ids
            .iter()
            .enumerate()
            .map(|(i, id)| match self.known.get(id) {
                Some(t) => self.row_for(t, i == at, true),
                None => TrackRow {
                    title: id.clone().into(),
                    current: i == at,
                    ..Default::default()
                },
            })
            .collect();
        self.queue_rows.set_vec(rows);
        let current = self.player.current().map(str::to_string);
        for (i, t) in self.page_tracks.iter().enumerate() {
            let is = current.as_deref() == Some(t.video_id.as_str());
            if let Some(mut r) = self.page_rows.row_data(i)
                && r.current != is
            {
                r.current = is;
                self.page_rows.set_row_data(i, r);
            }
        }
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_queue_current(at as i32);
        g.set_shuffle(self.player.shuffle());
        g.set_repeat(match self.player.repeat {
            Repeat::Off => 0,
            Repeat::All => 1,
            Repeat::One => 2,
        });
        self.save_later();
    }

    fn save_queue(&self) {
        let (order, at) = self.player.up_next();
        SavedQueue {
            tracks: order
                .iter()
                .filter_map(|id| self.known.get(*id).cloned())
                .collect(),
            at,
            position_ms: self
                .player
                .engine
                .position()
                .map(|p| p.ms)
                .or(self.resume_at)
                .unwrap_or(0),
            shuffle: self.player.shuffle(),
            repeat: self.player.repeat,
        }
        .save();
    }

    /// Saves settings and queue a second after the last change.
    fn save_later(&mut self) {
        self.save_timer
            .start(TimerMode::SingleShot, Duration::from_secs(1), || {
                with(|a| {
                    a.settings.save();
                    a.save_queue();
                });
            });
    }

    fn toggle_like(&mut self) {
        let Some(id) = self.player.current().map(str::to_string) else {
            return;
        };
        let Some(t) = self.known.get_mut(&id) else {
            return;
        };
        let like = if t.like == Some(Like::Like) {
            Like::Indifferent
        } else {
            Like::Like
        };
        t.like = Some(like);
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut np = g.get_np();
        np.liked = like == Like::Like;
        g.set_np(np);
        let client = self.client.clone();
        workers::spawn(move || {
            if let Err(e) = client.rate(&id, like) {
                log::warn!("like: {e}");
            }
        });
    }

    fn panel_changed(&mut self) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        self.settings.panel_open = g.get_panel_open();
        self.settings.panel_tab = g.get_panel_tab();
        self.save_later();
    }

    fn remote(&mut self, r: Remote) {
        log::info!("remote: {r:?}");
        match r {
            Remote::PlayPause => self.play_pause(),
            Remote::Play => {
                if self.player.engine.position().is_none_or(|p| p.paused) {
                    self.play_pause();
                }
            }
            Remote::Pause => {
                if self.player.engine.position().is_some_and(|p| !p.paused) {
                    self.play_pause();
                }
            }
            Remote::Next => self.player.next(),
            Remote::Previous => self.player.previous(),
            Remote::Minimized => self.minimize(),
            Remote::Restored => self.restore(),
        }
    }

    // ---- minimize ---------------------------------------------------------------------------

    /// While minimized nothing is drawn: drop every decoded image and give memory back.
    fn minimize(&mut self) {
        if self.minimized {
            return;
        }
        self.minimized = true;
        self.position_timer.stop();
        self.images.clear();
        let blank = |rows: &VecModel<TrackRow>| {
            for i in 0..rows.row_count() {
                if let Some(mut r) = rows.row_data(i)
                    && r.art.size().width > 0
                {
                    r.art = Image::default();
                    rows.set_row_data(i, r);
                }
            }
        };
        blank(&self.page_rows);
        blank(&self.queue_rows);
        for i in 0..self.playlist_rows.row_count() {
            if let Some(mut r) = self.playlist_rows.row_data(i) {
                r.cover = Image::default();
                self.playlist_rows.set_row_data(i, r);
            }
        }
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut np = g.get_np();
        np.art = Image::default();
        g.set_np(np);
        g.set_account_photo(Image::default());
        // Let the renderer release its textures first.
        Timer::single_shot(Duration::from_millis(300), win::trim_working_set);
    }

    fn restore(&mut self) {
        if !self.minimized {
            return;
        }
        self.minimized = false;
        self.show_now_playing(self.player.engine.position().is_some_and(|p| !p.paused));
        self.account_images();
        for i in 0..self.playlists.len() {
            self.sidebar_cover(i);
        }
        // Visible rows ask again as they're drawn; reload the first screenful now.
        for i in 0..self.page_rows.row_count().min(30) {
            self.row_shown(0, i);
        }
        let at = self.player.up_next().1;
        for i in at.saturating_sub(5)..(at + 15).min(self.queue_ids.len()) {
            self.row_shown(1, i);
        }
    }

    // ---- account ---------------------------------------------------------------------------

    fn sign_in(&mut self) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_signing_in(true);
        let client = self.client.clone();
        // The sign-in window runs its own message loop on its own thread.
        std::thread::spawn(move || {
            let dir = music_core::paths::cache_dir()
                .parent()
                .expect("cache dir")
                .join("webview");
            let r = signin::sign_in(&dir)
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
                    }
                    a.account_changed();
                    a.load_page();
                });
            });
        });
    }

    fn sign_out(&mut self) {
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
    fn account_changed(&mut self) {
        let signed_in = self.client.signed_in();
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_signed_in(signed_in);
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
        let client = self.client.clone();
        workers::run(
            move || client.library_playlists(),
            |r| {
                with(|a| match r {
                    Ok(f) => {
                        a.playlists = f
                            .shelves
                            .into_iter()
                            .flat_map(|s| s.items)
                            .filter_map(|i| match i {
                                Item::Playlist(p) if p.id != "VLLM" => Some(p),
                                _ => None,
                            })
                            .collect();
                        let rows: Vec<SidebarPlaylist> = a
                            .playlists
                            .iter()
                            .map(|p| SidebarPlaylist {
                                id: p.id.clone().into(),
                                title: p.title.clone().into(),
                                cover: Image::default(),
                            })
                            .collect();
                        a.playlist_rows.set_vec(rows);
                        for i in 0..a.playlists.len() {
                            a.sidebar_cover(i);
                        }
                    }
                    Err(e) => log::warn!("playlists: {e}"),
                });
            },
        );
    }

    fn account_images(&mut self) {
        let Some(acc) = &self.account else { return };
        let px = self.px(ACCOUNT_ART);
        if let Some(url) = images::url_for(&acc.photo, px)
            && let Some(img) = self.images.get(&url, px, image_loaded)
        {
            self.ui().global::<Ui>().set_account_photo(img);
        }
    }

    fn sidebar_cover(&mut self, i: usize) {
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

fn image_loaded(url: String, img: Image) {
    with(|a| a.image_arrived(url, img));
}

fn tracks_of(items: Vec<Item>) -> Vec<Track> {
    items
        .into_iter()
        .filter_map(|i| match i {
            Item::Track(t) => Some(t),
            _ => None,
        })
        .collect()
}

/// "237 tracks, 14 hours 3 minutes" / "12 tracks, 48 minutes".
fn summary_line(tracks: &[Track]) -> String {
    let plural = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mins = tracks.iter().filter_map(|t| t.duration_ms).sum::<u64>() / 60_000;
    let time = match (mins / 60, mins % 60) {
        (0, m) => plural(m, "minute", "minutes"),
        (h, 0) => plural(h, "hour", "hours"),
        (h, m) => format!(
            "{} {}",
            plural(h, "hour", "hours"),
            plural(m, "minute", "minutes")
        ),
    };
    format!("{}, {time}", plural(tracks.len() as u64, "track", "tracks"))
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

fn fmt_ms(ms: u64) -> String {
    format!("{}:{:02}", ms / 60000, ms / 1000 % 60)
}

/// A small file logger: `app.log` in the local app folder, the previous run kept as `app.1.log`.
mod logger {
    use std::io::Write;
    use std::sync::Mutex;

    struct FileLog(Mutex<std::fs::File>);

    impl log::Log for FileLog {
        fn enabled(&self, m: &log::Metadata) -> bool {
            m.level() <= log::Level::Info || m.target().starts_with("music")
        }
        fn log(&self, r: &log::Record) {
            if !self.enabled(r.metadata()) {
                return;
            }
            let line = format!(
                "{} [{:5}] {}\n",
                music_core::paths::unix_now(),
                r.level(),
                r.args()
            );
            if cfg!(debug_assertions) {
                eprint!("{line}");
            }
            let _ = self.0.lock().unwrap().write_all(line.as_bytes());
        }
        fn flush(&self) {}
    }

    pub fn init() {
        let dir = music_core::paths::cache_dir()
            .parent()
            .expect("cache dir")
            .to_path_buf();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("app.log");
        let _ = std::fs::rename(&path, dir.join("app.1.log"));
        if let Ok(f) = std::fs::File::create(&path) {
            let _ = log::set_boxed_logger(Box::new(FileLog(Mutex::new(f))));
            log::set_max_level(log::LevelFilter::Info);
        }
    }
}
