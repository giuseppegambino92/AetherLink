//! System-audio capture backend for Android and iOS.
//!
//! Mobile OSes do not let a library tap the audio mix directly. The platform layer
//! captures it and pushes interleaved `f32` PCM here:
//!
//! * Android — `AudioPlaybackCapture` (API 29+) on the MediaProjection token, read by the
//!   screen-share foreground service and handed over through JNI.
//! * iOS — `RPSampleBufferType.audioApp` buffers from the ReplayKit broadcast extension,
//!   forwarded to the main app over the App Group socket.
//!
//! [`push_pcm`] fans each buffer out to every running [`MobileAudioCapturer`]. When the
//! platform layer is not feeding audio, a started capturer simply receives nothing (and
//! [`is_feeding`] reports `false`) — no synthetic samples are ever produced.

use super::{AudioCapturer, AudioFormat, AudioSink};
use crate::Result;
use aether_protocol::{AudioCodec, AudioPacket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

struct Registry {
    sinks: Vec<(u64, AudioSink)>,
    format: Option<AudioFormat>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry { sinks: Vec::new(), format: None });
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static SEQ: AtomicU64 = AtomicU64::new(0);
static FEEDING: AtomicBool = AtomicBool::new(false);

/// Default format reported before the platform delivered its first buffer
/// (both AudioPlaybackCapture and ReplayKit app audio are 48 kHz stereo in practice).
const DEFAULT_FORMAT: AudioFormat = AudioFormat { sample_rate: 48_000, channels: 2 };

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|p| p.into_inner())
}

/// Push one buffer of interleaved `f32` PCM captured by the platform layer.
pub fn push_pcm(samples: &[f32], sample_rate: u32, channels: u8, pts_us: u64) {
    if samples.is_empty() || channels == 0 || sample_rate == 0 {
        return;
    }
    FEEDING.store(true, Ordering::Relaxed);
    let mut reg = registry();
    reg.format = Some(AudioFormat { sample_rate, channels });
    if reg.sinks.is_empty() {
        return;
    }
    let mut data = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        data.extend_from_slice(&s.to_le_bytes());
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let n = reg.sinks.len();
    for (i, (_, sink)) in reg.sinks.iter_mut().enumerate() {
        let payload = if i + 1 == n { std::mem::take(&mut data) } else { data.clone() };
        sink(AudioPacket { seq, pts_us, codec: AudioCodec::PcmF32Le, sample_rate, channels, data: payload });
    }
}

/// Push interleaved signed 16-bit PCM (converted to `f32`).
pub fn push_pcm_i16(samples: &[i16], sample_rate: u32, channels: u8, pts_us: u64) {
    let f: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
    push_pcm(&f, sample_rate, channels, pts_us);
}

/// The platform layer stopped capturing (projection / broadcast ended).
pub fn platform_stopped() {
    FEEDING.store(false, Ordering::Relaxed);
}

/// Whether the platform layer is currently delivering audio.
pub fn is_feeding() -> bool {
    FEEDING.load(Ordering::Relaxed)
}

/// Number of running capturers (lets the platform layer skip audio work nobody consumes).
pub fn active_capturers() -> usize {
    registry().sinks.len()
}

/// Capturer fed by [`push_pcm`].
pub struct MobileAudioCapturer {
    id: Option<u64>,
}

impl AudioCapturer for MobileAudioCapturer {
    fn start(&mut self, sink: AudioSink) -> Result<AudioFormat> {
        self.stop();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut reg = registry();
        reg.sinks.push((id, sink));
        self.id = Some(id);
        Ok(reg.format.unwrap_or(DEFAULT_FORMAT))
    }

    fn stop(&mut self) {
        if let Some(id) = self.id.take() {
            registry().sinks.retain(|(i, _)| *i != id);
        }
    }
}

impl Drop for MobileAudioCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn create() -> Result<Box<dyn AudioCapturer>> {
    Ok(Box::new(MobileAudioCapturer { id: None }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn fans_out_pcm_to_running_capturers() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let mut cap = create().unwrap();
        cap.start(Box::new(move |p| g.lock().unwrap().push(p))).unwrap();
        push_pcm(&[0.5, -0.5], 44_100, 2, 7);
        cap.stop();
        push_pcm(&[0.25], 44_100, 1, 8);
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].sample_rate, 44_100);
        assert_eq!(got[0].data.len(), 8);
    }
}
