//! The sign-in window: a small WebView2 window showing Google's own sign-in page. The user signs
//! in there themselves; once the browser lands on music.youtube.com with a signed-in session, the
//! window reads the YouTube cookies and closes.
//!
//! The browser's profile is kept while signed in: Google now hands out short-lived cookies that
//! only the browser that signed in can renew, so copies of them stop working within hours.
//! `renew` opens the same profile unseen and reads the cookies the browser renewed. A sign-in
//! that doesn't finish, and `forget` (signing out), delete it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    GetCookiesCompletedHandler, NavigationCompletedEventHandler, take_pwstr,
};
use windows::Win32::Foundation::{E_POINTER, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, Interface, PWSTR, w};

const SIGN_IN_URL: &str = "https://accounts.google.com/ServiceLogin?service=youtube&passive=true\
    &continue=https%3A%2F%2Fwww.youtube.com%2Fsignin%3Faction_handle_signin%3Dtrue%26app%3Ddesktop\
    %26next%3Dhttps%253A%252F%252Fmusic.youtube.com%252F";
const COOKIE_URL: &str = "https://music.youtube.com";
/// The kept profile's folder in `profile_root`.
const PROFILE: &str = "session";
/// How long `renew` waits for YouTube before giving up.
const RENEW_TIMEOUT_MS: u32 = 30_000;

pub type Cookies = Vec<(String, String)>;

#[derive(Debug)]
pub enum Error {
    /// The WebView2 runtime is missing or failed to start.
    WebView(String),
    /// The user closed the window before signing in.
    Cancelled,
    /// The kept browser isn't signed in any more (or there is none): only signing in again helps.
    SignedOut,
    /// YouTube didn't load (offline, or too slow).
    Unreachable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::WebView(e) => write!(f, "sign-in window: {e}"),
            Error::Cancelled => write!(f, "sign-in cancelled"),
            Error::SignedOut => write!(f, "the sign-in browser is signed out"),
            Error::Unreachable => write!(f, "YouTube didn't load"),
        }
    }
}

impl std::error::Error for Error {}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::WebView(e.to_string())
    }
}

impl From<webview2_com::Error> for Error {
    fn from(e: webview2_com::Error) -> Self {
        Error::WebView(format!("{e:?}"))
    }
}

thread_local! {
    /// The controller of this thread's sign-in window, for resizing.
    static CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// The user signs in, in a window of its own.
    SignIn,
    /// Unseen: the kept browser opens YouTube Music, renewing its cookies on the way.
    Renew,
}

/// Shows the sign-in window and blocks until the user has signed in (returns the cookies for
/// music.youtube.com) or closed it. Runs its own message loop, so call it from a thread with
/// no other windows, e.g. a dedicated one.
///
/// It uses the kept profile when there is one: still signed in to Google there, it passes
/// straight through. Kept afterwards for `renew`; a sign-in that doesn't finish deletes it.
pub fn sign_in(profile_root: &std::path::Path) -> Result<Cookies, Error> {
    let result = run(&profile_root.join(PROFILE), Mode::SignIn);
    if result.is_err() {
        forget(profile_root);
    }
    result
}

/// The cookies for music.youtube.com as the profile kept by `sign_in` holds them now: unseen,
/// it opens YouTube Music, and the browser renews what it must on the way, as it would for any
/// open tab. Blocks for up to 30 s; call it from a thread with no other windows.
pub fn renew(profile_root: &std::path::Path) -> Result<Cookies, Error> {
    let dir = profile_root.join(PROFILE);
    if !dir.exists() {
        return Err(Error::SignedOut);
    }
    run(&dir, Mode::Renew)
}

/// Signing out: deletes the kept profile (it holds the browser's Google cookies), and anything
/// else earlier sign-ins left in `profile_root`.
pub fn forget(profile_root: &std::path::Path) {
    let dir = profile_root.join(PROFILE);
    // The browser processes release the profile shortly after the window is gone.
    for _ in 0..25 {
        if std::fs::remove_dir_all(&dir).is_ok() || !dir.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    clean_up(profile_root);
}

/// Deletes the WebView profiles earlier versions and crashed sign-ins left in `profile_root`
/// (they hold that browser session's Google cookies), all but the kept one. Call this at start.
pub fn clean_up(profile_root: &std::path::Path) {
    if let Ok(old) = std::fs::read_dir(profile_root) {
        for e in old.flatten() {
            if e.file_name() != PROFILE {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

fn run(profile_dir: &std::path::Path, mode: Mode) -> Result<Cookies, Error> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let hwnd = create_window(mode == Mode::SignIn)?;
    if mode == Mode::Renew {
        // WM_TIMER closes the window: given up.
        unsafe { SetTimer(Some(hwnd), 1, RENEW_TIMEOUT_MS, None) };
    }
    let result = Rc::new(RefCell::new(None::<Result<Cookies, Error>>));

    let environment = {
        let (tx, rx) = mpsc::channel();
        let folder = HSTRING::from(profile_dir.as_os_str());
        CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                CreateCoreWebView2EnvironmentWithOptions(None, &folder, None, &handler)
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |hr, env| {
                hr?;
                tx.send(env.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                    .ok();
                Ok(())
            }),
        )?;
        rx.recv()
            .map_err(|_| Error::WebView("no environment".into()))??
    };

    let controller = {
        let (tx, rx) = mpsc::channel();
        CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                environment
                    .CreateCoreWebView2Controller(hwnd, &handler)
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |hr, c| {
                hr?;
                tx.send(c.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                    .ok();
                Ok(())
            }),
        )?;
        rx.recv()
            .map_err(|_| Error::WebView("no controller".into()))??
    };

    unsafe {
        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);
        controller.SetBounds(rect)?;
        controller.SetIsVisible(true)?;
        CONTROLLER.with(|c| *c.borrow_mut() = Some(controller.clone()));

        let webview = controller.CoreWebView2()?;
        let settings = webview.Settings()?;
        settings.SetAreDevToolsEnabled(false)?;
        settings.SetIsStatusBarEnabled(false)?;
        let cookies: ICoreWebView2CookieManager =
            webview.cast::<ICoreWebView2_2>()?.CookieManager()?;

        // After each page load: once on YouTube with a session, take the cookies. A Google
        // interstitial (security checkup, recovery info) can drop the sign-in token on its way
        // back, leaving www.youtube.com showing "There was an issue signing you in"; Google is
        // signed in by then, so starting over passes straight through. Renewing, the page it
        // lands on decides: anywhere but YouTube is Google asking to sign in again.
        let found = result.clone();
        let retries = Rc::new(std::cell::Cell::new(0u32));
        let mut token = 0i64;
        webview.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |sender, args| {
                let Some(sender) = sender else { return Ok(()) };
                let mut uri = PWSTR::null();
                sender.Source(&mut uri)?;
                let uri = take_pwstr(uri);
                if mode == Mode::Renew {
                    let (mut ok, mut status) = (BOOL(1), COREWEBVIEW2_WEB_ERROR_STATUS::default());
                    if let Some(args) = &args {
                        args.IsSuccess(&mut ok)?;
                        args.WebErrorStatus(&mut status)?;
                    }
                    let failed = if ok.as_bool() {
                        (!is_youtube(&uri)).then_some(Error::SignedOut)
                    } else if status == COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED {
                        // Replaced by another navigation (a redirect): that one decides.
                        return Ok(());
                    } else {
                        Some(Error::Unreachable)
                    };
                    if let Some(e) = failed {
                        *found.borrow_mut() = Some(Err(e));
                        let _ = DestroyWindow(hwnd);
                        return Ok(());
                    }
                } else if !is_youtube(&uri) {
                    return Ok(());
                }
                let on_music = uri.starts_with(COOKIE_URL);
                let (found, retries) = (found.clone(), retries.clone());
                cookies.GetCookies(
                    &HSTRING::from(COOKIE_URL),
                    &GetCookiesCompletedHandler::create(Box::new(move |hr, list| {
                        hr?;
                        let Some(list) = list else { return Ok(()) };
                        let all = read_cookies(&list)?;
                        if all
                            .iter()
                            .any(|(k, _)| k == "SAPISID" || k == "__Secure-3PAPISID")
                        {
                            *found.borrow_mut() = Some(Ok(all));
                            let _ = DestroyWindow(hwnd);
                        } else if mode == Mode::Renew {
                            *found.borrow_mut() = Some(Err(Error::SignedOut));
                            let _ = DestroyWindow(hwnd);
                        } else if !on_music && retries.get() < 2 {
                            retries.set(retries.get() + 1);
                            sender.Navigate(&HSTRING::from(SIGN_IN_URL))?;
                        }
                        Ok(())
                    })),
                )
            })),
            &mut token,
        )?;
        webview.Navigate(&HSTRING::from(match mode {
            Mode::SignIn => SIGN_IN_URL,
            Mode::Renew => "https://music.youtube.com/",
        }))?;

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        CONTROLLER.with(|c| c.borrow_mut().take());
        let _ = controller.Close();
    }
    // Closed with no answer: by the user, or renewing timed out.
    result.borrow_mut().take().unwrap_or(Err(match mode {
        Mode::SignIn => Error::Cancelled,
        Mode::Renew => Error::Unreachable,
    }))
}

/// Whether `uri` is an https page on youtube.com or one of its subdomains.
fn is_youtube(uri: &str) -> bool {
    let Some(rest) = uri.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host == "youtube.com" || host.ends_with(".youtube.com")
}

unsafe fn read_cookies(list: &ICoreWebView2CookieList) -> windows::core::Result<Cookies> {
    let mut n = 0u32;
    unsafe {
        list.Count(&mut n)?;
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let c = list.GetValueAtIndex(i)?;
            let (mut name, mut value) = (PWSTR::null(), PWSTR::null());
            c.Name(&mut name)?;
            c.Value(&mut value)?;
            out.push((take_pwstr(name), take_pwstr(value)));
        }
        Ok(out)
    }
}

/// The sign-in window, shown when `visible`; renewing it's never shown.
fn create_window(visible: bool) -> Result<HWND, Error> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("MusicAppSignIn");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc); // fails harmlessly when already registered
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("Sign in to YouTube Music"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        // 500×720 logical pixels, centred on the screen.
        let scale = GetDpiForWindow(hwnd).max(96) as i32;
        let (w, h) = (500 * scale / 96, 720 * scale / 96);
        if !visible {
            let _ = SetWindowPos(hwnd, None, 0, 0, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
            return Ok(hwnd);
        }
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        // Briefly topmost, so it shows above other windows even when started from a console
        // that doesn't own the foreground.
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            SWP_SHOWWINDOW,
        );
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
        let _ = FlashWindow(hwnd, true);
        Ok(hwnd)
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_SIZE => {
                CONTROLLER.with(|c| {
                    if let Some(c) = c.borrow().as_ref() {
                        let mut rect = RECT::default();
                        let _ = GetClientRect(hwnd, &mut rect);
                        let _ = c.SetBounds(rect);
                    }
                });
                LRESULT(0)
            }
            WM_TIMER => {
                // Renewing took too long.
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
