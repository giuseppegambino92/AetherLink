//! Linux capture backends.
//!
//! * **xdg-desktop-portal ScreenCast + PipeWire** ([`PortalCapturer`]) — works on Wayland
//!   (GNOME, KDE, wlroots, …) and on X11 desktops that ship a portal backend.
//!   `CreateSession → SelectSources(monitor|window, cursor embedded/metadata,
//!   persist_mode = until revoked, restore token) → Start → OpenPipeWireRemote`, then a
//!   PipeWire stream ([`PipeWireCapturer`]) negotiates BGRx/BGRA (RGBx/RGBA swizzled),
//!   preferring linear DMA-BUFs and falling back to MemFd/MemPtr buffers.
//!   `SPA_META_VideoDamage` becomes [`CapturedFrame::dirty_rects`](crate::CapturedFrame).
//!   The restore token is persisted (see [`portal_restore_token_path`]) so the portal
//!   picker is shown only once.
//! * **X11** ([`X11Capturer`]) — MIT-SHM `ShmGetImage` of the root window per RandR monitor,
//!   XFixes cursor, `_NET_CLIENT_LIST` windows. No prompt, no portal needed.
//!
//! Backend choice ([`preferred_backend`]): `AETHERLINK_CAPTURE_BACKEND=portal|x11` overrides;
//! a Wayland session uses the portal; an X11 session uses X11 directly (unattended, and the
//! only way to enumerate displays/windows without the picker); with neither, the portal.
//!
//! Wayland limits (by design of the portal): displays cannot be enumerated or chosen without
//! the user's picker — [`list_displays`] returns the monitors granted to this process's
//! last portal session, else the XWayland RandR view when `$DISPLAY` is set; window
//! enumeration is impossible ([`list_windows`] returns `Unsupported` on Wayland — pick a
//! window in the portal dialog instead); `excluded_pids` cannot be honoured on either backend.

use std::sync::Mutex;

use crate::{BgraImage, CaptureError, DisplayInfo, Rect, Result, ScreenCapturer, WindowInfo};

#[path = "linux_audio.rs"]
mod audio_backends;
#[path = "linux_pipewire.rs"]
mod pipewire_stream;
#[path = "linux_portal.rs"]
mod portal;
#[path = "linux_x11.rs"]
mod x11;

pub use audio_backends::{
    AUDIO_CHANNELS, AUDIO_RATE, LinuxAudioBackend, LinuxAudioCapturer, PipeWireAudioCapturer, PulseAudioCapturer,
};
pub use pipewire_stream::PipeWireCapturer;
pub use portal::{PortalCapturer, portal_restore_token_path};
pub use x11::X11Capturer;

/// Which Linux capture backend is (or would be) used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxCaptureBackend {
    /// xdg-desktop-portal ScreenCast + PipeWire.
    Portal,
    /// X11 MIT-SHM.
    X11,
}

/// True when the session is a Wayland session.
pub fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
}

fn has_x_display() -> bool {
    std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
}

/// The backend [`create_capturer`] picks (see module docs).
pub fn preferred_backend() -> LinuxCaptureBackend {
    match std::env::var("AETHERLINK_CAPTURE_BACKEND").map(|v| v.to_ascii_lowercase()).as_deref() {
        Ok("portal" | "pipewire") => return LinuxCaptureBackend::Portal,
        Ok("x11") => return LinuxCaptureBackend::X11,
        _ => {}
    }
    if !is_wayland_session() && has_x_display() {
        LinuxCaptureBackend::X11
    } else {
        LinuxCaptureBackend::Portal
    }
}

/// Creates the capturer chosen by [`preferred_backend`].
pub fn create_capturer() -> Result<Box<dyn ScreenCapturer>> {
    Ok(match preferred_backend() {
        LinuxCaptureBackend::X11 => Box::new(X11Capturer::new()),
        LinuxCaptureBackend::Portal => Box::new(PortalCapturer::new()),
    })
}

/// Monitors granted by the most recent portal session in this process.
static PORTAL_DISPLAYS: Mutex<Vec<DisplayInfo>> = Mutex::new(Vec::new());

pub(crate) fn remember_portal_displays(d: Vec<DisplayInfo>) {
    *PORTAL_DISPLAYS.lock().unwrap_or_else(|e| e.into_inner()) = d;
}

/// Lists displays (see module docs for the Wayland behaviour).
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    if preferred_backend() == LinuxCaptureBackend::X11 {
        return x11::list_displays();
    }
    let granted = PORTAL_DISPLAYS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if !granted.is_empty() {
        return Ok(granted);
    }
    if has_x_display()
        && let Ok(d) = x11::list_displays()
    {
        return Ok(d);
    }
    Err(CaptureError::Unsupported(
        "Wayland displays are only known after the ScreenCast portal granted a capture (start a capture first)",
    ))
}

/// Lists capturable windows (X11 only; Wayland exposes windows only through the portal picker).
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    if preferred_backend() == LinuxCaptureBackend::X11 {
        return x11::list_windows();
    }
    Err(CaptureError::Unsupported(
        "Wayland does not allow enumerating windows; choose a window in the ScreenCast portal dialog",
    ))
}

// ---------------------------------------------------------------------------------------------
// Pixel helpers shared by the backends
// ---------------------------------------------------------------------------------------------

/// A cursor bitmap positioned in frame coordinates (top-left corner, hotspot applied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CursorImage {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Row-major `0xAARRGGBB`, colour premultiplied by alpha.
    pub argb_premultiplied: Vec<u32>,
}

/// Composites `c` over `img` (premultiplied "over").
pub(crate) fn blend_cursor(img: &mut BgraImage, c: &CursorImage) {
    if c.argb_premultiplied.len() < c.width as usize * c.height as usize {
        return;
    }
    for cy in 0..c.height as i32 {
        let y = c.y + cy;
        if y < 0 || y >= img.height as i32 {
            continue;
        }
        for cx in 0..c.width as i32 {
            let x = c.x + cx;
            if x < 0 || x >= img.width as i32 {
                continue;
            }
            let p = c.argb_premultiplied[(cy as u32 * c.width + cx as u32) as usize];
            let a = p >> 24;
            if a == 0 {
                continue;
            }
            let o = y as usize * img.stride + x as usize * 4;
            let px = &mut img.data[o..o + 4];
            let inv = 255 - a;
            let src = [p & 0xFF, (p >> 8) & 0xFF, (p >> 16) & 0xFF];
            for i in 0..3 {
                px[i] = (src[i] + (px[i] as u32 * inv + 127) / 255).min(255) as u8;
            }
            px[3] = 0xFF;
        }
    }
}

/// Box-filter downscale by `scale` (0 < scale < 1).
pub(crate) fn scale_image(src: &BgraImage, scale: f32) -> BgraImage {
    let dw = ((src.width as f32 * scale).round() as u32).clamp(1, src.width.max(1));
    let dh = ((src.height as f32 * scale).round() as u32).clamp(1, src.height.max(1));
    if (dw, dh) == (src.width, src.height) {
        return src.to_packed();
    }
    let ranges = |s: u32, d: u32| -> Vec<(usize, usize)> {
        (0..d)
            .map(|i| {
                let a = (i as u64 * s as u64 / d as u64) as usize;
                let b = (((i as u64 + 1) * s as u64).div_ceil(d as u64) as usize).max(a + 1);
                (a, b.min(s as usize))
            })
            .collect()
    };
    let xr = ranges(src.width, dw);
    let yr = ranges(src.height, dh);
    let mut out = BgraImage::new(dw, dh);
    let mut acc = vec![0u32; dw as usize * 4];
    for (oy, &(y0, y1)) in yr.iter().enumerate() {
        acc.iter_mut().for_each(|v| *v = 0);
        for y in y0..y1 {
            let row = src.row(y as u32);
            for (ox, &(x0, x1)) in xr.iter().enumerate() {
                let a = &mut acc[ox * 4..ox * 4 + 4];
                for px in row[x0 * 4..x1 * 4].as_chunks::<4>().0.iter() {
                    a[0] += px[0] as u32;
                    a[1] += px[1] as u32;
                    a[2] += px[2] as u32;
                    a[3] += px[3] as u32;
                }
            }
        }
        let dst = out.row_mut(oy as u32);
        for (ox, &(x0, x1)) in xr.iter().enumerate() {
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            for c in 0..4 {
                dst[ox * 4 + c] = ((acc[ox * 4 + c] + n / 2) / n) as u8;
            }
        }
    }
    out
}

/// Tile size used for change detection when the OS does not report damage.
pub(crate) const DIFF_TILE: u32 = 64;

/// Rectangles (64-px tiles merged into horizontal runs) where `cur` differs from `prev`.
/// Both images must have the same size. Empty = identical.
pub(crate) fn tile_diff(prev: &BgraImage, cur: &BgraImage) -> Vec<Rect> {
    debug_assert_eq!((prev.width, prev.height), (cur.width, cur.height));
    let (w, h) = (cur.width, cur.height);
    let mut out: Vec<Rect> = Vec::new();
    for ty in (0..h).step_by(DIFF_TILE as usize) {
        let th = DIFF_TILE.min(h - ty);
        let mut run: Option<(u32, u32)> = None; // (x0, x1)
        for tx in (0..w).step_by(DIFF_TILE as usize) {
            let tw = DIFF_TILE.min(w - tx);
            let range = tx as usize * 4..(tx + tw) as usize * 4;
            let changed = (ty..ty + th).any(|y| prev.row(y)[range.clone()] != cur.row(y)[range.clone()]);
            match (changed, run) {
                (true, Some((x0, _))) => run = Some((x0, tx + tw)),
                (true, None) => run = Some((tx, tx + tw)),
                (false, Some((x0, x1))) => {
                    out.push(Rect::new(x0 as i32, ty as i32, x1 - x0, th));
                    run = None;
                }
                (false, None) => {}
            }
        }
        if let Some((x0, x1)) = run {
            out.push(Rect::new(x0 as i32, ty as i32, x1 - x0, th));
        }
    }
    // Merge vertically adjacent runs with identical horizontal extent.
    let mut merged: Vec<Rect> = Vec::with_capacity(out.len());
    for r in out {
        if let Some(m) = merged
            .iter_mut()
            .find(|m| m.x == r.x && m.width == r.width && m.y + m.height as i32 == r.y)
        {
            m.height += r.height;
        } else {
            merged.push(r);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_diff_finds_changes() {
        let a = BgraImage::new(200, 130);
        let mut b = a.clone();
        assert!(tile_diff(&a, &b).is_empty());
        b.row_mut(70)[150 * 4] = 9; // tile (2, 1)
        b.row_mut(129)[4] = 1; // tile (0, 2), partial last row
        let d = tile_diff(&a, &b);
        assert_eq!(d, vec![Rect::new(128, 64, 64, 64), Rect::new(0, 128, 64, 2)]);
        // A full-width change merges into one rectangle.
        let mut c = a.clone();
        for y in 0..130 {
            c.row_mut(y).iter_mut().for_each(|v| *v = 1);
        }
        assert_eq!(tile_diff(&a, &c), vec![Rect::new(0, 0, 200, 130)]);
    }

    #[test]
    fn box_downscale_averages() {
        let mut img = BgraImage::new(4, 2);
        for x in 0..4u32 {
            let v = if x % 2 == 0 { 100 } else { 200 };
            for y in 0..2 {
                img.row_mut(y)[x as usize * 4..x as usize * 4 + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let half = scale_image(&img, 0.5);
        assert_eq!((half.width, half.height), (2, 1));
        assert_eq!(half.pixel(0, 0), [150, 150, 150, 255]);
        assert_eq!(half.pixel(1, 0), [150, 150, 150, 255]);
        let odd = scale_image(&BgraImage::new(333, 101), 0.37);
        assert_eq!((odd.width, odd.height), (123, 37));
    }

    #[test]
    fn cursor_blending_is_premultiplied_over() {
        let mut img = BgraImage::new(4, 4);
        for y in 0..4 {
            img.row_mut(y).as_chunks_mut::<4>().0.iter_mut().for_each(|p| p.copy_from_slice(&[200, 100, 50, 255]));
        }
        let c = CursorImage {
            x: -1,
            y: 2,
            width: 2,
            height: 3,
            // opaque white, half-transparent black (premultiplied), transparent…
            argb_premultiplied: vec![0, 0xFFFF_FFFF, 0, 0x8000_0000, 0, 0],
        };
        blend_cursor(&mut img, &c);
        assert_eq!(img.pixel(0, 2), [255, 255, 255, 255]);
        assert_eq!(img.pixel(0, 3), [100, 50, 25, 255]);
        assert_eq!(img.pixel(1, 2), [200, 100, 50, 255]);
    }

    #[test]
    fn backend_override() {
        // Only the pure decision logic; env is process-global so restore it.
        let old = std::env::var_os("AETHERLINK_CAPTURE_BACKEND");
        unsafe { std::env::set_var("AETHERLINK_CAPTURE_BACKEND", "x11") };
        assert_eq!(preferred_backend(), LinuxCaptureBackend::X11);
        unsafe { std::env::set_var("AETHERLINK_CAPTURE_BACKEND", "portal") };
        assert_eq!(preferred_backend(), LinuxCaptureBackend::Portal);
        match old {
            Some(v) => unsafe { std::env::set_var("AETHERLINK_CAPTURE_BACKEND", v) },
            None => unsafe { std::env::remove_var("AETHERLINK_CAPTURE_BACKEND") },
        }
    }
}
