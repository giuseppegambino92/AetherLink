//! System-audio loopback capture ("what the host is playing"), as interleaved `f32` PCM.
//!
//! One backend per OS behind [`create_audio_capturer`]:
//! * macOS — ScreenCaptureKit audio (`capturesAudio`, `excludesCurrentProcessAudio`)
//! * Windows — WASAPI loopback (`AUDCLNT_STREAMFLAGS_LOOPBACK`) on the default render endpoint
//! * Linux — PipeWire / PulseAudio monitor source of the default sink
//! * Android / iOS — PCM pushed by the platform layer (AudioPlaybackCapture / ReplayKit) through
//!   [`mobile::push_pcm`]
//!
//! Packets carry `AudioCodec::PcmF32Le`; `aether-codec` turns them into Opus for the wire.

use crate::Result;
pub use aether_protocol::{AudioCodec, AudioPacket};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "android", target_os = "ios"))]
pub mod mobile;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
pub(crate) mod windows;
mod test_tone;
pub use test_tone::{SineAudioCapturer, sine};

/// Negotiated PCM format of a running capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
}

/// Receives captured PCM packets (on a backend-owned thread).
pub type AudioSink = Box<dyn FnMut(AudioPacket) + Send>;

/// A system-audio loopback capturer.
pub trait AudioCapturer: Send {
    /// Start capturing; returns the PCM format delivered to `sink`.
    fn start(&mut self, sink: AudioSink) -> Result<AudioFormat>;
    /// Stop (idempotent). No packets are delivered after return.
    fn stop(&mut self);
}

/// Create the system-audio capturer for this OS.
pub fn create_audio_capturer() -> Result<Box<dyn AudioCapturer>> {
    #[cfg(target_os = "macos")]
    return macos::create();
    #[cfg(target_os = "windows")]
    return windows::create();
    #[cfg(target_os = "linux")]
    return linux::create();
    #[cfg(any(target_os = "android", target_os = "ios"))]
    return mobile::create();
    #[allow(unreachable_code)]
    Err(crate::CaptureError::Unsupported("no audio backend for this OS"))
}
