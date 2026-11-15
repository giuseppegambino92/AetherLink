//! # aether-codec
//!
//! * [`tiles`] — lossless 64×64 dirty-tile codec ([`TileEncoder`] / [`Framebuffer`]):
//!   xxh3 tile hashing, solid detection, zstd, deterministic client tile cache (`CacheRef`),
//!   merged runs, OS dirty-rect hints.
//! * [`videotoolbox`] (macOS) — hardware H.264 / HEVC (incl. HEVC 4:4:4 where supported)
//!   low-latency encoder producing Annex-B, and a decoder producing [`BgraImage`].
//! * `mediafoundation` (Windows) — Media Foundation H.264 / HEVC encoder MFTs (hardware
//!   NVENC / AMF / Quick Sync first, Microsoft software H.264 encoder as the guaranteed
//!   fallback) and decoder MFTs, NV12 via [`mediafoundation_convert`].
//! * `openh264_codec` (Linux) — OpenH264 software H.264 4:2:0 encoder (built from source,
//!   low-latency screen-content mode, Annex-B with SPS/PPS on every IDR) and BGRA decoder.
//! * [`annexb`] — Annex-B ↔ AVCC/HVCC, parameter sets, `avcC`/`hvcC` records, codec strings
//!   (WebCodecs `VideoDecoderConfig`).
//! * [`scheduler`] — [`HybridScheduler`]: video while regions change, lossless tile
//!   refinement once they are static for 80 ms, or tiles-only mode.
//! * [`audio`] — Opus [`AudioEncoder`] / [`AudioDecoder`] (48 kHz, FEC, PLC), adaptive
//!   [`JitterBuffer`], [`AudioReceiver`] and a streaming [`Resampler`].

pub mod annexb;
pub mod audio;
mod audio_jitter;
mod audio_opus;
mod audio_resample;
pub mod scheduler;
pub mod tiles;
#[cfg(target_os = "macos")]
pub mod videotoolbox;
#[cfg(target_os = "linux")]
pub mod openh264_codec;
#[cfg(target_os = "windows")]
pub mod mediafoundation;
pub mod mediafoundation_convert;

pub use aether_capture::{BgraImage, CapturedFrame, FramePixels};
pub use aether_protocol::{ChromaFormat, DirtyTile, DirtyTileBatch, Rect, TileEncoding, VideoCodec, VideoFrame};
pub use annexb::{
    H264Sps, HevcSps, ParameterSets, annexb_to_avcc, avc_decoder_config, avcc_to_annexb, hevc_decoder_config,
    nal_units, parameter_sets_from_config,
};
pub use audio::{
    AudioDecoder, AudioEncoder, AudioEncoderConfig, AudioReceiver, AudioReceiverStats, FrameKind, JitterBuffer,
    JitterConfig, JitterStats, OpusApplication, Playout, PushOutcome, Resampler,
};
pub use scheduler::{FramePlan, HybridScheduler, SchedulerConfig, StreamMode};
pub use tiles::{DEFAULT_CACHE_TILES, DEFAULT_TILE_SIZE, Framebuffer, TileConfig, TileDiff, TileEncoder, TileGrid, tile_hash};

/// Errors produced by this crate.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("invalid data: {0}")]
    InvalidData(String),
    /// An OS codec API failed (`OSStatus`).
    #[error("{context} failed with OSStatus {status}")]
    Os { context: &'static str, status: i32 },
    /// A `CacheRef` tile referenced a hash the client cache does not hold (desync).
    #[error("tile cache miss for hash {0:#018x}")]
    CacheMiss(u64),
    /// A decoded literal tile does not match its hash (corruption or cache desync).
    #[error("tile hash mismatch (expected {expected:#018x}, got {actual:#018x})")]
    HashMismatch { expected: u64, actual: u64 },
    #[error(transparent)]
    Capture(#[from] aether_capture::CaptureError),
}

pub type Result<T> = std::result::Result<T, CodecError>;

/// Video encoder parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    pub codec: VideoCodec,
    pub chroma: ChromaFormat,
    pub width: u32,
    pub height: u32,
    pub fps: u16,
    pub bitrate_kbps: u32,
    /// Real-time low-latency rate control, no frame reordering.
    pub low_latency: bool,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            codec: VideoCodec::Hevc,
            chroma: ChromaFormat::Yuv444,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            low_latency: true,
        }
    }
}

/// One encoded access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Annex-B; keyframes start with the parameter sets (VPS/SPS/PPS or SPS/PPS).
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_us: u64,
    pub codec: VideoCodec,
    /// Chroma format actually present in the bitstream.
    pub chroma: ChromaFormat,
    pub width: u32,
    pub height: u32,
}

impl EncodedFrame {
    /// Wraps the access unit into the wire type.
    pub fn into_video_frame(self, display_id: u32, seq: u64) -> VideoFrame {
        VideoFrame {
            display_id,
            seq,
            pts_us: self.pts_us,
            codec: self.codec,
            chroma: self.chroma,
            keyframe: self.keyframe,
            width: self.width,
            height: self.height,
            data: self.data,
        }
    }
}

/// A (hardware) video encoder.
pub trait VideoEncoder: Send {
    /// Encodes one frame synchronously. `Ok(None)` if the encoder dropped the frame.
    /// A frame with a different size than `config()` reconfigures the encoder (next output
    /// is a keyframe).
    fn encode(&mut self, f: &CapturedFrame, force_keyframe: bool) -> Result<Option<EncodedFrame>>;
    fn set_bitrate(&mut self, kbps: u32) -> Result<()>;
    /// Effective configuration (codec/chroma reflect what the hardware actually produces).
    fn config(&self) -> &EncoderConfig;
}

/// A video decoder producing BGRA images.
pub trait VideoDecoder: Send {
    /// Decodes one access unit. `Ok(None)` while waiting for a keyframe with parameter sets.
    fn decode(&mut self, f: &VideoFrame) -> Result<Option<BgraImage>>;
}

/// Creates the platform encoder. On Apple platforms this is VideoToolbox with the fallback
/// chain HEVC 4:4:4 → HEVC 4:2:0 → H.264 High (see [`EncoderConfig`]); on Windows it is a
/// Media Foundation encoder MFT (hardware first, then the software H.264 MFT; 4:2:0 only,
/// HEVC falls back to H.264 when no HEVC encoder is installed). Check
/// [`VideoEncoder::config`] for what was selected. `VideoCodec::Tiles` is served by
/// [`TileEncoder`], not by this factory.
pub fn create_encoder(cfg: EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
    match cfg.codec {
        VideoCodec::Tiles => Err(CodecError::Unsupported("VideoCodec::Tiles is encoded with TileEncoder")),
        VideoCodec::Av1 => Err(CodecError::Unsupported("AV1 encoding is not available")),
        VideoCodec::H264 | VideoCodec::Hevc => {
            #[cfg(target_os = "macos")]
            {
                Ok(Box::new(videotoolbox::VtEncoder::new(cfg)?))
            }
            #[cfg(target_os = "windows")]
            {
                Ok(Box::new(mediafoundation::MfEncoder::new(cfg)?))
            }
            #[cfg(target_os = "linux")]
            {
                Ok(Box::new(openh264_codec::OpenH264Encoder::new(cfg)?))
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            {
                Err(CodecError::Unsupported("hardware video encoding is not implemented on this OS"))
            }
        }
    }
}

/// Creates the platform decoder for `codec`.
pub fn create_decoder(codec: VideoCodec) -> Result<Box<dyn VideoDecoder>> {
    match codec {
        VideoCodec::Tiles => Err(CodecError::Unsupported("VideoCodec::Tiles is decoded with Framebuffer")),
        VideoCodec::Av1 => Err(CodecError::Unsupported("AV1 decoding is not implemented")),
        VideoCodec::H264 | VideoCodec::Hevc => {
            #[cfg(target_os = "macos")]
            {
                Ok(Box::new(videotoolbox::VtDecoder::new(codec)?))
            }
            #[cfg(target_os = "windows")]
            {
                Ok(Box::new(mediafoundation::MfDecoder::new(codec)?))
            }
            #[cfg(target_os = "linux")]
            {
                Ok(Box::new(openh264_codec::OpenH264Decoder::new(codec)?))
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            {
                Err(CodecError::Unsupported("hardware video decoding is not implemented on this OS"))
            }
        }
    }
}

/// Codec/chroma combinations this machine can encode, best first. Probed once (by encoding
/// a test frame and checking the SPS) and cached. `(Tiles, Yuv444)` — the lossless tile
/// codec — is always present.
pub fn encoder_capabilities() -> Vec<(VideoCodec, ChromaFormat)> {
    #[cfg(target_os = "macos")]
    let hardware = videotoolbox::probe_capabilities();
    #[cfg(target_os = "windows")]
    let hardware = mediafoundation::probe_capabilities();
    #[cfg(target_os = "linux")]
    let hardware = openh264_codec::probe_capabilities();
    // Mobile hosts encode in the platform layer (MediaCodec / ReplayKit + VideoToolbox) and
    // hand frames to aether-core directly; only the portable tile codec lives here.
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let hardware: Vec<(VideoCodec, ChromaFormat)> = Vec::new();
    hardware.into_iter().chain(std::iter::once((VideoCodec::Tiles, ChromaFormat::Yuv444))).collect()
}
