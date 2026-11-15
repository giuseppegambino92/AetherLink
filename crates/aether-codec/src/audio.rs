//! Audio for the system-audio stream (`Datagram::Audio`).
//!
//! * [`AudioEncoder`] / [`AudioDecoder`] — Opus at 48 kHz, 10/20 ms frames, in-band FEC,
//!   settable bitrate, PLC (pure-Rust libopus 1.3.1 translation, see [`crate::audio_opus`]).
//! * [`JitterBuffer`] — adaptive (20–60 ms) reorder / late-drop / loss-concealment logic.
//! * [`AudioReceiver`] — jitter buffer + decoder: packets in, fixed-size PCM frames out
//!   (decoded, FEC-recovered, concealed or silence).
//! * [`Resampler`] — band-limited streaming resampler for sources not at 48 kHz.
//! * PCM helpers and signal analysis ([`sine_fit`], [`peak_frequency`]) used by tests and the
//!   audio self-test.

use crate::{CodecError, Result};
use serde::Serialize;

pub use crate::audio_jitter::{JitterBuffer, JitterConfig, JitterStats, Playout, PushOutcome};
pub use crate::audio_opus::{
    AudioDecoder, AudioEncoder, AudioEncoderConfig, OPUS_SAMPLE_RATE, OpusApplication, opus_version,
};
pub use crate::audio_resample::Resampler;
pub use aether_protocol::{AudioCodec, AudioPacket};

// ---------------------------------------------------------------------------------------------
// PCM helpers
// ---------------------------------------------------------------------------------------------

/// Interleaved `f32` samples of a PCM packet (`PcmF32Le` or `PcmS16Le`).
pub fn pcm_from_packet(p: &AudioPacket) -> Result<Vec<f32>> {
    match p.codec {
        AudioCodec::PcmF32Le => {
            Ok(p.data.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect())
        }
        AudioCodec::PcmS16Le => {
            Ok(p.data.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as f32 / 32768.0).collect())
        }
        AudioCodec::Opus => Err(CodecError::InvalidData("expected PCM, got an Opus packet".into())),
    }
}

/// Little-endian bytes of `f32` samples (`AudioCodec::PcmF32Le` payload).
pub fn pcm_to_f32le(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Channel conversion of interleaved PCM: mono is duplicated, extra channels beyond the target
/// are dropped (a multichannel source keeps front L/R), downmix to mono averages L/R.
pub fn remix(pcm: &[f32], from: u8, to: u8) -> Vec<f32> {
    let (from, to) = (from.max(1) as usize, to.max(1) as usize);
    if from == to {
        return pcm.to_vec();
    }
    let frames = pcm.len() / from;
    let mut out = Vec::with_capacity(frames * to);
    for f in pcm.chunks_exact(from) {
        if to == 1 {
            out.push(if from >= 2 { (f[0] + f[1]) * 0.5 } else { f[0] });
        } else {
            for c in 0..to {
                out.push(if from == 1 { f[0] } else if c < from { f[c] } else { 0.0 });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Receiver: jitter buffer + decoder
// ---------------------------------------------------------------------------------------------

/// How a pulled frame was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FrameKind {
    Decoded,
    /// Lost frame recovered via the next packet's in-band FEC (libopus falls back to PLC when
    /// that packet carries no LBRR data, e.g. CELT-only mode).
    Fec,
    /// Lost frame concealed by PLC.
    Plc,
    /// Concealment frame inserted to grow the playout delay.
    Stretch,
    /// Not playing yet: silence.
    Silence,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioReceiverStats {
    #[serde(flatten)]
    pub jitter: JitterStats,
    pub decoded: u64,
    pub fec_frames: u64,
    pub plc_frames: u64,
    pub stretch_frames: u64,
    pub silence_frames: u64,
    pub decode_errors: u64,
    /// Non-Opus packets ignored.
    pub rejected: u64,
}

/// Jitter buffer + Opus decoder producing fixed-size 48 kHz interleaved frames.
pub struct AudioReceiver {
    jitter: JitterBuffer,
    decoder: AudioDecoder,
    channels: u8,
    frame_samples: usize,
    stats: AudioReceiverStats,
}

impl AudioReceiver {
    /// `channels`: output channel count (1 or 2); mono/stereo packets are both accepted.
    pub fn new(channels: u8, cfg: JitterConfig) -> Result<Self> {
        let frame_samples = (OPUS_SAMPLE_RATE as u64 * cfg.frame_us as u64 / 1_000_000).max(1) as usize;
        Ok(Self {
            jitter: JitterBuffer::new(cfg),
            decoder: AudioDecoder::new(channels)?,
            channels,
            frame_samples,
            stats: AudioReceiverStats::default(),
        })
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Samples per channel of each pulled frame.
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    pub fn push(&mut self, p: AudioPacket, arrival_us: u64) -> PushOutcome {
        if p.codec != AudioCodec::Opus || p.sample_rate != OPUS_SAMPLE_RATE {
            self.stats.rejected += 1;
            return PushOutcome::Late;
        }
        let out = self.jitter.push(p, arrival_us);
        if out == PushOutcome::Reset {
            self.decoder.reset();
        }
        out
    }

    fn conceal(&mut self) -> Vec<f32> {
        self.decoder.conceal(self.frame_samples).unwrap_or_else(|_| vec![0.0; self.frame_samples * self.channels as usize])
    }

    /// The next frame at receiver time `now_us` (call once per frame of played audio).
    pub fn pull(&mut self, now_us: u64) -> (Vec<f32>, FrameKind) {
        let silence = |me: &Self| vec![0.0; me.frame_samples * me.channels as usize];
        match self.jitter.pop(now_us) {
            Playout::Buffering => {
                self.stats.silence_frames += 1;
                (silence(self), FrameKind::Silence)
            }
            Playout::Stretch => {
                self.stats.stretch_frames += 1;
                (self.conceal(), FrameKind::Stretch)
            }
            Playout::Packet(p) => match self.decoder.decode(&p.data) {
                Ok(pcm) => {
                    let n = pcm.len() / self.channels as usize;
                    if n > 0 && n != self.frame_samples {
                        // The host uses a different frame duration: follow it.
                        self.frame_samples = n;
                        self.jitter.set_frame_us((n as u64 * 1_000_000 / OPUS_SAMPLE_RATE as u64) as u32);
                    }
                    self.stats.decoded += 1;
                    (pcm, FrameKind::Decoded)
                }
                Err(_) => {
                    self.stats.decode_errors += 1;
                    self.stats.plc_frames += 1;
                    (self.conceal(), FrameKind::Plc)
                }
            },
            Playout::Lost { next: Some(next), .. } => match self.decoder.decode_fec(&next.data, self.frame_samples) {
                Ok(pcm) => {
                    self.stats.fec_frames += 1;
                    (pcm, FrameKind::Fec)
                }
                Err(_) => {
                    self.stats.plc_frames += 1;
                    (self.conceal(), FrameKind::Plc)
                }
            },
            Playout::Lost { next: None, .. } => {
                self.stats.plc_frames += 1;
                (self.conceal(), FrameKind::Plc)
            }
        }
    }

    pub fn stats(&self) -> AudioReceiverStats {
        AudioReceiverStats { jitter: self.jitter.stats(), ..self.stats.clone() }
    }

    pub fn jitter(&self) -> &JitterBuffer {
        &self.jitter
    }

    /// Forget queued packets (e.g. audio disabled); keeps cumulative stats.
    pub fn reset(&mut self) {
        self.jitter.reset();
        self.decoder.reset();
    }
}

// ---------------------------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------------------------

/// Least-squares fit of a sinusoid at a known frequency.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SineFit {
    pub amplitude: f64,
    pub phase: f64,
    /// Fitted sine power over residual power, dB.
    pub snr_db: f64,
}

/// Fit `A·sin(2πft + φ)` to one channel of interleaved PCM; the residual (noise, distortion,
/// concealment artefacts) gives the SNR. Alignment-independent (codec delay does not matter).
pub fn sine_fit(pcm: &[f32], channels: u8, channel: u8, sample_rate: u32, freq: f64) -> SineFit {
    let ch = channels.max(1) as usize;
    let x: Vec<f64> = pcm.iter().skip(channel as usize).step_by(ch).map(|&s| s as f64).collect();
    let w = std::f64::consts::TAU * freq / sample_rate as f64;
    let (mut scc, mut sss, mut scs, mut sxc, mut sxs) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (i, &v) in x.iter().enumerate() {
        let (s, c) = (w * i as f64).sin_cos();
        scc += c * c;
        sss += s * s;
        scs += s * c;
        sxc += v * c;
        sxs += v * s;
    }
    let det = scc * sss - scs * scs;
    if x.is_empty() || det.abs() < 1e-12 {
        return SineFit { amplitude: 0.0, phase: 0.0, snr_db: f64::NEG_INFINITY };
    }
    // v ≈ a·cos + b·sin
    let a = (sxc * sss - sxs * scs) / det;
    let b = (sxs * scc - sxc * scs) / det;
    let mut res = 0.0;
    for (i, &v) in x.iter().enumerate() {
        let (s, c) = (w * i as f64).sin_cos();
        let e = v - (a * c + b * s);
        res += e * e;
    }
    let amplitude = (a * a + b * b).sqrt();
    let signal = amplitude * amplitude / 2.0;
    let noise = res / x.len() as f64;
    SineFit { amplitude, phase: a.atan2(b), snr_db: 10.0 * (signal / noise.max(1e-20)).log10() }
}

/// Frequency of the strongest spectral peak of one channel (Hann window, radix-2 FFT over up to
/// 16384 samples, parabolic interpolation between bins).
pub fn peak_frequency(pcm: &[f32], channels: u8, channel: u8, sample_rate: u32) -> f64 {
    let ch = channels.max(1) as usize;
    let x: Vec<f64> = pcm.iter().skip(channel as usize).step_by(ch).map(|&s| s as f64).collect();
    let mut n = 1usize;
    while n * 2 <= x.len().min(16_384) {
        n *= 2;
    }
    if n < 64 {
        return 0.0;
    }
    let mut re: Vec<f64> = (0..n)
        .map(|i| x[i] * (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (n - 1) as f64).cos()))
        .collect();
    let mut im = vec![0.0; n];
    fft(&mut re, &mut im);
    let mag: Vec<f64> = (0..n / 2).map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt().max(1e-30).ln()).collect();
    let k = (1..n / 2 - 1).max_by(|&a, &b| mag[a].total_cmp(&mag[b])).unwrap_or(1);
    let (a, b, c) = (mag[k - 1], mag[k], mag[k + 1]);
    let denom = a - 2.0 * b + c;
    let delta = if denom.abs() > 1e-12 { 0.5 * (a - c) / denom } else { 0.0 };
    (k as f64 + delta) * sample_rate as f64 / n as f64
}

/// In-place iterative radix-2 FFT (`re.len()` must be a power of two).
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -std::f64::consts::TAU / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, frames: usize, ch: usize, amp: f64) -> Vec<f32> {
        let mut v = Vec::new();
        for i in 0..frames {
            let s = (std::f64::consts::TAU * freq * i as f64 / 48_000.0).sin() * amp;
            for _ in 0..ch {
                v.push(s as f32);
            }
        }
        v
    }

    #[test]
    fn pcm_round_trips_and_remixes() {
        let x = [0.25f32, -0.5, 1.0, 0.0];
        let p = AudioPacket {
            seq: 0,
            pts_us: 0,
            codec: AudioCodec::PcmF32Le,
            sample_rate: 48_000,
            channels: 2,
            data: pcm_to_f32le(&x),
        };
        assert_eq!(pcm_from_packet(&p).unwrap(), x);
        let s16 = AudioPacket { codec: AudioCodec::PcmS16Le, data: vec![0x00, 0x40, 0x00, 0xC0], ..p };
        assert_eq!(pcm_from_packet(&s16).unwrap(), [0.5, -0.5]);
        assert_eq!(remix(&[1.0, 2.0], 1, 2), [1.0, 1.0, 2.0, 2.0]);
        assert_eq!(remix(&[1.0, 3.0], 2, 1), [2.0]);
        assert_eq!(remix(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 6, 2), [1.0, 2.0]);
    }

    #[test]
    fn analysis_helpers() {
        let x = sine(1000.0, 9600, 2, 0.5);
        let fit = sine_fit(&x, 2, 1, 48_000, 1000.0);
        assert!((fit.amplitude - 0.5).abs() < 1e-6 && fit.snr_db > 100.0, "{fit:?}");
        assert!((peak_frequency(&x, 2, 0, 48_000) - 1000.0).abs() < 2.0);
        assert!((peak_frequency(&sine(3333.0, 9600, 1, 0.3), 1, 0, 48_000) - 3333.0).abs() < 3.0);
    }

    #[test]
    fn opus_round_trip_1khz() {
        let mut enc = AudioEncoder::new(AudioEncoderConfig::default()).unwrap();
        let mut dec = AudioDecoder::new(2).unwrap();
        let input = sine(1000.0, 48_000, 2, 0.5);
        // Arbitrary chunking: 7 ms pieces.
        let mut packets = Vec::new();
        for c in input.chunks(336 * 2) {
            packets.extend(enc.push_pcm(c).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        assert_eq!(packets.len(), 100);
        for (i, p) in packets.iter().enumerate() {
            assert_eq!((p.seq, p.pts_us, p.codec, p.channels), (i as u64, i as u64 * 10_000, AudioCodec::Opus, 2));
            assert!(!p.data.is_empty() && p.data.len() < 400, "{} bytes", p.data.len());
        }
        let mut out = Vec::new();
        for p in &packets {
            out.extend(dec.decode(&p.data).unwrap());
        }
        assert_eq!(out.len(), input.len());
        let fit = sine_fit(&out[4800..], 2, 0, 48_000, 1000.0);
        assert!((fit.amplitude - 0.5).abs() < 0.05, "{fit:?}");
        assert!(fit.snr_db > 30.0, "{fit:?}");
        // Concealment and FEC calls produce a frame of the requested size.
        assert_eq!(dec.conceal(480).unwrap().len(), 960);
        assert_eq!(dec.decode_fec(&packets[5].data, 480).unwrap().len(), 960);
    }

    #[test]
    fn encoder_resamples_and_remixes_capture_packets() {
        let mut enc = AudioEncoder::new(AudioEncoderConfig { frame_ms: 20, bitrate_bps: 64_000, ..Default::default() }).unwrap();
        assert_eq!(enc.frame_samples(), 960);
        let mut packets = Vec::new();
        // 1 s of 44.1 kHz mono S16 in 10 ms chunks, pts starting at 5 s.
        for i in 0..100u64 {
            let pcm: Vec<f32> = (0..441)
                .map(|k| ((std::f64::consts::TAU * 440.0 * (i * 441 + k) as f64 / 44_100.0).sin() * 0.4) as f32)
                .collect();
            let data = pcm.iter().flat_map(|s| ((s * 32767.0) as i16).to_le_bytes()).collect();
            let p = AudioPacket {
                seq: i,
                pts_us: 5_000_000 + i * 10_000,
                codec: AudioCodec::PcmS16Le,
                sample_rate: 44_100,
                channels: 1,
                data,
            };
            packets.extend(enc.push_packet(&p).unwrap());
        }
        assert!((48..=50).contains(&packets.len()), "{}", packets.len());
        assert_eq!(packets[0].pts_us, 5_000_000);
        assert_eq!(packets[1].pts_us, 5_020_000);
        let mut dec = AudioDecoder::new(2).unwrap();
        let out: Vec<f32> = packets.iter().flat_map(|p| dec.decode(&p.data).unwrap()).collect();
        assert!((peak_frequency(&out[9600..], 2, 1, 48_000) - 440.0).abs() < 3.0);
        assert!(sine_fit(&out[9600..], 2, 0, 48_000, 440.0).snr_db > 20.0);
        enc.set_bitrate(32_000).unwrap();
        assert_eq!(enc.config().bitrate_bps, 32_000);
    }

    #[test]
    fn receiver_conceals_loss_with_fec_or_plc() {
        // Low bitrate speech-like settings so SILK/hybrid (and therefore LBRR FEC) is used.
        let cfg = AudioEncoderConfig {
            bitrate_bps: 24_000,
            application: OpusApplication::Voip,
            expected_loss_percent: 20,
            channels: 1,
            ..Default::default()
        };
        let mut enc = AudioEncoder::new(cfg).unwrap();
        let input = sine(440.0, 48_000, 1, 0.3);
        let packets = enc.push_pcm(&input).unwrap();
        let mut rx = AudioReceiver::new(2, JitterConfig::default()).unwrap();
        let mut t = 0;
        let mut kinds = Vec::new();
        for p in packets.iter() {
            if p.seq != 40 && p.seq != 70 && p.seq != 71 {
                rx.push(p.clone(), p.pts_us + 1_000);
            }
            if p.seq >= 2 {
                let (pcm, kind) = rx.pull(t);
                assert_eq!(pcm.len(), 960);
                kinds.push(kind);
                t += 10_000;
            } else {
                t = p.pts_us + 10_000;
            }
        }
        let s = rx.stats();
        assert_eq!(s.jitter.lost, 3, "{s:?}");
        assert_eq!(s.fec_frames, 2, "40 (41 present) and 71 (72 present): {s:?}");
        assert_eq!(s.plc_frames, 1, "70 (71 missing too): {s:?}");
        assert!(s.decoded >= 90, "{s:?}");
        assert!(kinds.contains(&FrameKind::Fec));
    }
}
