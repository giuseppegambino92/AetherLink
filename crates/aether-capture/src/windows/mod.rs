//! Windows capture backends.
//!
//! * [`WinCapturer`] — `CaptureTarget::Display` uses **DXGI Desktop Duplication**
//!   ([`dxgi`]): dirty / move rects, cursor composited from the duplication's pointer shape,
//!   automatic re-creation on `DXGI_ERROR_ACCESS_LOST` (desktop switch, UAC secure desktop,
//!   mode change, full-screen exclusive apps) after re-attaching the capture thread to the
//!   current input desktop. When duplication is unavailable (`DXGI_ERROR_UNSUPPORTED`,
//!   `E_ACCESSDENIED` at start, e.g. some hybrid-GPU or remote sessions) the display is
//!   captured with **Windows.Graphics.Capture** instead ([`wgc`]), which is also the backend
//!   for `CaptureTarget::Window` (`window_id` = `HWND`).
//! * [`list_displays`] — DXGI outputs (`\\.\DISPLAYn` → id `n`), `GetDpiForMonitor`,
//!   `EnumDisplaySettingsW` refresh rate, friendly monitor name.
//! * [`list_windows`] — visible, uncloaked, unowned, non-tool top-level windows with a title,
//!   bounds from `DWMWA_EXTENDED_FRAME_BOUNDS`.
//! * Audio (`CaptureConfig::capture_audio`) — WASAPI loopback ([`crate::audio`]), packets
//!   delivered to the same sink with timestamps on the same QPC clock as the video.
//!
//! Frames are delivered as CPU BGRA ([`crate::FramePixels::Bgra`]) read back through a
//! `D3D11_USAGE_STAGING` texture. All timestamps derive from `QueryPerformanceCounter`.
//!
//! `CaptureConfig::excluded_pids` cannot be honoured by either API: the Ghost Mode curtain
//! window must call `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` itself, which
//! removes it from Desktop Duplication and WGC output alike.
//!
//! Compile-verified by cross-compilation; not run-tested from the macOS dev machine.

pub(crate) mod d3d;
pub(crate) mod dxgi;
pub(crate) mod wgc;

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::Duration;

use ::windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HWND, LPARAM, RECT};
use ::windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use ::windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, DXGI_OUTPUT_DESC};
use ::windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplaySettingsW, GetMonitorInfoW,
    HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
};
use ::windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use ::windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use ::windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, OpenInputDesktop, SetThreadDesktop,
};
use ::windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use ::windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, WS_EX_TOOLWINDOW,
};
use ::windows::core::{BOOL, PCWSTR, PWSTR};

use crate::audio::AudioCapturer;
use crate::audio::windows::WasapiCapturer;
use crate::windows_pure::{parse_display_number, rect_from_ltrb, wide_to_string};
use crate::{
    CaptureConfig, CaptureError, CaptureEvent, CaptureSink, CaptureTarget, DisplayInfo, Rect, Result, ScreenCapturer,
    WindowInfo,
};

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

pub(crate) fn os_err(context: &str, e: ::windows::core::Error) -> CaptureError {
    CaptureError::Os(format!("{context}: {} ({:#010x})", e.message(), e.code().0 as u32))
}

/// Process-wide per-monitor DPI awareness (best effort; a manifest may already set it).
/// Without it DXGI/GDI coordinates of scaled monitors are virtualised.
pub(crate) fn init_dpi_awareness() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: plain Win32 call without pointers; fails harmlessly if already set.
        let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    });
}

/// `QueryPerformanceCounter` in 100 ns units — the clock DXGI (`LastPresentTime`, converted),
/// WGC (`SystemRelativeTime`) and WASAPI (`qpc position`) timestamps are expressed in.
pub(crate) fn qpc_100ns() -> i64 {
    let mut c = 0i64;
    // SAFETY: valid out-pointer; QPC cannot fail on XP+.
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    qpc_ticks_to_100ns(c)
}

/// Converts raw QPC ticks to 100 ns units.
pub(crate) fn qpc_ticks_to_100ns(ticks: i64) -> i64 {
    let mut f = 0i64;
    // SAFETY: valid out-pointer.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    if f <= 0 {
        return 0;
    }
    (ticks as i128 * 10_000_000 / f as i128) as i64
}

/// `t` (100 ns QPC time) relative to `epoch`, in µs.
pub(crate) fn rel_us(t_100ns: i64, epoch_100ns: i64) -> u64 {
    (t_100ns.saturating_sub(epoch_100ns).max(0) / 10) as u64
}

thread_local! {
    /// Desktop handle the current thread was switched to (must stay open while assigned).
    static THREAD_DESKTOP: Cell<isize> = const { Cell::new(0) };
}

/// Switches the calling thread to the current *input* desktop (Default / Winlogon /
/// Screen-saver). Succeeds only with enough rights (LocalSystem, as launched by
/// `aether-winsvc`) and when the thread owns no windows or hooks; returns whether the
/// thread is now on the input desktop. Desktop Duplication must be (re)created from a
/// thread attached to the desktop being duplicated.
pub(crate) fn attach_to_input_desktop() -> bool {
    // SAFETY: handles come from OpenInputDesktop and are closed exactly once: the previous
    // thread desktop after the switch succeeded, the new one on failure.
    unsafe {
        let Ok(desk) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0)) else {
            return false;
        };
        if SetThreadDesktop(desk).is_err() {
            let _ = CloseDesktop(desk);
            return false;
        }
        let prev = THREAD_DESKTOP.with(|c| c.replace(desk.0 as isize));
        if prev != 0 && prev != desk.0 as isize {
            let _ = CloseDesktop(HDESK(prev as *mut _));
        }
        true
    }
}

/// Sink shared by the video thread and the audio thread.
pub(crate) struct SinkHub {
    sink: Mutex<Option<CaptureSink>>,
}

impl SinkHub {
    fn new(sink: CaptureSink) -> Self {
        Self { sink: Mutex::new(Some(sink)) }
    }

    pub(crate) fn deliver(&self, ev: CaptureEvent) {
        let mut g = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = g.as_mut() {
            s(ev);
        }
    }

    fn close(&self) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Everything a capture thread needs.
pub(crate) struct ThreadCtx {
    pub cfg: CaptureConfig,
    pub hub: Arc<SinkHub>,
    pub stop: Arc<AtomicBool>,
    /// 100 ns QPC time of `start()`; all pts are relative to it.
    pub epoch: i64,
}

impl ThreadCtx {
    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    pub(crate) fn frame_interval(&self) -> Duration {
        Duration::from_micros(1_000_000 / self.cfg.fps.clamp(1, 240) as u64)
    }

    /// Sleeps up to `d`, waking early when stopped.
    pub(crate) fn sleep(&self, d: Duration) {
        let end = std::time::Instant::now() + d;
        while !self.stopped() {
            let now = std::time::Instant::now();
            if now >= end {
                break;
            }
            std::thread::sleep((end - now).min(Duration::from_millis(20)));
        }
    }
}

/// Which backend a capture thread ended up using.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    DesktopDuplication,
    GraphicsCapture,
}

pub(crate) type Ready = SyncSender<Result<Backend>>;

// ---------------------------------------------------------------------------------------------
// Capturer
// ---------------------------------------------------------------------------------------------

struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    audio: Option<WasapiCapturer>,
    hub: Arc<SinkHub>,
}

/// Desktop Duplication / Windows.Graphics.Capture screen capturer (see module docs).
#[derive(Default)]
pub struct WinCapturer {
    running: Option<Running>,
    backend: Option<Backend>,
}

impl WinCapturer {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when the running capture uses Windows.Graphics.Capture (window target or
    /// Desktop Duplication unavailable), false for Desktop Duplication, `None` when idle.
    pub fn uses_graphics_capture(&self) -> Option<bool> {
        self.running.as_ref().and(self.backend).map(|b| b == Backend::GraphicsCapture)
    }
}

fn capture_thread(target: CaptureTarget, ctx: ThreadCtx, ready: Ready) {
    if matches!(target, CaptureTarget::Display { .. }) {
        // Before COM init: SetThreadDesktop requires a thread without windows or hooks.
        attach_to_input_desktop();
    }
    // SAFETY: balanced with CoUninitialize below on this thread. MTA is required for the
    // free-threaded WGC frame pool and harmless for DXGI.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    match target {
        CaptureTarget::Display { display_id } => match dxgi::open(display_id) {
            Ok(dup) => {
                let _ = ready.send(Ok(Backend::DesktopDuplication));
                dxgi::run(display_id, Some(dup), &ctx);
            }
            Err(dxgi::OpenError::Fallback(reason)) => {
                tracing::info!(display_id, "Desktop Duplication unavailable ({reason}); using Windows.Graphics.Capture");
                match find_monitor(display_id) {
                    Ok(mon) => wgc::run_monitor(mon, display_id, &ctx, ready),
                    Err(e) => {
                        let _ = ready.send(Err(e));
                    }
                }
            }
            Err(dxgi::OpenError::Fatal(e)) => {
                let _ = ready.send(Err(e));
            }
            Err(dxgi::OpenError::Transient(e)) => {
                // E.g. secure desktop showing while we lack SYSTEM rights: keep retrying.
                tracing::warn!(display_id, "Desktop Duplication not available yet: {e}");
                let _ = ready.send(Ok(Backend::DesktopDuplication));
                dxgi::run(display_id, None, &ctx);
            }
        },
        CaptureTarget::Window { window_id } => wgc::run_window(HWND(window_id as usize as *mut _), &ctx, ready),
    }
    if com {
        // SAFETY: matches the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

impl ScreenCapturer for WinCapturer {
    fn start(&mut self, target: CaptureTarget, cfg: CaptureConfig, sink: CaptureSink) -> Result<()> {
        if self.running.is_some() {
            return Err(CaptureError::AlreadyRunning);
        }
        init_dpi_awareness();
        let hub = Arc::new(SinkHub::new(sink));
        let stop = Arc::new(AtomicBool::new(false));
        let epoch = qpc_100ns();
        let capture_audio = cfg.capture_audio;
        let ctx = ThreadCtx { cfg, hub: hub.clone(), stop: stop.clone(), epoch };
        let (tx, rx) = sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("aether-capture-win".into())
            .spawn(move || capture_thread(target, ctx, tx))
            .map_err(|e| CaptureError::Os(format!("spawn capture thread: {e}")))?;
        let backend = match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                stop.store(true, Ordering::Release);
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                // The thread may be stuck in a driver call; don't block on it.
                hub.close();
                return Err(CaptureError::Timeout("capture start"));
            }
        };
        let audio = if capture_audio {
            let mut a = WasapiCapturer::with_epoch(Some(epoch));
            let h = hub.clone();
            match a.start(Box::new(move |p| h.deliver(CaptureEvent::Audio(p)))) {
                Ok(fmt) => {
                    tracing::debug!(?fmt, "WASAPI loopback started");
                    Some(a)
                }
                Err(e) => {
                    // Video keeps running; report the audio failure through the sink.
                    tracing::warn!("system audio capture unavailable: {e}");
                    hub.deliver(CaptureEvent::Error(e));
                    None
                }
            }
        } else {
            None
        };
        self.backend = Some(backend);
        self.running = Some(Running { stop, thread: Some(thread), audio, hub });
        Ok(())
    }

    fn stop(&mut self) {
        let Some(mut r) = self.running.take() else { return };
        r.stop.store(true, Ordering::Release);
        if let Some(t) = r.thread.take() {
            let _ = t.join();
        }
        if let Some(mut a) = r.audio.take() {
            a.stop();
        }
        r.hub.close();
        self.backend = None;
    }
}

impl Drop for WinCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------------------------

/// One attached DXGI output.
pub(crate) struct OutputEntry {
    pub id: u32,
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput,
    pub desc: DXGI_OUTPUT_DESC,
}

/// All outputs attached to the desktop, deduplicated by display number.
pub(crate) fn enum_outputs() -> Result<Vec<OutputEntry>> {
    // SAFETY: COM factory creation and enumeration with owned interface results.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| os_err("CreateDXGIFactory1", e))?;
    let mut out: Vec<OutputEntry> = Vec::new();
    let mut ai = 0;
    // SAFETY: as above; EnumAdapters1/EnumOutputs return DXGI_ERROR_NOT_FOUND past the end.
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(ai) } {
        ai += 1;
        let mut oi = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(oi) } {
            oi += 1;
            let Ok(desc) = (unsafe { output.GetDesc() }) else { continue };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let Some(id) = parse_display_number(&wide_to_string(&desc.DeviceName)) else { continue };
            if out.iter().any(|o| o.id == id) {
                continue;
            }
            out.push(OutputEntry { id, adapter: adapter.clone(), output, desc });
        }
    }
    out.sort_by_key(|o| o.id);
    Ok(out)
}

fn find_monitor(display_id: u32) -> Result<HMONITOR> {
    enum_outputs()?
        .into_iter()
        .find(|o| o.id == display_id)
        .map(|o| o.desc.Monitor)
        .ok_or_else(|| CaptureError::TargetNotFound(format!("display {display_id}")))
}

fn monitor_info(mon: HMONITOR) -> Option<MONITORINFOEXW> {
    let mut mi = MONITORINFOEXW::default();
    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: MONITORINFOEXW starts with MONITORINFO and cbSize announces the extended size.
    let ok = unsafe { GetMonitorInfoW(mon, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) };
    ok.as_bool().then_some(mi)
}

/// Display number of the monitor showing most of `hwnd`.
pub(crate) fn display_id_for_window(hwnd: HWND) -> u32 {
    // SAFETY: MonitorFromWindow tolerates invalid HWNDs (returns the nearest/primary).
    let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    monitor_info(mon).and_then(|mi| parse_display_number(&wide_to_string(&mi.szDevice))).unwrap_or(0)
}

fn refresh_hz(device: &[u16]) -> f32 {
    let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    // SAFETY: `device` is the NUL-terminated DeviceName of DXGI_OUTPUT_DESC; dm is sized.
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR(device.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) };
    // 0 and 1 mean "hardware default".
    if ok.as_bool() && dm.dmDisplayFrequency > 1 { dm.dmDisplayFrequency as f32 } else { 60.0 }
}

fn friendly_name(device: &[u16]) -> Option<String> {
    let mut dd = DISPLAY_DEVICEW { cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
    // SAFETY: index 0 of an adapter device name enumerates its first monitor; dd is sized.
    let ok = unsafe { EnumDisplayDevicesW(PCWSTR(device.as_ptr()), 0, &mut dd, 0) };
    let s = wide_to_string(&dd.DeviceString);
    (ok.as_bool() && !s.trim().is_empty()).then(|| s.trim().to_string())
}

/// Displays attached to the desktop (physical pixel bounds in virtual-desktop space).
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    init_dpi_awareness();
    let mut out = Vec::new();
    for o in enum_outputs()? {
        let d = &o.desc;
        let c = d.DesktopCoordinates;
        let Some(bounds) = rect_from_ltrb(c.left, c.top, c.right, c.bottom) else { continue };
        let (mut dx, mut dy) = (96u32, 96u32);
        // SAFETY: valid HMONITOR from DXGI and out-pointers.
        let scale = if unsafe { GetDpiForMonitor(d.Monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) }.is_ok() && dx > 0 {
            dx as f32 / 96.0
        } else {
            1.0
        };
        let primary = monitor_info(d.Monitor).is_some_and(|mi| mi.monitorInfo.dwFlags & 1 != 0); // MONITORINFOF_PRIMARY
        let device = wide_to_string(&d.DeviceName);
        let name = friendly_name(&d.DeviceName).map(|n| format!("{n} ({})", o.id)).unwrap_or(device);
        out.push(DisplayInfo {
            id: o.id,
            name,
            bounds,
            scale,
            refresh_hz: refresh_hz(&d.DeviceName),
            primary,
            is_virtual: false,
        });
    }
    Ok(out)
}

fn window_title(hwnd: HWND) -> String {
    // SAFETY: buffer sized from GetWindowTextLengthW (+1 for NUL).
    unsafe {
        let n = GetWindowTextLengthW(hwnd);
        if n <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

fn process_name(pid: u32) -> String {
    // SAFETY: handle closed below; buffer length passed in/out.
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(path)
    }
}

pub(crate) fn window_bounds(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    // SAFETY: out-pointer of the documented size for DWMWA_EXTENDED_FRAME_BOUNDS.
    let ok = unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut RECT as *mut _, std::mem::size_of::<RECT>() as u32)
    }
    .is_ok()
        // SAFETY: plain out-pointer.
        || unsafe { GetWindowRect(hwnd, &mut r) }.is_ok();
    if ok { rect_from_ltrb(r.left, r.top, r.right, r.bottom) } else { None }
}

fn is_capturable_window(hwnd: HWND) -> bool {
    // SAFETY: read-only window queries on an HWND handed to us by EnumWindows.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return false;
        }
        if GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
            return false;
        }
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        let mut cloaked = 0u32;
        let r = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
        if r.is_ok() && cloaked != 0 {
            return false;
        }
    }
    true
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the &mut Vec<HWND> passed by list_windows for the duration of the call.
    let v = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    v.push(hwnd);
    BOOL(1)
}

/// Visible, capturable top-level application windows (front to back, z-order).
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    init_dpi_awareness();
    let mut hwnds: Vec<HWND> = Vec::new();
    // SAFETY: the callback only pushes into `hwnds`, which outlives the call.
    unsafe { EnumWindows(Some(enum_cb), LPARAM(&mut hwnds as *mut Vec<HWND> as isize)) }
        .map_err(|e| os_err("EnumWindows", e))?;
    let mut out = Vec::new();
    for hwnd in hwnds {
        if !is_capturable_window(hwnd) {
            continue;
        }
        let title = window_title(hwnd);
        if title.trim().is_empty() {
            continue;
        }
        let Some(bounds) = window_bounds(hwnd) else { continue };
        if bounds.width < 2 || bounds.height < 2 {
            continue;
        }
        let mut pid = 0u32;
        // SAFETY: out-pointer for the owning process id.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        out.push(WindowInfo { id: hwnd.0 as usize as u64, app_name: process_name(pid), title, bounds, pid });
    }
    Ok(out)
}
