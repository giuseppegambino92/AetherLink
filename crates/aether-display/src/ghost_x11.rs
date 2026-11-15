//! Linux X11 Ghost Mode curtain (see the [`crate::ghost`] module docs for the design).
//!
//! * One override-redirect, black `InputOutput` window per RandR 1.5 monitor (the whole root
//!   window without RandR), owned by a keeper thread with its own X connection. Every 200 ms the
//!   thread re-raises the curtain if anything got stacked above it (menus, notifications and
//!   other override-redirect windows) and rebuilds it when the monitor layout changes. When the
//!   process dies the X server destroys the windows with the connection, so a crash can never
//!   leave the screen black.
//! * The input shape is empty (SHAPE extension): pointer events pass through to the windows
//!   below, which is what makes XTest-injected remote input work. Local input is not blocked.
//! * Window ids are published in the `_AETHERLINK_GHOST_CURTAINS` root property (`WINDOW[]`);
//!   the AetherLink X11 capturer reads it and composes frames from the Composite pixmaps of the
//!   windows below the curtain, so the remote side never sees it. Other X11 capture tools do.
//! * Wayland sessions are rejected: there is no protocol for a client to cover other clients'
//!   surfaces (wlr-layer-shell is not implemented by GNOME/Mutter), XWayland windows are not
//!   guaranteed to stack above native ones, and portal/PipeWire captures include every surface.

use crate::ghost::Shown;
use crate::{DisplayError, Result};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ClipOrdering, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, PropMode, StackMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Root-window property listing the curtain windows (read by aether-capture's X11 backend).
pub(crate) const CURTAINS_PROPERTY: &str = "_AETHERLINK_GHOST_CURTAINS";
const KEEPER_TICK: Duration = Duration::from_millis(200);

struct Keeper {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

static KEEPER: Mutex<Option<Keeper>> = Mutex::new(None);

fn os<E: std::fmt::Display>(ctx: &'static str) -> impl FnOnce(E) -> DisplayError {
    move |e| DisplayError::Os(format!("X11 {ctx}: {e}"))
}

pub fn shielding_level() -> i64 {
    1
}

/// True for Wayland sessions (also when XWayland provides a `$DISPLAY`).
fn wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
}

pub fn show() -> Result<Shown> {
    hide()?;
    if wayland_session() {
        return Err(DisplayError::Unsupported(
            "Wayland: covering other clients needs a compositor protocol (wlr-layer-shell, absent on GNOME) and portal \
             captures would include the curtain",
        ));
    }
    if std::env::var_os("DISPLAY").is_none_or(|v| v.is_empty()) {
        return Err(DisplayError::Unsupported("no X11 display ($DISPLAY is not set)"));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("aether-ghost-x11".into())
        .spawn(move || match Curtain::raise() {
            Ok(mut c) => {
                let _ = tx.send(Ok(c.shown()));
                c.keep_on_top(&thread_stop);
                c.lower();
            }
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        })
        .map_err(|e| DisplayError::Os(format!("spawn curtain thread: {e}")))?;
    match rx.recv() {
        Ok(Ok(shown)) => {
            *KEEPER.lock() = Some(Keeper { stop, thread });
            Ok(shown)
        }
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(DisplayError::Os("curtain thread exited unexpectedly".into()))
        }
    }
}

pub fn hide() -> Result<()> {
    if let Some(k) = KEEPER.lock().take() {
        k.stop.store(true, Ordering::Release);
        let _ = k.thread.join();
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MonitorRect {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// RandR 1.5 active monitors, or the whole root window.
pub(crate) fn monitors(conn: &RustConnection, root: Window, root_w: u16, root_h: u16) -> Vec<MonitorRect> {
    let whole = vec![MonitorRect { x: 0, y: 0, width: root_w, height: root_h }];
    if conn.extension_information(randr::X11_EXTENSION_NAME).ok().flatten().is_none() {
        return whole;
    }
    let Some(ver) = conn.randr_query_version(1, 5).ok().and_then(|c| c.reply().ok()) else { return whole };
    if (ver.major_version, ver.minor_version) < (1, 5) {
        return whole;
    }
    let Some(mons) = conn.randr_get_monitors(root, true).ok().and_then(|c| c.reply().ok()) else { return whole };
    let out: Vec<_> = mons
        .monitors
        .iter()
        .filter(|m| m.width > 0 && m.height > 0)
        .map(|m| MonitorRect { x: m.x, y: m.y, width: m.width, height: m.height })
        .collect();
    if out.is_empty() { whole } else { out }
}

struct Curtain {
    conn: RustConnection,
    root: Window,
    root_size: (u16, u16),
    black: u32,
    has_shape: bool,
    curtains_atom: u32,
    layout: Vec<MonitorRect>,
    windows: Vec<Window>,
    level: i64,
}

impl Curtain {
    fn raise() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).map_err(os("connect"))?;
        let s = &conn.setup().roots[screen];
        let (root, root_size, black) = (s.root, (s.width_in_pixels, s.height_in_pixels), s.black_pixel);
        let has_shape = conn.extension_information(shape::X11_EXTENSION_NAME).ok().flatten().is_some();
        let curtains_atom = conn.intern_atom(false, CURTAINS_PROPERTY.as_bytes()).map_err(os("InternAtom"))?.reply().map_err(os("InternAtom"))?.atom;
        let mut c = Self { conn, root, root_size, black, has_shape, curtains_atom, layout: Vec::new(), windows: Vec::new(), level: 0 };
        c.build()?;
        c.level = if c.on_top() { 1 } else { 0 };
        Ok(c)
    }

    fn shown(&self) -> Shown {
        let composite = self.conn.extension_information("Composite").ok().flatten().is_some();
        Shown {
            windows: self.windows.len(),
            level: self.level,
            shielding: shielding_level(),
            exclusion: if composite { "x11Composite" } else { "none" },
        }
    }

    fn build(&mut self) -> Result<()> {
        self.layout = monitors(&self.conn, self.root, self.root_size.0, self.root_size.1);
        let pid = std::process::id();
        let net_wm_pid = self.conn.intern_atom(false, b"_NET_WM_PID").map_err(os("InternAtom"))?.reply().map_err(os("InternAtom"))?.atom;
        for m in self.layout.clone() {
            let w = self.conn.generate_id().map_err(os("generate_id"))?;
            self.conn
                .create_window(
                    x11rb::COPY_DEPTH_FROM_PARENT,
                    w,
                    self.root,
                    m.x,
                    m.y,
                    m.width,
                    m.height,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    x11rb::COPY_FROM_PARENT,
                    &CreateWindowAux::new().background_pixel(self.black).border_pixel(self.black).override_redirect(1),
                )
                .map_err(os("CreateWindow"))?;
            self.windows.push(w);
            self.conn
                .change_property8(PropMode::REPLACE, w, AtomEnum::WM_NAME, AtomEnum::STRING, b"AetherLink Ghost Mode")
                .map_err(os("ChangeProperty"))?;
            self.conn
                .change_property8(PropMode::REPLACE, w, AtomEnum::WM_CLASS, AtomEnum::STRING, b"aetherlink-ghost\0AetherLink\0")
                .map_err(os("ChangeProperty"))?;
            self.conn.change_property32(PropMode::REPLACE, w, net_wm_pid, AtomEnum::CARDINAL, &[pid]).map_err(os("ChangeProperty"))?;
            if self.has_shape {
                // Empty input region: clicks (local and XTest-injected) reach the windows below.
                self.conn
                    .shape_rectangles(shape::SO::SET, shape::SK::INPUT, ClipOrdering::UNSORTED, w, 0, 0, &[])
                    .map_err(os("ShapeRectangles"))?;
            }
            self.conn.map_window(w).map_err(os("MapWindow"))?;
            self.conn.configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE)).map_err(os("ConfigureWindow"))?;
        }
        self.conn
            .change_property32(PropMode::REPLACE, self.root, self.curtains_atom, AtomEnum::WINDOW, &self.windows)
            .map_err(os("ChangeProperty"))?;
        self.conn.sync().map_err(os("sync"))?;
        Ok(())
    }

    fn destroy_windows(&mut self) {
        let _ = self.conn.delete_property(self.root, self.curtains_atom);
        for w in self.windows.drain(..) {
            let _ = self.conn.destroy_window(w);
        }
        let _ = self.conn.sync();
    }

    /// Whether the curtain windows are the topmost children of the root window.
    fn on_top(&self) -> bool {
        let Some(tree) = self.conn.query_tree(self.root).ok().and_then(|c| c.reply().ok()) else { return false };
        let n = self.windows.len();
        n > 0 && tree.children.len() >= n && tree.children[tree.children.len() - n..].iter().all(|w| self.windows.contains(w))
    }

    fn keep_on_top(&mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::Acquire) {
            std::thread::sleep(KEEPER_TICK);
            while let Ok(Some(_)) = self.conn.poll_for_event() {}
            let layout = monitors(&self.conn, self.root, self.root_size.0, self.root_size.1);
            if layout != self.layout {
                self.destroy_windows();
                if let Err(e) = self.build() {
                    tracing::warn!("ghost curtain rebuild after a monitor change failed: {e}");
                }
                continue;
            }
            if !self.on_top() {
                for &w in &self.windows {
                    let _ = self.conn.configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
                }
                let _ = self.conn.flush();
            }
        }
    }

    fn lower(mut self) {
        self.destroy_windows();
    }
}

#[cfg(test)]
mod tests {
    //! Needs an X server (`Xvfb :99`, see ci/linux-test.sh): ignored by default.
    use super::*;
    use crate::ghost::{self, GhostOptions};
    use x11rb::protocol::xproto::{GetImageReply, ImageFormat, MapState};

    fn pixel(conn: &RustConnection, root: Window, x: i16, y: i16) -> [u8; 4] {
        let r: GetImageReply = conn.get_image(ImageFormat::Z_PIXMAP, root, x, y, 1, 1, !0).unwrap().reply().unwrap();
        [r.data[0], r.data[1], r.data[2], r.data[3]]
    }

    fn property_windows(conn: &RustConnection, root: Window, atom: u32) -> Vec<u32> {
        let r = conn.get_property(false, root, atom, AtomEnum::WINDOW, 0, 1024).unwrap().reply().unwrap();
        r.value32().map(|v| v.collect()).unwrap_or_default()
    }

    #[test]
    #[ignore = "needs an X server ($DISPLAY, e.g. Xvfb via ci/linux-test.sh)"]
    fn curtain_covers_every_monitor_stays_on_top_and_goes_away() {
        let (conn, screen) = x11rb::connect(None).unwrap();
        let s = conn.setup().roots[screen].clone();
        let root = s.root;
        let atom = conn.intern_atom(false, CURTAINS_PROPERTY.as_bytes()).unwrap().reply().unwrap().atom;
        // A red application window that the curtain must hide.
        let red = conn.generate_id().unwrap();
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            red,
            root,
            40,
            40,
            120,
            90,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().background_pixel(0xFF_0000).override_redirect(1),
        )
        .unwrap();
        conn.map_window(red).unwrap();
        conn.sync().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(&pixel(&conn, root, 60, 60)[..3], &[0, 0, 0xFF], "red window drawn");

        let status = ghost::enable(&GhostOptions { dim_backlight: false, restore_file: None }).unwrap();
        println!("ghost status: {status:?}");
        let mons = monitors(&conn, root, s.width_in_pixels, s.height_in_pixels);
        assert_eq!(status.curtain_windows, mons.len());
        assert!(status.active);
        assert_eq!(status.window_level, status.shielding_level, "curtain on top of the stacking order");
        assert!(["x11Composite", "none"].contains(&status.capture_exclusion.as_str()));

        let curtains = property_windows(&conn, root, atom);
        assert_eq!(curtains.len(), mons.len());
        let tree = conn.query_tree(root).unwrap().reply().unwrap();
        assert!(tree.children.ends_with(&curtains), "curtains are topmost: {:?} vs {curtains:?}", tree.children);
        let mut covered = Vec::new();
        for &w in &curtains {
            let a = conn.get_window_attributes(w).unwrap().reply().unwrap();
            assert!(a.override_redirect);
            assert_eq!(a.map_state, MapState::VIEWABLE);
            let g = conn.get_geometry(w).unwrap().reply().unwrap();
            covered.push(MonitorRect { x: g.x, y: g.y, width: g.width, height: g.height });
            let input = conn.shape_get_rectangles(w, shape::SK::INPUT).unwrap().reply().unwrap();
            assert!(input.rectangles.is_empty(), "click-through input shape");
        }
        covered.sort_by_key(|m| (m.x, m.y));
        let mut expected = mons.clone();
        expected.sort_by_key(|m| (m.x, m.y));
        assert_eq!(covered, expected, "one curtain per RandR monitor");
        // Every monitor area (including the red window) reads back black on the root window.
        assert_eq!(&pixel(&conn, root, 60, 60)[..3], &[0, 0, 0]);
        for m in &mons {
            let (cx, cy) = (m.x + (m.width / 2) as i16, m.y + (m.height / 2) as i16);
            assert_eq!(&pixel(&conn, root, cx, cy)[..3], &[0, 0, 0]);
            assert_eq!(&pixel(&conn, root, m.x + m.width as i16 - 1, m.y + m.height as i16 - 1)[..3], &[0, 0, 0]);
        }

        // Something raised above the curtain is pushed back below within a keeper tick or two.
        conn.configure_window(red, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE)).unwrap();
        conn.sync().unwrap();
        let tree = conn.query_tree(root).unwrap().reply().unwrap();
        assert_eq!(tree.children.last(), Some(&red));
        std::thread::sleep(KEEPER_TICK * 3);
        let tree = conn.query_tree(root).unwrap().reply().unwrap();
        assert!(tree.children.ends_with(&curtains), "curtain re-raised");
        assert_eq!(&pixel(&conn, root, 60, 60)[..3], &[0, 0, 0]);

        // Re-enable is idempotent (fresh windows), disable removes everything.
        let again = ghost::enable(&GhostOptions::default()).unwrap();
        assert_eq!(again.curtain_windows, mons.len());
        let curtains2 = property_windows(&conn, root, atom);
        ghost::disable().unwrap();
        assert!(!ghost::status().active);
        conn.sync().unwrap();
        for w in curtains.iter().chain(&curtains2) {
            assert!(conn.get_window_attributes(*w).unwrap().reply().is_err(), "curtain {w:#x} destroyed");
        }
        assert!(property_windows(&conn, root, atom).is_empty());
        assert_eq!(&pixel(&conn, root, 60, 60)[..3], &[0, 0, 0xFF], "desktop visible again");
        conn.destroy_window(red).unwrap();
        conn.sync().unwrap();
    }
}
