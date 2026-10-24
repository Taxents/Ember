//! Windows integration: dark title bar, the media flyout (media keys), taskbar buttons,
//! minimize/restore notifications and trimming memory while minimized.

use std::cell::RefCell;
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs,
    SystemMediaTransportControls, SystemMediaTransportControlsButton,
    SystemMediaTransportControlsTimelineProperties,
};
use windows::Storage::Streams::RandomAccessStreamReference;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};
use windows::Win32::System::WinRT::ISystemMediaTransportControlsInterop;
use windows::Win32::UI::Shell::{
    DefSubclassProc, ITaskbarList3, SetWindowSubclass, THB_FLAGS, THB_ICON, THB_TOOLTIP,
    THBF_ENABLED, THBN_CLICKED, THUMBBUTTON, TaskbarList,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, HICON, ICONINFO, IsIconic, RegisterWindowMessageW, SIZE_MAXIMIZED,
    SIZE_MINIMIZED, SIZE_RESTORED, WM_COMMAND, WM_SIZE, WM_XBUTTONUP,
};
use windows::core::{HSTRING, w};

/// What the taskbar buttons and the media flyout ask for.
#[derive(Debug, Clone, Copy)]
pub enum Remote {
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    Minimized,
    Restored,
    /// The mouse's back and forward side buttons.
    Back,
    Forward,
    /// The media flyout's seek bar was moved, to this many ms.
    Seek(u64),
}

/// Draw at the display's real resolution instead of letting Windows stretch a 96-DPI bitmap.
/// Must run before any window exists.
pub fn dpi_aware() {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    unsafe {
        if let Err(e) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            log::warn!("DPI awareness: {e}");
        }
    }
}

pub fn below_normal_priority() {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// Gives the process's memory back to Windows (used while minimized).
pub fn trim_working_set() {
    unsafe {
        let _ = EmptyWorkingSet(GetCurrentProcess());
    }
}

/// Dark title bar in the carbon colour (Windows 11; harmless elsewhere).
pub fn dark_title_bar(hwnd: HWND) {
    unsafe {
        let on: i32 = 1;
        let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(20), &on as *const _ as _, 4);
        let carbon = COLORREF(0x000F_1117); // #17110F as 0x00BBGGRR
        let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(35), &carbon as *const _ as _, 4);
    }
}

type RemoteHandler = Box<dyn Fn(Remote)>;

struct Taskbar {
    list: Option<ITaskbarList3>,
    icons: [HICON; 4], // previous, play, pause, next
    playing: bool,
}

/// The "TaskbarButtonCreated" message id. Separate from `TASKBAR`, because taskbar calls send
/// messages back to the window while that is borrowed.
static CREATED_MSG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

thread_local! {
    static TASKBAR: RefCell<Option<Taskbar>> = const { RefCell::new(None) };
    static REMOTE: RefCell<Option<RemoteHandler>> = const { RefCell::new(None) };
}

const ID_PREV: u32 = 1;
const ID_PLAY: u32 = 2;
const ID_NEXT: u32 = 3;

/// Hooks the window: taskbar buttons, minimize/restore. `remote` runs on the UI thread, outside
/// the window procedure.
pub fn install(hwnd: HWND, remote: impl Fn(Remote) + 'static) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    REMOTE.with(|r| *r.borrow_mut() = Some(Box::new(remote)));
    let icons = [
        glyph(Glyph::Previous),
        glyph(Glyph::Play),
        glyph(Glyph::Pause),
        glyph(Glyph::Next),
    ];
    let created = unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) };
    CREATED_MSG.store(created, std::sync::atomic::Ordering::Relaxed);
    TASKBAR.with(|t| {
        *t.borrow_mut() = Some(Taskbar {
            list: None,
            icons,
            playing: false,
        })
    });
    unsafe {
        let _ = SetWindowSubclass(hwnd, Some(subclass), 1, 0);
    }
    // The taskbar button may already exist (the window was shown before we hooked it).
    add_buttons(hwnd);
}

fn emit(r: Remote) {
    // Deferred, so the handler never runs inside the window procedure.
    let _ = slint::invoke_from_event_loop(move || {
        REMOTE.with(|f| {
            if let Some(f) = f.borrow().as_ref() {
                f(r)
            }
        })
    });
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        // The size type alone isn't reliable (a restored-size message can arrive while still
        // minimized), so ask the window itself.
        WM_SIZE => match wp.0 as u32 {
            SIZE_MINIMIZED | SIZE_RESTORED | SIZE_MAXIMIZED => {
                emit(if unsafe { IsIconic(hwnd) }.as_bool() {
                    Remote::Minimized
                } else {
                    Remote::Restored
                })
            }
            _ => {}
        },
        // Side buttons: XBUTTON1 is back, XBUTTON2 forward.
        WM_XBUTTONUP => {
            match (wp.0 >> 16) & 0xFFFF {
                1 => emit(Remote::Back),
                2 => emit(Remote::Forward),
                _ => {}
            }
            return LRESULT(1);
        }
        WM_COMMAND if (wp.0 >> 16) as u32 == THBN_CLICKED => match (wp.0 & 0xFFFF) as u32 {
            ID_PREV => emit(Remote::Previous),
            ID_PLAY => emit(Remote::PlayPause),
            ID_NEXT => emit(Remote::Next),
            _ => {}
        },
        m if m != 0 && m == CREATED_MSG.load(std::sync::atomic::Ordering::Relaxed) => {
            add_buttons(hwnd)
        }
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

fn button(id: u32, icon: HICON, tip: &str) -> THUMBBUTTON {
    let mut b = THUMBBUTTON {
        dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
        iId: id,
        hIcon: icon,
        dwFlags: THBF_ENABLED,
        ..Default::default()
    };
    for (d, s) in b.szTip.iter_mut().zip(tip.encode_utf16()) {
        *d = s;
    }
    b
}

fn buttons(t: &Taskbar) -> [THUMBBUTTON; 3] {
    [
        button(ID_PREV, t.icons[0], "Previous"),
        button(
            ID_PLAY,
            if t.playing { t.icons[2] } else { t.icons[1] },
            if t.playing { "Pause" } else { "Play" },
        ),
        button(ID_NEXT, t.icons[3], "Next"),
    ]
}

fn add_buttons(hwnd: HWND) {
    // Take what's needed and release the borrow before calling the taskbar.
    let Some((list, buttons)) = TASKBAR.with(|t| {
        let mut t = t.try_borrow_mut().ok()?;
        let t = t.as_mut()?;
        if t.list.is_none() {
            match unsafe {
                CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER)
            } {
                Ok(l) => {
                    let _ = unsafe { l.HrInit() };
                    t.list = Some(l);
                }
                Err(e) => {
                    log::warn!("taskbar: {e}");
                    return None;
                }
            }
        }
        Some((t.list.clone()?, buttons(t)))
    }) else {
        return;
    };
    // Fails harmlessly if the buttons were already added.
    let _ = unsafe { list.ThumbBarAddButtons(hwnd, &buttons) };
}

/// Switches the taskbar's middle button between play and pause.
pub fn taskbar_playing(hwnd: HWND, playing: bool) {
    let Some((list, buttons)) = TASKBAR.with(|t| {
        let mut t = t.try_borrow_mut().ok()?;
        let t = t.as_mut()?;
        if t.playing == playing {
            return None;
        }
        t.playing = playing;
        Some((t.list.clone()?, buttons(t)))
    }) else {
        return;
    };
    let _ = unsafe { list.ThumbBarUpdateButtons(hwnd, &buttons) };
}

// ---- media flyout ------------------------------------------------------------------------------

pub struct Media {
    smtc: SystemMediaTransportControls,
}

impl Media {
    pub fn new(hwnd: HWND) -> windows::core::Result<Self> {
        let interop = windows::core::factory::<
            SystemMediaTransportControls,
            ISystemMediaTransportControlsInterop,
        >()?;
        let smtc: SystemMediaTransportControls = unsafe { interop.GetForWindow(hwnd)? };
        smtc.SetIsEnabled(true)?;
        smtc.SetIsPlayEnabled(true)?;
        smtc.SetIsPauseEnabled(true)?;
        smtc.SetIsNextEnabled(true)?;
        smtc.SetIsPreviousEnabled(true)?;
        smtc.ButtonPressed(&windows::Foundation::TypedEventHandler::new(
            move |_,
                  args: windows::core::Ref<
                windows::Media::SystemMediaTransportControlsButtonPressedEventArgs,
            >| {
                if let Some(a) = args.as_ref() {
                    let r = match a.Button()? {
                        SystemMediaTransportControlsButton::Play => Remote::Play,
                        SystemMediaTransportControlsButton::Pause => Remote::Pause,
                        SystemMediaTransportControlsButton::Next => Remote::Next,
                        SystemMediaTransportControlsButton::Previous => Remote::Previous,
                        _ => return Ok(()),
                    };
                    emit(r);
                }
                Ok(())
            },
        ))?;
        smtc.PlaybackPositionChangeRequested(&windows::Foundation::TypedEventHandler::new(
            move |_, args: windows::core::Ref<PlaybackPositionChangeRequestedEventArgs>| {
                if let Some(a) = args.as_ref() {
                    // 100 ns units.
                    let ms = a.RequestedPlaybackPosition()?.Duration.max(0) / 10_000;
                    emit(Remote::Seek(ms as u64));
                }
                Ok(())
            },
        ))?;
        Ok(Media { smtc })
    }

    /// Where the song is, for the flyout's seek bar (both 0: none).
    pub fn timeline(&self, position_ms: u64, duration_ms: u64) {
        let span = |ms: u64| windows::Foundation::TimeSpan {
            Duration: ms as i64 * 10_000,
        };
        let r = (|| -> windows::core::Result<()> {
            let t = SystemMediaTransportControlsTimelineProperties::new()?;
            t.SetStartTime(span(0))?;
            t.SetMinSeekTime(span(0))?;
            t.SetEndTime(span(duration_ms))?;
            t.SetMaxSeekTime(span(duration_ms))?;
            t.SetPosition(span(position_ms.min(duration_ms)))?;
            self.smtc.UpdateTimelineProperties(&t)
        })();
        if let Err(e) = r {
            log::debug!("media timeline: {e}");
        }
    }

    pub fn track(&self, title: &str, artist: &str, album: &str, art_url: Option<&str>) {
        let r = (|| -> windows::core::Result<()> {
            let d = self.smtc.DisplayUpdater()?;
            d.SetType(MediaPlaybackType::Music)?;
            let m = d.MusicProperties()?;
            m.SetTitle(&HSTRING::from(title))?;
            m.SetArtist(&HSTRING::from(artist))?;
            m.SetAlbumTitle(&HSTRING::from(album))?;
            match art_url {
                Some(u) => d.SetThumbnail(&RandomAccessStreamReference::CreateFromUri(
                    &windows::Foundation::Uri::CreateUri(&HSTRING::from(u))?,
                )?)?,
                None => d.SetThumbnail(None)?,
            }
            d.Update()
        })();
        if let Err(e) = r {
            log::warn!("media controls: {e}");
        }
    }

    pub fn status(&self, playing: Option<bool>) {
        let s = match playing {
            Some(true) => MediaPlaybackStatus::Playing,
            Some(false) => MediaPlaybackStatus::Paused,
            None => MediaPlaybackStatus::Stopped,
        };
        let _ = self.smtc.SetPlaybackStatus(s);
    }
}

// ---- taskbar glyphs ----------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Glyph {
    Previous,
    Play,
    Pause,
    Next,
}

/// Draws a 20×20 white glyph (4× supersampled) as an icon.
fn glyph(g: Glyph) -> HICON {
    const N: usize = 20;
    type Poly = &'static [(f32, f32)];
    let polys: &[Poly] = match g {
        Glyph::Play => &[&[(6.0, 4.0), (16.0, 10.0), (6.0, 16.0)]],
        Glyph::Pause => &[
            &[(5.0, 4.0), (8.5, 4.0), (8.5, 16.0), (5.0, 16.0)],
            &[(11.5, 4.0), (15.0, 4.0), (15.0, 16.0), (11.5, 16.0)],
        ],
        Glyph::Next => &[
            &[(4.0, 4.5), (12.5, 10.0), (4.0, 15.5)],
            &[(13.5, 4.5), (16.0, 4.5), (16.0, 15.5), (13.5, 15.5)],
        ],
        Glyph::Previous => &[
            &[(16.0, 4.5), (7.5, 10.0), (16.0, 15.5)],
            &[(4.0, 4.5), (6.5, 4.5), (6.5, 15.5), (4.0, 15.5)],
        ],
    };
    let inside = |x: f32, y: f32| {
        polys.iter().any(|p| {
            let mut c = false;
            let mut j = p.len() - 1;
            for i in 0..p.len() {
                let (xi, yi) = p[i];
                let (xj, yj) = p[j];
                if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                    c = !c;
                }
                j = i;
            }
            c
        })
    };
    let mut bgra = vec![0u8; N * N * 4];
    for py in 0..N {
        for px in 0..N {
            let mut hits = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    if inside(
                        px as f32 + (sx as f32 + 0.5) / 4.0,
                        py as f32 + (sy as f32 + 0.5) / 4.0,
                    ) {
                        hits += 1;
                    }
                }
            }
            let a = (hits * 255 / 16) as u8;
            let o = (py * N + px) * 4;
            bgra[o..o + 4].copy_from_slice(&[a, a, a, a]); // premultiplied white
        }
    }
    unsafe {
        let color = CreateBitmap(N as i32, N as i32, 1, 32, Some(bgra.as_ptr() as _));
        let mask_bits = [0u8; N * N / 8 + N];
        let mask = CreateBitmap(N as i32, N as i32, 1, 1, Some(mask_bits.as_ptr() as _));
        let info = ICONINFO {
            fIcon: true.into(),
            hbmMask: mask,
            hbmColor: color,
            ..Default::default()
        };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// Puts text on the clipboard.
pub fn copy_text(hwnd: HWND, text: &str) {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let p = GlobalLock(mem) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                let _ = GlobalUnlock(mem);
                // The clipboard owns the memory from here.
                let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0)));
            }
        }
        let _ = CloseClipboard();
    }
}

/// The clipboard's text, if it holds any.
pub fn paste_text(hwnd: HWND) -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;
    unsafe {
        OpenClipboard(Some(hwnd)).ok()?;
        let mut text = None;
        if let Ok(h) = GetClipboardData(CF_UNICODETEXT.0 as u32) {
            let mem = HGLOBAL(h.0);
            let p = GlobalLock(mem) as *const u16;
            if !p.is_null() {
                let max = GlobalSize(mem) / 2;
                let len = (0..max).take_while(|&i| *p.add(i) != 0).count();
                text = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)));
                let _ = GlobalUnlock(mem);
            }
        }
        let _ = CloseClipboard();
        text
    }
}
