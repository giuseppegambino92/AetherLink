//! X11 capture backend (used when no Wayland session / portal is available, or on request).
//!
//! * Enumeration: RandR 1.5 monitors (`GetMonitors`) → [`DisplayInfo`] (id = first RandR
//!   output XID, refresh rate from the CRTC's mode line); windows from the window manager's
//!   `_NET_CLIENT_LIST` (EWMH) or, without a window manager, the viewable top-level children
//!   of the root window.
//! * Capture: MIT-SHM `ShmGetImage` of the monitor (or window) rectangle of the root window
//!   into a shared segment — a `memfd` passed with `ShmAttachFd` (MIT-SHM 1.2) or a SysV
//!   segment for older servers; plain `GetImage` when MIT-SHM is unavailable (remote X).
//!   Window targets capture the window's on-screen rectangle (what the user sees, including
//!   overlapping windows), re-resolving its position every frame.
//! * Cursor: X servers never include the pointer in `GetImage`; with `show_cursor` the XFixes
//!   cursor image is composited into the frame.
//! * Dirty rects: 64×64 tile comparison with the previous frame; unchanged frames are not
//!   delivered (the equivalent of ScreenCaptureKit's idle frames).
//! * Ghost Mode: while AetherLink's privacy curtain is up (its window ids are listed in the
//!   `_AETHERLINK_GHOST_CURTAINS` root property, set by `aether-display`), a root-window grab
//!   would only show black. The capturer then redirects the root's children with the Composite
//!   extension (`RedirectSubwindows(Automatic)`: the server keeps painting the screen as before,
//!   but every top-level window gets a backing pixmap) and composes each frame itself, bottom to
//!   top, from the `NameWindowPixmap` contents of every viewable top-level window *except* the
//!   curtain, on top of the `_XROOTPMAP_ID` wallpaper (black without one). Window shapes and
//!   ARGB translucency are ignored (rectangular, opaque). Without Composite the curtain is
//!   captured like any other window.
//! * Other `excluded_pids` cannot be honoured: X11 has no per-window capture exclusion.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::composite::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Drawable, ImageFormat, MapState, Window, WindowClass};
use x11rb::rust_connection::RustConnection;

use super::{CursorImage, blend_cursor, scale_image, tile_diff};
use crate::{
    BgraImage, CaptureConfig, CaptureError, CaptureEvent, CaptureSink, CaptureTarget, CapturedFrame, DisplayInfo, Rect,
    Result, ScreenCapturer, WindowInfo,
};

fn os<E: std::fmt::Display>(ctx: &'static str) -> impl FnOnce(E) -> CaptureError {
    move |e| CaptureError::Os(format!("{ctx}: {e}"))
}

/// Byte order of the root visual's 32-bit pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PixelOrder {
    /// `0x00RRGGBB` little-endian → bytes B,G,R,x (the usual case).
    Bgrx,
    /// `0x00BBGGRR` little-endian → bytes R,G,B,x.
    Rgbx,
}

/// An open X connection plus the facts every operation needs.
pub(crate) struct X11 {
    pub(crate) conn: RustConnection,
    pub(crate) root: Window,
    root_w: u16,
    root_h: u16,
    order: PixelOrder,
}

impl X11 {
    /// Connects to `$DISPLAY`.
    pub(crate) fn connect() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| CaptureError::Unsupported(leak_msg(e)))?;
        let s = &conn.setup().roots[screen];
        let (root, root_w, root_h, root_visual, root_depth) =
            (s.root, s.width_in_pixels, s.height_in_pixels, s.root_visual, s.root_depth);
        let visual = s
            .allowed_depths
            .iter()
            .flat_map(|d| d.visuals.iter())
            .find(|v| v.visual_id == root_visual)
            .ok_or(CaptureError::Os("root visual not found".into()))?;
        let bpp = conn.setup().pixmap_formats.iter().find(|f| f.depth == root_depth).map(|f| f.bits_per_pixel);
        let order = match (root_depth, bpp, visual.red_mask, visual.green_mask, visual.blue_mask) {
            (24 | 32, Some(32), 0xFF_0000, 0xFF00, 0xFF) => PixelOrder::Bgrx,
            (24 | 32, Some(32), 0xFF, 0xFF00, 0xFF_0000) => PixelOrder::Rgbx,
            other => {
                tracing::warn!(?other, "unsupported X11 root visual");
                return Err(CaptureError::Unsupported("X11 root visual is not 24/32-bit TrueColor"));
            }
        };
        Ok(Self { conn, root, root_w, root_h, order })
    }

    fn atom(&self, name: &str) -> Result<u32> {
        Ok(self.conn.intern_atom(false, name.as_bytes()).map_err(os("InternAtom"))?.reply().map_err(os("InternAtom"))?.atom)
    }

    fn atom_name(&self, atom: u32) -> Option<String> {
        let r = self.conn.get_atom_name(atom).ok()?.reply().ok()?;
        Some(String::from_utf8_lossy(&r.name).into_owned())
    }

    fn has_extension(&self, name: &'static str) -> bool {
        self.conn.extension_information(name).ok().flatten().is_some()
    }

    /// RandR monitors (falls back to the whole root window when RandR ≥ 1.5 is missing).
    pub(crate) fn displays(&self) -> Result<Vec<DisplayInfo>> {
        let whole = || DisplayInfo {
            id: 0,
            name: "X11 screen".into(),
            bounds: Rect::new(0, 0, self.root_w as u32, self.root_h as u32),
            scale: 1.0,
            refresh_hz: 60.0,
            primary: true,
            is_virtual: false,
        };
        if !self.has_extension(randr::X11_EXTENSION_NAME) {
            return Ok(vec![whole()]);
        }
        let ver = self.conn.randr_query_version(1, 5).map_err(os("RRQueryVersion"))?.reply().map_err(os("RRQueryVersion"))?;
        if (ver.major_version, ver.minor_version) < (1, 5) {
            return Ok(vec![whole()]);
        }
        let mons = self.conn.randr_get_monitors(self.root, true).map_err(os("RRGetMonitors"))?.reply().map_err(os("RRGetMonitors"))?;
        let res = self
            .conn
            .randr_get_screen_resources_current(self.root)
            .ok()
            .and_then(|c| c.reply().ok());
        let mut out = Vec::new();
        for (i, m) in mons.monitors.iter().enumerate() {
            if m.width == 0 || m.height == 0 {
                continue;
            }
            let refresh = res.as_ref().and_then(|res| refresh_of_output(&self.conn, res, m.outputs.first().copied()));
            out.push(DisplayInfo {
                id: m.outputs.first().copied().unwrap_or(i as u32 + 1),
                name: self.atom_name(m.name).unwrap_or_else(|| format!("Monitor {}", i + 1)),
                bounds: Rect::new(m.x as i32, m.y as i32, m.width as u32, m.height as u32),
                scale: 1.0,
                refresh_hz: refresh.unwrap_or(60.0),
                primary: m.primary,
                is_virtual: false,
            });
        }
        if out.is_empty() {
            out.push(whole());
        }
        if !out.iter().any(|d| d.primary) {
            out[0].primary = true;
        }
        Ok(out)
    }

    fn property(&self, win: Window, prop: u32, ty: u32) -> Option<Vec<u8>> {
        let r = self.conn.get_property(false, win, prop, ty, 0, 1 << 16).ok()?.reply().ok()?;
        (r.type_ != u32::from(AtomEnum::NONE)).then_some(r.value)
    }

    /// Top-level application windows (EWMH client list, or viewable root children).
    pub(crate) fn windows(&self) -> Result<Vec<WindowInfo>> {
        let net_client_list = self.atom("_NET_CLIENT_LIST")?;
        let utf8 = self.atom("UTF8_STRING")?;
        let net_wm_name = self.atom("_NET_WM_NAME")?;
        let net_wm_pid = self.atom("_NET_WM_PID")?;
        let ids: Vec<Window> = match self.property(self.root, net_client_list, AtomEnum::WINDOW.into()) {
            Some(v) if !v.is_empty() => v.as_chunks::<4>().0.iter().map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect(),
            _ => self.conn.query_tree(self.root).map_err(os("QueryTree"))?.reply().map_err(os("QueryTree"))?.children,
        };
        let mut out = Vec::new();
        for w in ids {
            let Ok(Ok(attrs)) = self.conn.get_window_attributes(w).map(|c| c.reply()) else { continue };
            if attrs.map_state != MapState::VIEWABLE || attrs.override_redirect {
                continue;
            }
            let title = self
                .property(w, net_wm_name, utf8)
                .or_else(|| self.property(w, AtomEnum::WM_NAME.into(), AtomEnum::STRING.into()))
                .map(|v| String::from_utf8_lossy(&v).into_owned())
                .unwrap_or_default();
            // WM_CLASS = "instance\0class\0": the class is the application name.
            let app_name = self
                .property(w, AtomEnum::WM_CLASS.into(), AtomEnum::STRING.into())
                .map(|v| {
                    let parts: Vec<String> =
                        v.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
                    parts.last().cloned().unwrap_or_default()
                })
                .unwrap_or_default();
            if title.is_empty() && app_name.is_empty() {
                continue;
            }
            let pid = self
                .property(w, net_wm_pid, AtomEnum::CARDINAL.into())
                .filter(|v| v.len() >= 4)
                .map(|v| u32::from_ne_bytes([v[0], v[1], v[2], v[3]]))
                .unwrap_or(0);
            let Ok(bounds) = self.window_rect(w) else { continue };
            if bounds.width == 0 || bounds.height == 0 {
                continue;
            }
            out.push(WindowInfo { id: w as u64, app_name, title, bounds, pid });
        }
        Ok(out)
    }

    /// Window rectangle in root coordinates.
    pub(crate) fn window_rect(&self, w: Window) -> Result<Rect> {
        let g = self.conn.get_geometry(w).map_err(os("GetGeometry"))?.reply().map_err(|e| CaptureError::TargetNotFound(format!("window {w:#x}: {e}")))?;
        let t = self
            .conn
            .translate_coordinates(w, self.root, 0, 0)
            .map_err(os("TranslateCoordinates"))?
            .reply()
            .map_err(|e| CaptureError::TargetNotFound(format!("window {w:#x}: {e}")))?;
        Ok(Rect::new(t.dst_x as i32, t.dst_y as i32, g.width as u32, g.height as u32))
    }

    fn clamp_to_root(&self, r: Rect) -> Option<Rect> {
        let x0 = r.x.max(0);
        let y0 = r.y.max(0);
        let x1 = (r.x as i64 + r.width as i64).min(self.root_w as i64) as i32;
        let y1 = (r.y as i64 + r.height as i64).min(self.root_h as i64) as i32;
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
    }

    /// The desktop wallpaper pixmap (`_XROOTPMAP_ID`), if a desktop set one.
    fn root_pixmap(&self) -> Option<u32> {
        let atom = self.atom("_XROOTPMAP_ID").ok()?;
        let v = self.property(self.root, atom, AtomEnum::PIXMAP.into())?;
        let c = v.as_chunks::<4>().0.first()?;
        Some(u32::from_ne_bytes(*c)).filter(|p| *p != 0)
    }

    /// Current XFixes cursor (premultiplied ARGB) in root coordinates.
    fn cursor(&self) -> Option<CursorImage> {
        let c = self.conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
        Some(CursorImage {
            x: c.x as i32 - c.xhot as i32,
            y: c.y as i32 - c.yhot as i32,
            width: c.width as u32,
            height: c.height as u32,
            argb_premultiplied: c.cursor_image,
        })
    }
}

fn leak_msg(e: impl std::fmt::Display) -> &'static str {
    // Unsupported carries a &'static str; connection errors are rare and one-off, so leaking
    // the formatted message is acceptable.
    Box::leak(format!("cannot connect to the X server: {e}").into_boxed_str())
}

fn refresh_of_output(conn: &RustConnection, res: &randr::GetScreenResourcesCurrentReply, output: Option<u32>) -> Option<f32> {
    let out = conn.randr_get_output_info(output?, res.config_timestamp).ok()?.reply().ok()?;
    if out.crtc == 0 {
        return None;
    }
    let crtc = conn.randr_get_crtc_info(out.crtc, res.config_timestamp).ok()?.reply().ok()?;
    let mode = res.modes.iter().find(|m| m.id == crtc.mode)?;
    let total = mode.htotal as f64 * mode.vtotal as f64;
    (total > 0.0 && mode.dot_clock > 0).then(|| (mode.dot_clock as f64 / total) as f32)
}

/// Lists RandR monitors of `$DISPLAY`.
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    X11::connect()?.displays()
}

/// Lists top-level windows of `$DISPLAY`.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    X11::connect()?.windows()
}

// ---------------------------------------------------------------------------------------------
// Shared memory segment
// ---------------------------------------------------------------------------------------------

/// A mapping attached to the X server as an MIT-SHM segment.
struct ShmSegment {
    seg: shm::Seg,
    ptr: *mut u8,
    len: usize,
    sysv: bool,
}

impl ShmSegment {
    /// Attaches `len` bytes: memfd + `ShmAttachFd` (MIT-SHM 1.2), else SysV `shmget` + `ShmAttach`.
    fn new(x: &X11, len: usize) -> Option<Self> {
        if !x.has_extension(shm::X11_EXTENSION_NAME) {
            return None;
        }
        let ver = x.conn.shm_query_version().ok()?.reply().ok()?;
        if (ver.major_version, ver.minor_version) >= (1, 2)
            && let Some(s) = Self::memfd(x, len)
        {
            return Some(s);
        }
        Self::sysv(x, len)
    }

    fn memfd(x: &X11, len: usize) -> Option<Self> {
        // SAFETY: plain syscalls with checked results; the mapping is owned by the returned value.
        unsafe {
            let fd = libc::memfd_create(c"aetherlink-x11-shm".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                return None;
            }
            let fd = OwnedFd::from_raw_fd(fd);
            if libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) != 0 {
                return None;
            }
            let ptr = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0);
            if ptr == libc::MAP_FAILED {
                return None;
            }
            let seg = x.conn.generate_id().ok()?;
            let ok = x.conn.shm_attach_fd(seg, fd, false).ok().map(|c| c.check().is_ok()).unwrap_or(false);
            if !ok {
                libc::munmap(ptr, len);
                return None;
            }
            Some(Self { seg, ptr: ptr.cast(), len, sysv: false })
        }
    }

    fn sysv(x: &X11, len: usize) -> Option<Self> {
        // SAFETY: as above; the SysV id is marked for removal once both sides attached.
        unsafe {
            let id = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
            if id < 0 {
                return None;
            }
            let ptr = libc::shmat(id, std::ptr::null(), 0);
            if ptr as isize == -1 {
                libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
                return None;
            }
            let seg = x.conn.generate_id().ok();
            let ok = seg.and_then(|s| x.conn.shm_attach(s, id as u32, false).ok()).map(|c| c.check().is_ok()).unwrap_or(false);
            libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
            if !ok {
                libc::shmdt(ptr);
                return None;
            }
            Some(Self { seg: seg?, ptr: ptr.cast(), len, sysv: true })
        }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is `len` bytes and lives as long as `self`; the X server only
        // writes into it while we wait for the ShmGetImage reply.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn release(self, x: &X11) {
        let _ = x.conn.shm_detach(self.seg).map(|c| c.check());
        // Drop unmaps.
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        // SAFETY: ptr/len come from mmap/shmat in the constructors.
        unsafe {
            if self.sysv {
                libc::shmdt(self.ptr.cast());
            } else {
                libc::munmap(self.ptr.cast(), self.len);
            }
        }
    }
}

// SAFETY: the raw mapping pointer is only used from the capture thread that owns the segment.
unsafe impl Send for ShmSegment {}

/// Grabs `r` (root coordinates, already clamped) into a packed BGRA image.
struct Grabber {
    shm: Option<ShmSegment>,
    shm_failed: bool,
}

impl Grabber {
    fn new() -> Self {
        Self { shm: None, shm_failed: false }
    }

    fn grab(&mut self, x: &X11, r: Rect) -> Result<BgraImage> {
        self.read(x, x.root, r)
    }

    /// Reads `r` (in `drawable` coordinates) of a 24/32-bit window or pixmap.
    fn read(&mut self, x: &X11, drawable: Drawable, r: Rect) -> Result<BgraImage> {
        let len = r.width as usize * r.height as usize * 4;
        if !self.shm_failed && self.shm.as_ref().is_none_or(|s| s.len < len) {
            if let Some(old) = self.shm.take() {
                old.release(x);
            }
            self.shm = ShmSegment::new(x, len.max(4096));
            if self.shm.is_none() {
                tracing::info!("MIT-SHM unavailable, falling back to plain GetImage");
                self.shm_failed = true;
            }
        }
        let mut img = BgraImage::new(r.width, r.height);
        if let Some(seg) = &self.shm {
            let reply = x
                .conn
                .shm_get_image(drawable, r.x as i16, r.y as i16, r.width as u16, r.height as u16, !0, ImageFormat::Z_PIXMAP.into(), seg.seg, 0)
                .map_err(os("ShmGetImage"))?
                .reply()
                .map_err(os("ShmGetImage"))?;
            if (reply.size as usize) < len {
                return Err(CaptureError::InvalidFrame(format!("ShmGetImage returned {} bytes, expected {len}", reply.size)));
            }
            copy_pixels(&seg.bytes()[..len], &mut img.data, x.order);
        } else {
            let reply = x
                .conn
                .get_image(ImageFormat::Z_PIXMAP, drawable, r.x as i16, r.y as i16, r.width as u16, r.height as u16, !0)
                .map_err(os("GetImage"))?
                .reply()
                .map_err(os("GetImage"))?;
            if reply.data.len() < len {
                return Err(CaptureError::InvalidFrame(format!("GetImage returned {} bytes, expected {len}", reply.data.len())));
            }
            copy_pixels(&reply.data[..len], &mut img.data, x.order);
        }
        Ok(img)
    }

    fn release(&mut self, x: &X11) {
        if let Some(s) = self.shm.take() {
            s.release(x);
        }
    }

    /// Composes `r` (root coordinates) from the Composite pixmaps of the top-level windows,
    /// leaving out `excluded` (the Ghost Mode curtain). See the module docs.
    fn grab_excluding(&mut self, x: &X11, r: Rect, excluded: &[Window]) -> Result<BgraImage> {
        let mut img = BgraImage::new(r.width, r.height);
        for px in img.data.as_chunks_mut::<4>().0 {
            *px = [0, 0, 0, 0xFF];
        }
        // Wallpaper (root pixmap set by the desktop), same coordinates as the root window.
        if let Some(pm) = x.root_pixmap()
            && let Ok(bg) = self.read(x, pm, r)
        {
            blit(&mut img, &bg, 0, 0);
        }
        let children = x.conn.query_tree(x.root).map_err(os("QueryTree"))?.reply().map_err(os("QueryTree"))?.children;
        // Pipeline the attribute + geometry queries for all children.
        let cookies: Vec<_> = children
            .iter()
            .filter(|w| !excluded.contains(w))
            .map(|&w| (w, x.conn.get_window_attributes(w).ok(), x.conn.get_geometry(w).ok()))
            .collect();
        for (w, attrs, geom) in cookies {
            let (Some(Ok(attrs)), Some(Ok(g))) = (attrs.map(|c| c.reply()), geom.map(|c| c.reply())) else { continue };
            if attrs.map_state != MapState::VIEWABLE || attrs.class != WindowClass::INPUT_OUTPUT || !matches!(g.depth, 24 | 32) {
                continue;
            }
            let outer = Rect::new(g.x as i32, g.y as i32, g.width as u32 + 2 * g.border_width as u32, g.height as u32 + 2 * g.border_width as u32);
            let Some(vis) = intersect(outer, r) else { continue };
            let Ok(pm) = x.conn.generate_id() else { continue };
            let named = x.conn.composite_name_window_pixmap(w, pm).ok().map(|c| c.check().is_ok()).unwrap_or(false);
            if !named {
                continue; // unmapped meanwhile, or not redirected
            }
            let local = Rect::new(vis.x - outer.x, vis.y - outer.y, vis.width, vis.height);
            let part = self.read(x, pm, local);
            let _ = x.conn.free_pixmap(pm);
            if let Ok(part) = part {
                blit(&mut img, &part, (vis.x - r.x) as u32, (vis.y - r.y) as u32);
            }
        }
        Ok(img)
    }
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x as i64 + a.width as i64).min(b.x as i64 + b.width as i64) as i32;
    let y1 = (a.y as i64 + a.height as i64).min(b.y as i64 + b.height as i64) as i32;
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
}

/// Copies `src` into `dst` at (`dx`, `dy`), clipped.
fn blit(dst: &mut BgraImage, src: &BgraImage, dx: u32, dy: u32) {
    if dx >= dst.width || dy >= dst.height {
        return;
    }
    let w = src.width.min(dst.width - dx) as usize;
    for row in 0..src.height.min(dst.height - dy) as usize {
        let s = row * src.width as usize * 4;
        let d = ((dy as usize + row) * dst.width as usize + dx as usize) * 4;
        dst.data[d..d + w * 4].copy_from_slice(&src.data[s..s + w * 4]);
    }
}

/// Root property listing the Ghost Mode curtain windows (written by aether-display).
const GHOST_CURTAINS_PROPERTY: &str = "_AETHERLINK_GHOST_CURTAINS";

/// Tracks the Ghost Mode curtain and the Composite redirection needed to see below it.
struct GhostExclusion {
    atom: Option<u32>,
    composite: bool,
    redirected: bool,
}

impl GhostExclusion {
    fn new(x: &X11) -> Self {
        let atom = x.conn.intern_atom(false, GHOST_CURTAINS_PROPERTY.as_bytes()).ok().and_then(|c| c.reply().ok()).map(|r| r.atom);
        let composite = x.has_extension(composite::X11_EXTENSION_NAME)
            && x.conn.composite_query_version(0, 4).ok().and_then(|c| c.reply().ok()).is_some_and(|v| (v.major_version, v.minor_version) >= (0, 2));
        Self { atom, composite, redirected: false }
    }

    /// Viewable curtain windows to exclude from this frame (empty when Ghost Mode is off or
    /// cannot be excluded).
    fn curtains(&mut self, x: &X11) -> Vec<Window> {
        let (Some(atom), true) = (self.atom, self.composite) else { return Vec::new() };
        let Some(v) = x.property(x.root, atom, AtomEnum::WINDOW.into()) else { return Vec::new() };
        let ids: Vec<Window> = v.as_chunks::<4>().0.iter().map(|c| u32::from_ne_bytes(*c)).collect();
        // A crashed owner leaves the property behind: only live, mapped windows count.
        let live: Vec<Window> = ids
            .into_iter()
            .filter(|&w| {
                x.conn.get_window_attributes(w).ok().and_then(|c| c.reply().ok()).is_some_and(|a| a.map_state == MapState::VIEWABLE)
            })
            .collect();
        if !live.is_empty() && !self.redirected {
            // Automatic redirection keeps the screen unchanged and gives every top-level a pixmap.
            // A compositing manager's manual redirection already provides them, so errors are fine.
            let _ = x.conn.composite_redirect_subwindows(x.root, composite::Redirect::AUTOMATIC).map(|c| c.check());
            self.redirected = true;
            tracing::info!("Ghost Mode curtain detected: composing X11 frames without it");
        }
        live
    }
}

fn copy_pixels(src: &[u8], dst: &mut [u8], order: PixelOrder) {
    match order {
        PixelOrder::Bgrx => {
            for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0.iter()) {
                d[0] = s[0];
                d[1] = s[1];
                d[2] = s[2];
                d[3] = 0xFF;
            }
        }
        PixelOrder::Rgbx => {
            for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0.iter()) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 0xFF;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Capturer
// ---------------------------------------------------------------------------------------------

/// X11 MIT-SHM screen / window capturer.
#[derive(Default)]
pub struct X11Capturer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl X11Capturer {
    pub fn new() -> Self {
        Self::default()
    }
}

enum Region {
    Fixed { display_id: u32, rect: Rect },
    Window(Window),
}

impl ScreenCapturer for X11Capturer {
    fn start(&mut self, target: CaptureTarget, cfg: CaptureConfig, mut sink: CaptureSink) -> Result<()> {
        if self.thread.is_some() {
            return Err(CaptureError::AlreadyRunning);
        }
        let x = X11::connect()?;
        let region = match target {
            CaptureTarget::Display { display_id } => {
                let d = x
                    .displays()?
                    .into_iter()
                    .find(|d| d.id == display_id)
                    .ok_or_else(|| CaptureError::TargetNotFound(format!("display {display_id}")))?;
                Region::Fixed { display_id, rect: d.bounds }
            }
            CaptureTarget::Window { window_id } => {
                let w = u32::try_from(window_id).map_err(|_| CaptureError::TargetNotFound(format!("window {window_id}")))?;
                x.window_rect(w)?;
                Region::Window(w)
            }
        };
        let show_cursor = cfg.show_cursor && x.has_extension(xfixes::X11_EXTENSION_NAME) && {
            x.conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some()
        };
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = stop.clone();
        let interval = Duration::from_secs_f64(1.0 / cfg.fps.max(1) as f64);
        let scale = if cfg.scale.is_finite() && cfg.scale > 0.0 { cfg.scale.min(1.0) } else { 1.0 };
        let thread = std::thread::Builder::new()
            .name("aether-x11-capture".into())
            .spawn(move || {
                let mut grabber = Grabber::new();
                let mut ghost = GhostExclusion::new(&x);
                let start = Instant::now();
                let mut prev: Option<BgraImage> = None;
                let mut next = Instant::now();
                while !stop.load(Ordering::Acquire) {
                    let now = Instant::now();
                    if now < next {
                        std::thread::sleep((next - now).min(Duration::from_millis(50)));
                        continue;
                    }
                    next += interval;
                    if next < now {
                        next = now + interval; // fell behind: don't burst
                    }
                    let (display_id, rect) = match &region {
                        Region::Fixed { display_id, rect } => (*display_id, *rect),
                        Region::Window(w) => match x.window_rect(*w) {
                            Ok(r) => (0, r),
                            Err(e) => {
                                sink(CaptureEvent::Error(e));
                                break;
                            }
                        },
                    };
                    let Some(rect) = x.clamp_to_root(rect) else {
                        continue; // window entirely off-screen
                    };
                    let curtains = ghost.curtains(&x);
                    let grabbed = if curtains.is_empty() { grabber.grab(&x, rect) } else { grabber.grab_excluding(&x, rect, &curtains) };
                    let mut img = match grabbed {
                        Ok(i) => i,
                        Err(e) => {
                            sink(CaptureEvent::Error(e));
                            break;
                        }
                    };
                    if show_cursor && let Some(mut c) = x.cursor() {
                        c.x -= rect.x;
                        c.y -= rect.y;
                        blend_cursor(&mut img, &c);
                    }
                    if scale < 0.999 {
                        img = scale_image(&img, scale);
                    }
                    let dirty = match &prev {
                        Some(p) if p.width == img.width && p.height == img.height => {
                            let d = tile_diff(p, &img);
                            if d.is_empty() {
                                continue; // idle frame
                            }
                            Some(d)
                        }
                        _ => None,
                    };
                    let pts = start.elapsed().as_micros() as u64;
                    let frame = CapturedFrame::from_image(display_id, pts, img.clone(), dirty);
                    prev = Some(img);
                    sink(CaptureEvent::Frame(frame));
                }
                grabber.release(&x);
            })
            .map_err(|e| CaptureError::Os(format!("spawn capture thread: {e}")))?;
        self.thread = Some(thread);
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for X11Capturer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    //! These tests need an X server (`Xvfb :99`, see ci/linux-test.sh); they are ignored
    //! by default and run by the harness with `--ignored`.
    use super::*;
    use std::sync::mpsc;
    use x11rb::protocol::xproto::{CreateGCAux, CreateWindowAux, EventMask, PropMode, Rectangle, WindowClass};
    use x11rb::wrapper::ConnectionExt as _;

    /// Paints `color` (0xRRGGBB) into a rectangle of the root window.
    fn fill_root(x: &X11, rect: Rectangle, color: u32) {
        let gc = x.conn.generate_id().unwrap();
        x.conn.create_gc(gc, x.root, &CreateGCAux::new().foreground(color).subwindow_mode(x11rb::protocol::xproto::SubwindowMode::INCLUDE_INFERIORS)).unwrap();
        x.conn.poly_fill_rectangle(x.root, gc, &[rect]).unwrap();
        x.conn.free_gc(gc).unwrap();
        x.conn.sync().unwrap();
    }

    #[test]
    #[ignore = "needs an X server ($DISPLAY, e.g. Xvfb via ci/linux-test.sh)"]
    fn enumerates_randr_monitors() {
        let d = list_displays().unwrap();
        println!("displays: {d:?}");
        assert!(!d.is_empty());
        assert!(d.iter().any(|d| d.primary));
        assert!(d.iter().all(|d| d.bounds.width > 0 && d.bounds.height > 0 && d.refresh_hz > 0.0));
    }

    #[test]
    #[ignore = "needs an X server ($DISPLAY, e.g. Xvfb via ci/linux-test.sh)"]
    fn shm_capture_sees_drawn_pixels() {
        let x = X11::connect().unwrap();
        let display = x.displays().unwrap().into_iter().find(|d| d.primary).unwrap();
        // Probe the bottom-right corner: the harness keeps a GTK window open at the top-left for
        // the AT-SPI tests, and pixels under it would (correctly) show that window instead.
        let ox = display.bounds.width as i16 - 300;
        let oy = display.bounds.height as i16 - 150;
        let (ux, uy) = (ox as u32, oy as u32);
        // Keep the (composited) pointer away from the probed pixels.
        x.conn.warp_pointer(x11rb::NONE, x.root, 0, 0, 0, 0, 5, 5).unwrap();
        // Draw a known pattern on the root window: a red and a blue block.
        fill_root(&x, Rectangle { x: ox + 10, y: oy + 20, width: 40, height: 30 }, 0xFF_0000);
        fill_root(&x, Rectangle { x: ox + 100, y: oy + 50, width: 16, height: 16 }, 0x00_00FF);

        let mut grabber = Grabber::new();
        let img = grabber.grab(&x, display.bounds).unwrap();
        assert!(grabber.shm.is_some(), "MIT-SHM path should be used against a local server");
        assert_eq!((img.width, img.height), (display.bounds.width, display.bounds.height));
        assert_eq!(img.pixel(ux + 10, uy + 20), [0, 0, 0xFF, 0xFF]); // BGRA red
        assert_eq!(img.pixel(ux + 49, uy + 49), [0, 0, 0xFF, 0xFF]);
        assert_eq!(img.pixel(ux + 107, uy + 57), [0xFF, 0, 0, 0xFF]); // blue
        grabber.release(&x);

        // Full capturer: frames arrive, then a change produces a dirty rect covering it.
        let (tx, rx) = mpsc::channel();
        let mut cap = X11Capturer::new();
        cap.start(
            CaptureTarget::Display { display_id: display.id },
            CaptureConfig { fps: 30, show_cursor: true, ..Default::default() },
            Box::new(move |ev| {
                let _ = tx.send(ev);
            }),
        )
        .unwrap();
        let first = match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            CaptureEvent::Frame(f) => f,
            other => panic!("unexpected {other:?}"),
        };
        assert!(first.dirty_rects.is_none());
        assert_eq!(first.to_bgra().unwrap().pixel(ux + 20, uy + 30), [0, 0, 0xFF, 0xFF]);
        fill_root(&x, Rectangle { x: ox + 200, y: oy + 120, width: 8, height: 8 }, 0x00_FF00);
        let changed = loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                CaptureEvent::Frame(f) if f.to_bgra().unwrap().pixel(ux + 203, uy + 123) == [0, 0xFF, 0, 0xFF] => break f,
                CaptureEvent::Frame(_) => continue,
                other => panic!("unexpected {other:?}"),
            }
        };
        let dirty = changed.dirty_rects.expect("dirty rects after the first frame");
        assert!(dirty.iter().any(|r| r.contains(ox as i32 + 203, oy as i32 + 123)), "{dirty:?}");
        cap.stop();
        assert!(rx.try_iter().all(|e| matches!(e, CaptureEvent::Frame(_))));
    }

    #[test]
    #[ignore = "needs an X server ($DISPLAY, e.g. Xvfb via ci/linux-test.sh)"]
    fn lists_and_captures_a_window() {
        let x = X11::connect().unwrap();
        let win = x.conn.generate_id().unwrap();
        x.conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                win,
                x.root,
                300,
                200,
                64,
                48,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().background_pixel(0x12_3456).event_mask(EventMask::EXPOSURE),
            )
            .unwrap();
        x.conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, b"aether capture test").unwrap();
        x.conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"aethertest\0AetherTest\0").unwrap();
        let pid_atom = x.atom("_NET_WM_PID").unwrap();
        x.conn.change_property32(PropMode::REPLACE, win, pid_atom, AtomEnum::CARDINAL, &[std::process::id()]).unwrap();
        x.conn.map_window(win).unwrap();
        x.conn.sync().unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let wins = list_windows().unwrap();
        let w = wins.iter().find(|w| w.id == win as u64).unwrap_or_else(|| panic!("window not listed: {wins:?}"));
        assert_eq!(w.title, "aether capture test");
        assert_eq!(w.app_name, "AetherTest");
        assert_eq!(w.pid, std::process::id());
        assert_eq!((w.bounds.width, w.bounds.height), (64, 48));

        let (tx, rx) = mpsc::channel();
        let mut cap = X11Capturer::new();
        cap.start(
            CaptureTarget::Window { window_id: win as u64 },
            CaptureConfig { fps: 30, show_cursor: false, ..Default::default() },
            Box::new(move |ev| {
                let _ = tx.send(ev);
            }),
        )
        .unwrap();
        let f = match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            CaptureEvent::Frame(f) => f,
            other => panic!("unexpected {other:?}"),
        };
        cap.stop();
        assert_eq!((f.width, f.height), (64, 48));
        assert_eq!(f.to_bgra().unwrap().pixel(32, 24), [0x56, 0x34, 0x12, 0xFF]);
        x.conn.destroy_window(win).unwrap();
        x.conn.sync().unwrap();
    }

    #[test]
    #[ignore = "needs an X server ($DISPLAY, e.g. Xvfb via ci/linux-test.sh)"]
    fn ghost_curtain_is_excluded_from_capture() {
        let x = X11::connect().unwrap();
        if !x.has_extension(composite::X11_EXTENSION_NAME) {
            panic!("the X server lacks the Composite extension");
        }
        let display = x.displays().unwrap().into_iter().find(|d| d.primary).unwrap();
        x.conn.warp_pointer(x11rb::NONE, x.root, 0, 0, 0, 0, 5, 5).unwrap();
        let mk = |x0: i16, y0: i16, w: u16, h: u16, color: u32, override_redirect: bool| {
            let id = x.conn.generate_id().unwrap();
            x.conn
                .create_window(
                    x11rb::COPY_DEPTH_FROM_PARENT,
                    id,
                    x.root,
                    x0,
                    y0,
                    w,
                    h,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    x11rb::COPY_FROM_PARENT,
                    &CreateWindowAux::new().background_pixel(color).override_redirect(override_redirect as u32),
                )
                .unwrap();
            x.conn.map_window(id).unwrap();
            id
        };
        // An application window (green) below a full-screen black "curtain".
        let app = mk(display.bounds.x as i16 + 300, display.bounds.y as i16 + 150, 80, 60, 0x00_FF00, true);
        let curtain = mk(0, 0, x.root_w, x.root_h, 0x00_0000, true);
        let atom = x.atom(GHOST_CURTAINS_PROPERTY).unwrap();
        x.conn.change_property32(PropMode::REPLACE, x.root, atom, AtomEnum::WINDOW, &[curtain]).unwrap();
        x.conn.sync().unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let (px, py) = (display.bounds.x + 340, display.bounds.y + 180);
        // What a plain root grab sees: the curtain.
        let mut g = Grabber::new();
        let r = Rect::new(px, py, 1, 1);
        assert_eq!(g.grab(&x, r).unwrap().pixel(0, 0), [0, 0, 0, 0xFF], "root grab shows the curtain");
        g.release(&x);

        let (tx, rx) = mpsc::channel();
        let mut cap = X11Capturer::new();
        cap.start(
            CaptureTarget::Display { display_id: display.id },
            CaptureConfig { fps: 30, show_cursor: false, ..Default::default() },
            Box::new(move |ev| {
                let _ = tx.send(ev);
            }),
        )
        .unwrap();
        let (lx, ly) = ((px - display.bounds.x) as u32, (py - display.bounds.y) as u32);
        let deadline = Instant::now() + Duration::from_secs(5);
        let seen_app = loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                CaptureEvent::Frame(f) if f.to_bgra().unwrap().pixel(lx, ly) == [0, 0xFF, 0, 0xFF] => break true,
                CaptureEvent::Frame(_) if Instant::now() < deadline => continue,
                CaptureEvent::Frame(_) => break false,
                other => panic!("unexpected {other:?}"),
            }
        };
        assert!(seen_app, "capture shows the window below the curtain");
        // Ghost Mode off (property removed, curtain still mapped): the curtain is just a window again.
        x.conn.delete_property(x.root, atom).unwrap();
        x.conn.sync().unwrap();
        let back_to_black = loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(CaptureEvent::Frame(f)) if f.to_bgra().unwrap().pixel(lx, ly) == [0, 0, 0, 0xFF] => break true,
                Ok(CaptureEvent::Frame(_)) => continue,
                _ => break false,
            }
        };
        cap.stop();
        assert!(back_to_black, "without the property the curtain is captured like any window");
        x.conn.destroy_window(curtain).unwrap();
        x.conn.destroy_window(app).unwrap();
        x.conn.sync().unwrap();
    }

    #[test]
    fn blit_and_intersect_clip() {
        let mut dst = BgraImage::new(4, 3);
        let mut src = BgraImage::new(3, 2);
        src.data.fill(7);
        blit(&mut dst, &src, 2, 2);
        assert_eq!(dst.pixel(2, 2), [7; 4]);
        assert_eq!(dst.pixel(3, 2), [7; 4]);
        assert_eq!(dst.pixel(1, 2), [0; 4]);
        assert_eq!(dst.pixel(2, 1), [0; 4]);
        blit(&mut dst, &src, 9, 0); // fully outside: no panic
        assert_eq!(intersect(Rect::new(0, 0, 10, 10), Rect::new(5, 5, 10, 10)), Some(Rect::new(5, 5, 5, 5)));
        assert_eq!(intersect(Rect::new(0, 0, 10, 10), Rect::new(10, 0, 5, 5)), None);
    }
}
