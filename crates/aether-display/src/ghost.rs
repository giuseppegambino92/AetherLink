//! Ghost Mode: system-level privacy curtain + backlight off.
//!
//! The curtain must hide the screen from people at the host while the remote controller keeps
//! seeing the real desktop, so every backend pairs "opaque window above everything" with a way
//! to keep that window out of the capture ([`GhostStatus::capture_exclusion`]).
//!
//! macOS implementation:
//! * one borderless, opaque, black `NSWindow` per `NSScreen`, at
//!   `CGShieldingWindowLevel()` — the level used by display capture, which sits above the menu
//!   bar, the Dock, Notification Center, Mission Control and full-screen spaces — with
//!   `canJoinAllSpaces | fullScreenAuxiliary | stationary | ignoresCycle` so it follows every
//!   Space, and `sharingType = none` so it is never part of a screen capture (the capture
//!   filter additionally excludes the AetherLink process);
//! * the curtain swallows local mouse clicks (it is a real window), so people at the machine
//!   cannot interfere while the remote controller works;
//! * optional hardware backlight dimming to 0 via [`crate::brightness`], persisted to a
//!   restore file first so a crash is undone at next launch.
//!
//! Windows implementation (`ghost_windows.rs`): one borderless `WS_POPUP` black window per
//! monitor (`EnumDisplayMonitors`) with `WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`,
//! owned by a dedicated UI thread with its own message loop, re-asserted above the taskbar every
//! 250 ms and rebuilt on `WM_DISPLAYCHANGE`. `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`
//! (Windows 10 2004+) keeps it out of DXGI duplication and Windows.Graphics.Capture; older builds
//! fall back to `WDA_MONITOR`, which makes captures show *black* where the curtain is (reported
//! as `capture_exclusion = "monitor"`). The windows are layered + `WS_EX_TRANSPARENT`, i.e.
//! click-through: input injected by the remote controller (`SendInput`) must reach the apps
//! underneath, so the curtain hides the screen but does not lock local input.
//!
//! Linux implementation (`ghost_x11.rs`, X11 only): one override-redirect black window per RandR
//! monitor, kept on top of the root stacking order by a keeper thread, with an empty input shape
//! (click-through, for the same reason as on Windows). An X11 capture of the root window
//! *includes* every mapped window, so the curtain would be captured; the AetherLink X11
//! capturer (`aether-capture`, `linux_x11.rs`) therefore recognises the curtain windows (listed
//! in the `_AETHERLINK_GHOST_CURTAINS` root property) and composes the frame from the
//! Composite-extension pixmaps of the windows *below* them (`capture_exclusion =
//! "x11Composite"`). Without the Composite extension, or with third-party X11 capture tools, the
//! curtain is visible in the capture (`"none"`). Wayland returns [`DisplayError::Unsupported`]:
//! a client cannot place a surface above other clients without a compositor protocol
//! (wlr-layer-shell is not available on GNOME), and portal captures include every surface.
//!
//! ## `window_level` / `shielding_level` per OS
//! * macOS: the `NSWindow` level applied / `CGShieldingWindowLevel()`.
//! * Windows: z-order band — `0` normal, `1` the topmost band (`HWND_TOPMOST`, the band of the
//!   taskbar). `shielding_level` is `1`, the highest band an app without UIAccess can use (the
//!   Start menu, Action Center, UAC and the lock screen live in higher system bands).
//! * Linux X11: `1` when the curtain is the top of the root window's stacking order at enable
//!   time; `shielding_level` is `1`.
//!
//! The selftest treats `window_level == shielding_level` as "above everything we can be above".
//!
//! The panic hotkey (Ctrl+Opt+Cmd+Esc) is registered by the app shell and calls [`disable`].

use crate::{Result, brightness};
use parking_lot::Mutex;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct GhostOptions {
    /// Turn the built-in backlight off while the curtain is up.
    pub dim_backlight: bool,
    /// Where to persist the previous backlight levels (crash-safe restore).
    pub restore_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GhostStatus {
    pub active: bool,
    pub curtain_windows: usize,
    /// The window level actually applied (equals `CGShieldingWindowLevel()` on macOS).
    pub window_level: i64,
    pub shielding_level: i64,
    pub dimmed_displays: Vec<u32>,
    pub backlight_error: Option<String>,
    /// How the curtain is kept out of AetherLink's own screen capture: `"sharingTypeNone"`
    /// (macOS), `"excludeFromCapture"` / `"monitor"` (Windows `WDA_*`; `monitor` = captures show
    /// black), `"x11Composite"` / `"none"` (Linux X11). Empty while inactive.
    pub capture_exclusion: String,
}

/// What a backend reports after raising its curtain.
pub(crate) struct Shown {
    pub windows: usize,
    pub level: i64,
    pub shielding: i64,
    pub exclusion: &'static str,
}

struct State {
    restore: Option<brightness::BrightnessSnapshot>,
    restore_file: Option<PathBuf>,
    status: GhostStatus,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Raise the curtain (idempotent: re-raising refreshes it for newly attached screens).
pub fn enable(opts: &GhostOptions) -> Result<GhostStatus> {
    let shown = imp::show()?;
    let mut guard = STATE.lock();
    let already_dimmed = guard.as_ref().and_then(|s| s.restore.clone());
    let (restore, dim_err) = match (opts.dim_backlight, already_dimmed) {
        (_, Some(prev)) => (Some(prev), None),
        (true, None) => match brightness::dim(opts.restore_file.as_deref()) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e.to_string())),
        },
        (false, None) => (None, None),
    };
    let status = GhostStatus {
        active: true,
        curtain_windows: shown.windows,
        window_level: shown.level,
        shielding_level: shown.shielding,
        dimmed_displays: restore.as_ref().map(|s| s.levels.iter().map(|(id, _)| *id).collect()).unwrap_or_default(),
        backlight_error: dim_err,
        capture_exclusion: shown.exclusion.to_string(),
    };
    *guard = Some(State { restore, restore_file: opts.restore_file.clone(), status: status.clone() });
    Ok(status)
}

/// Lower the curtain and restore the exact previous backlight levels (idempotent).
pub fn disable() -> Result<()> {
    let prev = STATE.lock().take();
    let hide = imp::hide();
    if let Some(st) = prev {
        if let Some(snap) = st.restore {
            brightness::restore(&snap)?;
        }
        if let Some(f) = st.restore_file {
            let _ = std::fs::remove_file(f);
        }
    }
    hide
}

pub fn status() -> GhostStatus {
    STATE.lock().as_ref().map(|s| s.status.clone()).unwrap_or(GhostStatus {
        active: false,
        curtain_windows: 0,
        window_level: 0,
        shielding_level: imp::shielding_level(),
        dimmed_displays: vec![],
        backlight_error: None,
        capture_exclusion: String::new(),
    })
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Shown;
    use crate::main_thread::run_sync;
    use crate::{DisplayError, Result};
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2::rc::Retained;
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSColor, NSScreen, NSWindow, NSWindowCollectionBehavior, NSWindowSharingType,
        NSWindowStyleMask,
    };
    use std::cell::RefCell;

    thread_local! {
        // Only ever touched on the main thread (inside `run_sync`).
        static WINDOWS: RefCell<Vec<Retained<NSWindow>>> = const { RefCell::new(Vec::new()) };
    }

    pub fn shielding_level() -> i64 {
        objc2_core_graphics::CGShieldingWindowLevel() as i64
    }

    pub fn show() -> Result<Shown> {
        run_sync(show_on_main)
    }

    fn show_on_main(mtm: MainThreadMarker) -> Result<Shown> {
        // Make sure an NSApplication exists (no-op inside the desktop app).
        let _app = NSApplication::sharedApplication(mtm);
        hide_on_main();
        let shielding = objc2_core_graphics::CGShieldingWindowLevel() as isize;
        let screens = NSScreen::screens(mtm);
        let mut level = 0i64;
        let mut created = Vec::new();
        for screen in screens.iter() {
            let frame = screen.frame();
            // SAFETY: standard designated initializer on the main thread.
            let w = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer_screen(
                    NSWindow::alloc(mtm),
                    frame,
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::Buffered,
                    false,
                    Some(&screen),
                )
            };
            // SAFETY: we keep the window alive in WINDOWS; AppKit must not free it on close.
            unsafe { w.setReleasedWhenClosed(false) };
            w.setLevel(shielding);
            w.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
                    | NSWindowCollectionBehavior::Stationary
                    | NSWindowCollectionBehavior::IgnoresCycle,
            );
            w.setSharingType(NSWindowSharingType::None);
            w.setBackgroundColor(Some(&NSColor::blackColor()));
            w.setOpaque(true);
            w.setHasShadow(false);
            w.setIgnoresMouseEvents(false);
            w.setFrame_display(frame, true);
            w.orderFrontRegardless();
            level = w.level() as i64;
            created.push(w);
        }
        if created.is_empty() {
            return Err(DisplayError::Os("no screens to cover".into()));
        }
        let n = created.len();
        WINDOWS.with(|ws| ws.borrow_mut().extend(created));
        Ok(Shown { windows: n, level, shielding: shielding as i64, exclusion: "sharingTypeNone" })
    }

    fn hide_on_main() {
        WINDOWS.with(|ws| {
            for w in ws.borrow_mut().drain(..) {
                w.orderOut(None);
                w.close();
            }
        });
    }

    pub fn hide() -> Result<()> {
        run_sync(|_| hide_on_main());
        Ok(())
    }

    /// Number of curtain windows currently on screen (main-thread query).
    #[allow(dead_code)]
    pub fn visible_count() -> usize {
        run_sync(|_| WINDOWS.with(|ws| ws.borrow().iter().filter(|w| w.isVisible()).count()))
    }
}

#[cfg(target_os = "windows")]
use crate::ghost_windows as imp;

#[cfg(target_os = "linux")]
use crate::ghost_x11 as imp;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod imp {
    use super::Shown;
    use crate::{DisplayError, Result};

    pub fn shielding_level() -> i64 {
        0
    }

    pub fn show() -> Result<Shown> {
        Err(DisplayError::Unsupported("the system-level privacy curtain is implemented for macOS, Windows and Linux X11"))
    }

    pub fn hide() -> Result<()> {
        Ok(())
    }
}

/// Pump the main run loop for `secs` so curtain windows actually render (CLI self-test only;
/// inside the app the AppKit run loop does this). Must be called on the main thread.
#[cfg(target_os = "macos")]
pub fn pump_main_runloop(secs: f64) {
    use objc2_foundation::{NSDate, NSRunLoop};
    let until = NSDate::dateWithTimeIntervalSinceNow(secs);
    NSRunLoop::currentRunLoop().runUntilDate(&until);
}
