//! C-ABI bridge to the Swift side of the iOS app (`gen/apple/Sources/aetherlink-mobile/`).
//!
//! Flow: ReplayKit broadcast extension (`AetherBroadcast`, VideoToolbox HEVC/H.264) →
//! App Group Unix-domain socket → `BroadcastReceiver.swift` in the main app →
//! `aether_ios_host_*` below → [`MobileHost`] (registered `PlatformHost`).
//!
//! Swift implements the `aether_swift_*` callbacks (`@_cdecl`); they are resolved when Xcode
//! links `libapp.a` into the app binary.

use aether_core::mobile::{MobileEncoderControl, MobileHost};
use aether_protocol::VideoCodec;
use std::ffi::{CStr, c_char};
use std::sync::{Arc, OnceLock};

unsafe extern "C" {
    fn aether_swift_bridge_start();
    fn aether_swift_request_keyframe();
    fn aether_swift_viewers_changed(viewers: u32);
    fn aether_swift_show_broadcast_picker();
    fn aether_swift_stop_broadcast();
    fn aether_swift_scan_qr();
}

static HOST: OnceLock<MobileHost> = OnceLock::new();
static SCAN: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<Result<String, String>>>> = parking_lot::Mutex::new(None);

pub fn host() -> Option<MobileHost> {
    HOST.get().cloned()
}

struct SwiftControl;

impl MobileEncoderControl for SwiftControl {
    fn request_keyframe(&self) {
        // SAFETY: Swift callback without arguments; it only enqueues a socket message.
        unsafe { aether_swift_request_keyframe() }
    }

    fn viewers_changed(&self, viewers: usize) {
        // SAFETY: plain integer argument.
        unsafe { aether_swift_viewers_changed(viewers.min(u32::MAX as usize) as u32) }
    }
}

pub fn install(_app: &tauri::AppHandle) {
    HOST.get_or_init(|| MobileHost::install(Arc::new(SwiftControl), "iPhone"));
    // SAFETY: starts the Swift socket listener (idempotent on the Swift side).
    unsafe { aether_swift_bridge_start() }
}

pub fn show_broadcast_picker() {
    // SAFETY: Swift hops to the main queue itself.
    unsafe { aether_swift_show_broadcast_picker() }
}

pub fn stop_broadcast() {
    // SAFETY: Swift sends a stop message to the extension.
    unsafe { aether_swift_stop_broadcast() }
}

/// Present the native QR scanner; resolves with the payload (empty when cancelled) or an
/// `errors.*` key (`errors.cameraDenied`).
pub async fn scan_qr() -> Result<String, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    // A newer scan replaces (and thereby cancels) a pending one.
    *SCAN.lock() = Some(tx);
    // SAFETY: Swift hops to the main queue itself.
    unsafe { aether_swift_scan_qr() }
    rx.await.unwrap_or_else(|_| Ok(String::new()))
}

// ---------------------------------------------------------------- Swift → Rust

/// Result of `aether_swift_scan_qr`: `content` (may be empty = cancelled) or `error` (i18n key).
///
/// # Safety
/// Both pointers must be null or valid NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_ios_qr_result(content: *const c_char, error: *const c_char) {
    let read = |p: *const c_char| (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned());
    let result = match (read(content), read(error)) {
        (_, Some(e)) => Err(e),
        (c, None) => Ok(c.unwrap_or_default()),
    };
    if let Some(tx) = SCAN.lock().take() {
        let _ = tx.send(result);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_ios_host_display(width: u32, height: u32, scale: f32, refresh_hz: f32) {
    if let Some(h) = HOST.get() {
        h.set_display(width, height, scale, refresh_hz);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_ios_host_config(hevc: bool, width: u32, height: u32, fps: u16) {
    if let Some(h) = HOST.get() {
        let codec = if hevc { VideoCodec::Hevc } else { VideoCodec::H264 };
        h.push_config(codec, width.max(1), height.max(1), fps.max(1));
        tracing::info!(?codec, width, height, fps, "broadcast encoder configured");
    }
}

/// # Safety
/// `ptr` must point to `len` readable bytes (or be null with `len == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_ios_host_codec_config(ptr: *const u8, len: usize) {
    if let (Some(h), Some(data)) = (HOST.get(), unsafe { slice(ptr, len) }) {
        h.push_codec_config(data);
    }
}

/// # Safety
/// `ptr` must point to `len` readable bytes of one Annex-B access unit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_ios_host_frame(ptr: *const u8, len: usize, pts_us: u64, keyframe: bool) {
    if let (Some(h), Some(data)) = (HOST.get(), unsafe { slice(ptr, len) }) {
        h.push_frame(data, pts_us, keyframe);
    }
}

/// # Safety
/// `ptr` must point to `count` interleaved `f32` samples.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_ios_host_audio(ptr: *const f32, count: usize, sample_rate: u32, channels: u8, pts_us: u64) {
    if ptr.is_null() || count == 0 {
        return;
    }
    // SAFETY: guaranteed by the caller.
    let samples = unsafe { std::slice::from_raw_parts(ptr, count) };
    aether_capture::audio::mobile::push_pcm(samples, sample_rate, channels, pts_us);
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_ios_host_stopped() {
    if let Some(h) = HOST.get() {
        h.push_stopped();
    }
    aether_capture::audio::mobile::platform_stopped();
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_ios_audio_wanted() -> bool {
    aether_capture::audio::mobile::active_capturers() > 0
}

/// # Safety
/// `name` must be a valid NUL-terminated UTF-8 string (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_ios_set_default_device_name(name: *const c_char) {
    if name.is_null() {
        return;
    }
    // SAFETY: guaranteed by the caller.
    let s = unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned();
    crate::mobile::apply_default_device_name(s);
}

unsafe fn slice<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if ptr.is_null() || len == 0 {
        return None;
    }
    // SAFETY: guaranteed by the exported functions' callers.
    Some(unsafe { std::slice::from_raw_parts(ptr, len) })
}
