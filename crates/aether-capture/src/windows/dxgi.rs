//! DXGI Desktop Duplication (`IDXGIOutputDuplication`) display capture.
//!
//! Per frame: `AcquireNextFrame` → pointer position/shape updates → dirty + move rects →
//! `CopyResource` into a staging texture → `Map` → only the changed regions are copied into
//! a persistent "clean" CPU image → `ReleaseFrame`. The cursor (never part of the duplicated
//! image) is composited onto a copy of that image when `show_cursor` is set; its previous and
//! new positions are added to the dirty rects. Pointer-only updates re-emit the cached image
//! with the cursor moved; frames with neither image nor pointer change are skipped (idle).
//!
//! `DXGI_ERROR_ACCESS_LOST` (desktop switch, UAC secure desktop, display-mode change,
//! full-screen exclusive app) and other duplication failures drop the duplication; it is
//! re-created after re-attaching the thread to the current input desktop, with exponential
//! backoff (50 ms → 1 s). A target that stays unavailable for 3 s is reported once through
//! `CaptureEvent::Error` while retrying continues.
//!
//! Rotated outputs (portrait): DXGI hands out the unrotated image; it is rotated on the CPU
//! and the frame's dirty rects are reported as unknown (`None`).

use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{E_ACCESSDENIED, E_INVALIDARG};
use ::windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use ::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180,
    DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use ::windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use ::windows::Win32::Foundation::RECT;
use ::windows::core::{HRESULT, Interface};

use super::d3d::{Readback, create_device};
use super::{ThreadCtx, attach_to_input_desktop, enum_outputs, os_err, qpc_100ns, qpc_ticks_to_100ns, rel_us};
use crate::windows_pure::{
    CursorKind, CursorShape, composite_cursor, downscale, normalize_dirty, rect_from_ltrb, rotate_cw, scale_rects,
    scaled_size,
};
use crate::{BgraImage, CaptureError, CaptureEvent, CapturedFrame, Rect};

/// A live duplication of one output.
pub(crate) struct Duplication {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    /// Clockwise quarter turns to apply to the duplicated image.
    rotation: u8,
}

/// Why opening a duplication failed.
pub(crate) enum OpenError {
    /// Desktop Duplication cannot work for this output: use Windows.Graphics.Capture.
    Fallback(String),
    /// Temporarily unavailable (secure desktop, mode change in progress): retry later.
    Transient(CaptureError),
    /// The target does not exist or the system failed otherwise.
    Fatal(CaptureError),
}

fn classify(hr: HRESULT, err: CaptureError) -> OpenError {
    if hr == DXGI_ERROR_UNSUPPORTED || hr == E_INVALIDARG {
        OpenError::Fallback(err.to_string())
    } else if hr == E_ACCESSDENIED
        || hr == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE
        || hr == DXGI_ERROR_SESSION_DISCONNECTED
        || hr == DXGI_ERROR_ACCESS_LOST
    {
        OpenError::Transient(err)
    } else {
        OpenError::Fatal(err)
    }
}

fn rotation_turns(r: ::windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION) -> u8 {
    match r {
        DXGI_MODE_ROTATION_ROTATE90 => 1,
        DXGI_MODE_ROTATION_ROTATE180 => 2,
        DXGI_MODE_ROTATION_ROTATE270 => 3,
        DXGI_MODE_ROTATION_IDENTITY | DXGI_MODE_ROTATION_UNSPECIFIED => 0,
        _ => 0,
    }
}

/// Opens a duplication of display `display_id` on its owning adapter.
pub(crate) fn open(display_id: u32) -> std::result::Result<Duplication, OpenError> {
    let entry = enum_outputs()
        .map_err(OpenError::Fatal)?
        .into_iter()
        .find(|o| o.id == display_id)
        .ok_or_else(|| OpenError::Fatal(CaptureError::TargetNotFound(format!("display {display_id}"))))?;
    let (device, ctx) = create_device(Some(&entry.adapter)).map_err(OpenError::Fatal)?;
    // SAFETY: COM calls on valid interfaces; `device` belongs to the output's adapter.
    let dup = unsafe {
        match entry.output.cast::<IDXGIOutput5>() {
            // DuplicateOutput1 lets us request BGRA even on HDR / FP16 desktops.
            Ok(o5) => o5.DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]),
            Err(_) => match entry.output.cast::<IDXGIOutput1>() {
                Ok(o1) => o1.DuplicateOutput(&device),
                Err(e) => Err(e),
            },
        }
    };
    let dup = dup.map_err(|e| {
        let hr = e.code();
        classify(hr, os_err("DuplicateOutput", e))
    })?;
    // SAFETY: plain getter.
    let desc: DXGI_OUTDUPL_DESC = unsafe { dup.GetDesc() };
    Ok(Duplication { device, ctx, dup, rotation: rotation_turns(desc.Rotation) })
}

#[derive(Default)]
struct CursorState {
    x: i32,
    y: i32,
    visible: bool,
    shape: Option<CursorShape>,
    /// Region the cursor occupied in the last emitted frame.
    last_drawn: Option<Rect>,
}

/// Releases the acquired frame on every exit path.
struct FrameGuard<'a>(&'a IDXGIOutputDuplication);

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: only constructed after a successful AcquireNextFrame.
        let _ = unsafe { self.0.ReleaseFrame() };
    }
}

fn frame_rects(dup: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) -> Option<Vec<Rect>> {
    if info.TotalMetadataBufferSize == 0 {
        return Some(Vec::new());
    }
    let cap = info.TotalMetadataBufferSize as usize;
    let mut out = Vec::new();
    // Move rects: both the source and the destination area changed.
    let mut moves = vec![DXGI_OUTDUPL_MOVE_RECT::default(); cap / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>() + 1];
    let mut needed = 0u32;
    // SAFETY: buffer size passed in bytes; `needed` receives the bytes written.
    unsafe {
        dup.GetFrameMoveRects(
            (moves.len() * std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32,
            moves.as_mut_ptr(),
            &mut needed,
        )
    }
    .ok()?;
    for m in &moves[..needed as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>()] {
        let d = m.DestinationRect;
        out.extend(rect_from_ltrb(d.left, d.top, d.right, d.bottom));
        let (w, h) = (d.right - d.left, d.bottom - d.top);
        out.extend(rect_from_ltrb(m.SourcePoint.x, m.SourcePoint.y, m.SourcePoint.x + w, m.SourcePoint.y + h));
    }
    let mut dirty = vec![RECT::default(); cap / std::mem::size_of::<RECT>() + 1];
    // SAFETY: as above.
    unsafe {
        dup.GetFrameDirtyRects((dirty.len() * std::mem::size_of::<RECT>()) as u32, dirty.as_mut_ptr(), &mut needed)
    }
    .ok()?;
    for r in &dirty[..needed as usize / std::mem::size_of::<RECT>()] {
        out.extend(rect_from_ltrb(r.left, r.top, r.right, r.bottom));
    }
    Some(out)
}

fn pointer_shape(dup: &IDXGIOutputDuplication, size: u32) -> Option<CursorShape> {
    let mut buf = vec![0u8; size as usize];
    let mut needed = 0u32;
    let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
    // SAFETY: buffer of `size` bytes as announced by the frame info.
    unsafe { dup.GetFramePointerShape(size, buf.as_mut_ptr().cast(), &mut needed, &mut info) }.ok()?;
    buf.truncate(needed as usize);
    Some(CursorShape {
        kind: CursorKind::from_dxgi(info.Type)?,
        width: info.Width,
        height: info.Height,
        pitch: info.Pitch,
        data: buf,
    })
}

enum Step {
    /// Nothing new (timeout or idle frame).
    Idle,
    /// Emit a frame: (image changed, image dirty rects or None = unknown, present time in QPC ticks).
    Emit { image_rects: Option<Vec<Rect>>, present_qpc: i64 },
    /// The duplication is gone: re-create it.
    Lost(CaptureError),
}

struct DupLoop {
    clean: Option<BgraImage>,
    readback: Readback,
    cursor: CursorState,
}

impl DupLoop {
    fn acquire(&mut self, d: &Duplication, timeout_ms: u32, show_cursor: bool) -> Step {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        // SAFETY: out-pointers valid for the call.
        if let Err(e) = unsafe { d.dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) } {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                return Step::Idle;
            }
            return Step::Lost(os_err("AcquireNextFrame", e));
        }
        let _guard = FrameGuard(&d.dup);
        let mut pointer_changed = false;
        if info.LastMouseUpdateTime != 0 {
            let p = info.PointerPosition;
            pointer_changed = (p.Position.x, p.Position.y, p.Visible.as_bool())
                != (self.cursor.x, self.cursor.y, self.cursor.visible);
            self.cursor.x = p.Position.x;
            self.cursor.y = p.Position.y;
            self.cursor.visible = p.Visible.as_bool();
        }
        if info.PointerShapeBufferSize > 0
            && let Some(s) = pointer_shape(&d.dup, info.PointerShapeBufferSize)
        {
            pointer_changed = true;
            self.cursor.shape = Some(s);
        }
        let image_changed = info.LastPresentTime != 0 && info.AccumulatedFrames > 0;
        if !image_changed {
            if show_cursor && pointer_changed && self.clean.is_some() {
                return Step::Emit { image_rects: Some(Vec::new()), present_qpc: 0 };
            }
            return Step::Idle;
        }
        let Some(res) = res else { return Step::Idle };
        let rects = if self.clean.is_some() && d.rotation == 0 { frame_rects(&d.dup, &info) } else { None };
        let tex = match res.cast::<ID3D11Texture2D>() {
            Ok(t) => t,
            Err(e) => return Step::Lost(os_err("desktop resource is not a texture", e)),
        };
        if let Err(e) = self.readback.read(&d.device, &d.ctx, &tex, None, &mut self.clean, rects.as_deref()) {
            return Step::Lost(e);
        }
        Step::Emit { image_rects: rects, present_qpc: info.LastPresentTime }
    }

    /// Builds the delivered frame from the clean image (+ cursor, rotation, scale).
    fn compose(&mut self, display_id: u32, rotation: u8, ctx: &ThreadCtx, image_rects: Option<Vec<Rect>>, pts: u64) -> Option<CapturedFrame> {
        let clean = self.clean.as_ref()?;
        let mut img = if rotation == 0 { clean.clone() } else { rotate_cw(clean, rotation) };
        let mut rects = if rotation == 0 { image_rects } else { None };
        if ctx.cfg.show_cursor {
            let prev = self.cursor.last_drawn.take();
            let now = match (&self.cursor.shape, self.cursor.visible) {
                (Some(shape), true) => composite_cursor(&mut img, shape, self.cursor.x, self.cursor.y),
                _ => None,
            };
            self.cursor.last_drawn = now;
            if let Some(r) = rects.as_mut() {
                r.extend(prev);
                r.extend(now);
            }
        }
        let (w, h) = (img.width, img.height);
        let rects = rects.map(|r| normalize_dirty(r, w, h));
        if rects.as_ref().is_some_and(|r| r.is_empty()) {
            return None; // nothing visible changed
        }
        let (dw, dh) = scaled_size(w, h, ctx.cfg.scale);
        let (img, rects) = if (dw, dh) != (w, h) {
            (downscale(&img, dw, dh), rects.map(|r| scale_rects(&r, w, h, dw, dh)))
        } else {
            (img, rects)
        };
        Some(CapturedFrame::from_image(display_id, pts, img, rects))
    }
}

/// Capture loop; returns when `ctx.stop` is set. `initial` is a duplication opened by the
/// caller (to report start-up errors synchronously).
pub(crate) fn run(display_id: u32, initial: Option<Duplication>, ctx: &ThreadCtx) {
    let mut dup = initial;
    let mut state = DupLoop { clean: None, readback: Readback::default(), cursor: CursorState::default() };
    let interval = ctx.frame_interval();
    let mut next_due = Instant::now();
    let mut backoff = Duration::from_millis(50);
    let mut outage_since: Option<Instant> = None;
    let mut outage_reported = false;

    while !ctx.stopped() {
        let Some(d) = dup.as_ref() else {
            attach_to_input_desktop();
            match open(display_id) {
                Ok(new) => {
                    tracing::debug!(display_id, "desktop duplication (re)created");
                    dup = Some(new);
                    state.clean = None; // first frame after re-creation is a full frame
                    state.readback = Readback::default();
                    backoff = Duration::from_millis(50);
                    outage_since = None;
                    outage_reported = false;
                }
                Err(e) => {
                    let e = match e {
                        OpenError::Fallback(r) => CaptureError::Os(format!("desktop duplication unavailable: {r}")),
                        OpenError::Transient(e) | OpenError::Fatal(e) => e,
                    };
                    let since = *outage_since.get_or_insert_with(Instant::now);
                    if !outage_reported && since.elapsed() > Duration::from_secs(3) {
                        ctx.hub.deliver(CaptureEvent::Error(e));
                        outage_reported = true;
                    }
                    ctx.sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(1));
                }
            }
            continue;
        };

        // Frame pacing: let DXGI accumulate updates until the next frame is due.
        let now = Instant::now();
        if now < next_due {
            ctx.sleep(next_due - now);
            if ctx.stopped() {
                break;
            }
        }
        let timeout = interval.as_millis().clamp(5, 100) as u32;
        match state.acquire(d, timeout, ctx.cfg.show_cursor) {
            Step::Idle => {}
            Step::Lost(e) => {
                tracing::debug!(display_id, "desktop duplication lost: {e}");
                dup = None;
            }
            Step::Emit { image_rects, present_qpc } => {
                let t = if present_qpc != 0 { qpc_ticks_to_100ns(present_qpc) } else { qpc_100ns() };
                let pts = rel_us(t, ctx.epoch);
                let rotation = d.rotation;
                if let Some(frame) = state.compose(display_id, rotation, ctx, image_rects, pts) {
                    ctx.hub.deliver(CaptureEvent::Frame(frame));
                    next_due = (next_due + interval).max(Instant::now());
                }
            }
        }
    }
}
