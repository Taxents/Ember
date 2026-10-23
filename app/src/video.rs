//! Music videos on their song page.
//!
//! A music video plays like any song: its sound comes from the audio engine, and nothing else
//! happens. Only while the window shows the song page of the video that is playing does its
//! picture appear, in a 16:9 panel under the page's banner, decoded on a worker
//! (`music_core::video`) and kept in step with the engine's position. Leaving the page (or
//! minimizing, or the track changing) drops the decoder, so audio-only costs nothing extra.
//!
//! Songs that have a separate music video aren't synced to it (the timing differs): the song
//! page's "Music videos" shelf plays the video itself, and its own page shows the picture.

use crate::{App, AppWindow, PageKind, Ui};
use music_core::video::{Note, VideoPlayer};
use music_core::yt::Resolver;
use music_core::yt::model::{Item, TrackKind};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};
use std::cell::RefCell;
use std::sync::{Arc, Mutex, OnceLock};

/// The stream asked for is about this tall, in logical pixels (the panel under the banner is
/// at most three quarters of the view tall).
const BOX_H: f32 = 720.0;

static RESOLVER: OnceLock<Arc<Resolver>> = OnceLock::new();

/// The resolver the audio engine uses, so both share one URL cache.
pub fn init(resolver: Arc<Resolver>) {
    let _ = RESOLVER.set(resolver);
}

/// The video being shown.
struct Shown {
    id: String,
    serial: u64,
    _player: VideoPlayer,
    /// The newest frame, waiting for the UI thread. One at most: a newer one replaces it.
    slot: Arc<Mutex<Option<SharedPixelBuffer<Rgba8Pixel>>>>,
}

thread_local! {
    static SHOWN: RefCell<Option<Shown>> = const { RefCell::new(None) };
    static SERIAL: RefCell<u64> = const { RefCell::new(0) };
    /// A video whose picture failed: not tried again until something else plays.
    static FAILED: RefCell<Option<String>> = const { RefCell::new(None) };
}

impl App {
    /// Shows the playing music video's picture when its song page is open; stops it otherwise.
    /// Call after anything that changes the page, the playing track or the window's state.
    pub(crate) fn video_sync(&mut self) {
        // A failure is remembered only while that video plays.
        FAILED.with(|f| {
            let mut f = f.borrow_mut();
            if f.as_deref() != self.player.current() {
                *f = None;
            }
        });
        let want = self.video_wanted();
        let shown = SHOWN.with(|s| s.borrow().as_ref().map(|s| s.id.clone()));
        if want == shown {
            return;
        }
        SHOWN.with(|s| s.borrow_mut().take());
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_video_frame(Image::default());
        g.set_video_on(want.is_some());
        self.page_rows.set_video(want.is_some());
        let (Some(id), Some(resolver)) = (want, RESOLVER.get()) else {
            return;
        };
        let serial = SERIAL.with(|s| {
            *s.borrow_mut() += 1;
            *s.borrow()
        });
        let slot = Arc::new(Mutex::new(None));
        let sink_slot = slot.clone();
        let weak = ui.as_weak();
        let failed_id = id.clone();
        let player = VideoPlayer::start(
            id.clone(),
            self.px(BOX_H),
            resolver.clone(),
            self.player.engine.clock(),
            move |note| match note {
                Note::Frame(f) => {
                    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(f.width, f.height);
                    buf.make_mut_bytes().copy_from_slice(f.rgba);
                    let first = sink_slot.lock().unwrap().replace(buf).is_none();
                    // One hand-over in flight: later frames just replace the waiting one.
                    if first {
                        let _ = weak.upgrade_in_event_loop(move |ui| show_frame(&ui, serial));
                    }
                }
                Note::Failed(_) => {
                    let id = failed_id.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        FAILED.with(|f| *f.borrow_mut() = Some(id));
                        crate::with(|a| a.video_sync());
                    });
                }
            },
        );
        SHOWN.with(|s| {
            *s.borrow_mut() = Some(Shown {
                id,
                serial,
                _player: player,
                slot,
            })
        });
    }

    /// The video to show now, if any: the playing track, when it's a music video and its song
    /// page is open in a visible window.
    fn video_wanted(&self) -> Option<String> {
        if self.minimized {
            return None;
        }
        let r = self.history.get(self.at)?;
        let cur = self.player.current()?;
        if r.kind != PageKind::Song || r.id != cur {
            return None;
        }
        if FAILED.with(|f| f.borrow().as_deref() == Some(cur)) {
            return None;
        }
        let page_kind = self.header.as_ref().and_then(|h| match &h.play {
            Some(crate::pages::HeroPlay::Item(Item::Track(t))) if t.video_id == cur => {
                Some(t.kind)
            }
            _ => None,
        });
        let kind = page_kind.or_else(|| self.known.get(cur).map(|t| t.kind))?;
        (kind == TrackKind::Video).then(|| cur.to_string())
    }
}

/// Puts the waiting frame on the banner, if it belongs to the video still shown.
fn show_frame(ui: &AppWindow, serial: u64) {
    let buf = SHOWN.with(|s| {
        s.borrow()
            .as_ref()
            .filter(|s| s.serial == serial)
            .and_then(|s| s.slot.lock().unwrap().take())
    });
    if let Some(buf) = buf {
        ui.global::<Ui>().set_video_frame(Image::from_rgba8(buf));
    }
}
