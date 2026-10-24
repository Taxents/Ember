//! Ember: one window, the audio engine, and the YouTube Music client.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod images;
mod listen;
mod logos;
mod lyric_view;
mod pages;
mod radio;
mod rows;
mod settings;
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
    /// Another window is active: the clock moves once a second and the meter rests.
    unfocused: bool,
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
        unfocused: false,
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
    g.on_reset_lyrics_id(|| {
        workers::run_ordered(music_core::lyrics::reset_key_id, |()| {
            with(|a| a.notify("Unison has a new ID for this app"));
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
            self.player.restore(ids, q.at);
            self.player.set_repeat(q.repeat);
            if q.shuffle {
                self.player.set_shuffle(true);
            }
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

    // ---- navigation ------------------------------------------------------------------------

    fn navigate(&mut self, r: Route) {
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
    fn reload(&mut self) {
        let key = page_key(&self.history[self.at]);
        self.page_cache.remove(&key);
        self.load_page();
    }

    /// The kept copy of the page shown is out of date (it was changed here).
    fn forget_page(&mut self) {
        let key = page_key(&self.history[self.at]);
        self.page_cache.remove(&key);
    }

    /// Kept pages of `kind` are out of date (a change made from elsewhere).
    fn forget_pages(&mut self, kind: PageKind) {
        let prefix = format!("{kind:?}|");
        self.page_cache.retain(|k, _| !k.starts_with(&prefix));
    }

    fn go(&mut self, d: i32) {
        let to = self.at as i32 + d;
        if to >= 0 && (to as usize) < self.history.len() {
            self.scrolls.insert(self.at, self.page_rows.scroll());
            self.at = to as usize;
            self.scroll_wanted = self.scrolls.get(&self.at).copied();
            self.load_page();
        }
    }

    fn load_page(&mut self) {
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
    fn clear_rows(&mut self) {
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
    fn trim_caches(&mut self) {
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
    fn page_arrived(
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

    fn show_arrived(&mut self, generation: u64, res: music_core::Result<Loaded>) {
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
    fn show_skeleton(&mut self, r: &Route, from: Option<Item>) {
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

    fn page_loaded(&mut self, generation: u64, res: music_core::Result<Loaded>) {
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

    fn push_row(&mut self, r: RowRef, kind: RowKind, row: TrackRow) {
        self.rows.push(r);
        self.art_index.clear();
        self.page_rows.push(TrackRow { kind, ..row });
    }

    /// Adds to the page's own tracks, keeping the footer after them.
    fn add_page_tracks(&mut self, tracks: Vec<Track>) {
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

    fn add_blocks(&mut self, blocks: Vec<Block>) {
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
                    // No tracks: a heading over the grid that follows (Radio).
                    if !tracks.is_empty() {
                        self.push_row(RowRef::Columns, RowKind::Columns, TrackRow::default());
                    }
                    for (n, t) in tracks.into_iter().enumerate() {
                        let mut row = self.row_for(
                            &t,
                            current.as_deref() == Some(t.video_id.as_str()),
                            false,
                        );
                        row.number = n as i32 + 1;
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

    fn add_grid(&mut self, items: Vec<Item>) {
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
    fn layout_grid(&mut self) {
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

    fn columns_changed(&mut self, n: usize) {
        if n != self.columns {
            self.columns = n;
            self.layout_grid();
        }
    }

    fn load_more(&mut self) {
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
            current,
            unavailable: t.unavailable,
            ..Default::default()
        }
    }

    /// A card, with its artwork if that's already in memory.
    fn card(&self, item: &Item) -> Card {
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
    fn track_at(&self, row: usize) -> Option<usize> {
        match self.rows.get(row) {
            Some(RowRef::Track(i)) => Some(*i),
            _ => None,
        }
    }

    /// The item behind a card.
    fn card_item(&self, row: usize, i: usize) -> Option<&Item> {
        match self.rows.get(row)? {
            RowRef::Shelf(items, _) => items.get(i),
            RowRef::Grid(start) => self.grid.get(start + i),
            _ => None,
        }
    }

    /// The size page rows show artwork at, if they show it.
    fn page_art(&self) -> Option<f32> {
        match &self.header {
            Some(h) if h.album => None,
            _ => Some(COLLECTION_ART),
        }
    }

    fn open_album(&mut self, row: usize) {
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

    fn card_clicked(&mut self, row: usize, i: usize) {
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

    /// A song card plays with the other songs beside it.
    fn play_shelf_track(&mut self, row: usize, i: usize, item: Item) {
        let items: Vec<Item> = match &self.rows[row] {
            RowRef::Shelf(items, _) => items.clone(),
            _ => vec![item],
        };
        let start = items[..i.min(items.len())]
            .iter()
            .filter(|x| matches!(x, Item::Track(_)))
            .count();
        self.play_list(pages::tracks_of(items), Some(start), None);
    }

    /// The play button on a card: it plays, opening nothing.
    fn card_play(&mut self, row: usize, i: usize) {
        match self.card_item(row, i).cloned() {
            Some(Item::Track(_)) if self.radio_card_clicked(row, i) => {}
            Some(item @ Item::Track(_)) => self.play_shelf_track(row, i, item),
            Some(item) => self.play_item(item, Mode::Play),
            None => {}
        }
    }

    fn see_all(&mut self, row: usize) {
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

    /// Moves a track in your own playlist from row `from` to before row `to`: here at once,
    /// then on YouTube (reloading the page if that fails).
    fn track_move(&mut self, from: usize, to: usize) {
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

    /// Shows an artist's logo in place of the name: at once when it's known (the name waits
    /// for the picture), else after asking on a worker.
    fn find_logo(&mut self, name: String, channel: Option<String>) {
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
    fn header_images(&mut self) {
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
    fn row_shown(&mut self, list: i32, i: usize) {
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
    fn card_shown(&mut self, row: usize, i: usize) {
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
    fn want_stats(&mut self, id: String) {
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
    fn send_stats(&mut self) {
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
    fn ask_stats(&mut self, id: String) {
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

    fn got_stats(&mut self, r: Option<music_core::Result<VideoStats>>, id: String) {
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
    fn page_metrics(&mut self, m: rows::Metrics) {
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

    fn save_plays_later(&mut self) {
        self.plays_timer
            .start(TimerMode::SingleShot, Duration::from_secs(5), || {
                with(|a| settings::save_plays(&a.plays));
            });
    }

    /// An image arrived: put it everywhere it's wanted.
    /// Brings `art_index` up to date with the page's rows and artwork sizes.
    fn index_art(&mut self) {
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

    fn image_arrived(&mut self, url: String, img: Image) {
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

    // ---- playback --------------------------------------------------------------------------

    /// Plays from row `i`, with the tracks beside it. -1 shuffles the page (or what its banner
    /// stands for), -2 plays it from the top in order.
    fn play_page(&mut self, i: i32) {
        if i < 0 {
            let shuffle = i == -1;
            match self.header.as_ref().and_then(|h| h.play.clone()) {
                Some(HeroPlay::Item(item)) => {
                    self.play_item(item, if shuffle { Mode::Shuffle } else { Mode::Play })
                }
                Some(HeroPlay::Artist {
                    radio,
                    shuffle: mix,
                }) => {
                    let r = if shuffle {
                        mix.or(radio)
                    } else {
                        radio.or(mix)
                    };
                    if let Some(r) = r {
                        self.play_watch(r, shuffle);
                    }
                }
                None => {
                    let own: Vec<Track> = self
                        .page_tracks
                        .iter()
                        .zip(&self.page_groups)
                        .filter(|(_, g)| **g == 0)
                        .map(|(t, _)| t.clone())
                        .collect();
                    self.play_list(own, None, Some(shuffle));
                }
            }
            return;
        }
        let Some(ti) = self.track_at(i as usize) else {
            return;
        };
        // From search or a song page, like YouTube Music: the song, then its radio, not the
        // other results.
        if matches!(
            self.history[self.at].kind,
            PageKind::Search | PageKind::Song
        ) {
            let t = self.page_tracks[ti].clone();
            // A music video found by search opens on its song page, the picture centred.
            let video =
                t.kind == TrackKind::Video && self.history[self.at].kind == PageKind::Search;
            if video {
                self.autoplay_from(t.clone(), None);
                return self.open_video(&t);
            }
            return self.autoplay_from(t, None);
        }
        let group = self.page_groups[ti];
        let mut start = 0;
        let mut tracks = Vec::new();
        for (j, t) in self.page_tracks.iter().enumerate() {
            if self.page_groups[j] == group {
                if j == ti {
                    start = tracks.len();
                }
                tracks.push(t.clone());
            }
        }
        // A list showing only its top few (an artist's Top songs): the whole list plays, from
        // its "See all" playlist. The clicked song starts at once; the rest follow when loaded.
        let full = (group != 0)
            .then(|| {
                self.rows[..i as usize].iter().rev().find_map(|r| match r {
                    RowRef::Heading(m) => Some(m.clone()),
                    _ => None,
                })
            })
            .flatten()
            .flatten()
            .filter(|m| m.browse_id.starts_with("VL"));
        let Some(more) = full else {
            return self.play_list(tracks, Some(start), None);
        };
        let shown: Vec<String> = tracks.iter().map(|t| t.video_id.clone()).collect();
        let chosen = shown[start].clone();
        if !self.listening() {
            self.play_list(tracks, Some(start), None);
        }
        self.play_seq += 1;
        let seq = self.play_seq;
        let client = self.client.clone();
        workers::run_urgent(
            {
                let id = more.browse_id.clone();
                move || client.playlist_all(&id, 500).map(|p| p.tracks)
            },
            move |res| {
                with(|a| match res {
                    // Something else was played since.
                    Ok(_) if a.play_seq != seq => {}
                    Ok(_) if !a.listening() && !a.player.queue().starts_with(&shown) => {}
                    Ok(all) if a.listening() => {
                        let start = all.iter().position(|t| t.video_id == chosen);
                        a.play_list(all, start, None);
                    }
                    Ok(all) => {
                        let rest: Vec<Track> = all
                            .into_iter()
                            .filter(|t| !t.unavailable && !shown.contains(&t.video_id))
                            .collect();
                        if rest.is_empty() {
                            return;
                        }
                        let ids = rest.iter().map(|t| t.video_id.clone()).collect();
                        for t in rest {
                            a.known.insert(t.video_id.clone(), t);
                        }
                        a.player.append(ids);
                        a.queue_changed();
                    }
                    Err(e) => log::warn!("{}: {e}", more.browse_id),
                });
            },
        );
    }

    /// Plays `tracks` from `start` (else a random one when shuffling, else the first). `shuffle`
    /// sets shuffle on or off; `None` leaves it.
    fn play_list(&mut self, tracks: Vec<Track>, start: Option<usize>, shuffle: Option<bool>) {
        if self.listening() {
            let tracks: Vec<Track> = tracks.into_iter().filter(|t| !t.unavailable).collect();
            self.ask_host(listen::Req::Play {
                tracks,
                start,
                shuffle,
            });
            return;
        }
        self.cancel_skip();
        self.autoplay = None;
        if let Some(on) = shuffle {
            self.player.set_shuffle(on);
        }
        let chosen = start
            .and_then(|s| tracks.get(s))
            .map(|t| t.video_id.clone());
        let playable: Vec<Track> = tracks.into_iter().filter(|t| !t.unavailable).collect();
        if playable.is_empty() {
            return;
        }
        let start = match (&chosen, shuffle) {
            (Some(id), _) => playable.iter().position(|t| &t.video_id == id).unwrap_or(0),
            (None, Some(true)) => (music_core::paths::unix_now() as usize) % playable.len(),
            (None, _) => 0,
        };
        let ids: Vec<String> = playable.iter().map(|t| t.video_id.clone()).collect();
        for t in playable {
            self.known.insert(t.video_id.clone(), t);
        }
        self.resume_at = None;
        self.player.play_queue(ids, start);
        self.queue_changed();
        self.show_now_playing(false);
    }

    /// Fetches the tracks of an album, playlist or artist (their radio, or shuffle), or a
    /// song's radio, then plays or queues them.
    fn play_item(&mut self, item: Item, mode: Mode) {
        if let (Item::Track(t), Mode::Play) = (&item, mode) {
            return self.autoplay_from(t.clone(), None);
        }
        self.play_seq += 1;
        let seq = self.play_seq;
        let client = self.client.clone();
        workers::run_urgent(
            move || -> music_core::Result<Vec<Track>> {
                let watch = |r: WatchRef| {
                    client
                        .watch_next(
                            r.video_id.as_deref(),
                            Some(&r.playlist_id),
                            r.params.as_deref(),
                        )
                        .map(|w| w.tracks)
                };
                Ok(match item {
                    Item::Track(t) if matches!(mode, Mode::Next | Mode::Queue) => vec![t],
                    Item::Track(t) => {
                        let mut radio = client.radio(&t.video_id)?.tracks;
                        if radio.first().map(|r| &r.video_id) != Some(&t.video_id) {
                            radio.insert(0, t);
                        }
                        radio
                    }
                    Item::Album(a) => client.album(&a.id)?.tracks,
                    Item::Playlist(p) => client.playlist(&p.id)?.tracks,
                    Item::Artist(a) => {
                        let p = client.artist(&a.id)?;
                        let r = if mode == Mode::Shuffle {
                            p.shuffle.or(p.radio)
                        } else {
                            p.radio.or(p.shuffle)
                        };
                        match r {
                            Some(r) => watch(r)?,
                            None => Vec::new(),
                        }
                    }
                    Item::Link(_) => Vec::new(),
                })
            },
            move |res| {
                with(|a| match res {
                    Ok(tracks) => a.use_tracks(tracks, mode, seq),
                    Err(e) => log::warn!("play: {e}"),
                });
            },
        );
    }

    /// Plays a song at once (unless it's already playing) and lets YouTube's radio fill the
    /// queue after it, from `chip` (else "All").
    fn autoplay_from(&mut self, t: Track, chip: Option<(usize, WatchRef)>) {
        self.play_seq += 1;
        let id = t.video_id.clone();
        if self.player.current() != Some(id.as_str()) {
            self.play_list(vec![t], Some(0), Some(false));
        }
        // Listening along: the host's app grows the radio.
        if self.listening() {
            return;
        }
        let (picked, watch) = match chip {
            Some((i, w)) => (i, Some(w)),
            None => (0, None),
        };
        let chips = match self.autoplay.take() {
            Some(a) if watch.is_some() => a.chips,
            _ => Vec::new(),
        };
        let seq = self.play_seq;
        self.autoplay = Some(Autoplay {
            seed: id.clone(),
            chips,
            picked,
            seq,
        });
        self.show_autoplay();
        let client = self.client.clone();
        workers::run_urgent(
            move || match &watch {
                Some(w) => client.watch_next(Some(&id), Some(&w.playlist_id), w.params.as_deref()),
                None => client.radio(&id),
            },
            move |r| {
                with(|a| match r {
                    Ok(w) => a.autoplay_arrived(w, seq),
                    Err(e) => log::warn!("autoplay: {e}"),
                });
            },
        );
    }

    /// The radio arrived: it becomes everything after the song, if that's still what plays.
    fn autoplay_arrived(&mut self, w: WatchNext, seq: u64) {
        let Some(ap) = &mut self.autoplay else {
            return;
        };
        // Asked again since (another choice, the radio button) or something else plays.
        if ap.seq != seq || self.player.current() != Some(ap.seed.as_str()) {
            return;
        }
        // The choices come with the first answer; picking one keeps them.
        if ap.chips.is_empty() {
            ap.picked = w.chips.iter().position(|c| c.selected).unwrap_or(0);
            ap.chips = w.chips;
        }
        let seed = ap.seed.clone();
        let rest: Vec<Track> = w
            .tracks
            .into_iter()
            .filter(|t| t.video_id != seed && !t.unavailable)
            .collect();
        let ids = rest.iter().map(|t| t.video_id.clone()).collect();
        for t in rest {
            self.known.insert(t.video_id.clone(), t);
        }
        self.player.replace_rest(ids);
        self.queue_changed();
        self.show_autoplay();
    }

    /// An autoplay choice was picked: the radio again from the song playing now.
    fn autoplay_chip(&mut self, i: usize) {
        let Some(chip) = self.autoplay.as_ref().and_then(|a| a.chips.get(i)).cloned() else {
            return;
        };
        let current = self
            .player
            .current()
            .and_then(|id| self.known.get(id))
            .cloned();
        if let Some(t) = current {
            self.autoplay_from(t, Some((i, chip.watch)));
        }
    }

    fn show_autoplay(&self) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let (labels, picked): (Vec<SharedString>, i32) = match &self.autoplay {
            Some(a) => (
                a.chips.iter().map(|c| c.label.clone().into()).collect(),
                a.picked as i32,
            ),
            None => (Vec::new(), -1),
        };
        g.set_autoplay_chips(ModelRc::new(VecModel::from(labels)));
        g.set_autoplay_picked(picked);
    }

    /// Plays an artist's radio or shuffle.
    fn play_watch(&mut self, r: WatchRef, shuffle: bool) {
        self.play_seq += 1;
        let seq = self.play_seq;
        let client = self.client.clone();
        workers::run_urgent(
            move || {
                client
                    .watch_next(
                        r.video_id.as_deref(),
                        Some(&r.playlist_id),
                        r.params.as_deref(),
                    )
                    .map(|w| w.tracks)
            },
            move |res| {
                with(|a| match res {
                    Ok(tracks) => {
                        let mode = if shuffle { Mode::Shuffle } else { Mode::Play };
                        a.use_tracks(tracks, mode, seq)
                    }
                    Err(e) => log::warn!("play: {e}"),
                });
            },
        );
    }

    fn use_tracks(&mut self, tracks: Vec<Track>, mode: Mode, seq: u64) {
        // Queueing adds up; playing is only for the latest click.
        if matches!(mode, Mode::Play | Mode::Shuffle) && seq != self.play_seq {
            return;
        }
        match mode {
            Mode::Play => self.play_list(tracks, None, Some(false)),
            Mode::Shuffle => self.play_list(tracks, None, Some(true)),
            Mode::Next | Mode::Queue => {
                let playable = tracks.iter().filter(|t| !t.unavailable).cloned().collect();
                if self.ask_host(listen::Req::Add {
                    tracks: playable,
                    next: mode == Mode::Next,
                }) {
                    return;
                }
                let ids: Vec<String> = tracks
                    .iter()
                    .filter(|t| !t.unavailable)
                    .map(|t| t.video_id.clone())
                    .collect();
                if ids.is_empty() {
                    return;
                }
                for t in tracks {
                    self.known.insert(t.video_id.clone(), t);
                }
                if mode == Mode::Next {
                    self.player.insert_next(ids);
                } else {
                    self.player.append(ids);
                }
                self.queue_changed();
                self.show_now_playing(false);
            }
        }
    }

    /// Next (1) or previous (-1). The first press acts at once; presses that follow quickly
    /// only move along the queue (showing each song), and the one they stop at plays, so
    /// pressing a lot doesn't start and abandon a load per press.
    fn skip(&mut self, dir: i64) {
        if self.ask_host(if dir > 0 {
            listen::Req::Next
        } else {
            listen::Req::Previous
        }) {
            return;
        }
        if !self.skip_timer.running() {
            if dir > 0 {
                self.player.next();
            } else {
                self.player.previous();
            }
        } else if self.player.step_to(dir) {
            // What was loading (or playing) is no longer wanted.
            self.player.engine.stop();
            self.skip_pending = true;
            self.resume_at = None;
            self.show_now_playing(false);
            self.queue_changed();
        }
        self.skip_timer
            .start(TimerMode::SingleShot, Duration::from_millis(400), || {
                with(|a| {
                    if std::mem::take(&mut a.skip_pending) {
                        a.player.play_current();
                    }
                });
            });
    }

    /// Tells YouTube a song was played once 30 s of it were heard (a shorter song: once it's
    /// nearly over), so skipping through a queue doesn't fill the history.
    fn report_play_when_heard(&mut self, pos: Option<&music_core::audio::Position>) {
        let (Some(p), Some((id, duration))) = (pos, &self.play_report) else {
            return;
        };
        if p.id != *id {
            return;
        }
        let duration = p.duration_ms.or(*duration);
        let enough = duration.map_or(30_000, |d| d.saturating_sub(2_000).clamp(1_000, 30_000));
        if p.ms < enough {
            return;
        }
        let id = id.clone();
        self.play_report = None;
        if !self.client.signed_in() {
            return;
        }
        let client = self.client.clone();
        workers::spawn(move || {
            if let Err(e) = client.report_play(&id) {
                log::warn!("history: {e}");
            }
        });
    }

    /// Quick skips are over: what's played now was chosen some other way.
    fn cancel_skip(&mut self) {
        self.skip_timer.stop();
        self.skip_pending = false;
    }

    fn play_pause(&mut self) {
        if self.ask_host(listen::Req::Toggle) {
            return;
        }
        self.cancel_skip();
        match self.player.engine.position() {
            // Hosting Listen Along: everyone starts again together.
            Some(p) if p.paused => {
                if !self.together_resume() {
                    self.player.engine.resume();
                }
            }
            Some(_) => {
                self.player.engine.pause();
                self.together_paused();
            }
            None => self.player.play_current(),
        }
        self.tick();
    }

    fn seek(&mut self, ms: u64) {
        if !self.ask_host(listen::Req::Seek(ms)) {
            self.player.engine.seek(ms);
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
        // A failure is shown only for the song meant to be heard, not a preload.
        let failed_now =
            matches!(&e, Event::Failed { id, .. } if self.player.current() == Some(id.as_str()));
        // Skipping quickly: a song passed over that starts after all mustn't take the queue back.
        if self.skip_pending
            && matches!(&e, Event::Started { id, .. } if self.player.current() != Some(id.as_str()))
        {
            return;
        }
        self.player.on_event(&e);
        self.together_engine(&e);
        match &e {
            Event::Started { id, duration_ms } => {
                // Its length, kept with the song (and the saved queue, for the bar after a
                // restart).
                if let Some(d) = duration_ms
                    && let Some(t) = self.known.get_mut(id)
                    && t.duration_ms.is_none()
                {
                    t.duration_ms = Some(*d);
                }
                self.video_sync();
                if let Some(ms) = self.resume_at.take().filter(|&ms| ms > 5000) {
                    self.player.engine.seek(ms);
                }
                self.queue_changed();
                if self.lyrics_wanted() {
                    self.lyrics_for_current();
                } else {
                    self.forget_lyrics();
                }
                // Nothing after this song (a single, a search result, a list's last song): let
                // its radio follow, once per song.
                let station = radio::is_station(id);
                if station {
                    self.station_started(id);
                }
                if !station
                    && !self.listening()
                    && self.queue_from >= self.queue_ids.len()
                    && self.player.repeat == Repeat::Off
                    && self.autoplay.as_ref().is_none_or(|ap| ap.seed != *id)
                    && let Some(t) = self.known.get(id).cloned()
                {
                    self.autoplay_from(t, None);
                }
                self.show_now_playing(true);
                // Into YouTube's history once it's really been listened to (see `tick`); live
                // radio isn't YouTube's to know about.
                self.play_report = (!station).then(|| (id.clone(), *duration_ms));
            }
            Event::Finished => self.tick(),
            Event::Failed { id, error, .. } => {
                log::warn!("{id}: {error}");
                if failed_now && !matches!(error, music_core::Error::Cancelled) {
                    let title = self
                        .known
                        .get(id)
                        .map_or("this song".to_string(), |t| t.title.clone());
                    let msg = match error {
                        music_core::Error::Unavailable(_) => {
                            format!("'{title}' isn't available to play")
                        }
                        music_core::Error::Net(_) => {
                            self.went_offline();
                            format!("Couldn't play '{title}': check your connection")
                        }
                        _ => format!("Couldn't play '{title}'"),
                    };
                    self.notify(msg);
                }
            }
            Event::Device(d) => log::info!("device: {d}"),
            Event::Title { id, title } => {
                self.together_title(id, title);
                self.station_title(id, title);
            }
            Event::Cueing { .. } | Event::Cued { .. } => {}
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
        self.reported_playing = None;
        if let Some(m) = &self.media {
            let art_url = images::url_for(&t.thumbs, 300);
            let album = t.album.as_ref().map(|a| a.name.as_str()).unwrap_or("");
            m.track(&t.title, &t.artist_line(), album, art_url.as_deref());
        }
        self.station_now_playing(&t);
        self.tick();
    }

    /// Turns the engine's band levels into 0..1 bars: each band against its own recent peak (so
    /// quiet songs move too), jumping up at once and falling back smoothly.
    /// Every redraw is the whole window, so things that move share frames: the clock moves on
    /// every 8th tick (~4 Hz), and levels (the sung line's glow, the playing row's bars) go out
    /// ~15 times a second, or with any frame drawn anyway.
    fn meter_tick(&mut self) {
        self.meter_ticks = self.meter_ticks.wrapping_add(1);
        let drew = if self.meter_ticks.is_multiple_of(8) {
            self.tick();
            true
        } else {
            self.lyrics_tick()
        };
        if !self.meter_timer.running() {
            return; // `tick` stopped it
        }
        let write = drew || self.meter_ticks.is_multiple_of(2);
        let raw = self.player.engine.levels();
        for (i, (peak, shown)) in self.meter.iter_mut().enumerate() {
            let v = raw[i];
            // The peak forgets over ~10 s, so the scale follows the song.
            *peak = v.max(*peak * 0.997).max(1e-4);
            let x = (v / *peak).clamp(0.0, 1.0);
            // Bass gets a punchier curve, like the old immersive background's pulse.
            let x = if i == 0 { x * x } else { x.powf(1.4) };
            *shown = x.max(*shown * 0.82);
            if write {
                self.levels.set_row_data(i, *shown);
            }
        }
    }

    /// Fetches the playing song's lyrics for the panel, and lets a song page of it follow along.
    fn lyrics_for_current(&mut self) {
        let Some(id) = self.player.current().map(str::to_string) else {
            return;
        };
        for ri in 0..self.rows.len() {
            if let RowRef::Details(d) = &self.rows[ri]
                && let Some(mut row) = self.page_rows.row_data(ri)
            {
                row.live = *d == id;
                self.page_rows.set_row_data(ri, row);
            }
        }
        if self.lyrics.as_ref().is_some_and(|l| l.id == id) {
            return;
        }
        self.lyrics = Some(lyric_view::Follow::new(id.clone(), Default::default()));
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_lyrics(ModelRc::default());
        g.set_lyric_line(-1);
        g.set_lyric_pause_row(-1);
        g.set_lyrics_missing(false);
        g.set_lyrics_note("Finding lyrics…".into());
        let Some(t) = self.known.get(&id).cloned() else {
            self.no_lyrics();
            return;
        };
        let client = self.client.clone();
        workers::run_background(
            move || pages::lyrics_for(&client, &t, None),
            move |found| {
                with(|a| {
                    if a.lyrics.as_ref().is_none_or(|l| l.id != id) {
                        return;
                    }
                    let ui = a.ui();
                    let g = ui.global::<Ui>();
                    match found {
                        Some(l) => {
                            let f = lyric_view::Follow::new(id, l);
                            g.set_lyrics(ModelRc::from(f.model.clone()));
                            g.set_lyrics_note(SharedString::default());
                            a.lyrics = Some(f);
                            a.lyrics_tick();
                        }
                        None => a.no_lyrics(),
                    }
                });
            },
        );
    }

    /// The playing song has no lyrics: the lyrics controls dim, and a panel showing lyrics shows
    /// the queue instead (it stays there until Lyrics is chosen again).
    fn no_lyrics(&mut self) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_lyrics_note("No lyrics for this song.".into());
        g.set_lyrics_missing(true);
        if g.get_panel_open() && g.get_panel_tab() == 0 {
            g.set_panel_tab(1);
        }
    }

    /// Lyrics are fetched (from YouTube and, if allowed, the lyrics services) only once they've
    /// been shown this session, or while they're on screen.
    fn lyrics_wanted(&self) -> bool {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        self.lyrics_opened || (g.get_panel_open() && g.get_panel_tab() == 0)
    }

    /// Nothing known about the playing song's lyrics (they weren't asked for).
    fn forget_lyrics(&mut self) {
        self.lyrics = None;
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_lyrics(ModelRc::default());
        g.set_lyric_line(-1);
        g.set_lyric_pause_row(-1);
        g.set_lyrics_missing(false);
        g.set_lyrics_note(SharedString::default());
    }

    /// Moves the lyrics to the playback position: the sung line at 4 Hz from `tick`, and its
    /// word-by-word fill at ~30 Hz from the meter timer, the fill only while lyrics are on
    /// screen (the panel's lyrics tab, or this song's page).
    /// Moves the lyrics along; whether any row changed (a frame is drawn anyway).
    fn lyrics_tick(&mut self) -> bool {
        if self
            .lyrics
            .as_ref()
            .is_none_or(|l| l.lyrics.lines.is_empty())
        {
            return false;
        }
        let Some(p) = self.player.engine.position() else {
            return false;
        };
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let Some(f) = self.lyrics.as_mut() else {
            return false;
        };
        let page = g.get_page();
        let visible = (g.get_panel_open() && g.get_panel_tab() == 0)
            || (page.kind == PageKind::Song && page.id.as_str() == f.id);
        let theme = ui.global::<Theme>();
        let (sung, todo) = (theme.get_amber(), theme.get_ivory().with_alpha(0.5));
        if let Some(line) = f.at(p.ms, visible, sung, todo) {
            g.set_lyric_line(line);
        }
        let (row, through) = f.pause.map_or((-1, 0.0), |(r, k)| (r as i32, k));
        let drew = f.drew;
        // The waiting dots move ~15 times a second, unless this tick draws anyway.
        if drew || row != g.get_lyric_pause_row() || self.meter_ticks.is_multiple_of(2) {
            g.set_lyric_pause(through);
        }
        g.set_lyric_pause_row(row);
        drew
    }

    fn tick(&mut self) {
        let pos = self.player.engine.position();
        self.lyrics_tick();
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
        np.playing = playing;
        self.report_play_when_heard(pos.as_ref());
        // Compared with what Windows was told, not with `np`: a new song sets `np` first.
        if self.reported_playing != Some(playing) {
            self.reported_playing = Some(playing);
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
        // The flyout's seek bar: once a second, and at once after a jump (a seek, a new song).
        if let Some(m) = &self.media {
            let (sec, dur) = (
                np.position_ms.max(0) as u64 / 1000,
                np.duration_ms.max(0) as u64,
            );
            let (was_sec, was_dur) = self.reported_timeline;
            if dur != was_dur || sec != was_sec {
                self.reported_timeline = (sec, dur);
                m.timeline(np.position_ms.max(0) as u64, dur);
            }
        }
        g.set_np(np);
        // A song that's current but not audible yet (loading, seeking, a skip) keeps the clock
        // asking for a while: its start can come without an event that would wake it again.
        let waiting = pos.is_none() && self.player.current().is_some();
        let waited = if waiting {
            self.waiting_since
                .get_or_insert_with(std::time::Instant::now)
                .elapsed()
        } else {
            self.waiting_since = None;
            Duration::ZERO
        };
        if waiting && (3000..3250).contains(&waited.as_millis()) {
            log::warn!(
                "{}: no audible position after 3 s",
                self.player.current().unwrap_or_default()
            );
        }
        let want = (playing || waiting && waited < Duration::from_secs(20)) && !self.minimized;
        // The meter's timer moves the clock too (see `meter_tick`); with another window active
        // it's only the clock, once a second.
        if want && !self.meter_timer.running() {
            g.set_levels(ModelRc::from(self.levels.clone()));
            if self.unfocused {
                self.meter_timer
                    .start(TimerMode::Repeated, Duration::from_secs(1), || {
                        with(|a| {
                            a.tick();
                            a.lyrics_tick();
                        });
                    });
            } else {
                self.meter_timer
                    .start(TimerMode::Repeated, Duration::from_millis(33), || {
                        with(|a| a.meter_tick());
                    });
            }
        } else if !want && self.meter_timer.running() {
            self.meter_timer.stop();
        }
    }

    /// Rebuilds the queue panel (the songs after the playing one) and marks the playing row on
    /// the page.
    fn queue_changed(&mut self) {
        // Songs still in the queue keep the artwork they have rather than blinking and loading it
        // again.
        let from = self.queue_from.min(self.queue_ids.len());
        let mut had: HashMap<String, Image> = self.queue_ids[from..]
            .iter()
            .zip(self.queue_rows.iter())
            .filter(|(_, r)| r.art.size().width > 0)
            .map(|(id, r)| (id.clone(), r.art))
            .collect();
        let (order, at) = self.player.up_next();
        self.queue_ids = order.iter().map(|s| s.to_string()).collect();
        self.queue_from = (at + 1).min(self.queue_ids.len());
        let rows: Vec<TrackRow> = self.queue_ids[self.queue_from..]
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut row = match self.known.get(id) {
                    Some(t) => self.row_for(t, false, true),
                    None => TrackRow {
                        title: id.clone().into(),
                        ..Default::default()
                    },
                };
                // Listen Along: who added it.
                if let Some(by) = self.added_by(id, self.queue_from + i) {
                    row.artist = format!("{} \u{b7} added by {by}", row.artist).into();
                }
                if let Some(art) = had.remove(id) {
                    row.art = art;
                }
                row
            })
            .collect();
        self.queue_rows.set_vec(rows);
        self.together_queue_changed();
        let current = self.player.current().map(str::to_string);
        for (ri, r) in self.rows.iter().enumerate() {
            let RowRef::Track(ti) = r else { continue };
            let is = current.as_deref() == Some(self.page_tracks[*ti].video_id.as_str());
            if let Some(mut row) = self.page_rows.row_data(ri)
                && row.current != is
            {
                row.current = is;
                self.page_rows.set_row_data(ri, row);
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
                .filter_map(|id| {
                    let mut t = self.known.get(*id).cloned()?;
                    t.duration_ms = t.duration_ms.or_else(|| self.lengths.get(*id).copied());
                    Some(t)
                })
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
            autoplay: self
                .autoplay
                .as_ref()
                .map(|a| (a.seed.clone(), a.chips.clone(), a.picked)),
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

    fn panel_changed(&mut self) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        self.settings.panel_open = g.get_panel_open();
        self.settings.panel_tab = g.get_panel_tab();
        self.save_later();
        if g.get_panel_open() && g.get_panel_tab() == 0 {
            // Lyrics asked for: from now on they're fetched for every song.
            self.lyrics_opened = true;
            self.lyrics_for_current();
        }
        // Lyrics shown now: fill the sung line at once, even while paused.
        self.lyrics_tick();
    }

    fn remote(&mut self, r: Remote) {
        if let Remote::Focused(on) = r {
            return self.focus_changed(on);
        }
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
            Remote::Next => self.skip(1),
            Remote::Previous => self.skip(-1),
            Remote::Back => self.go(-1),
            Remote::Forward => self.go(1),
            Remote::Minimized => self.minimize(),
            Remote::Restored => self.restore(),
            Remote::Seek(ms) => self.seek(ms),
            Remote::Focused(_) => {}
        }
    }

    /// The meter's timer runs at its other pace from the next tick.
    fn focus_changed(&mut self, focused: bool) {
        if self.unfocused == !focused {
            return;
        }
        self.unfocused = !focused;
        if self.unfocused {
            // The visualizer rests rather than freezing mid-beat.
            for (i, m) in self.meter.iter_mut().enumerate() {
                m.1 = 0.0;
                self.levels.set_row_data(i, 0.0);
            }
        }
        self.meter_timer.stop();
        self.tick();
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

    // ---- account ---------------------------------------------------------------------------

    fn sign_in(&mut self) {
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
    fn load_playlists(&mut self) {
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
    fn set_playlists(&mut self, mut all: Vec<PlaylistCard>) {
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
    fn move_playlist(&mut self, from: usize, to: usize) {
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

    // ---- right-click menu ------------------------------------------------------------------

    /// Opens the menu for what was right-clicked at `at` (window coordinates).
    fn context(&mut self, what: &str, row: usize, id: String, at: slint::LogicalPosition) {
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

    fn show_menu(&mut self, target: MenuTarget, items: Vec<MenuEntry>, at: slint::LogicalPosition) {
        self.menu = Some(target);
        self.menu_at = at;
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_menu_items(ModelRc::new(VecModel::from(items)));
        g.set_menu_at(at);
        g.set_menu_open(true);
    }

    fn menu_chosen(&mut self, choice: &str) {
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
                        self.known.insert(id.clone(), t);
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

    /// Whether a song is liked, by what we last heard or did.
    fn liked(&self, t: &Track) -> bool {
        self.known.get(&t.video_id).and_then(|k| k.like).or(t.like) == Some(Like::Like)
    }

    /// Likes a song or takes the like away: here everywhere it shows, then on YouTube (and back
    /// here if YouTube refuses).
    fn set_like(&mut self, id: &str, like: Like) {
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

    /// A queue song's menu entry was chosen; `pos` is where it is now.
    fn queue_chosen(&mut self, pos: usize, choice: &str) {
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

    /// Shows a song liked (or not) everywhere it is here.
    fn mark_like(&mut self, id: &str, like: Like) {
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
    fn needs_account(&mut self, to: &str) -> bool {
        if self.client.signed_in() {
            return false;
        }
        self.notify_with(format!("Sign in to {to}"), "Sign in", |a| a.sign_in());
        true
    }

    /// An account change YouTube refused (already undone here): says so, with Retry when
    /// `retry` can do it again.
    fn account_failed(&mut self, what: &str, e: &music_core::Error, retry: Option<toast::Then>) {
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
    fn add_to(
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
    fn session_check(&mut self) -> bool {
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

    fn open_song(&mut self, t: &Track) {
        self.navigate(Route {
            kind: PageKind::Song,
            id: t.video_id.clone(),
            title: t.title.clone(),
        });
    }

    /// A music video's song page, which scrolls to centre the picture once it shows.
    fn open_video(&mut self, t: &Track) {
        self.open_song(t);
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_centre_video_serial(g.get_centre_video_serial().wrapping_add(1));
    }

    /// The song entries every song menu shares (track rows, cards, the queue).
    fn song_chosen(&mut self, choice: &str, t: Track) {
        match choice {
            "radio" => self.play_item(Item::Track(t), Mode::Play),
            "link" => self.copy_link(&Item::Track(t)),
            "libadd" | "libremove" => self.save_song(&t, choice == "libadd"),
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

    /// One of the sidebar's playlists that you made (its byline is your name).
    fn mine(&self, p: &PlaylistCard) -> bool {
        self.account
            .as_ref()
            .is_some_and(|a| !a.name.is_empty() && p.subtitle.starts_with(&a.name))
    }

    /// Renames the playlist on screen: here at once, then on YouTube, then the sidebar.
    fn rename_playlist(&mut self, name: String) {
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
    fn save_song(&mut self, t: &Track, save: bool) {
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
    fn mark_saved(&mut self, id: &str, save: bool) {
        self.forget_pages(PageKind::Songs);
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
    fn save_item(&mut self, item: Item, save: bool) {
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
    fn toggle_saved(&mut self) {
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
    fn toggle_follow(&mut self) {
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

    fn copy_link(&self, item: &Item) {
        if let Some(hwnd) = self.hwnd {
            win::copy_text(hwnd, &pages::link(item));
        }
    }

    /// "Don't recommend this song": off this page's recommendations and the queue at once,
    /// skipping it if it's playing (the page's own list, a playlist or an album, keeps it); told
    /// to YouTube when its toast goes, so Undo can still take it back.
    fn dislike(&mut self, t: Track) {
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
    fn not_interested(&mut self, item: Item) {
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

    /// Takes matching cards, and songs in headed lists (search results, top songs), off the
    /// page. A song taken off stays in `page_tracks`, marked so it isn't played.
    fn drop_from_page(&mut self, gone: &dyn Fn(&Item) -> bool) {
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

    /// A click on an artist name: on a page row, the banner (-1) or the now-playing bar (-2).
    /// One artist opens their page; several open a small menu to pick from.
    fn open_artist(&mut self, row: i32, at: slint::LogicalPosition) {
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

    /// Takes a track out of your own playlist: here at once, then on YouTube (reloading the
    /// page if that fails).
    fn remove_from_playlist(&mut self, i: usize) {
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
            r.title = summary_line(&self.page_tracks).into();
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

    /// Hides a playlist from the app for good (kept in settings).
    /// The now-playing bar's cover opens the album, its title the song. Songs without an album
    /// (videos) open their song page either way.
    fn np_open(&mut self, what: &str) {
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

    /// Asks for a new playlist's name in the sidebar (it gets these songs, if any).
    fn new_playlist(&mut self, ids: Vec<String>) {
        self.naming_ids = ids;
        self.ui().global::<Ui>().set_naming_playlist(true);
    }

    /// The name typed for a new playlist ("" when cancelled): makes it, and it shows up in the
    /// sidebar.
    fn create_playlist(&mut self, name: String) {
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
    fn delete_playlist(&mut self, id: String) {
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

    fn hide_playlist(&mut self, id: String) {
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
    fn show_hidden_lists(&self) {
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

/// "Like", or "Remove from liked" when it already is.
fn menu_entry(id: &str, label: &str, icon: &str, danger: bool) -> MenuEntry {
    MenuEntry {
        id: id.into(),
        label: label.into(),
        icon: icon.into(),
        danger,
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

/// Library, Add to playlist and radio entries for a song.
fn song_entries(t: &Track) -> Vec<MenuEntry> {
    let entry = |id: &str, label: &str, icon: &str| MenuEntry {
        id: id.into(),
        label: label.into(),
        icon: icon.into(),
        danger: false,
    };
    let mut v = Vec::new();
    match &t.library {
        Some(l) if l.saved => v.push(entry("libremove", "Remove from library", "check")),
        Some(_) => v.push(entry("libadd", "Add to library", "plus")),
        None => {}
    }
    v.push(entry("add-to", "Add to playlist…", "queue"));
    v.push(entry("radio", "Start radio", "radio"));
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

/// Lyrics as the UI's lines: unsynced ones have no time.
fn lyric_lines(l: Option<&lyrics::Lyrics>) -> ModelRc<LyricLine> {
    ModelRc::new(VecModel::from(l.map(lyric_view::rows).unwrap_or_default()))
}

/// Nothing: the app names no lyric source (PLAN R6).
fn lyrics_note(_: &lyrics::Lyrics) -> String {
    String::new()
}

/// A new page: the page view goes back to the top.
/// Where sign-in's browser keeps its profile while it runs.
fn webview_dir() -> std::path::PathBuf {
    music_core::paths::cache_dir()
        .parent()
        .expect("cache dir")
        .join("webview")
}

fn g_serial_bump(ui: &AppWindow) {
    let g = ui.global::<Ui>();
    g.set_page_serial(g.get_page_serial().wrapping_add(1));
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
