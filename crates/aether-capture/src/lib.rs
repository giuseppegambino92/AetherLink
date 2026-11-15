//! # aether-capture
//!
//! Screen (and optional system-audio) capture for the AetherLink host.
//!
//! * **macOS** — ScreenCaptureKit (`SCShareableContent` / `SCStream`), BGRA output delivered
//!   zero-copy as [`FramePixels::PixelBuffer`] (a retained `CVPixelBuffer` backed by an
//!   `IOSurface`), dirty rects from `SCStreamFrameInfo`, idle frames skipped, optional system
//!   audio as interleaved `f32` PCM.
//! * **Linux** — xdg-desktop-portal ScreenCast + PipeWire (Wayland and portal-enabled X11),
//!   or X11 MIT-SHM with RandR monitors / EWMH windows / XFixes cursor; system audio from the
//!   default sink's monitor via PipeWire or PulseAudio (module `linux`).
//! * **Windows** — DXGI Desktop Duplication (dirty/move rects, composited cursor, recovery
//!   from `ACCESS_LOST` / secure-desktop switches) with Windows.Graphics.Capture for single
//!   windows and as fallback, CPU BGRA frames, WASAPI loopback system audio (module `windows`).
//! * [`TestPatternCapturer`] — a deterministic animated BGRA source (moving block, static
//!   text-like region) used by tests and benchmarks on every OS. It is a synthetic *source*,
//!   not a mock of an OS API.
//!
//! Pixel format everywhere is BGRA8 (premultiplied, as delivered by the OS compositor).

use std::sync::Arc;

pub use aether_protocol::{AudioCodec, AudioPacket, CaptureTarget, DisplayInfo, Rect, WindowInfo};

pub mod audio;
mod test_pattern;
pub use test_pattern::{TestPattern, TestPatternCapturer};
pub use audio::{AudioCapturer, AudioFormat, AudioSink, SineAudioCapturer, create_audio_capturer};

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(any(target_os = "windows", test))]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod windows_pure;
#[cfg(target_os = "linux")]
pub mod linux;

pub mod platform_notes;

/// Errors produced by this crate.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CaptureError {
    /// The platform or the requested feature is not supported on this OS / build.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// Screen Recording permission (TCC) is missing.
    #[error("screen recording permission not granted")]
    PermissionDenied,
    /// The requested display or window does not exist (anymore).
    #[error("capture target not found: {0}")]
    TargetNotFound(String),
    /// `start` called while a capture is already running.
    #[error("capture already running")]
    AlreadyRunning,
    /// A frame could not be accessed / converted.
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
    /// An OS API reported an error.
    #[error("os error: {0}")]
    Os(String),
    /// An asynchronous OS call did not complete in time.
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
}

pub type Result<T> = std::result::Result<T, CaptureError>;

/// A tightly or loosely packed BGRA8 image owned by Rust.
#[derive(Clone, PartialEq, Eq)]
pub struct BgraImage {
    pub width: u32,
    pub height: u32,
    /// Bytes per row (≥ `width * 4`).
    pub stride: usize,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for BgraImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BgraImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl BgraImage {
    /// A zero-filled (transparent black) tightly packed image.
    pub fn new(width: u32, height: u32) -> Self {
        let stride = width as usize * 4;
        Self { width, height, stride, data: vec![0; stride * height as usize] }
    }

    /// Wraps existing BGRA bytes, validating the size.
    pub fn from_raw(width: u32, height: u32, stride: usize, data: Vec<u8>) -> Result<Self> {
        if stride < width as usize * 4 {
            return Err(CaptureError::InvalidFrame(format!("stride {stride} < width*4 ({})", width * 4)));
        }
        let needed = if height == 0 { 0 } else { stride * (height as usize - 1) + width as usize * 4 };
        if data.len() < needed {
            return Err(CaptureError::InvalidFrame(format!("buffer {} < required {needed}", data.len())));
        }
        Ok(Self { width, height, stride, data })
    }

    /// Row `y` (exactly `width * 4` bytes).
    #[inline]
    pub fn row(&self, y: u32) -> &[u8] {
        let start = y as usize * self.stride;
        &self.data[start..start + self.width as usize * 4]
    }

    /// Mutable row `y` (exactly `width * 4` bytes).
    #[inline]
    pub fn row_mut(&mut self, y: u32) -> &mut [u8] {
        let start = y as usize * self.stride;
        let w = self.width as usize * 4;
        &mut self.data[start..start + w]
    }

    /// BGRA pixel at (x, y).
    #[inline]
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let o = y as usize * self.stride + x as usize * 4;
        [self.data[o], self.data[o + 1], self.data[o + 2], self.data[o + 3]]
    }

    /// Returns a tightly packed copy (stride == width * 4).
    pub fn to_packed(&self) -> BgraImage {
        if self.stride == self.width as usize * 4 && self.data.len() == self.stride * self.height as usize {
            return self.clone();
        }
        let mut out = BgraImage::new(self.width, self.height);
        for y in 0..self.height {
            out.row_mut(y).copy_from_slice(self.row(y));
        }
        out
    }

    /// Copies `src` rows (with its own stride) into a new packed image.
    pub fn copy_from_rows(width: u32, height: u32, src: &[u8], src_stride: usize) -> Result<Self> {
        let row_bytes = width as usize * 4;
        if src_stride < row_bytes || (height > 0 && src.len() < src_stride * (height as usize - 1) + row_bytes) {
            return Err(CaptureError::InvalidFrame("source buffer too small".into()));
        }
        let mut out = BgraImage::new(width, height);
        for y in 0..height as usize {
            out.data[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(&src[y * src_stride..y * src_stride + row_bytes]);
        }
        Ok(out)
    }
}

/// Pixel storage of a captured frame.
#[derive(Clone, Debug)]
pub enum FramePixels {
    /// CPU-side BGRA8 rows.
    Bgra { stride: usize, data: Arc<[u8]> },
    /// Zero-copy OS buffer (retained `CVPixelBuffer` / `IOSurface`, BGRA).
    ///
    /// Note: ScreenCaptureKit recycles a small pool of surfaces (`queueDepth`); holding many of
    /// these frames for long stalls the stream. Convert with [`CapturedFrame::to_bgra`] if a
    /// frame must be kept around.
    #[cfg(target_os = "macos")]
    PixelBuffer(macos::PixelBuffer),
}

/// One captured video frame.
#[derive(Clone, Debug)]
pub struct CapturedFrame {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    /// Presentation timestamp in microseconds since the start of the capture.
    pub pts_us: u64,
    /// Regions that changed since the previous delivered frame (physical pixels of this frame),
    /// when the OS reports them. `None` means "unknown — assume everything changed".
    pub dirty_rects: Option<Vec<Rect>>,
    pub pixels: FramePixels,
}

impl CapturedFrame {
    /// Builds a frame from an owned BGRA image.
    pub fn from_image(display_id: u32, pts_us: u64, img: BgraImage, dirty_rects: Option<Vec<Rect>>) -> Self {
        Self {
            display_id,
            width: img.width,
            height: img.height,
            pts_us,
            dirty_rects,
            pixels: FramePixels::Bgra { stride: img.stride, data: Arc::from(img.data) },
        }
    }

    /// Returns a packed CPU copy of the pixels (locks + copies native buffers).
    pub fn to_bgra(&self) -> Result<BgraImage> {
        match &self.pixels {
            FramePixels::Bgra { stride, data } => BgraImage::copy_from_rows(self.width, self.height, data, *stride),
            #[cfg(target_os = "macos")]
            FramePixels::PixelBuffer(pb) => pb.to_bgra(),
        }
    }
}

/// Capture parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureConfig {
    /// Maximum frame rate.
    pub fps: u16,
    pub show_cursor: bool,
    /// Output scale relative to the native pixel size (1.0 = native, 0.5 = half).
    pub scale: f32,
    /// Capture system audio (macOS 13+).
    pub capture_audio: bool,
    /// Processes whose windows must not appear in the capture (e.g. our Ghost Mode curtain).
    pub excluded_pids: Vec<u32>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self { fps: 60, show_cursor: true, scale: 1.0, capture_audio: false, excluded_pids: Vec::new() }
    }
}

/// Events delivered to the capture sink (on an OS-owned thread).
#[derive(Debug)]
pub enum CaptureEvent {
    Frame(CapturedFrame),
    Audio(AudioPacket),
    Error(CaptureError),
}

/// Sink receiving capture events.
pub type CaptureSink = Box<dyn FnMut(CaptureEvent) + Send>;

/// A screen capture backend.
pub trait ScreenCapturer: Send {
    /// Starts capturing `target`; events are delivered to `sink` until [`stop`](Self::stop).
    fn start(&mut self, target: CaptureTarget, cfg: CaptureConfig, sink: CaptureSink) -> Result<()>;
    /// Stops the capture (idempotent). After return no further events are delivered.
    /// Must not be called from inside the sink (the sink runs on the capture thread).
    fn stop(&mut self);
}

/// Creates the native capturer for this OS.
pub fn create_capturer() -> Result<Box<dyn ScreenCapturer>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::SckCapturer::new()))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WinCapturer::new()))
    }
    #[cfg(target_os = "linux")]
    {
        linux::create_capturer()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Err(CaptureError::Unsupported(platform_notes::UNSUPPORTED_MSG))
    }
}

/// Lists capturable displays.
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    #[cfg(target_os = "macos")]
    {
        macos::list_displays()
    }
    #[cfg(target_os = "windows")]
    {
        windows::list_displays()
    }
    #[cfg(target_os = "linux")]
    {
        linux::list_displays()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Err(CaptureError::Unsupported(platform_notes::UNSUPPORTED_MSG))
    }
}

/// Lists capturable, on-screen application windows.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    #[cfg(target_os = "macos")]
    {
        macos::list_windows()
    }
    #[cfg(target_os = "windows")]
    {
        windows::list_windows()
    }
    #[cfg(target_os = "linux")]
    {
        linux::list_windows()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Err(CaptureError::Unsupported(platform_notes::UNSUPPORTED_MSG))
    }
}

/// True when the process may capture the screen without prompting.
///
/// macOS: `CGPreflightScreenCaptureAccess`. Other OSes: no permission model applies to the
/// (unimplemented) backends, so this returns `true`.
pub fn permission_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::permission_granted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Requests screen capture permission (may show the system prompt). Returns the current grant.
pub fn request_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::request_permission()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_image_strided_copy() {
        let w = 3u32;
        let h = 2u32;
        let stride = 16usize;
        let mut src = vec![0xEEu8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..(w as usize * 4) {
                src[y * stride + x] = (y * 100 + x) as u8;
            }
        }
        let img = BgraImage::copy_from_rows(w, h, &src, stride).unwrap();
        assert_eq!(img.stride, 12);
        assert_eq!(img.row(1)[0], 100);
        assert_eq!(img.pixel(2, 1), [108, 109, 110, 111]);
        let strided = BgraImage::from_raw(w, h, stride, src).unwrap();
        assert_eq!(strided.to_packed(), img);
        assert!(BgraImage::from_raw(4, 2, 8, vec![0; 64]).is_err());
    }

    #[test]
    fn captured_frame_to_bgra_roundtrip() {
        let img = TestPattern::new(130, 70).frame(3);
        let f = CapturedFrame::from_image(1, 5, img.clone(), None);
        assert_eq!(f.to_bgra().unwrap(), img);
    }
}
