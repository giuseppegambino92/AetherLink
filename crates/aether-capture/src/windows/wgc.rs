//! Windows.Graphics.Capture (Windows 10 1803+) for single windows and as the display
//! fallback when Desktop Duplication is unavailable.
//!
//! `GraphicsCaptureItem` from `IGraphicsCaptureItemInterop::CreateForWindow/CreateForMonitor`
//! → free-threaded `Direct3D11CaptureFramePool` (BGRA, 2 buffers) whose `FrameArrived`
//! handler only wakes the capture thread; the capture thread drains the pool to the latest
//! frame, re-creates the pool when `ContentSize` changes (window resized), reads the surface
//! back through a staging texture and crops it to the content size. The cursor is captured
//! by WGC itself (`IsCursorCaptureEnabled`), the yellow border is disabled where the OS
//! allows (`IsBorderRequired = false`, Windows 11), and dirty regions are reported on
//! Windows 11 24H2+ (`DirtyRegionMode::ReportAndRender`), `None` otherwise.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ::windows::Foundation::{TimeSpan, TypedEventHandler};
use ::windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureDirtyRegionMode, GraphicsCaptureItem,
    GraphicsCaptureSession,
};
use ::windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use ::windows::Graphics::DirectX::DirectXPixelFormat;
use ::windows::Graphics::SizeInt32;
use ::windows::Win32::Foundation::HWND;
use ::windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use ::windows::Win32::Graphics::Dxgi::IDXGIDevice;
use ::windows::Win32::Graphics::Gdi::HMONITOR;
use ::windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use ::windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use ::windows::Win32::UI::WindowsAndMessaging::IsWindow;
use ::windows::core::{IInspectable, Interface};

use super::d3d::{Readback, create_device};
use super::{Backend, Ready, ThreadCtx, display_id_for_window, os_err, rel_us};
use crate::windows_pure::{downscale, normalize_dirty, rect_from_ltrb, scale_rects, scaled_size};
use crate::{BgraImage, CaptureError, CaptureEvent, CapturedFrame, Rect, Result};

const BUFFERS: i32 = 2;
const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;

/// Wake-up signal from the WGC callbacks to the capture thread.
#[derive(Default)]
struct Signal {
    pending: Mutex<bool>,
    cv: Condvar,
}

impl Signal {
    fn notify(&self) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_one();
    }

    fn wait(&self, timeout: Duration) {
        let g = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let (mut g, _) = self.cv.wait_timeout_while(g, timeout, |p| !*p).unwrap_or_else(|e| e.into_inner());
        *g = false;
    }
}

fn interop() -> Result<IGraphicsCaptureItemInterop> {
    ::windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(|e| os_err("GraphicsCaptureItem factory", e))
}

fn supported() -> Result<()> {
    match GraphicsCaptureSession::IsSupported() {
        Ok(true) => Ok(()),
        _ => Err(CaptureError::Unsupported("Windows.Graphics.Capture requires Windows 10 1803 or later")),
    }
}

/// Captures a single top-level window.
pub(crate) fn run_window(hwnd: HWND, ctx: &ThreadCtx, ready: Ready) {
    let item = (|| -> Result<GraphicsCaptureItem> {
        // SAFETY: IsWindow accepts any value.
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err(CaptureError::TargetNotFound(format!("window {:#x}", hwnd.0 as usize)));
        }
        supported()?;
        // SAFETY: valid HWND checked above.
        unsafe { interop()?.CreateForWindow(hwnd) }.map_err(|e| os_err("CreateForWindow", e))
    })();
    match item {
        Ok(item) => run(item, || display_id_for_window(hwnd), ctx, ready),
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

/// Captures a whole monitor (fallback when Desktop Duplication is unavailable).
pub(crate) fn run_monitor(mon: HMONITOR, display_id: u32, ctx: &ThreadCtx, ready: Ready) {
    let item = supported().and_then(|_| {
        // SAFETY: HMONITOR from DXGI_OUTPUT_DESC.
        unsafe { interop()?.CreateForMonitor(mon) }.map_err(|e| os_err("CreateForMonitor", e))
    });
    match item {
        Ok(item) => run(item, || display_id, ctx, ready),
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

struct Session {
    device: ID3D11Device,
    d3d_ctx: ID3D11DeviceContext,
    rt_device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    dirty_regions: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn start_session(item: &GraphicsCaptureItem, ctx: &ThreadCtx, signal: &Arc<Signal>, closed: &Arc<AtomicBool>) -> Result<Session> {
    let (device, d3d_ctx) = create_device(None)?;
    let dxgi: IDXGIDevice = device.cast().map_err(|e| os_err("ID3D11Device → IDXGIDevice", e))?;
    // SAFETY: valid DXGI device.
    let inspectable: IInspectable =
        unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }.map_err(|e| os_err("CreateDirect3D11DeviceFromDXGIDevice", e))?;
    let rt_device: IDirect3DDevice = inspectable.cast().map_err(|e| os_err("IDirect3DDevice", e))?;
    let size = item.Size().map_err(|e| os_err("GraphicsCaptureItem.Size", e))?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&rt_device, FORMAT, BUFFERS, size)
        .map_err(|e| os_err("Direct3D11CaptureFramePool.CreateFreeThreaded", e))?;
    let s = signal.clone();
    pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_, _| {
        s.notify();
        Ok(())
    }))
    .map_err(|e| os_err("FrameArrived", e))?;
    let (s, c) = (signal.clone(), closed.clone());
    item.Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
        c.store(true, Ordering::Release);
        s.notify();
        Ok(())
    }))
    .map_err(|e| os_err("GraphicsCaptureItem.Closed", e))?;
    let session = pool.CreateCaptureSession(item).map_err(|e| os_err("CreateCaptureSession", e))?;
    // Optional properties: each exists only on newer Windows builds.
    let _ = session.SetIsCursorCaptureEnabled(ctx.cfg.show_cursor);
    let _ = session.SetIsBorderRequired(false);
    let _ = session.SetMinUpdateInterval(TimeSpan { Duration: ctx.frame_interval().as_micros() as i64 * 10 });
    let dirty_regions = session.SetDirtyRegionMode(GraphicsCaptureDirtyRegionMode::ReportAndRender).is_ok();
    session.StartCapture().map_err(|e| os_err("StartCapture", e))?;
    Ok(Session { device, d3d_ctx, rt_device, pool, session, dirty_regions })
}

fn frame_dirty(frame: &Direct3D11CaptureFrame) -> Option<Vec<Rect>> {
    let regions = frame.DirtyRegions().ok()?;
    let n = regions.Size().ok()?;
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let r = regions.GetAt(i).ok()?;
        out.extend(rect_from_ltrb(r.X, r.Y, r.X + r.Width, r.Y + r.Height));
    }
    Some(out)
}

fn run(item: GraphicsCaptureItem, display_id: impl Fn() -> u32, ctx: &ThreadCtx, ready: Ready) {
    let signal = Arc::new(Signal::default());
    let closed = Arc::new(AtomicBool::new(false));
    let s = match start_session(&item, ctx, &signal, &closed) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(Backend::GraphicsCapture));

    let interval = ctx.frame_interval();
    let mut next_due = Instant::now();
    let mut pool_size = item.Size().unwrap_or_default();
    let mut readback = Readback::default();
    let mut last: Option<BgraImage> = None;

    while !ctx.stopped() {
        if closed.load(Ordering::Acquire) {
            ctx.hub.deliver(CaptureEvent::Error(CaptureError::TargetNotFound("captured window was closed".into())));
            break;
        }
        signal.wait(Duration::from_millis(100));
        let now = Instant::now();
        if now < next_due {
            ctx.sleep(next_due - now);
        }
        if ctx.stopped() {
            break;
        }
        // Drain to the newest frame (older ones are returned to the pool on Close).
        let mut latest: Option<Direct3D11CaptureFrame> = None;
        while let Ok(f) = s.pool.TryGetNextFrame() {
            if let Some(old) = latest.replace(f) {
                let _ = old.Close();
            }
        }
        let Some(frame) = latest else { continue };
        match capture_frame(&s, &frame, &mut pool_size, &mut readback, &mut last, ctx, display_id()) {
            Ok(Some(f)) => {
                ctx.hub.deliver(CaptureEvent::Frame(f));
                next_due = (next_due + interval).max(Instant::now());
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("Windows.Graphics.Capture frame failed: {e}");
                ctx.hub.deliver(CaptureEvent::Error(e));
            }
        }
        let _ = frame.Close();
    }
    drop(s);
}

fn capture_frame(
    s: &Session,
    frame: &Direct3D11CaptureFrame,
    pool_size: &mut SizeInt32,
    readback: &mut Readback,
    last: &mut Option<BgraImage>,
    ctx: &ThreadCtx,
    display_id: u32,
) -> Result<Option<CapturedFrame>> {
    let content = frame.ContentSize().map_err(|e| os_err("ContentSize", e))?;
    if content.Width <= 0 || content.Height <= 0 {
        return Ok(None); // minimised window
    }
    if content != *pool_size {
        // Window resized: future frames use the new size; this one is cropped below.
        s.pool.Recreate(&s.rt_device, FORMAT, BUFFERS, content).map_err(|e| os_err("FramePool.Recreate", e))?;
        *pool_size = content;
    }
    let surface = frame.Surface().map_err(|e| os_err("Surface", e))?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|e| os_err("IDirect3DDxgiInterfaceAccess", e))?;
    // SAFETY: the surface wraps a D3D11 texture of our device.
    let tex: ID3D11Texture2D = unsafe { access.GetInterface() }.map_err(|e| os_err("GetInterface(ID3D11Texture2D)", e))?;
    let prev_size = last.as_ref().map(|i| (i.width, i.height));
    let crop = (content.Width as u32, content.Height as u32);
    let rects = if s.dirty_regions && prev_size == Some(crop) { frame_dirty(frame) } else { None };
    readback.read(&s.device, &s.d3d_ctx, &tex, Some(crop), last, None)?;
    let Some(img) = last.as_ref() else { return Ok(None) };
    let (w, h) = (img.width, img.height);
    let rects = rects.map(|r| normalize_dirty(r, w, h));
    if rects.as_ref().is_some_and(|r| r.is_empty()) {
        return Ok(None);
    }
    let t = frame.SystemRelativeTime().map(|t| t.Duration).unwrap_or_else(|_| super::qpc_100ns());
    let pts = rel_us(t, ctx.epoch);
    let (dw, dh) = scaled_size(w, h, ctx.cfg.scale);
    let (img, rects) = if (dw, dh) != (w, h) {
        (downscale(img, dw, dh), rects.map(|r| scale_rects(&r, w, h, dw, dh)))
    } else {
        (img.clone(), rects)
    };
    Ok(Some(CapturedFrame::from_image(display_id, pts, img, rects)))
}
