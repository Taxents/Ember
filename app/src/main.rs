//! Ember: one window, the audio engine, and the YouTube Music client.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod images;
mod library;
mod listen;
mod logos;
mod lyric_view;
mod menus;
mod nav;
mod pages;
mod playback;
mod queue_view;
mod radio;
mod rows;
mod settings;
mod suggest;
mod toast;
mod update;
mod video;
mod win;
mod workers;

use images::Images;
use music_core::audio::{Engine, Event};
use music_core::lyrics;
use music_core::player::{Player, Repeat};
use music_core::yt::model::*;
use music_core::yt::{Client, Resolver, Session};
use pages::{Block, Extra, HeroPlay, Loaded, More};
use settings::{SavedQueue, Settings};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use win::Remote;
use windows::Win32::Foundation::HWND;

slint::include_modules!();

/// Sizes artwork is shown at, in logical pixels.
const BAR_ART: f32 = 52.0;
const SIDEBAR_ART: f32 = 18.0;
const ACCOUNT_ART: f32 = 28.0;
const QUEUE_ART: f32 = 36.0;
const COLLECTION_ART: f32 = 44.0;
const HEADER_ART: f32 = 300.0;
const CARD_ART: f32 = 200.0;
/// Logos are fetched up to twice this wide.
const LOGO_ART: u32 = 420;
/// The banner is blurred from a small copy of the cover, in physical pixels.
const BANNER_SOURCE: u32 = 226;

/// How long a page seen is shown again at once (while it's fetched again), and how many are kept.
const PAGE_KEPT: Duration = Duration::from_secs(20 * 60);
const PAGES_KEPT: usize = 40;

/// A page's key among those kept.
fn page_key(r: &Route) -> String {
    format!("{:?}|{}", r.kind, r.id)
}

#[derive(Clone, PartialEq)]
struct Route {
    kind: PageKind,
    id: String,
    title: String,
}

/// What a row of the page list is.
enum RowRef {
    /// A placeholder while the page loads.
    Skeleton,
    Header,
    Title,
    Footer,
    /// A heading over tracks, maybe with "See all".
    Heading(Option<BrowseRef>),
    /// Column titles over a track list.
    Columns,
    /// A song's lyrics and credits, by its video id.
    Details(String),
    /// A track, by its index in `page_tracks`.
    Track(usize),
    /// A shelf of cards, maybe with "See all".
    Shelf(Vec<Item>, Option<BrowseRef>),
    /// A grid line, by the index of its first card in `grid`.
    Grid(usize),
}

/// What the open right-click menu acts on.
enum MenuTarget {
    /// A track on the page: its index in `page_tracks`, and its row.
    Track(usize, usize),
    /// A queue position (and `menu_queue_id`, the song there when the menu opened).
    Queue(usize),
    /// A sidebar playlist, by id.
    Playlist(String),
    /// Confirming a playlist's deletion, by id.
    DeletePlaylist(String),
    /// Choosing between a song's artists.
    Artists,
    /// Choosing a playlist to add these songs to.
    AddTo(Vec<String>),
    /// A card's album, playlist, artist or song.
    Card(Box<Item>),
}

/// What to do with the tracks of something that isn't on the page yet.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Play,
    Shuffle,
    Next,
    Queue,
}

/// A queue YouTube's radio fills: the song it grows from and the choices it offers.
struct Autoplay {
    seed: String,
    chips: Vec<Chip>,
    picked: usize,
    /// The `play_seq` of the request in flight: an older one arriving later is dropped.
    seq: u64,
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
    /// The queue is a radio growing from this song, with YouTube's choices for it.
    autoplay: Option<Autoplay>,

    history: Vec<Route>,
    /// Where each page in `history` was scrolled to when it was left, and where the one being
    /// opened by Back/Forward goes back to once its rows are in.
    scrolls: HashMap<usize, f32>,
    scroll_wanted: Option<f32>,
    at: usize,
    /// Bumped on every navigation, so late results for an old page are dropped.
    generation: u64,
    page_tracks: Vec<Track>,
    /// Tracks in one group play together: 0 is the page's own list, then each headed list.
    page_groups: Vec<usize>,
    page_rows: rows::PageRows,
    /// What each of `page_rows` is.
    rows: Vec<RowRef>,
    /// The row of the page's first own track, and how many it has.
    tracks_at: usize,
    main_len: usize,
    /// The card grid, which is always last, from row `grid_at`; laid out for (columns, cards).
    grid: Vec<Item>,
    /// Which page rows (and cards in them) show each artwork URL, so an arriving picture
    /// doesn't walk the whole page; rebuilt when its key (what it was built from) goes stale.
    art_index: HashMap<String, Vec<(usize, Option<usize>)>>,
    art_index_key: (u64, usize, usize, usize, Option<u32>, u32),
    grid_at: usize,
    grid_laid: (usize, usize),
    columns: usize,
    more: Option<More>,
    loading_more: bool,
    /// The page ends with "237 tracks, 14 hours 3 minutes".
    summary: bool,
    /// The songs recommended under a playlist.
    suggested: suggest::Suggested,
    /// The banner, which is the list's first row.
    header: Option<pages::Header>,
    /// The artist's logo on the banner, once found.
    logo_url: Option<String>,
    menu: Option<MenuTarget>,
    /// The song a queue row's menu was opened on.
    menu_queue_id: Option<String>,
    /// The artists the open menu's "Go to" entries lead to.
    menu_artists: Vec<Named>,
    /// Where the menu opened, for a second menu in its place.
    menu_at: slint::LogicalPosition,
    /// Play counts looked up for rows that don't carry one: video id → (count, unix time).
    plays: HashMap<String, (u64, u64)>,
    plays_asked: std::collections::HashSet<String>,
    /// Songs whose stats are wanted once scrolling settles (newest last), how many requests are
    /// out, and the settle timer: a fast scroll past hundreds of rows asks for none of them.
    stats_queue: std::collections::VecDeque<String>,
    stats_in_flight: usize,
    stats_timer: Timer,
    /// Bumped by every "play this": an older click's tracks arriving late don't replace it.
    play_seq: u64,
    /// Pages seen lately (by [`page_key`]), shown at once when opened again, then fetched again
    /// to catch changes.
    page_cache: HashMap<String, (Loaded, std::time::Instant)>,
    /// What was clicked to open the page: its banner shows before the page arrives.
    opened_from: Option<Item>,
    /// More of the page was loaded below: it isn't laid out again when it changes.
    extended: bool,
    /// Song lengths the player told us, for lists that leave them out.
    lengths: HashMap<String, u64>,

    queue_rows: Rc<VecModel<TrackRow>>,
    queue_ids: Vec<String>,
    /// Play-order position of the queue panel's first row (the song after the current one).
    queue_from: usize,
    /// The playing song's lyrics (empty while they load) and where the singing is.
    lyrics: Option<lyric_view::Follow>,
    playlists: Vec<PlaylistCard>,
    /// Your playlists hidden from the sidebar (Settings lists them).
    hidden_lists: Vec<PlaylistCard>,
    /// Songs for the playlist being named in the sidebar.
    naming_ids: Vec<String>,
    /// Bumped each time the sidebar's playlists are asked for: only the latest answer shows.
    playlists_seq: u64,
    /// Playlists just saved to (`Some`) or removed from (`None`) your library, shown so until
    /// YouTube's list (which lags a little) agrees.
    library_wish: Vec<(String, Option<PlaylistCard>)>,
    playlist_rows: Rc<VecModel<SidebarPlaylist>>,
    account: Option<Account>,

    /// Seek to this once the restored track starts.
    resume_at: Option<u64>,
    minimized: bool,
    /// A pause (`false`) or resume (`true`) asked of the engine and when, shown until it's taken.
    toggled: Option<(bool, std::time::Instant)>,
    /// The signed-out toast was shown for the current expiry.
    told_expired: bool,
    /// Runs while skips come quickly; when it ends, the song they stopped at plays.
    skip_timer: Timer,
    /// Skips moved past songs without playing them (see [`App::skip`]).
    skip_pending: bool,
    /// What the media flyout and taskbar were last told (playing or not); `None` after a new
    /// song, so the next tick tells them again.
    reported_playing: Option<bool>,
    /// The flyout's timeline was last set at this second of the song (and for this length).
    reported_timeline: (u64, u64),
    /// Since when the current song has had no audible position (see [`App::tick`]).
    waiting_since: Option<std::time::Instant>,
    /// Reads the band levels for the visualizer while playing (about 30 times a second).
    meter_timer: Timer,
    /// Meter ticks so far: the clock moves every 8th, levels every 2nd (see [`App::meter_tick`]).
    meter_ticks: u32,
    /// Per band: the recent peak (for scale) and the shown level.
    meter: [(f32, f32); 4],
    levels: Rc<VecModel<f32>>,
    save_timer: Timer,
    plays_timer: Timer,
    /// Sizes the page list waits to lay out at (see [`App::page_metrics`]).
    metrics: Option<rows::Metrics>,
    metrics_timer: Timer,
    /// Listen Along, while in a session.
    together: Option<listen::Session>,
    together_timer: Timer,
    toast: toast::Toast,
    /// The song playing and its length, until it's been heard long enough to count as played.
    play_report: Option<(String, Option<u64>)>,
    /// Lyrics were shown this session (see [`App::lyrics_wanted`]).
    lyrics_opened: bool,
    /// A request failed on the network; `offline_timer` checks until one gets through.
    offline: bool,
    offline_timer: Timer,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// The page on screen (its generation), for workers to drop look-ups for pages since left.
static PAGE: AtomicU64 = AtomicU64::new(0);

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
    video::init(resolver.clone());
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
        scrolls: HashMap::new(),
        scroll_wanted: None,
        at: 0,
        generation: 0,
        art_index: HashMap::new(),
        art_index_key: Default::default(),
        page_tracks: Vec::new(),
        page_groups: Vec::new(),
        page_rows: rows::PageRows::new(),
        rows: Vec::new(),
        tracks_at: 0,
        main_len: 0,
        grid: Vec::new(),
        grid_at: 0,
        grid_laid: (0, 0),
        columns: 5,
        more: None,
        loading_more: false,
        summary: false,
        suggested: Default::default(),
        header: None,
        logo_url: None,
        menu: None,
        menu_queue_id: None,
        menu_artists: Vec::new(),
        menu_at: Default::default(),
        autoplay: None,
        plays: settings::load_plays(),
        plays_asked: Default::default(),
        stats_queue: Default::default(),
        stats_in_flight: 0,
        stats_timer: Timer::default(),
        play_seq: 0,
        playlists_seq: 0,
        library_wish: Vec::new(),
        page_cache: HashMap::new(),
        opened_from: None,
        extended: false,
        lengths: HashMap::new(),
        queue_rows: Rc::new(VecModel::default()),
        queue_ids: Vec::new(),
        queue_from: 0,
        lyrics: None,
        playlists: Vec::new(),
        hidden_lists: Vec::new(),
        naming_ids: Vec::new(),
        playlist_rows: Rc::new(VecModel::default()),
        account: None,
        resume_at: None,
        minimized: false,
        toggled: None,
        told_expired: false,
        skip_timer: Timer::default(),
        skip_pending: false,
        waiting_since: None,
        reported_playing: None,
        reported_timeline: (u64::MAX, 0),
        together: None,
        together_timer: Timer::default(),
        meter_timer: Timer::default(),
        meter_ticks: 0,
        meter: [(0.0, 0.0); 4],
        levels: Rc::new(VecModel::from(vec![0.0; 4])),
        save_timer: Timer::default(),
        plays_timer: Timer::default(),
        metrics: None,
        metrics_timer: Timer::default(),
        toast: Default::default(),
        play_report: None,
        lyrics_opened: false,
        offline: false,
        offline_timer: Timer::default(),
    };
    APP.with(|a| *a.borrow_mut() = Some(app));

    wire(&ui);
    with(|a| a.start());
    forward_engine_events();
    workers::spawn(images::prune_disk);
    update::clean_up();
    update::watch(update::show);
    // A sign-in that crashed or was killed leaves a browser profile holding Google cookies.
    workers::spawn(|| signin::clean_up(&webview_dir()));

    // Window handle work needs the native window, which exists once the event loop runs.
    let _ = slint::invoke_from_event_loop(|| {
        with(|a| a.attach_window());
    });
    ui.run().expect("event loop");
    with(|a| a.shutdown());
    // Drop the app (and the engine's threads) here, not during thread-local teardown.
    let app = APP.with(|a| a.borrow_mut().take());
    drop(app);
    if update::RESTART.load(std::sync::atomic::Ordering::Relaxed) {
        update::restart();
    }
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
    toast::wire(&g);
    g.on_retry_page(|| {
        with(|a| a.reload());
    });
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
        use music_core::yt::uploads::{Pasted, pasted};
        // A pasted link opens what it points to.
        let route = match pasted(&q) {
            Some(Pasted::Video(id)) => Route {
                kind: PageKind::Song,
                id,
                title: String::new(),
            },
            Some(Pasted::Playlist(id)) => Route {
                kind: PageKind::Playlist,
                id,
                title: String::new(),
            },
            None => Route {
                kind: PageKind::Search,
                id: q.to_string(),
                title: q.to_string(),
            },
        };
        with(|a| a.navigate(route));
    });
    g.on_sign_in(|| {
        with(|a| a.sign_in());
    });
    g.on_context(|what, row, id, at| {
        with(|a| a.context(&what, row.max(0) as usize, id.to_string(), at));
    });
    g.on_toggle_saved(|| {
        with(|a| a.toggle_saved());
    });
    g.on_toggle_follow(|| {
        with(|a| a.toggle_follow());
    });
    g.on_open_artist(|row, at| {
        with(|a| a.open_artist(row, at));
    });
    g.on_menu_chosen(|choice| {
        with(|a| a.menu_chosen(&choice));
    });
    g.on_sign_out(|| {
        with(|a| a.sign_out());
    });
    g.on_restart_to_update(|| {
        update::RESTART.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = slint::quit_event_loop();
    });
    g.on_np_open(|what| {
        with(|a| a.np_open(&what));
    });
    g.on_check_updates(|| update::check_now(update::show));
    update::wire(&g);
    g.on_clear_cache(|| {
        workers::run(
            || {
                clear_dir(&music_core::paths::cache_dir());
                dir_size(&music_core::paths::cache_dir())
            },
            |size| {
                with(|a| a.ui().global::<Ui>().set_cache_size(fmt_bytes(size).into()));
            },
        );
    });
    g.on_together_opened(|| {
        with(|a| a.together_opened());
    });
    g.on_together_start(|| {
        with(|a| a.together_start());
    });
    g.on_together_join(|code| {
        with(|a| a.together_join(&code));
    });
    g.on_together_copy(|| {
        with(|a| a.together_copy());
    });
    g.on_together_leave(|| {
        with(|a| a.together_leave(""));
    });
    g.on_together_resync(|| {
        with(|a| a.together_resync());
    });
    g.on_together_name_changed(|name| {
        with(|a| {
            a.settings.together_name = name.trim().to_string();
            a.save_later();
        });
    });
    g.on_move_playlist(|from, to| {
        with(|a| a.move_playlist(from.max(0) as usize, to.max(0) as usize));
    });
    g.on_unhide_playlist(|id| {
        with(|a| {
            a.settings
                .hidden_playlists
                .retain(|h| h.as_str() != id.as_str());
            a.settings.save();
            a.forget_pages(PageKind::AllPlaylists);
            a.load_playlists();
        });
    });
    g.on_lyrics_third_party_changed(|on| {
        pages::THIRD_PARTY_LYRICS.store(on, std::sync::atomic::Ordering::Relaxed);
        with(|a| {
            a.settings.lyrics_third_party = on;
            a.save_later();
        });
    });
    g.on_open_data_folder(|| {
        let _ = std::process::Command::new("explorer.exe")
            .arg(music_core::paths::config_dir())
            .spawn();
    });
    g.on_create_playlist(|name| {
        with(|a| a.create_playlist(name.to_string()));
    });
    g.on_new_playlist(|| {
        with(|a| a.new_playlist(Vec::new()));
    });
    g.on_rename(|name| {
        with(|a| a.rename_playlist(name.to_string()));
    });
    g.on_autoplay_chip(|i| {
        with(|a| a.autoplay_chip(i as usize));
    });
    g.on_radio_from_current(|| {
        with(|a| {
            let current = a.player.current().and_then(|id| a.known.get(id)).cloned();
            if let Some(t) = current {
                a.autoplay_from(t, None);
            }
        });
    });
    g.on_play_tracks(|i| {
        with(|a| a.play_page(i));
    });
    g.on_open_album(|i| {
        with(|a| a.open_album(i.max(0) as usize));
    });
    g.on_track_move(|from, to| {
        with(|a| a.track_move(from.max(0) as usize, to.max(0) as usize));
    });
    g.on_page_metrics(|banner_h, grid_card, view_h, view_w| {
        with(|a| {
            let ui = a.ui();
            let g = ui.global::<Ui>();
            a.page_metrics(rows::Metrics {
                banner_h,
                grid_card,
                shelf_card: 176.0,
                view_h,
                view_w,
                playable: g.get_page().playable,
                video: g.get_video_on(),
            })
        });
    });
    g.on_page_scrolled(|top| {
        with(|a| a.page_rows.scrolled(top));
    });
    g.on_row_measured(|i, h| {
        with(|a| a.page_rows.set_measured(i.max(0) as usize, h));
    });
    g.on_near_end(|| {
        with(|a| a.load_more());
    });
    g.on_row_shown(|list, i| {
        with(|a| a.row_shown(list, i as usize));
    });
    g.on_columns_changed(|n| {
        with(|a| a.columns_changed(n.max(1) as usize));
    });
    g.on_card_clicked(|row, i| {
        with(|a| a.card_clicked(row.max(0) as usize, i.max(0) as usize));
    });
    g.on_card_play(|row, i| {
        with(|a| a.card_play(row.max(0) as usize, i.max(0) as usize));
    });
    g.on_card_shown(|row, i| {
        with(|a| a.card_shown(row.max(0) as usize, i.max(0) as usize));
    });
    g.on_suggestion_add(|row| {
        with(|a| a.suggestion_add(row.max(0) as usize));
    });
    g.on_suggestions_refresh(|| {
        with(|a| a.suggestions_refresh());
    });
    g.on_see_all(|row| {
        with(|a| a.see_all(row.max(0) as usize));
    });
    g.on_play_pause(|| {
        with(|a| a.play_pause());
    });
    g.on_next(|| {
        with(|a| a.skip(1));
    });
    g.on_previous(|| {
        with(|a| a.skip(-1));
    });
    g.on_seek(|ms| {
        with(|a| a.seek(ms.max(0) as u64));
    });
    g.on_set_volume(|v| {
        with(|a| a.set_volume(v));
    });
    g.on_toggle_shuffle(|| {
        with(|a| {
            let on = !a.player.shuffle();
            if !a.ask_host(listen::Req::Shuffle(on)) {
                a.player.set_shuffle(on);
                a.queue_changed();
            }
        });
    });
    g.on_cycle_repeat(|| {
        with(|a| {
            let r = match a.player.repeat {
                Repeat::Off => Repeat::All,
                Repeat::All => Repeat::One,
                Repeat::One => Repeat::Off,
            };
            let code = match r {
                Repeat::Off => 0,
                Repeat::All => 1,
                Repeat::One => 2,
            };
            if !a.ask_host(listen::Req::Repeat(code)) {
                a.player.set_repeat(r);
                a.queue_changed();
            }
        });
    });
    g.on_toggle_like(|| {
        with(|a| a.toggle_like());
    });
    g.on_panel_changed(|| {
        with(|a| a.panel_changed());
    });
    g.on_queue_jump(|i| {
        with(|a| {
            let pos = a.queue_from + i as usize;
            if !a.ask_host(listen::Req::Jump(pos)) {
                a.cancel_skip();
                a.player.jump(pos);
            }
        });
    });
    g.on_queue_remove(|i| {
        with(|a| {
            let pos = a.queue_from + i as usize;
            if !a.ask_host(listen::Req::Remove(pos)) {
                a.player.remove(pos);
                a.queue_changed();
            }
        });
    });
    g.on_queue_move(|from, to| {
        with(|a| {
            let (from, to) = (a.queue_from + from as usize, a.queue_from + to as usize);
            if !a.ask_host(listen::Req::Move(from, to)) {
                a.player.move_item(from, to);
                a.queue_changed();
            }
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
        g.set_tracks(self.page_rows.model());
        g.set_app_version(env!("CARGO_PKG_VERSION").into());
        let weak = ui.as_weak();
        self.page_rows.on_size(move |h, n| {
            if let Some(ui) = weak.upgrade() {
                let g = ui.global::<Ui>();
                g.set_page_height(h);
                g.set_page_count(n as i32);
            }
        });
        g.set_queue(ModelRc::from(self.queue_rows.clone()));
        g.set_playlists(ModelRc::from(self.playlist_rows.clone()));
        g.set_volume(self.settings.volume);
        g.set_sidebar_collapsed(self.settings.sidebar_collapsed);
        g.set_lyrics_third_party(self.settings.lyrics_third_party);
        pages::THIRD_PARTY_LYRICS.store(
            self.settings.lyrics_third_party,
            std::sync::atomic::Ordering::Relaxed,
        );
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
            // Shuffled: the queue's own order back from where each song came from, so it plays
            // on in the same order and shuffle off goes back to the one before.
            let mut queue = vec![String::new(); ids.len()];
            let exact = q.shuffle
                && q.shuffled_from.len() == ids.len()
                && q.shuffled_from.iter().all(|&i| i < ids.len())
                && {
                    for (k, &i) in q.shuffled_from.iter().enumerate() {
                        queue[i] = ids[k].clone();
                    }
                    self.player
                        .restore_shuffled(queue, q.shuffled_from.clone(), q.at)
                };
            if !exact {
                self.player.restore(ids, q.at);
                if q.shuffle {
                    self.player.set_shuffle(true);
                }
            }
            self.player.set_repeat(q.repeat);
            self.resume_at = Some(q.position_ms);
            // Still a radio: its choices show, and it isn't started again from scratch.
            self.autoplay = q.autoplay.map(|(seed, chips, picked)| Autoplay {
                seed,
                chips,
                picked,
                seq: 0,
            });
            self.show_autoplay();
            self.queue_changed();
            self.show_now_playing(false);
            // The panel showing lyrics shows the restored song's, not an empty page.
            if self.lyrics_wanted() {
                self.lyrics_for_current();
            }
        } else {
            g.set_lyrics_note("Play something to see its lyrics here.".into());
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
        // Where it was, or (a first start, or an older version's size only) where Windows put
        // it, moved and shrunk to fit the screen.
        if let Some((x, y, w, h, maximized)) = self.settings.place.or_else(|| win::placement(hwnd))
        {
            win::place(hwnd, x, y, w, h, maximized);
        }
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
        // A change waiting on its toast's Undo goes through.
        self.close_toast();
        workers::finish_ordered(Duration::from_secs(3));
        let size = self.ui().window().size().to_logical(self.scale());
        self.settings.window = (size.width, size.height);
        if let Some(h) = self.hwnd
            && let Some(p) = win::placement(h)
        {
            self.settings.place = Some(p);
        }
        self.settings.save();
        self.save_queue();
        self.player.engine.stop();
    }

    /// A request failed on the network: says so, and checks every 10 s until it's back.
    fn went_offline(&mut self) {
        if self.offline {
            return;
        }
        self.offline = true;
        self.ui().global::<Ui>().set_offline(true);
        self.offline_timer
            .start(TimerMode::Repeated, Duration::from_secs(10), || {
                workers::run(
                    || {
                        music_core::net::agent()
                            .head("https://music.youtube.com/")
                            .call()
                            .is_ok()
                    },
                    |ok| {
                        if ok {
                            with(|a| a.back_online());
                        }
                    },
                );
            });
    }

    fn back_online(&mut self) {
        if !self.offline {
            return;
        }
        self.offline = false;
        self.offline_timer.stop();
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_offline(false);
        // A page that failed loads now.
        if !g.get_page().error.is_empty() {
            self.reload();
        }
    }

    // ---- minimize ---------------------------------------------------------------------------

    /// While minimized nothing is drawn: drop every decoded image and give memory back.
    fn minimize(&mut self) {
        if self.minimized {
            return;
        }
        self.minimized = true;
        self.video_sync();
        self.meter_timer.stop();
        self.images.clear();
        let blank = |rows: &dyn rows::RowStore| {
            for i in 0..rows.count() {
                let Some(mut r) = rows.get(i) else {
                    continue;
                };
                for j in 0..r.cards.row_count() {
                    if let Some(mut c) = r.cards.row_data(j)
                        && c.art.size().width > 0
                    {
                        c.art = Image::default();
                        r.cards.set_row_data(j, c);
                    }
                }
                if r.art.size().width > 0 {
                    r.art = Image::default();
                    rows.put(i, r);
                }
            }
        };
        blank(&self.page_rows);
        blank(&*self.queue_rows);
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
        let mut c = g.get_collection();
        c.cover = Image::default();
        c.banner = Image::default();
        c.logo = Image::default();
        g.set_collection(c);
        // Let the renderer release its textures first.
        Timer::single_shot(Duration::from_millis(300), win::trim_working_set);
    }

    fn restore(&mut self) {
        if !self.minimized {
            return;
        }
        self.minimized = false;
        self.video_sync();
        self.show_now_playing(self.player.engine.position().is_some_and(|p| !p.paused));
        self.account_images();
        for i in 0..self.playlists.len() {
            self.sidebar_cover(i);
        }
        self.header_images();
        // Visible rows ask again as they're drawn; reload the first screenful now.
        for i in 0..self.page_rows.row_count().min(30) {
            self.row_shown(0, i);
            let cards = self
                .page_rows
                .row_data(i)
                .map_or(0, |r| r.cards.row_count());
            for j in 0..cards {
                self.card_shown(i, j);
            }
        }
        for i in 0..self.queue_rows.row_count().min(15) {
            self.row_shown(1, i);
        }
    }

    /// Settings opened: what it shows that isn't kept up to date anyway.
    fn settings_opened(&mut self) {
        self.show_hidden_lists();
        workers::run(
            || dir_size(&music_core::paths::cache_dir()),
            |size| {
                with(|a| a.ui().global::<Ui>().set_cache_size(fmt_bytes(size).into()));
            },
        );
    }
}

fn image_loaded(url: String, img: Image) {
    with(|a| a.image_arrived(url, img));
}

/// "Like", or "Remove from liked" when it already is.
fn menu_entry(id: &str, label: &str, icon: &str, danger: bool) -> MenuEntry {
    MenuEntry {
        id: id.into(),
        label: label.into(),
        icon: icon.into(),
        danger,
    }
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

/// A new page: the page view goes back to the top.
/// Where sign-in's browser keeps its profile while it runs.
fn webview_dir() -> std::path::PathBuf {
    music_core::paths::cache_dir()
        .parent()
        .expect("cache dir")
        .join("webview")
}

/// How many square cards' widths a card takes.
fn card_units(video: bool) -> f32 {
    if video { 16.0 / 9.0 } else { 1.0 }
}

/// Sets where each card in a row starts, in card widths.
fn place_cards(cards: &mut [Card]) {
    let mut at = 0.0;
    for c in cards {
        c.at = at;
        at += card_units(c.video);
    }
}

/// How much a folder holds, all the way down.
fn dir_size(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Empties a folder; files in use stay.
fn clear_dir(dir: &std::path::Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let _ = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
    }
}

fn fmt_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{} MB", n >> 20),
        n => format!("{} KB", n.div_ceil(1024)),
    }
}

/// A small file logger: `app.log` in the local app folder, the previous run kept as `app.1.log`.
mod logger {
    use std::io::Write;
    use std::sync::Mutex;

    struct FileLog(Mutex<std::fs::File>);

    impl log::Log for FileLog {
        fn enabled(&self, m: &log::Metadata) -> bool {
            // Our own crates: the `ember` binary and `music_core`.
            m.level() <= log::Level::Info
                || m.target().starts_with("ember")
                || m.target().starts_with("music")
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
