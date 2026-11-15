//! Windows Ghost Mode curtain (see the [`crate::ghost`] module docs for the design).
//!
//! A dedicated UI thread owns the curtain windows and runs their message loop:
//! * one `WS_POPUP` window per monitor (`EnumDisplayMonitors` → `rcMonitor`, per-monitor-v2 DPI
//!   aware so the rectangles are physical pixels), class background = black brush;
//! * `WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` (topmost band, no taskbar button, never
//!   takes focus) plus `WS_EX_LAYERED | WS_EX_TRANSPARENT` at alpha 255: opaque but click-through,
//!   so input injected by the remote controller reaches the applications underneath;
//! * `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` (Windows 10 2004+): DXGI Desktop
//!   Duplication and Windows.Graphics.Capture see the desktop *without* the curtain. Older builds
//!   only support `WDA_MONITOR`, which renders the window black in captures (the remote side would
//!   see black) — reported as `capture_exclusion = "monitor"`;
//! * a 250 ms timer re-inserts the windows at the top of the topmost band (the taskbar raises
//!   itself whenever it is activated) and `WM_DISPLAYCHANGE` rebuilds them for the new layout.
//!
//! `hide` posts `WM_QUIT` to the thread, which destroys its windows before exiting.

use crate::ghost::Shown;
use crate::{DisplayError, Result};
use parking_lot::Mutex;
use std::sync::mpsc;
use std::thread::JoinHandle;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BLACK_BRUSH, EnumDisplayMonitors, GetMonitorInfoW, GetStockObject, HBRUSH, HDC, HMONITOR, MONITORINFO};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWL_EXSTYLE, GetMessageW,
    GetWindowLongW, HWND_TOPMOST, IDC_ARROW, KillTimer, LWA_ALPHA, LoadCursorW, MA_NOACTIVATE, MSG, PostThreadMessageW,
    RegisterClassExW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetLayeredWindowAttributes,
    SetTimer, SetWindowDisplayAffinity, SetWindowPos, ShowWindow, TranslateMessage, WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR,
    WM_APP, WM_CLOSE, WM_DISPLAYCHANGE, WM_MOUSEACTIVATE, WM_QUIT, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{BOOL, PCWSTR, w};

/// z-order band codes reported in `GhostStatus::window_level` / `shielding_level`.
pub(crate) const BAND_NORMAL: i64 = 0;
pub(crate) const BAND_TOPMOST: i64 = 1;

const CLASS_NAME: PCWSTR = w!("AetherLinkGhostCurtain");
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 250;
/// Posted to the UI thread to rebuild the curtain after a display change.
const WM_REBUILD: u32 = WM_APP + 1;

struct UiThread {
    thread_id: u32,
    join: JoinHandle<()>,
}

static UI: Mutex<Option<UiThread>> = Mutex::new(None);

pub fn shielding_level() -> i64 {
    BAND_TOPMOST
}

pub fn show() -> Result<Shown> {
    hide()?;
    let (tx, rx) = mpsc::channel::<Result<(u32, Shown)>>();
    let join = std::thread::Builder::new()
        .name("aether-ghost-ui".into())
        .spawn(move || ui_thread(tx))
        .map_err(|e| DisplayError::Os(format!("spawn curtain thread: {e}")))?;
    match rx.recv() {
        Ok(Ok((thread_id, shown))) => {
            *UI.lock() = Some(UiThread { thread_id, join });
            Ok(shown)
        }
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => {
            let _ = join.join();
            Err(DisplayError::Os("curtain thread exited unexpectedly".into()))
        }
    }
}

pub fn hide() -> Result<()> {
    if let Some(ui) = UI.lock().take() {
        // SAFETY: posting to a thread id we own; failure means the thread is already gone.
        let _ = unsafe { PostThreadMessageW(ui.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        let _ = ui.join.join();
    }
    Ok(())
}

fn ui_thread(tx: mpsc::Sender<Result<(u32, Shown)>>) {
    // SAFETY: Win32 calls on this thread with valid arguments; windows are destroyed before exit.
    unsafe {
        // Physical-pixel monitor rectangles regardless of the process DPI awareness (1607+).
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let mut windows = match build() {
            Ok((wins, shown)) => {
                let _ = tx.send(Ok((GetCurrentThreadId(), shown)));
                wins
            }
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        let _ = SetTimer(None, TIMER_ID, TIMER_MS, None);
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 == 0 || r.0 == -1 {
                break; // WM_QUIT or error
            }
            match (msg.hwnd.is_invalid(), msg.message) {
                (true, WM_TIMER) => raise(&windows),
                (true, WM_REBUILD) => {
                    destroy(&mut windows);
                    match build() {
                        Ok((w, _)) => windows = w,
                        Err(e) => tracing::warn!("ghost curtain rebuild failed: {e}"),
                    }
                }
                _ => {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        let _ = KillTimer(None, TIMER_ID);
        destroy(&mut windows);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Never take focus, never close on Alt+F4 / WM_CLOSE from other processes.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_CLOSE => LRESULT(0),
        WM_DISPLAYCHANGE => {
            // SAFETY: the window procedure runs on the UI thread; post to its own queue.
            let _ = unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_REBUILD, WPARAM(0), LPARAM(0)) };
            LRESULT(0)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn register_class() -> Result<()> {
    // SAFETY: registering a window class with a static name and our wndproc; re-registration
    // (ERROR_CLASS_ALREADY_EXISTS) on a later show is fine.
    unsafe {
        let hinstance = GetModuleHandleW(None).map_err(|e| DisplayError::Os(format!("GetModuleHandleW: {e}")))?;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
    Ok(())
}

fn monitor_rects() -> Vec<RECT> {
    unsafe extern "system" fn cb(hmon: HMONITOR, _hdc: HDC, _rc: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the &mut Vec passed below, alive for the EnumDisplayMonitors call.
        let out = unsafe { &mut *(data.0 as *mut Vec<RECT>) };
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        // SAFETY: valid monitor handle from the enumeration.
        if unsafe { GetMonitorInfoW(hmon, &mut mi) }.as_bool() {
            out.push(mi.rcMonitor);
        }
        BOOL(1)
    }
    let mut out: Vec<RECT> = Vec::new();
    // SAFETY: the callback only touches `out` during the call.
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut out as *mut _ as isize)) };
    out
}

/// Creates the curtain windows on the current (UI) thread.
fn build() -> Result<(Vec<HWND>, Shown)> {
    register_class()?;
    let rects = monitor_rects();
    if rects.is_empty() {
        return Err(DisplayError::Os("no monitors to cover".into()));
    }
    let mut windows = Vec::new();
    let mut all_excluded = true;
    let mut all_topmost = true;
    for r in rects {
        // SAFETY: standard window creation on the thread that runs the message loop.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
                CLASS_NAME,
                w!("AetherLink Ghost Mode"),
                WS_POPUP,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                None,
                None,
                None,
                None,
            )
        };
        let hwnd = match hwnd {
            Ok(h) => h,
            Err(e) => {
                destroy(&mut windows);
                return Err(DisplayError::Os(format!("CreateWindowExW: {e}")));
            }
        };
        windows.push(hwnd);
        // SAFETY: valid window owned by this thread.
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            if SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).is_err() {
                all_excluded = false;
                if let Err(e) = SetWindowDisplayAffinity(hwnd, WDA_MONITOR) {
                    tracing::warn!("SetWindowDisplayAffinity(WDA_MONITOR) failed: {e}");
                }
            }
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            all_topmost &= GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0;
        }
    }
    let shown = Shown {
        windows: windows.len(),
        level: if all_topmost { BAND_TOPMOST } else { BAND_NORMAL },
        shielding: BAND_TOPMOST,
        exclusion: if all_excluded { "excludeFromCapture" } else { "monitor" },
    };
    Ok((windows, shown))
}

/// Re-inserts the curtain at the top of the topmost band (above the taskbar).
fn raise(windows: &[HWND]) {
    for &h in windows {
        // SAFETY: windows owned by this thread.
        let _ = unsafe { SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
    }
}

fn destroy(windows: &mut Vec<HWND>) {
    for h in windows.drain(..) {
        // SAFETY: windows owned by this thread.
        let _ = unsafe { DestroyWindow(h) };
    }
}
