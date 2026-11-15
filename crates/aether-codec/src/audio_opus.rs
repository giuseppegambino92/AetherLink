//! Opus encoder / decoder (48 kHz, mono or stereo).
//!
//! Backed by [`unsafe_libopus`], libopus 1.3.1 translated to Rust with c2rust: bit-exact with the
//! reference C library, no C toolchain / cmake / pkg-config needed on any target.

use crate::audio::{AudioCodec, AudioPacket, pcm_from_packet, remix};
use crate::audio_resample::Resampler;
use crate::{CodecError, Result};
use std::ptr::NonNull;
use unsafe_libopus as unsafe_op;

/// Opus always runs at 48 kHz here (the wire format of `Datagram::Audio`).
pub const OPUS_SAMPLE_RATE: u32 = 48_000;
/// Largest Opus packet we ever produce for one frame (RFC 6716 limit is 1275 bytes per frame).
const MAX_PACKET_BYTES: usize = 1500;
/// Largest frame the decoder can return (120 ms at 48 kHz, per channel).
const MAX_FRAME_SAMPLES: usize = 5760;

/// `OPUS_APPLICATION_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpusApplication {
    /// General audio (music + speech). Allows SILK/hybrid modes, hence in-band FEC.
    #[default]
    Audio,
    /// `OPUS_APPLICATION_RESTRICTED_LOWDELAY`: CELT only, 2.5 ms lookahead; FEC is inert.
    LowDelay,
    /// Speech-optimized.
    Voip,
}

impl OpusApplication {
    fn raw(self) -> i32 {
        match self {
            OpusApplication::Audio => unsafe_op::OPUS_APPLICATION_AUDIO,
            OpusApplication::LowDelay => unsafe_op::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
            OpusApplication::Voip => unsafe_op::OPUS_APPLICATION_VOIP,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioEncoderConfig {
    /// 1 or 2.
    pub channels: u8,
    /// Frame duration: 10 or 20 ms (also 5, 40, 60).
    pub frame_ms: u8,
    pub bitrate_bps: u32,
    pub application: OpusApplication,
    /// In-band forward error correction (SILK LBRR; effective in SILK/hybrid modes).
    pub inband_fec: bool,
    /// Expected packet loss (%). LBRR FEC is only emitted when this is > 0; libopus then
    /// favours SILK/hybrid modes, so keep it at 0 on clean links (pure CELT quality) and raise it
    /// from measured loss ([`AudioEncoder::set_expected_loss`]).
    pub expected_loss_percent: u8,
    /// 0–10.
    pub complexity: u8,
}

impl Default for AudioEncoderConfig {
    fn default() -> Self {
        Self {
            channels: 2,
            frame_ms: 10,
            bitrate_bps: 96_000,
            application: OpusApplication::Audio,
            inband_fec: true,
            expected_loss_percent: 0,
            complexity: 8,
        }
    }
}

impl AudioEncoderConfig {
    /// Samples per channel in one frame.
    pub fn frame_samples(&self) -> usize {
        OPUS_SAMPLE_RATE as usize * self.frame_ms as usize / 1000
    }

    fn validate(&self) -> Result<()> {
        if !(1..=2).contains(&self.channels) {
            return Err(CodecError::InvalidData(format!("opus: {} channels (1 or 2 supported)", self.channels)));
        }
        if ![5, 10, 20, 40, 60].contains(&self.frame_ms) {
            return Err(CodecError::InvalidData(format!("opus: invalid frame duration {} ms", self.frame_ms)));
        }
        if !(6_000..=510_000).contains(&self.bitrate_bps) {
            return Err(CodecError::InvalidData(format!("opus: bitrate {} out of 6–510 kbps", self.bitrate_bps)));
        }
        Ok(())
    }
}

fn opus_err(what: &str, code: i32) -> CodecError {
    CodecError::InvalidData(format!("opus {what}: error {code}"))
}

/// Feeds arbitrary-length PCM, emits fixed-duration Opus [`AudioPacket`]s with consecutive
/// `seq` and sample-accurate `pts_us`.
pub struct AudioEncoder {
    st: NonNull<unsafe_op::OpusEncoder>,
    cfg: AudioEncoderConfig,
    frame_samples: usize,
    /// Interleaved 48 kHz samples not yet encoded.
    pending: Vec<f32>,
    seq: u64,
    /// Samples per channel already encoded.
    encoded_samples: u64,
    base_pts_us: Option<u64>,
    /// Input conversion for [`AudioEncoder::push_packet`]: (source rate, source channels, resampler).
    input: Option<(u32, u8, Resampler)>,
    out: Vec<u8>,
    lookahead: usize,
}

// SAFETY: the encoder state is a plain heap allocation owned exclusively by this value.
unsafe impl Send for AudioEncoder {}

impl AudioEncoder {
    pub fn new(cfg: AudioEncoderConfig) -> Result<Self> {
        cfg.validate()?;
        let mut err = 0i32;
        let st = unsafe { unsafe_op::opus_encoder_create(OPUS_SAMPLE_RATE as i32, cfg.channels as i32, cfg.application.raw(), &mut err) };
        let st = NonNull::new(st).filter(|_| err == unsafe_op::OPUS_OK).ok_or_else(|| opus_err("encoder_create", err))?;
        let mut enc = Self {
            st,
            frame_samples: cfg.frame_samples(),
            cfg,
            pending: Vec::new(),
            seq: 0,
            encoded_samples: 0,
            base_pts_us: None,
            input: None,
            out: vec![0; MAX_PACKET_BYTES],
            lookahead: 0,
        };
        let c = enc.cfg.clone();
        enc.ctl(unsafe_op::OPUS_SET_BITRATE_REQUEST, c.bitrate_bps as i32)?;
        enc.ctl(unsafe_op::OPUS_SET_INBAND_FEC_REQUEST, c.inband_fec as i32)?;
        enc.ctl(unsafe_op::OPUS_SET_PACKET_LOSS_PERC_REQUEST, c.expected_loss_percent.min(100) as i32)?;
        enc.ctl(unsafe_op::OPUS_SET_COMPLEXITY_REQUEST, c.complexity.min(10) as i32)?;
        enc.ctl(unsafe_op::OPUS_SET_VBR_REQUEST, 1)?;
        let mut la = 0i32;
        let rc = unsafe { unsafe_op::opus_encoder_ctl!(enc.st.as_ptr(), unsafe_op::OPUS_GET_LOOKAHEAD_REQUEST, &mut la) };
        if rc != unsafe_op::OPUS_OK {
            return Err(opus_err("get lookahead", rc));
        }
        enc.lookahead = la.max(0) as usize;
        Ok(enc)
    }

    fn ctl(&mut self, request: i32, value: i32) -> Result<()> {
        let rc = unsafe { unsafe_op::opus_encoder_ctl!(self.st.as_ptr(), request, value) };
        if rc == unsafe_op::OPUS_OK { Ok(()) } else { Err(opus_err("ctl", rc)) }
    }

    pub fn config(&self) -> &AudioEncoderConfig {
        &self.cfg
    }

    /// Samples per channel per packet.
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// Encoder algorithmic delay in samples per channel (decoded audio lags input by this).
    pub fn lookahead_samples(&self) -> usize {
        self.lookahead
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        let bps = bps.clamp(6_000, 510_000);
        self.ctl(unsafe_op::OPUS_SET_BITRATE_REQUEST, bps as i32)?;
        self.cfg.bitrate_bps = bps;
        Ok(())
    }

    pub fn set_expected_loss(&mut self, percent: u8) -> Result<()> {
        self.ctl(unsafe_op::OPUS_SET_PACKET_LOSS_PERC_REQUEST, percent.min(100) as i32)?;
        self.cfg.expected_loss_percent = percent.min(100);
        Ok(())
    }

    /// Sets the pts of the first sample if nothing was encoded yet (later pts are derived from
    /// the sample count, so they stay gap-free and monotonic).
    pub fn anchor_pts(&mut self, pts_us: u64) {
        if self.base_pts_us.is_none() {
            self.base_pts_us = Some(pts_us);
        }
    }

    /// Interleaved 48 kHz PCM with `config().channels` channels, any length.
    pub fn push_pcm(&mut self, pcm: &[f32]) -> Result<Vec<AudioPacket>> {
        self.pending.extend_from_slice(pcm);
        let ch = self.cfg.channels as usize;
        let need = self.frame_samples * ch;
        let mut packets = Vec::new();
        let mut off = 0;
        while self.pending.len() - off >= need {
            let frame = &self.pending[off..off + need];
            let n = unsafe {
                unsafe_op::opus_encode_float(
                    self.st.as_ptr(),
                    frame.as_ptr(),
                    self.frame_samples as i32,
                    self.out.as_mut_ptr(),
                    self.out.len() as i32,
                )
            };
            if n < 0 {
                self.pending.drain(..off);
                return Err(opus_err("encode", n));
            }
            let pts_us = self.base_pts_us.unwrap_or(0) + self.encoded_samples * 1_000_000 / OPUS_SAMPLE_RATE as u64;
            packets.push(AudioPacket {
                seq: self.seq,
                pts_us,
                codec: AudioCodec::Opus,
                sample_rate: OPUS_SAMPLE_RATE,
                channels: self.cfg.channels,
                data: self.out[..n as usize].to_vec(),
            });
            self.seq += 1;
            self.encoded_samples += self.frame_samples as u64;
            off += need;
        }
        self.pending.drain(..off);
        Ok(packets)
    }

    /// A captured PCM packet (`PcmF32Le` / `PcmS16Le`, any rate and channel count): remixed to
    /// the encoder's channel count, resampled to 48 kHz, then encoded.
    pub fn push_packet(&mut self, p: &AudioPacket) -> Result<Vec<AudioPacket>> {
        let pcm = pcm_from_packet(p)?;
        self.anchor_pts(p.pts_us);
        let pcm = remix(&pcm, p.channels, self.cfg.channels);
        let ch = self.cfg.channels;
        let rate = p.sample_rate;
        if rate == OPUS_SAMPLE_RATE {
            self.input = None;
            return self.push_pcm(&pcm);
        }
        if !matches!(&self.input, Some((r, c, _)) if *r == rate && *c == ch) {
            self.input = Some((rate, ch, Resampler::new(rate, OPUS_SAMPLE_RATE, ch)?));
        }
        let resampled = self.input.as_mut().map(|(_, _, r)| r.process(&pcm)).unwrap_or_default();
        self.push_pcm(&resampled)
    }

    /// Pads the remainder with silence and encodes it (end of stream).
    pub fn flush(&mut self) -> Result<Vec<AudioPacket>> {
        let need = self.frame_samples * self.cfg.channels as usize;
        let rem = self.pending.len() % need;
        if rem == 0 {
            return Ok(Vec::new());
        }
        let pad = vec![0.0; need - rem];
        self.push_pcm(&pad)
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        unsafe { unsafe_op::opus_encoder_destroy(self.st.as_ptr()) };
    }
}

/// Opus decoder with packet-loss concealment and in-band FEC recovery (48 kHz output).
pub struct AudioDecoder {
    st: NonNull<unsafe_op::OpusDecoder>,
    channels: u8,
    buf: Vec<f32>,
}

// SAFETY: the decoder state is a plain heap allocation owned exclusively by this value.
unsafe impl Send for AudioDecoder {}

impl AudioDecoder {
    pub fn new(channels: u8) -> Result<Self> {
        if !(1..=2).contains(&channels) {
            return Err(CodecError::InvalidData(format!("opus: {channels} channels (1 or 2 supported)")));
        }
        let mut err = 0i32;
        let st = unsafe { unsafe_op::opus_decoder_create(OPUS_SAMPLE_RATE as i32, channels as i32, &mut err) };
        let st = NonNull::new(st).filter(|_| err == unsafe_op::OPUS_OK).ok_or_else(|| opus_err("decoder_create", err))?;
        Ok(Self { st, channels, buf: vec![0.0; MAX_FRAME_SAMPLES * channels as usize] })
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    fn run(&mut self, data: Option<&[u8]>, frame_samples: usize, fec: bool) -> Result<Vec<f32>> {
        let frame_samples = frame_samples.clamp(1, MAX_FRAME_SAMPLES);
        let (ptr, len) = match data {
            Some(d) if !d.is_empty() => (d.as_ptr(), d.len() as i32),
            _ => (std::ptr::null(), 0),
        };
        let n = unsafe {
            unsafe_op::opus_decode_float(self.st.as_ptr(), ptr, len, self.buf.as_mut_ptr(), frame_samples as i32, fec as i32)
        };
        if n < 0 {
            return Err(opus_err("decode", n));
        }
        Ok(self.buf[..n as usize * self.channels as usize].to_vec())
    }

    /// Decode one packet into interleaved PCM.
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<f32>> {
        if data.is_empty() {
            return Err(CodecError::InvalidData("opus: empty packet".into()));
        }
        self.run(Some(data), MAX_FRAME_SAMPLES, false)
    }

    /// Recover the frame *before* `next` from its in-band FEC data (falls back to PLC inside
    /// libopus when `next` carries no LBRR data). `frame_samples` = duration of the lost frame.
    pub fn decode_fec(&mut self, next: &[u8], frame_samples: usize) -> Result<Vec<f32>> {
        self.run(Some(next), frame_samples, true)
    }

    /// Packet-loss concealment for one missing frame of `frame_samples`.
    pub fn conceal(&mut self, frame_samples: usize) -> Result<Vec<f32>> {
        self.run(None, frame_samples, false)
    }

    /// Reset decoder state (stream restart).
    pub fn reset(&mut self) {
        unsafe { unsafe_op::opus_decoder_ctl_impl(self.st.as_ptr(), unsafe_op::OPUS_RESET_STATE, unsafe_op::varargs!()) };
    }
}

impl Drop for AudioDecoder {
    fn drop(&mut self) {
        unsafe { unsafe_op::opus_decoder_destroy(self.st.as_ptr()) };
    }
}

/// Opus implementation version string.
pub fn opus_version() -> String {
    unsafe_op::opus_get_version_string().to_string()
}
