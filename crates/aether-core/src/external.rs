//! Platform-provided host sources.
//!
//! On Android (MediaProjection → MediaCodec) and iOS (ReplayKit broadcast extension) the OS
//! hands us *already encoded* H.264/HEVC, so the host session must not run the desktop
//! capture → encode pipeline. The platform glue (JNI / Swift bridge) registers a
//! [`PlatformHost`] at startup; host sessions then stream its `VideoMessage`s as-is.

use aether_protocol::{DisplayInfo, VideoMessage};
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::sync::mpsc;

/// A running platform video stream.
pub trait ExternalStream: Send {
    /// Ask the platform encoder for an IDR frame (new viewer, DVR resync, decoder error).
    fn request_keyframe(&self);
    /// Stop producing frames (idempotent).
    fn stop(&mut self);
}

/// Host-side platform integration (mobile).
pub trait PlatformHost: Send + Sync + 'static {
    /// Displays this device can share (usually exactly one).
    fn displays(&self) -> Vec<DisplayInfo>;
    /// Start streaming `display_id`. The source must send `VideoMessage::Config` first, then
    /// `VideoMessage::Frame`s whose keyframes carry their parameter sets in-band (Annex-B).
    fn start_video(&self, display_id: u32, out: mpsc::Sender<VideoMessage>) -> crate::Result<Box<dyn ExternalStream>>;
}

static HOST: RwLock<Option<Arc<dyn PlatformHost>>> = RwLock::new(None);

/// Register (or clear) the platform host integration.
pub fn set_platform_host(host: Option<Arc<dyn PlatformHost>>) {
    *HOST.write() = host;
}

pub fn platform_host() -> Option<Arc<dyn PlatformHost>> {
    HOST.read().clone()
}
