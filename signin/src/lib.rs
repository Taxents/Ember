//! The sign-in window: a small WebView2 window showing Google's own sign-in page. The user signs
//! in there themselves; once the browser lands on music.youtube.com with a signed-in session, the
//! window reads the YouTube cookies and closes. The WebView's profile folder is deleted afterwards,
//! so nothing of the browser session stays behind except what the caller stores (encrypted).

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
use windows::core::{HSTRING, Interface, PWSTR, w};

const SIGN_IN_URL: &str = "https://accounts.google.com/ServiceLogin?service=youtube&passive=true\
    &continue=https%3A%2F%2Fwww.youtube.com%2Fsignin%3Faction_handle_signin%3Dtrue%26app%3Ddesktop\
    %26next%3Dhttps%253A%252F%252Fmusic.youtube.com%252F";
const COOKIE_URL: &str = "https://music.youtube.com";

pub type Cookies = Vec<(String, String)>;

#[derive(Debug)]
pub enum Error {
    /// The WebView2 runtime is missing or failed to start.
    WebView(String),
    /// The user closed the window before signing in.
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::WebView(e) => write!(f, "sign-in window: {e}"),
            Error::Cancelled => write!(f, "sign-in cancelled"),
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

/// Shows the sign-in window and blocks until the user has signed in (returns the cookies for
/// music.youtube.com) or closed it. Runs its own message loop, so call it from a thread with
/// no other windows, e.g. a dedicated one.
pub fn sign_in(profile_root: &std::path::Path) -> Result<Cookies, Error> {
    // A fresh profile per attempt: one left locked by a crashed attempt can't block this one.
    clean_up(profile_root);
    let profile_dir = profile_root.join(format!("{}", std::process::id()));
    let result = run(&profile_dir);
    // The browser processes release the profile shortly after the window is gone.
    for _ in 0..25 {
        if std::fs::remove_dir_all(&profile_dir).is_ok() || !profile_dir.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    result
}

/// Deletes the WebView profiles earlier sign-ins left in `profile_root` (one that crashed, or
/// whose browser held on to its folder too long). They hold that browser session's Google
/// cookies, so call this at start too. One still in use by a running sign-in stays.
pub fn clean_up(profile_root: &std::path::Path) {
    if let Ok(old) = std::fs::read_dir(profile_root) {
        for e in old.flatten() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn run(profile_dir: &std::path::Path) -> Result<Cookies, Error> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let hwnd = create_window()?;
    let result = Rc::new(RefCell::new(None::<Cookies>));

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

        // After each page load: once on YouTube Music with a session, take the cookies.
        let found = result.clone();
        let mut token = 0i64;
        webview.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |sender, _args| {
                let Some(sender) = sender else { return Ok(()) };
                let mut uri = PWSTR::null();
                sender.Source(&mut uri)?;
                if !take_pwstr(uri).starts_with(COOKIE_URL) {
                    return Ok(());
                }
                let found = found.clone();
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
                            *found.borrow_mut() = Some(all);
                            let _ = DestroyWindow(hwnd);
                        }
                        Ok(())
                    })),
                )
            })),
            &mut token,
        )?;
        webview.Navigate(&HSTRING::from(SIGN_IN_URL))?;

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        CONTROLLER.with(|c| c.borrow_mut().take());
        let _ = controller.Close();
    }
    result.borrow_mut().take().ok_or(Error::Cancelled)
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

fn create_window() -> Result<HWND, Error> {
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
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
