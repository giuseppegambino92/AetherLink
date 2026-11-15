//! Lightweight streaming resampler for interleaved `f32` PCM.
//!
//! Band-limited interpolation with a 32-tap Blackman-windowed sinc kernel, tabulated at 256
//! sub-sample phases (linear interpolation between phases). The cut-off follows the lower of
//! the two rates, so downsampling is anti-aliased. Latency: 16 input samples. Good for capture
//! sources not at 48 kHz (44.1 kHz devices, 16 kHz mobile sources) and for 48 kHz → device rate
//! on playback; not a mastering-grade converter.

use crate::{CodecError, Result};

const HALF_TAPS: usize = 16;
const TAPS: usize = HALF_TAPS * 2;
const PHASES: usize = 256;

pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    channels: usize,
    /// Input frames advanced per output frame.
    step: f64,
    /// Position of the next output frame, in input frames relative to `buf[0]`.
    pos: f64,
    /// Interleaved input history.
    buf: Vec<f32>,
    /// `(PHASES + 1) × TAPS` kernel table.
    table: Vec<f32>,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: u8) -> Result<Self> {
        if in_rate == 0 || out_rate == 0 || channels == 0 {
            return Err(CodecError::InvalidData(format!("resampler {in_rate} → {out_rate} Hz × {channels} ch")));
        }
        let cutoff = (out_rate as f64 / in_rate as f64).min(1.0) * 0.94;
        let mut table = vec![0f32; (PHASES + 1) * TAPS];
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let mut sum = 0.0;
            let row = &mut table[p * TAPS..(p + 1) * TAPS];
            for (j, v) in row.iter_mut().enumerate() {
                // tap j multiplies input sample i0 + j - HALF_TAPS + 1; distance from the output instant:
                let x = j as f64 - HALF_TAPS as f64 + 1.0 - frac;
                let w = blackman(x / HALF_TAPS as f64);
                let h = cutoff * sinc(cutoff * x) * w;
                *v = h as f32;
                sum += h;
            }
            // Unity DC gain for every phase.
            if sum.abs() > 1e-9 {
                for v in row.iter_mut() {
                    *v = (*v as f64 / sum) as f32;
                }
            }
        }
        let channels = channels as usize;
        Ok(Self {
            in_rate,
            out_rate,
            channels,
            step: in_rate as f64 / out_rate as f64,
            // Start with HALF_TAPS-1 frames of silence so the first output sample is centred on
            // the first input sample.
            pos: (HALF_TAPS - 1) as f64,
            buf: vec![0.0; (HALF_TAPS - 1) * channels],
            table,
        })
    }

    pub fn is_passthrough(&self) -> bool {
        self.in_rate == self.out_rate
    }

    pub fn rates(&self) -> (u32, u32) {
        (self.in_rate, self.out_rate)
    }

    /// Resample a chunk of interleaved frames; returns the output frames available so far.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.is_passthrough() {
            return input.to_vec();
        }
        let ch = self.channels;
        self.buf.extend_from_slice(&input[..input.len() - input.len() % ch]);
        let frames = self.buf.len() / ch;
        let mut out = Vec::with_capacity(((input.len() / ch) as f64 / self.step + 2.0) as usize * ch);
        loop {
            let i0 = self.pos.floor() as usize;
            if i0 + HALF_TAPS >= frames {
                break;
            }
            let frac = self.pos - i0 as f64;
            let ph = frac * PHASES as f64;
            let p = (ph.floor() as usize).min(PHASES - 1);
            let t = (ph - p as f64) as f32;
            let (a, b) = (&self.table[p * TAPS..(p + 1) * TAPS], &self.table[(p + 1) * TAPS..(p + 2) * TAPS]);
            let start = i0 + 1 - HALF_TAPS;
            for c in 0..ch {
                let mut acc = 0f32;
                for j in 0..TAPS {
                    let k = a[j] + (b[j] - a[j]) * t;
                    acc += self.buf[(start + j) * ch + c] * k;
                }
                out.push(acc);
            }
            self.pos += self.step;
        }
        // Drop history no longer needed by the next output frame.
        let keep_from = (self.pos.floor() as usize + 1).saturating_sub(HALF_TAPS).min(frames);
        if keep_from > 0 {
            self.buf.drain(..keep_from * ch);
            self.pos -= keep_from as f64;
        }
        out
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) }
}

/// Blackman window on [-1, 1].
fn blackman(x: f64) -> f64 {
    if x.abs() > 1.0 {
        return 0.0;
    }
    let a = std::f64::consts::PI * x;
    0.42 + 0.5 * a.cos() + 0.08 * (2.0 * a).cos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{peak_frequency, sine_fit};

    fn tone(freq: f64, rate: u32, frames: usize, ch: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * ch);
        for i in 0..frames {
            let s = (std::f64::consts::TAU * freq * i as f64 / rate as f64).sin() * 0.5;
            for _ in 0..ch {
                v.push(s as f32);
            }
        }
        v
    }

    fn run_chunked(r: &mut Resampler, input: &[f32], chunk: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for c in input.chunks(chunk) {
            out.extend(r.process(c));
        }
        out
    }

    #[test]
    fn passthrough_is_identity() {
        let mut r = Resampler::new(48_000, 48_000, 2).unwrap();
        let x = tone(1000.0, 48_000, 480, 2);
        assert_eq!(r.process(&x), x);
    }

    #[test]
    fn upsamples_44k1_to_48k_cleanly() {
        let mut r = Resampler::new(44_100, 48_000, 2).unwrap();
        let input = tone(1000.0, 44_100, 44_100, 2);
        // Odd chunk sizes exercise the streaming state.
        let out = run_chunked(&mut r, &input, 2 * 441 + 6);
        let frames = out.len() / 2;
        assert!((47_950..=48_000).contains(&frames), "{frames} frames");
        let fit = sine_fit(&out[2000..], 2, 0, 48_000, 1000.0);
        assert!((fit.amplitude - 0.5).abs() < 0.01, "{fit:?}");
        assert!(fit.snr_db > 60.0, "{fit:?}");
        let peak = peak_frequency(&out[2000..2000 + 9600], 2, 0, 48_000);
        assert!((peak - 1000.0).abs() < 5.0, "peak {peak}");
    }

    #[test]
    fn downsampling_rejects_aliases() {
        // 30 kHz tone at 96 kHz would alias to 18 kHz at 48 kHz without filtering.
        let mut r = Resampler::new(96_000, 48_000, 1).unwrap();
        let out = run_chunked(&mut r, &tone(30_000.0, 96_000, 96_000, 1), 960);
        let rms = (out[1000..].iter().map(|s| s * s).sum::<f32>() / (out.len() - 1000) as f32).sqrt();
        assert!(rms < 0.5 / 2f32.sqrt() * 0.01, "alias rms {rms} (≥ 40 dB attenuation expected)");
        // An in-band tone passes.
        let mut r = Resampler::new(96_000, 48_000, 1).unwrap();
        let out = run_chunked(&mut r, &tone(5_000.0, 96_000, 96_000, 1), 1000);
        let fit = sine_fit(&out[1000..], 1, 0, 48_000, 5_000.0);
        assert!((fit.amplitude - 0.5).abs() < 0.01 && fit.snr_db > 50.0, "{fit:?}");
    }

    #[test]
    fn downsamples_48k_to_16k() {
        let mut r = Resampler::new(48_000, 16_000, 2).unwrap();
        let out = run_chunked(&mut r, &tone(440.0, 48_000, 48_000, 2), 480 * 2);
        assert!((15_980..=16_000).contains(&(out.len() / 2)));
        let fit = sine_fit(&out[1000..], 2, 1, 16_000, 440.0);
        assert!(fit.snr_db > 60.0, "{fit:?}");
    }
}
