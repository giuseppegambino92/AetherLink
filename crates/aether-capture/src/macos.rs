//! macOS backend: ScreenCaptureKit.
//!
//! * Enumeration: `SCShareableContent` → [`DisplayInfo`] / [`WindowInfo`] (bounds in physical
//!   pixels of the global desktop, i.e. point coordinates × backing scale).
//! * Capture: one `SCStream` per [`SckCapturer`], filter = whole display minus the windows of
//!   `excluded_pids` (Ghost Mode curtain), or a single desktop-independent window.
//!   BGRA output at native pixel resolution × `scale`, `minimumFrameInterval = 1/fps`,
//!   `queueDepth = 3`, sRGB colour space.
//! * Frames whose `SCStreamFrameInfoStatus` is not `complete` (idle/blank/suspended…) are
//!   dropped; `SCStreamFrameInfoDirtyRects` become [`CapturedFrame::dirty_rects`].
//! * The `CVPixelBuffer` is passed on zero-copy as [`FramePixels::PixelBuffer`].
//! * System audio (`capturesAudio`, 48 kHz stereo) is converted to interleaved `f32` PCM.

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription};
use objc2_core_foundation::{CFArray, CFDictionary, CFRetained, CGRect};
use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayIsBuiltin, CGDisplayMode, CGMainDisplayID, CGPreflightScreenCaptureAccess,
    CGRectMakeWithDictionaryRepresentation, CGRequestScreenCaptureAccess, kCGColorSpaceSRGB,
};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime, CMTimeFlags,
};
use objc2_core_video::{
    CVDisplayLink, CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCRunningApplication, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamDelegate, SCStreamFrameInfoDirtyRects, SCStreamFrameInfoStatus, SCStreamOutput,
    SCStreamOutputType, SCWindow,
};

use crate::{
    AudioCodec, AudioPacket, BgraImage, CaptureConfig, CaptureError, CaptureEvent, CaptureSink, CaptureTarget,
    CapturedFrame, DisplayInfo, FramePixels, Rect, Result, ScreenCapturer, WindowInfo,
};

const ASYNC_TIMEOUT: Duration = Duration::from_secs(10);
/// `SCStreamErrorUserDeclined`
const SC_ERROR_USER_DECLINED: isize = -3801;

// ---------------------------------------------------------------------------------------------
// Pixel buffer wrapper
// ---------------------------------------------------------------------------------------------

/// A retained `CVPixelBuffer` (IOSurface-backed when it comes from ScreenCaptureKit).
///
/// Cloning retains the same buffer (no pixel copy).
#[derive(Clone)]
pub struct PixelBuffer(CFRetained<CVPixelBuffer>);

// SAFETY: CVPixelBuffer is a reference-counted CF object; retain/release are thread-safe and we
// only read pixel data under CVPixelBufferLockBaseAddress.
unsafe impl Send for PixelBuffer {}
unsafe impl Sync for PixelBuffer {}

impl std::fmt::Debug for PixelBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fmt = self.pixel_format().to_be_bytes();
        f.debug_struct("PixelBuffer")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("format", &String::from_utf8_lossy(&fmt))
            .finish()
    }
}

impl PixelBuffer {
    /// Wraps a retained pixel buffer.
    pub fn new(buffer: CFRetained<CVPixelBuffer>) -> Self {
        Self(buffer)
    }

    /// The underlying `CVPixelBuffer` (e.g. to hand it to VideoToolbox without copying).
    pub fn as_cv(&self) -> &CVPixelBuffer {
        &self.0
    }

    pub fn width(&self) -> u32 {
        CVPixelBufferGetWidth(&self.0) as u32
    }

    pub fn height(&self) -> u32 {
        CVPixelBufferGetHeight(&self.0) as u32
    }

    /// FourCC pixel format (`'BGRA'` for capture output).
    pub fn pixel_format(&self) -> u32 {
        CVPixelBufferGetPixelFormatType(&self.0)
    }

    /// Locks the buffer read-only and copies it into a packed [`BgraImage`].
    pub fn to_bgra(&self) -> Result<BgraImage> {
        if self.pixel_format() != kCVPixelFormatType_32BGRA {
            return Err(CaptureError::InvalidFrame(format!("pixel format {:#x} is not BGRA", self.pixel_format())));
        }
        let rc = unsafe { CVPixelBufferLockBaseAddress(&self.0, CVPixelBufferLockFlags::ReadOnly) };
        if rc != 0 {
            return Err(CaptureError::InvalidFrame(format!("CVPixelBufferLockBaseAddress failed: {rc}")));
        }
        let res = (|| {
            let base = CVPixelBufferGetBaseAddress(&self.0) as *const u8;
            if base.is_null() {
                return Err(CaptureError::InvalidFrame("null base address".into()));
            }
            let (w, h) = (self.width(), self.height());
            let stride = CVPixelBufferGetBytesPerRow(&self.0);
            // SAFETY: the buffer is locked; it spans `stride * height` bytes.
            let src = unsafe { std::slice::from_raw_parts(base, stride * h as usize) };
            BgraImage::copy_from_rows(w, h, src, stride)
        })();
        unsafe { CVPixelBufferUnlockBaseAddress(&self.0, CVPixelBufferLockFlags::ReadOnly) };
        res
    }
}

// ---------------------------------------------------------------------------------------------
// Permission
// ---------------------------------------------------------------------------------------------

pub fn permission_granted() -> bool {
    CGPreflightScreenCaptureAccess()
}

pub fn request_permission() -> bool {
    CGRequestScreenCaptureAccess()
}

// ---------------------------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------------------------

/// `Retained<T>` asserted Send so it can cross the completion-handler thread boundary.
struct SendRetained<T>(Retained<T>);
// SAFETY: ScreenCaptureKit objects are immutable value-like objects that Apple documents as
// usable from any thread; we only move them from the completion queue to the caller.
unsafe impl<T> Send for SendRetained<T> {}

fn ns_error_message(err: *mut NSError) -> (String, isize) {
    match unsafe { err.as_ref() } {
        Some(e) => (e.localizedDescription().to_string(), e.code()),
        None => ("unknown error".to_string(), 0),
    }
}

fn map_sck_error(msg: String, code: isize) -> CaptureError {
    if code == SC_ERROR_USER_DECLINED || !permission_granted() {
        CaptureError::PermissionDenied
    } else {
        CaptureError::Os(format!("{msg} (code {code})"))
    }
}

pub(crate) fn shareable_content() -> Result<Retained<SCShareableContent>> {
    if !permission_granted() {
        // Querying SCShareableContent without permission would trigger the TCC prompt.
        return Err(CaptureError::PermissionDenied);
    }
    let (tx, rx) = mpsc::channel::<std::result::Result<SendRetained<SCShareableContent>, (String, isize)>>();
    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let res = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(SendRetained(c)),
            None => Err(ns_error_message(error)),
        };
        let _ = tx.send(res);
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &block,
        )
    };
    match rx.recv_timeout(ASYNC_TIMEOUT) {
        Ok(Ok(c)) => Ok(c.0),
        Ok(Err((msg, code))) => Err(map_sck_error(msg, code)),
        Err(_) => Err(CaptureError::Timeout("SCShareableContent")),
    }
}

struct ModeInfo {
    scale: f32,
    refresh_hz: f32,
}

#[allow(deprecated)] // CVDisplayLink is only used to read the nominal refresh period.
fn nominal_refresh_from_display_link(id: u32) -> Option<f32> {
    let mut link: *mut CVDisplayLink = ptr::null_mut();
    let rc = unsafe { CVDisplayLink::create_with_cg_display(id, NonNull::from(&mut link)) };
    if rc != 0 || link.is_null() {
        return None;
    }
    // SAFETY: created above with +1 retain count.
    let link = unsafe { CFRetained::from_raw(NonNull::new_unchecked(link)) };
    let t = link.nominal_output_video_refresh_period();
    if t.timeValue > 0 && (t.flags & 1) == 0 {
        Some(t.timeScale as f32 / t.timeValue as f32)
    } else {
        None
    }
}

fn mode_info(id: u32, points_width: f64) -> ModeInfo {
    let mode = CGDisplayCopyDisplayMode(id);
    let mode_ref = mode.as_deref();
    let px_w = CGDisplayMode::pixel_width(mode_ref) as f64;
    let pt_w = CGDisplayMode::width(mode_ref) as f64;
    let scale = if pt_w > 0.0 && px_w > 0.0 {
        px_w / pt_w
    } else if points_width > 0.0 && px_w > 0.0 {
        px_w / points_width
    } else {
        1.0
    };
    let mut refresh = CGDisplayMode::refresh_rate(mode_ref) as f32;
    if refresh <= 0.0 {
        refresh = nominal_refresh_from_display_link(id).unwrap_or(0.0);
    }
    ModeInfo { scale: scale as f32, refresh_hz: refresh }
}

fn scaled_rect(r: CGRect, scale: f64) -> Rect {
    Rect::new(
        (r.origin.x * scale).round() as i32,
        (r.origin.y * scale).round() as i32,
        (r.size.width * scale).round().max(0.0) as u32,
        (r.size.height * scale).round().max(0.0) as u32,
    )
}

fn display_info(d: &SCDisplay, main_id: u32) -> DisplayInfo {
    let id = unsafe { d.displayID() };
    let frame = unsafe { d.frame() };
    let mi = mode_info(id, frame.size.width);
    let builtin = CGDisplayIsBuiltin(id);
    DisplayInfo {
        id,
        name: if builtin { format!("Built-in Display ({id})") } else { format!("Display {id}") },
        bounds: scaled_rect(frame, mi.scale as f64),
        scale: mi.scale,
        refresh_hz: mi.refresh_hz,
        primary: id == main_id,
        is_virtual: false,
    }
}

/// Lists all displays visible to ScreenCaptureKit.
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    let content = shareable_content()?;
    let main_id = CGMainDisplayID();
    let displays = unsafe { content.displays() };
    Ok(displays.iter().map(|d| display_info(&d, main_id)).collect())
}

/// The display containing the centre of `r` (point coordinates).
fn display_for_rect(displays: &[Retained<SCDisplay>], r: CGRect) -> Option<&Retained<SCDisplay>> {
    let cx = r.origin.x + r.size.width / 2.0;
    let cy = r.origin.y + r.size.height / 2.0;
    displays.iter().find(|d| {
        let f = unsafe { d.frame() };
        cx >= f.origin.x && cy >= f.origin.y && cx < f.origin.x + f.size.width && cy < f.origin.y + f.size.height
    })
}

fn is_user_window(w: &SCWindow) -> bool {
    let frame = unsafe { w.frame() };
    let normal = unsafe { w.windowLayer() == 0 && w.isOnScreen() };
    normal && frame.size.width >= 2.0 && frame.size.height >= 2.0
}

/// Lists on-screen, normal-layer application windows.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    let content = shareable_content()?;
    let displays = unsafe { content.displays() }.to_vec();
    let mut out = Vec::new();
    for w in unsafe { content.windows() }.iter() {
        if !is_user_window(&w) {
            continue;
        }
        let Some(app) = (unsafe { w.owningApplication() }) else { continue };
        let frame = unsafe { w.frame() };
        let scale = display_for_rect(&displays, frame)
            .map(|d| mode_info(unsafe { d.displayID() }, unsafe { d.frame() }.size.width).scale as f64)
            .unwrap_or(1.0);
        out.push(WindowInfo {
            id: unsafe { w.windowID() } as u64,
            app_name: unsafe { app.applicationName() }.to_string(),
            title: unsafe { w.title() }.map(|t| t.to_string()).unwrap_or_default(),
            bounds: scaled_rect(frame, scale),
            pid: unsafe { app.processID() } as u32,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Stream output object
// ---------------------------------------------------------------------------------------------

pub(crate) struct Shared {
    /// The sink; `None` once stopped. Locked for every delivery so `stop()` can fence.
    pub(crate) sink: Mutex<Option<CaptureSink>>,
    pub(crate) display_id: u32,
    /// First video/audio pts (µs, host clock) — pts are reported relative to it.
    pub(crate) base_pts_us: Mutex<Option<i64>>,
    pub(crate) audio_seq: AtomicU64,
    pub(crate) stopping: AtomicBool,
}

impl Shared {
    fn deliver(&self, ev: CaptureEvent) {
        if let Ok(mut guard) = self.sink.lock()
            && let Some(sink) = guard.as_mut()
        {
            sink(ev);
        }
    }

    fn rel_pts(&self, t: CMTime) -> u64 {
        if t.timescale <= 0 || !t.flags.contains(CMTimeFlags::Valid) {
            return 0;
        }
        let us = (t.value as i128 * 1_000_000 / t.timescale as i128) as i64;
        let mut base = self.base_pts_us.lock().unwrap_or_else(|e| e.into_inner());
        let b = *base.get_or_insert(us);
        (us - b).max(0) as u64
    }

    fn handle_screen(&self, sample: &CMSampleBuffer) {
        // SAFETY: attachments array of a valid sample buffer; element 0 is a CFDictionary
        // (toll-free bridged to NSDictionary) with SCStreamFrameInfo keys.
        let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else { return };
        let Some(info) = (unsafe { first_dictionary(&attachments) }) else { return };
        let status = unsafe { info.objectForKey(SCStreamFrameInfoStatus) }
            .and_then(|o| o.downcast::<NSNumber>().ok())
            .map(|n| n.integerValue());
        if status != Some(SCFrameStatus::Complete.0) {
            return; // idle / blank / suspended / started / stopped
        }
        let Some(image) = (unsafe { sample.image_buffer() }) else { return };
        let pb = PixelBuffer::new(image);
        let (w, h) = (pb.width(), pb.height());
        let dirty = unsafe { info.objectForKey(SCStreamFrameInfoDirtyRects) }
            .and_then(|o| o.downcast::<NSArray>().ok())
            .map(|arr| {
                let mut rects = Vec::with_capacity(arr.len());
                for item in arr.iter() {
                    let mut r = CGRect::default();
                    let dict = Retained::as_ptr(&item) as *const CFDictionary;
                    // SAFETY: each element is a CGRect dictionary representation.
                    if unsafe { CGRectMakeWithDictionaryRepresentation(dict.as_ref(), &mut r) }
                        && let Some(rect) = clamp_rect(r, w, h)
                    {
                        rects.push(rect);
                    }
                }
                rects
            });
        let pts_us = self.rel_pts(unsafe { sample.presentation_time_stamp() });
        self.deliver(CaptureEvent::Frame(CapturedFrame {
            display_id: self.display_id,
            width: w,
            height: h,
            pts_us,
            dirty_rects: dirty,
            pixels: FramePixels::PixelBuffer(pb),
        }));
    }

    fn handle_audio(&self, sample: &CMSampleBuffer) {
        match audio_packet(sample, self) {
            Ok(Some(p)) => self.deliver(CaptureEvent::Audio(p)),
            Ok(None) => {}
            Err(e) => self.deliver(CaptureEvent::Error(e)),
        }
    }
}

/// First element of a CFArray of CFDictionaries, as an NSDictionary.
unsafe fn first_dictionary(arr: &CFArray) -> Option<Retained<NSDictionary<NSString, AnyObject>>> {
    if arr.is_empty() {
        return None;
    }
    unsafe extern "C-unwind" {
        fn CFArrayGetValueAtIndex(arr: &CFArray, idx: isize) -> *const c_void;
    }
    let p = unsafe { CFArrayGetValueAtIndex(arr, 0) } as *mut NSDictionary<NSString, AnyObject>;
    unsafe { Retained::retain(p) }
}

fn clamp_rect(r: CGRect, w: u32, h: u32) -> Option<Rect> {
    let x0 = r.origin.x.floor().max(0.0) as i64;
    let y0 = r.origin.y.floor().max(0.0) as i64;
    let x1 = ((r.origin.x + r.size.width).ceil() as i64).min(w as i64);
    let y1 = ((r.origin.y + r.size.height).ceil() as i64).min(h as i64);
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0 as i32, y0 as i32, (x1 - x0) as u32, (y1 - y0) as u32))
}

const K_AUDIO_FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
const K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;
const K_CM_SAMPLE_BUFFER_FLAG_ASSURE_16_BYTE_ALIGNMENT: u32 = 1 << 0;

fn audio_packet(sample: &CMSampleBuffer, shared: &Shared) -> Result<Option<AudioPacket>> {
    let Some(fd) = (unsafe { sample.format_description() }) else { return Ok(None) };
    let asbd_ptr: *const AudioStreamBasicDescription = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&fd) };
    let Some(asbd) = (unsafe { asbd_ptr.as_ref() }) else { return Ok(None) };
    if asbd.mFormatID != K_AUDIO_FORMAT_LINEAR_PCM
        || asbd.mFormatFlags & K_AUDIO_FORMAT_FLAG_IS_FLOAT == 0
        || asbd.mBitsPerChannel != 32
    {
        return Err(CaptureError::Unsupported("system audio is not 32-bit float linear PCM"));
    }
    let channels = asbd.mChannelsPerFrame.max(1) as usize;
    let non_interleaved = asbd.mFormatFlags & K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED != 0;

    let mut needed: usize = 0;
    unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            &mut needed,
            ptr::null_mut(),
            0,
            None,
            None,
            0,
            ptr::null_mut(),
        )
    };
    if needed == 0 {
        return Ok(None);
    }
    let mut storage = vec![0u64; needed.div_ceil(8)];
    let abl = storage.as_mut_ptr() as *mut AudioBufferList;
    let mut block: *mut CMBlockBuffer = ptr::null_mut();
    let rc = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            ptr::null_mut(),
            abl,
            needed,
            None,
            None,
            K_CM_SAMPLE_BUFFER_FLAG_ASSURE_16_BYTE_ALIGNMENT,
            &mut block,
        )
    };
    if rc != 0 {
        return Err(CaptureError::Os(format!("CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer: {rc}")));
    }
    // Keep the block buffer alive while reading, release afterwards.
    let _block = NonNull::new(block).map(|b| unsafe { CFRetained::from_raw(b) });
    let n_buffers = unsafe { (*abl).mNumberBuffers } as usize;
    let buffers = unsafe { std::slice::from_raw_parts((*abl).mBuffers.as_ptr(), n_buffers) };
    let as_f32 = |i: usize| -> &[f32] {
        let b = &buffers[i];
        if b.mData.is_null() {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4) }
    };
    let samples: Vec<f32> = if non_interleaved && n_buffers > 1 {
        let frames = (0..n_buffers).map(|i| as_f32(i).len()).min().unwrap_or(0);
        let mut out = Vec::with_capacity(frames * n_buffers);
        for f in 0..frames {
            for c in 0..n_buffers {
                out.push(as_f32(c)[f]);
            }
        }
        out
    } else if n_buffers >= 1 {
        as_f32(0).to_vec()
    } else {
        return Ok(None);
    };
    let channels = if non_interleaved { n_buffers.max(1) } else { channels };
    let mut data = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        data.extend_from_slice(&s.to_le_bytes());
    }
    Ok(Some(AudioPacket {
        seq: shared.audio_seq.fetch_add(1, Ordering::Relaxed),
        pts_us: shared.rel_pts(unsafe { sample.presentation_time_stamp() }),
        codec: AudioCodec::PcmF32Le,
        sample_rate: asbd.mSampleRate as u32,
        channels: channels as u8,
        data,
    }))
}

pub(crate) struct OutputIvars {
    shared: Arc<Shared>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; StreamOutput does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "AetherLinkSCStreamOutput"]
    #[ivars = OutputIvars]
    pub(crate) struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            let shared = &self.ivars().shared;
            if shared.stopping.load(Ordering::Relaxed) {
                return;
            }
            // Never unwind into Objective-C.
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                if kind == SCStreamOutputType::Screen {
                    shared.handle_screen(sample);
                } else if kind == SCStreamOutputType::Audio {
                    shared.handle_audio(sample);
                }
            }));
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            let shared = &self.ivars().shared;
            if shared.stopping.load(Ordering::Relaxed) {
                return;
            }
            let msg = error.localizedDescription().to_string();
            let code = error.code();
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                shared.deliver(CaptureEvent::Error(map_sck_error(format!("stream stopped: {msg}"), code)));
            }));
        }
    }
);

impl StreamOutput {
    pub(crate) fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { shared });
        unsafe { msg_send![super(this), init] }
    }
}

// ---------------------------------------------------------------------------------------------
// Capturer
// ---------------------------------------------------------------------------------------------

struct Running {
    stream: Retained<SCStream>,
    output: Retained<StreamOutput>,
    _video_queue: DispatchRetained<DispatchQueue>,
    _audio_queue: Option<DispatchRetained<DispatchQueue>>,
    audio: bool,
    shared: Arc<Shared>,
}

/// ScreenCaptureKit-based [`ScreenCapturer`].
#[derive(Default)]
pub struct SckCapturer {
    running: Option<Running>,
}

// SAFETY: SCStream and our output object are thread-safe Objective-C objects (SCStream's API is
// documented as callable from any thread; the output's ivars are Sync). The dispatch queues are
// thread-safe by design.
unsafe impl Send for SckCapturer {}

impl SckCapturer {
    pub fn new() -> Self {
        Self::default()
    }
}

fn even(v: f64) -> usize {
    let v = v.round().max(2.0) as usize;
    v & !1
}

pub(crate) fn wait_completion(what: &'static str, f: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Option<(String, isize)>>();
    let block = RcBlock::new(move |err: *mut NSError| {
        let _ = tx.send(if err.is_null() { None } else { Some(ns_error_message(err)) });
    });
    f(&block);
    match rx.recv_timeout(ASYNC_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some((msg, code))) => Err(map_sck_error(msg, code)),
        Err(_) => Err(CaptureError::Timeout(what)),
    }
}

impl ScreenCapturer for SckCapturer {
    fn start(&mut self, target: CaptureTarget, cfg: CaptureConfig, sink: CaptureSink) -> Result<()> {
        if self.running.is_some() {
            return Err(CaptureError::AlreadyRunning);
        }
        if cfg.fps == 0 {
            return Err(CaptureError::InvalidFrame("fps must be > 0".into()));
        }
        let content = shareable_content()?;
        let displays = unsafe { content.displays() }.to_vec();

        let (filter, display_id): (Retained<SCContentFilter>, u32) = match target {
            CaptureTarget::Display { display_id } => {
                let display = displays
                    .iter()
                    .find(|d| unsafe { d.displayID() } == display_id)
                    .ok_or_else(|| CaptureError::TargetNotFound(format!("display {display_id}")))?;
                let excluded: Vec<Retained<SCRunningApplication>> = unsafe { content.applications() }
                    .iter()
                    .filter(|a| cfg.excluded_pids.contains(&(unsafe { a.processID() } as u32)))
                    .collect();
                let apps = NSArray::from_retained_slice(&excluded);
                let none: Retained<NSArray<SCWindow>> = NSArray::new();
                let filter = unsafe {
                    SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                        SCContentFilter::alloc(),
                        display,
                        &apps,
                        &none,
                    )
                };
                (filter, display_id)
            }
            CaptureTarget::Window { window_id } => {
                let window = unsafe { content.windows() }
                    .iter()
                    .find(|w| unsafe { w.windowID() } as u64 == window_id)
                    .ok_or_else(|| CaptureError::TargetNotFound(format!("window {window_id}")))?;
                let frame = unsafe { window.frame() };
                let display_id = display_for_rect(&displays, frame)
                    .map(|d| unsafe { d.displayID() })
                    .unwrap_or_else(|| CGMainDisplayID());
                let filter =
                    unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window) };
                (filter, display_id)
            }
        };

        let content_rect = unsafe { filter.contentRect() };
        let px_scale = unsafe { filter.pointPixelScale() } as f64;
        let user_scale = (cfg.scale as f64).clamp(0.1, 2.0);
        let width = even(content_rect.size.width * px_scale * user_scale);
        let height = even(content_rect.size.height * px_scale * user_scale);

        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(width);
            config.setHeight(height);
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: cfg.fps as i32,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });
            config.setShowsCursor(cfg.show_cursor);
            config.setQueueDepth(3);
            config.setColorSpaceName(kCGColorSpaceSRGB);
            config.setCapturesAudio(cfg.capture_audio);
            if cfg.capture_audio {
                config.setSampleRate(48_000);
                config.setChannelCount(2);
                config.setExcludesCurrentProcessAudio(true);
            }
        }

        let shared = Arc::new(Shared {
            sink: Mutex::new(Some(sink)),
            display_id,
            base_pts_us: Mutex::new(None),
            audio_seq: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
        });
        let output = StreamOutput::new(shared.clone());
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            )
        };
        let video_queue = DispatchQueue::new("dev.aetherlink.capture.video", None);
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&video_queue),
            )
        }
        .map_err(|e| CaptureError::Os(format!("addStreamOutput(screen): {}", e.localizedDescription())))?;
        let audio_queue = if cfg.capture_audio {
            let q = DispatchQueue::new("dev.aetherlink.capture.audio", None);
            unsafe {
                stream.addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Audio,
                    Some(&q),
                )
            }
            .map_err(|e| CaptureError::Os(format!("addStreamOutput(audio): {}", e.localizedDescription())))?;
            Some(q)
        } else {
            None
        };

        wait_completion("SCStream start", |b| unsafe { stream.startCaptureWithCompletionHandler(Some(b)) })?;
        tracing::debug!(display_id, width, height, fps = cfg.fps, "ScreenCaptureKit stream started");
        self.running = Some(Running {
            stream,
            output,
            _video_queue: video_queue,
            _audio_queue: audio_queue,
            audio: cfg.capture_audio,
            shared,
        });
        Ok(())
    }

    fn stop(&mut self) {
        let Some(r) = self.running.take() else { return };
        r.shared.stopping.store(true, Ordering::SeqCst);
        if let Err(e) = wait_completion("SCStream stop", |b| unsafe { r.stream.stopCaptureWithCompletionHandler(Some(b)) }) {
            tracing::warn!("stopping ScreenCaptureKit stream: {e}");
        }
        unsafe {
            let _ = r.stream.removeStreamOutput_type_error(ProtocolObject::from_ref(&*r.output), SCStreamOutputType::Screen);
            if r.audio {
                let _ =
                    r.stream.removeStreamOutput_type_error(ProtocolObject::from_ref(&*r.output), SCStreamOutputType::Audio);
            }
        }
        // Fence: after the sink is taken under its lock, no callback can deliver anymore.
        let sink = r.shared.sink.lock().map(|mut g| g.take()).unwrap_or(None);
        drop(sink);
    }
}

impl Drop for SckCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_rect_bounds() {
        let r = CGRect::new(objc2_core_foundation::CGPoint::new(-3.5, 10.2), objc2_core_foundation::CGSize::new(20.0, 5.0));
        assert_eq!(clamp_rect(r, 10, 100), Some(Rect::new(0, 10, 10, 6)));
        let out = CGRect::new(objc2_core_foundation::CGPoint::new(50.0, 50.0), objc2_core_foundation::CGSize::new(5.0, 5.0));
        assert_eq!(clamp_rect(out, 10, 10), None);
    }

    #[test]
    fn permission_query_does_not_prompt() {
        // CGPreflightScreenCaptureAccess never shows UI; just make sure it links and returns.
        let _ = permission_granted();
    }

    #[test]
    #[ignore = "needs Screen Recording permission (would trigger a TCC prompt)"]
    fn enumerate_displays_and_windows() {
        let d = list_displays().unwrap();
        assert!(!d.is_empty());
        assert!(d.iter().any(|d| d.primary));
        for di in &d {
            assert!(di.scale >= 1.0 && di.bounds.width > 0);
        }
        let _ = list_windows().unwrap();
    }

    #[test]
    #[ignore = "needs Screen Recording permission (would trigger a TCC prompt)"]
    fn capture_main_display_frames() {
        let main = CGMainDisplayID();
        let frames = Arc::new(Mutex::new(Vec::<CapturedFrame>::new()));
        let f2 = frames.clone();
        let mut cap = SckCapturer::new();
        cap.start(
            CaptureTarget::Display { display_id: main },
            CaptureConfig { fps: 30, capture_audio: true, ..Default::default() },
            Box::new(move |ev| {
                if let CaptureEvent::Frame(f) = ev {
                    let mut v = f2.lock().unwrap();
                    if v.len() < 3 {
                        v.push(f);
                    }
                }
            }),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        cap.stop();
        let frames = frames.lock().unwrap();
        assert!(!frames.is_empty(), "no frame delivered");
        let img = frames[0].to_bgra().unwrap();
        assert_eq!((img.width, img.height), (frames[0].width, frames[0].height));
    }
}
