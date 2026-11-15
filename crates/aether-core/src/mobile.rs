//! Mobile host glue: a [`PlatformHost`] fed by the platform encoder.
//!
//! The Android app (MediaProjection → MediaCodec, via JNI) and the iOS app (ReplayKit
//! broadcast extension → VideoToolbox, via the App Group socket and a C bridge) push
//! *already encoded* Annex-B access units into a [`MobileHost`]. The host fans them out
//! to every viewer session as `VideoMessage::Config` + `VideoMessage::Frame`.
//!
//! * Codec-config buffers (SPS/PPS, + VPS for HEVC) are cached and prepended to every
//!   keyframe that does not already carry them, so each IDR is self-contained (what the
//!   client's WebCodecs decoder and the DVR need).
//! * A viewer that falls behind (full channel) drops frames until the next keyframe and
//!   asks the platform encoder for one.
//! * Platform-independent: no JNI / Objective-C here, the app crate owns the bridges.

use crate::external::{ExternalStream, PlatformHost};
use aether_protocol::{ChromaFormat, DisplayInfo, Rect, VideoCodec, VideoFrame, VideoMessage};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc;

/// Calls back into the platform encoder (implemented by the app's JNI / Swift bridge).
pub trait MobileEncoderControl: Send + Sync + 'static {
    /// Ask the encoder for an IDR frame as soon as possible.
    fn request_keyframe(&self);
    /// Number of active viewers changed (the platform may pause encoding at 0).
    fn viewers_changed(&self, _viewers: usize) {}
}

/// Current stream parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u16,
}

struct Viewer {
    id: u64,
    display_id: u32,
    tx: mpsc::Sender<VideoMessage>,
    /// Drop frames until the next keyframe (new viewer or overflow).
    waiting_for_key: bool,
    seq: u64,
}

struct Inner {
    control: Arc<dyn MobileEncoderControl>,
    display: Mutex<DisplayInfo>,
    format: Mutex<Option<StreamFormat>>,
    codec_config: Mutex<Vec<u8>>,
    viewers: Mutex<Vec<Viewer>>,
    sharing: AtomicBool,
    next_viewer: AtomicU64,
    frames_in: AtomicU64,
    bytes_in: AtomicU64,
}

/// Snapshot for the UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MobileHostStatus {
    pub sharing: bool,
    pub viewers: usize,
    pub codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u16,
    pub frames: u64,
    pub bytes: u64,
}

/// The registered platform host. Cheap to clone.
#[derive(Clone)]
pub struct MobileHost {
    inner: Arc<Inner>,
}

const DISPLAY_ID: u32 = 1;

impl MobileHost {
    /// Create the host and register it with [`crate::external::set_platform_host`].
    pub fn install(control: Arc<dyn MobileEncoderControl>, name: &str) -> Self {
        let host = Self::new(control, name);
        crate::external::set_platform_host(Some(Arc::new(host.clone())));
        host
    }

    /// Create an unregistered host.
    pub fn new(control: Arc<dyn MobileEncoderControl>, name: &str) -> Self {
        Self {
            inner: Arc::new(Inner {
                control,
                display: Mutex::new(DisplayInfo {
                    id: DISPLAY_ID,
                    name: name.to_owned(),
                    bounds: Rect::new(0, 0, 1080, 1920),
                    scale: 1.0,
                    refresh_hz: 60.0,
                    primary: true,
                    is_virtual: false,
                }),
                format: Mutex::new(None),
                codec_config: Mutex::new(Vec::new()),
                viewers: Mutex::new(Vec::new()),
                sharing: AtomicBool::new(false),
                next_viewer: AtomicU64::new(1),
                frames_in: AtomicU64::new(0),
                bytes_in: AtomicU64::new(0),
            }),
        }
    }

    /// Physical size of the shared screen (also the input coordinate space on Android).
    pub fn set_display(&self, width: u32, height: u32, scale: f32, refresh_hz: f32) {
        let mut d = self.inner.display.lock();
        d.bounds = Rect::new(0, 0, width.max(1), height.max(1));
        d.scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
        d.refresh_hz = refresh_hz;
    }

    /// The platform encoder (re)started with these parameters.
    pub fn push_config(&self, codec: VideoCodec, width: u32, height: u32, fps: u16) {
        let fmt = StreamFormat { codec, width, height, fps };
        *self.inner.format.lock() = Some(fmt);
        self.inner.codec_config.lock().clear();
        self.inner.sharing.store(true, Ordering::Relaxed);
        let mut viewers = self.inner.viewers.lock();
        viewers.retain_mut(|v| {
            v.waiting_for_key = true;
            send_config(v, fmt)
        });
        drop(viewers);
        self.inner.control.request_keyframe();
    }

    /// Codec-specific data (Annex-B SPS/PPS[/VPS]), e.g. MediaCodec's `BUFFER_FLAG_CODEC_CONFIG`.
    pub fn push_codec_config(&self, annexb: &[u8]) {
        *self.inner.codec_config.lock() = annexb.to_vec();
    }

    /// One encoded access unit in Annex-B format.
    pub fn push_frame(&self, annexb: &[u8], pts_us: u64, keyframe: bool) {
        self.inner.frames_in.fetch_add(1, Ordering::Relaxed);
        self.inner.bytes_in.fetch_add(annexb.len() as u64, Ordering::Relaxed);
        let Some(fmt) = *self.inner.format.lock() else { return };
        let mut viewers = self.inner.viewers.lock();
        if viewers.is_empty() {
            return;
        }
        let data = if keyframe { self.with_parameter_sets(annexb, fmt.codec) } else { annexb.to_vec() };
        let mut need_key = false;
        let before = viewers.len();
        viewers.retain_mut(|v| {
            if v.waiting_for_key && !keyframe {
                need_key = true;
                return true;
            }
            v.seq += 1;
            let frame = VideoFrame {
                display_id: v.display_id,
                seq: v.seq,
                pts_us,
                codec: fmt.codec,
                chroma: ChromaFormat::Yuv420,
                keyframe,
                width: fmt.width,
                height: fmt.height,
                data: data.clone(),
            };
            match v.tx.try_send(VideoMessage::Frame(frame)) {
                Ok(()) => {
                    v.waiting_for_key = false;
                    true
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    v.waiting_for_key = true;
                    need_key = true;
                    true
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
        let after = viewers.len();
        drop(viewers);
        if after != before {
            self.inner.control.viewers_changed(after);
        }
        if need_key {
            self.inner.control.request_keyframe();
        }
    }

    /// The platform stopped sharing (projection revoked / broadcast finished).
    pub fn push_stopped(&self) {
        self.inner.sharing.store(false, Ordering::Relaxed);
        *self.inner.format.lock() = None;
        self.inner.codec_config.lock().clear();
    }

    pub fn is_sharing(&self) -> bool {
        self.inner.sharing.load(Ordering::Relaxed)
    }

    pub fn status(&self) -> MobileHostStatus {
        let fmt = *self.inner.format.lock();
        MobileHostStatus {
            sharing: self.is_sharing(),
            viewers: self.inner.viewers.lock().len(),
            codec: fmt.map(|f| format!("{:?}", f.codec)),
            width: fmt.map(|f| f.width).unwrap_or(0),
            height: fmt.map(|f| f.height).unwrap_or(0),
            fps: fmt.map(|f| f.fps).unwrap_or(0),
            frames: self.inner.frames_in.load(Ordering::Relaxed),
            bytes: self.inner.bytes_in.load(Ordering::Relaxed),
        }
    }

    fn with_parameter_sets(&self, annexb: &[u8], codec: VideoCodec) -> Vec<u8> {
        let cfg = self.inner.codec_config.lock();
        if cfg.is_empty() || has_parameter_sets(annexb, codec) {
            return annexb.to_vec();
        }
        let mut v = Vec::with_capacity(cfg.len() + annexb.len());
        v.extend_from_slice(&cfg);
        v.extend_from_slice(annexb);
        v
    }
}

fn send_config(v: &mut Viewer, f: StreamFormat) -> bool {
    let msg = VideoMessage::Config {
        display_id: v.display_id,
        codec: f.codec,
        chroma: ChromaFormat::Yuv420,
        width: f.width,
        height: f.height,
        fps: f.fps,
    };
    !matches!(v.tx.try_send(msg), Err(mpsc::error::TrySendError::Closed(_)))
}

/// NAL unit types present in an Annex-B buffer.
fn nal_types(annexb: &[u8], codec: VideoCodec) -> impl Iterator<Item = u8> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 < annexb.len() {
            let sc = if annexb[i..].starts_with(&[0, 0, 1]) {
                3
            } else if annexb[i..].starts_with(&[0, 0, 0, 1]) {
                4
            } else {
                i += 1;
                continue;
            };
            i += sc;
            if let Some(&h) = annexb.get(i) {
                return Some(match codec {
                    VideoCodec::Hevc => (h >> 1) & 0x3F,
                    _ => h & 0x1F,
                });
            }
        }
        None
    })
}

fn has_parameter_sets(annexb: &[u8], codec: VideoCodec) -> bool {
    match codec {
        // HEVC: VPS 32, SPS 33, PPS 34.
        VideoCodec::Hevc => nal_types(annexb, codec).any(|t| t == 33),
        // H.264: SPS 7.
        _ => nal_types(annexb, codec).any(|t| t == 7),
    }
}

impl PlatformHost for MobileHost {
    fn displays(&self) -> Vec<DisplayInfo> {
        vec![self.inner.display.lock().clone()]
    }

    fn start_video(&self, display_id: u32, out: mpsc::Sender<VideoMessage>) -> crate::Result<Box<dyn ExternalStream>> {
        let id = self.inner.next_viewer.fetch_add(1, Ordering::Relaxed);
        let mut v = Viewer { id, display_id, tx: out, waiting_for_key: true, seq: 0 };
        if let Some(fmt) = *self.inner.format.lock() {
            send_config(&mut v, fmt);
        }
        let n = {
            let mut viewers = self.inner.viewers.lock();
            viewers.push(v);
            viewers.len()
        };
        self.inner.control.viewers_changed(n);
        self.inner.control.request_keyframe();
        Ok(Box::new(ViewerStream { host: self.clone(), id, stopped: false }))
    }
}

struct ViewerStream {
    host: MobileHost,
    id: u64,
    stopped: bool,
}

impl ExternalStream for ViewerStream {
    fn request_keyframe(&self) {
        if let Some(v) = self.host.inner.viewers.lock().iter_mut().find(|v| v.id == self.id) {
            v.waiting_for_key = true;
        }
        self.host.inner.control.request_keyframe();
    }

    fn stop(&mut self) {
        if std::mem::replace(&mut self.stopped, true) {
            return;
        }
        let n = {
            let mut viewers = self.host.inner.viewers.lock();
            viewers.retain(|v| v.id != self.id);
            viewers.len()
        };
        self.host.inner.control.viewers_changed(n);
    }
}

impl Drop for ViewerStream {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Ctl(AtomicUsize);
    impl MobileEncoderControl for Ctl {
        fn request_keyframe(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn keyframes_carry_parameter_sets_and_viewers_start_on_idr() {
        let ctl = Arc::new(Ctl(AtomicUsize::new(0)));
        let host = MobileHost::new(ctl.clone(), "test");
        host.push_config(VideoCodec::H264, 640, 480, 30);
        host.push_codec_config(&[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]);
        let (tx, mut rx) = mpsc::channel(8);
        let mut stream = host.start_video(1, tx).unwrap();
        assert!(matches!(rx.recv().await, Some(VideoMessage::Config { width: 640, .. })));
        host.push_frame(&[0, 0, 0, 1, 0x41, 9], 1, false); // P-frame before IDR: dropped
        host.push_frame(&[0, 0, 0, 1, 0x65, 9], 2, true);
        let Some(VideoMessage::Frame(f)) = rx.recv().await else { panic!("expected frame") };
        assert!(f.keyframe);
        assert!(has_parameter_sets(&f.data, VideoCodec::H264));
        assert!(f.data.ends_with(&[0x65, 9]));
        assert!(ctl.0.load(Ordering::Relaxed) >= 2);
        stream.stop();
        assert_eq!(host.status().viewers, 0);
    }
}
