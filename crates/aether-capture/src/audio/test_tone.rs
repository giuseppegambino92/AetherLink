//! Deterministic sine-wave [`AudioCapturer`] for tests, demos and the audio self-test.
//!
//! Like [`crate::TestPatternCapturer`] it is a synthetic *source*, not a mock of an OS API: a
//! real-time paced thread delivers interleaved `f32` PCM in fixed-size chunks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{AudioCapturer, AudioFormat, AudioSink};
use crate::{AudioCodec, AudioPacket, CaptureError, Result};

/// Synthesizes `amplitude · sin(2π·f·t)` on every channel (channel `c` phase-shifted by `c·π/2`
/// so stereo is distinguishable), paced in real time.
pub struct SineAudioCapturer {
    pub frequency_hz: f32,
    pub amplitude: f32,
    pub sample_rate: u32,
    pub channels: u8,
    /// Samples per channel in each delivered packet.
    pub chunk_frames: usize,
    running: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl Default for SineAudioCapturer {
    fn default() -> Self {
        Self::new(1000.0, 48_000, 2)
    }
}

impl SineAudioCapturer {
    /// 10 ms chunks at amplitude 0.5.
    pub fn new(frequency_hz: f32, sample_rate: u32, channels: u8) -> Self {
        Self {
            frequency_hz,
            amplitude: 0.5,
            sample_rate,
            channels,
            chunk_frames: (sample_rate / 100).max(1) as usize,
            running: None,
        }
    }

    /// Render `frames` samples per channel starting at absolute sample index `start`.
    pub fn render(&self, start: u64, frames: usize) -> Vec<f32> {
        sine(self.frequency_hz, self.amplitude, self.sample_rate, self.channels, start, frames)
    }
}

/// Interleaved sine samples (see [`SineAudioCapturer`]).
pub fn sine(frequency_hz: f32, amplitude: f32, sample_rate: u32, channels: u8, start: u64, frames: usize) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    let mut out = Vec::with_capacity(frames * ch);
    let w = std::f64::consts::TAU * frequency_hz as f64 / sample_rate as f64;
    for i in 0..frames as u64 {
        let phase = w * (start + i) as f64;
        for c in 0..ch {
            out.push((amplitude as f64 * (phase + c as f64 * std::f64::consts::FRAC_PI_2).sin()) as f32);
        }
    }
    out
}

impl AudioCapturer for SineAudioCapturer {
    fn start(&mut self, mut sink: AudioSink) -> Result<AudioFormat> {
        if self.running.is_some() {
            return Err(CaptureError::AlreadyRunning);
        }
        if self.sample_rate == 0 || self.channels == 0 || self.chunk_frames == 0 {
            return Err(CaptureError::InvalidFrame("sine source needs a non-zero rate, channel count and chunk".into()));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (freq, amp, rate, channels, chunk) =
            (self.frequency_hz, self.amplitude, self.sample_rate, self.channels, self.chunk_frames);
        let handle = std::thread::Builder::new()
            .name("aether-sine-audio".into())
            .spawn(move || {
                let start = Instant::now();
                let mut pos: u64 = 0;
                let mut seq = 0u64;
                while !stop2.load(Ordering::Relaxed) {
                    let pts_us = pos * 1_000_000 / rate as u64;
                    let due = start + Duration::from_micros(pts_us);
                    let now = Instant::now();
                    if due > now {
                        std::thread::sleep((due - now).min(Duration::from_millis(20)));
                        continue;
                    }
                    let pcm = sine(freq, amp, rate, channels, pos, chunk);
                    let mut data = Vec::with_capacity(pcm.len() * 4);
                    for s in pcm {
                        data.extend_from_slice(&s.to_le_bytes());
                    }
                    sink(AudioPacket { seq, pts_us, codec: AudioCodec::PcmF32Le, sample_rate: rate, channels, data });
                    seq += 1;
                    pos += chunk as u64;
                }
            })
            .map_err(|e| CaptureError::Os(format!("spawn sine thread: {e}")))?;
        self.running = Some((stop, handle));
        Ok(AudioFormat { sample_rate: rate, channels })
    }

    fn stop(&mut self) {
        if let Some((stop, handle)) = self.running.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
    }
}

impl Drop for SineAudioCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn delivers_paced_pcm() {
        let got = Arc::new(Mutex::new(Vec::<AudioPacket>::new()));
        let g2 = got.clone();
        let mut c = SineAudioCapturer::new(1000.0, 48_000, 2);
        let fmt = c.start(Box::new(move |p| g2.lock().unwrap().push(p))).unwrap();
        assert_eq!(fmt, AudioFormat { sample_rate: 48_000, channels: 2 });
        std::thread::sleep(Duration::from_millis(120));
        c.stop();
        let got = got.lock().unwrap();
        // ~13 chunks in 120 ms; generous bounds for loaded CI machines.
        assert!((5..=20).contains(&got.len()), "{} packets", got.len());
        for (i, p) in got.iter().enumerate() {
            assert_eq!(p.seq, i as u64);
            assert_eq!(p.pts_us, i as u64 * 10_000);
            assert_eq!(p.data.len(), 480 * 2 * 4);
        }
        let first = f32::from_le_bytes(got[0].data[4..8].try_into().unwrap());
        assert!((first - 0.5).abs() < 1e-6, "right channel starts at +amplitude (cos)");
        assert!(c.start(Box::new(|_| {})).is_ok(), "restartable");
    }
}
