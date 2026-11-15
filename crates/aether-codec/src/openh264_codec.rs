//! OpenH264 (Linux) software H.264 encoder and decoder.
//!
//! OpenH264 is compiled from source by the `openh264` crate (`source` feature), so Linux hosts
//! need no system codec library.
//!
//! ## Encoder ([`OpenH264Encoder`])
//! Constrained Baseline H.264, 4:2:0 only — a request for HEVC and/or 4:4:4 falls back to
//! H.264 4:2:0 (logged with `tracing::info!`, reflected in [`VideoEncoder::config`]), mirroring
//! the VideoToolbox fallback chain. Settings: `SCREEN_CONTENT_REAL_TIME` usage, bitrate rate
//! control (`RC_BITRATE_MODE`; frame skipping enabled because OpenH264 cannot hold the target
//! otherwise — a skipped frame is `Ok(None)` and a pending forced keyframe moves to the next
//! frame), no B-frames (OpenH264 never reorders), single slice, IDR every 2 s (like
//! VideoToolbox's `MaxKeyFrameIntervalDuration`). BT.709 limited-range colour, signalled in
//! the SPS VUI (same tags as the VideoToolbox encoder).
//!
//! * Output is Annex-B; every IDR access unit starts with SPS + PPS (OpenH264 writes them before
//!   each IDR with the `CONSTANT_ID` strategy; the encoder re-inserts the cached ones should an
//!   IDR ever come without them).
//! * A frame of a different size re-creates the encoder; the next output is an IDR.
//! * [`VideoEncoder::set_bitrate`] changes the rate controller's target at runtime
//!   (`ENCODER_OPTION_MAX_BITRATE` + `ENCODER_OPTION_BITRATE`, no new IDR); only if OpenH264
//!   rejects it is the encoder re-created (next frame an IDR).
//! * **Odd sizes**: 4:2:0 needs even dimensions (SPS cropping works in 2-pixel units), so an
//!   odd width/height is padded by one pixel (the last column/row is replicated). The SPS
//!   describes the even size; [`EncodedFrame::width`]/`height` keep the real size, and
//!   [`OpenH264Decoder`] crops the padding away using `VideoFrame::width`/`height`. Other
//!   decoders (VideoToolbox, WebCodecs) show the one replicated pixel column/row.
//! * Limits: 16×16 minimum (like VideoToolbox), 3840×2160 / 2160×3840 maximum (OpenH264 level 5.2).
//!
//! BGRA → I420 conversion is done here (fixed-point BT.709 limited range, 2×2 blocks, chroma
//! from the box-filtered block, honours the source stride). `openh264::formats` is not used:
//! its RGB sources have no stride support and use BT.601.
//!
//! ## Decoder ([`OpenH264Decoder`])
//! Annex-B access units → packed BGRA [`BgraImage`] (alpha 255), converted with the inverse
//! BT.709 limited-range matrix (VideoToolbox-encoded streams are BT.709 too). Returns `Ok(None)`
//! until an access unit carrying SPS + PPS arrives (and while OpenH264 has no picture to show).
//! HEVC is rejected with [`CodecError::Unsupported`].
//!
//! ## Hardware (VAAPI) — not included
//! A VAAPI path (`libva` H.264 `VAEntrypointEncSliceLP`/`EncSlice` + `VAEntrypointVLD`, DRM
//! render node, NV12 surfaces uploaded from BGRA or imported from a dma-buf, same Annex-B/IDR
//! contract as here, probed once like [`probe_capabilities`] and preferred when it works) is
//! deliberately left out: the Linux build/test container has no GPU / render node, so such
//! code could be neither exercised nor verified. OpenH264 is the always-available baseline.

use std::sync::OnceLock;

use aether_capture::{BgraImage, CapturedFrame, FramePixels, TestPattern};
use aether_protocol::{ChromaFormat, VideoCodec, VideoFrame};
use openh264::OpenH264API;
use openh264::Timestamp;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig as OhConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::YUVSource;
use openh264_sys2::{ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SBitrateInfo, SPATIAL_LAYER_0, SPATIAL_LAYER_ALL};

use crate::annexb::{self, H264Sps, ParameterSets, h264_nal_type, nal_type, push_annexb_nal};
use crate::{CodecError, EncodedFrame, EncoderConfig, Result, VideoDecoder, VideoEncoder};

/// OpenH264 level 5.2 limit (larger dimension × smaller dimension).
const MAX_DIMS: (u32, u32) = (3840, 2160);

fn oh_err(context: &str, e: openh264::Error) -> CodecError {
    CodecError::InvalidData(format!("OpenH264 {context}: {e}"))
}

#[inline]
fn even(v: u32) -> u32 {
    v + (v & 1)
}

fn check_size(width: u32, height: u32) -> Result<()> {
    if width < 16 || height < 16 {
        return Err(CodecError::InvalidData(format!("frame size {width}x{height} too small")));
    }
    let (w, h) = (even(width), even(height));
    if w.max(h) > MAX_DIMS.0 || w.min(h) > MAX_DIMS.1 {
        return Err(CodecError::Unsupported("OpenH264 encodes at most 3840x2160 (or 2160x3840)"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Colour conversion (BT.709, limited range, fixed point)
// ---------------------------------------------------------------------------------------------

/// Tightly packed I420 picture with even dimensions.
struct I420Buffer {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420Buffer {
    fn new(width: u32, height: u32) -> Self {
        let (w, h) = (even(width) as usize, even(height) as usize);
        Self { width: w, height: h, y: vec![0; w * h], u: vec![128; w * h / 4], v: vec![128; w * h / 4] }
    }
}

impl YUVSource for I420Buffer {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

// RGB → YCbCr, ×2^14. Y = 16 + 219/255·(0.2126 R + 0.7152 G + 0.0722 B);
// Cb/Cr = 128 + 224/255·(B−Y')/1.8556, (R−Y')/1.5748.
const YR: i32 = 2992;
const YG: i32 = 10064;
const YB: i32 = 1016;
const UR: i32 = -1649;
const UG: i32 = -5547;
const UB: i32 = 7196;
const VR: i32 = 7196;
const VG: i32 = -6536;
const VB: i32 = -660;
// YCbCr → RGB, ×2^14 (255/219 and 255/224-scaled BT.709 inverse).
const KY: i32 = 19077;
const KRV: i32 = 29372;
const KGU: i32 = -3494;
const KGV: i32 = -8731;
const KBU: i32 = 34610;

#[inline(always)]
fn luma(b: i32, g: i32, r: i32) -> u8 {
    ((YR * r + YG * g + YB * b + (16 << 14) + (1 << 13)) >> 14) as u8
}

/// Converts `w`×`h` BGRA rows (`stride` bytes apart) into `dst` (whose size is `w`, `h` rounded
/// up to even; the padding replicates the last column/row). Chroma is computed from the
/// average of each 2×2 block.
fn bgra_to_i420(src: &[u8], stride: usize, w: usize, h: usize, dst: &mut I420Buffer) {
    debug_assert!(w >= 1 && h >= 1 && dst.width == w + (w & 1) && dst.height == h + (h & 1));
    let ew = dst.width;
    let cw = ew / 2;
    let row = |y: usize| &src[y * stride..y * stride + w * 4];
    for (by, ((yrows, u_row), v_row)) in
        dst.y.chunks_exact_mut(2 * ew).zip(dst.u.chunks_exact_mut(cw)).zip(dst.v.chunks_exact_mut(cw)).enumerate()
    {
        let y0 = 2 * by;
        let (r0, r1) = (row(y0), row((y0 + 1).min(h - 1)));
        let (ya, yb) = yrows.split_at_mut(ew);
        for bx in 0..cw {
            let x0 = 2 * bx * 4;
            let x1 = (2 * bx + 1).min(w - 1) * 4;
            let px = |r: &[u8], o: usize| (r[o] as i32, r[o + 1] as i32, r[o + 2] as i32);
            let (b00, g00, r00) = px(r0, x0);
            let (b01, g01, r01) = px(r0, x1);
            let (b10, g10, r10) = px(r1, x0);
            let (b11, g11, r11) = px(r1, x1);
            ya[2 * bx] = luma(b00, g00, r00);
            ya[2 * bx + 1] = luma(b01, g01, r01);
            yb[2 * bx] = luma(b10, g10, r10);
            yb[2 * bx + 1] = luma(b11, g11, r11);
            let (sb, sg, sr) = (b00 + b01 + b10 + b11, g00 + g01 + g10 + g11, r00 + r01 + r10 + r11);
            // sums of 4 pixels: shift by 14 + 2
            u_row[bx] = ((UR * sr + UG * sg + UB * sb + (128 << 16) + (1 << 15)) >> 16) as u8;
            v_row[bx] = ((VR * sr + VG * sg + VB * sb + (128 << 16) + (1 << 15)) >> 16) as u8;
        }
    }
}

/// Converts the top-left `dst.width`×`dst.height` of an I420 picture into packed BGRA
/// (alpha 255).
fn i420_to_bgra(y: &[u8], y_stride: usize, u: &[u8], u_stride: usize, v: &[u8], v_stride: usize, dst: &mut BgraImage) {
    let w = dst.width as usize;
    for row_idx in 0..dst.height {
        let yr = &y[row_idx as usize * y_stride..][..w];
        let c = row_idx as usize / 2;
        let ur = &u[c * u_stride..][..w.div_ceil(2)];
        let vr = &v[c * v_stride..][..w.div_ceil(2)];
        let out = &mut dst.row_mut(row_idx)[..w * 4];
        for (cx, (pair, ys)) in out.chunks_mut(8).zip(yr.chunks(2)).enumerate() {
            let d = ur[cx] as i32 - 128;
            let e = vr[cx] as i32 - 128;
            let (rc, gc, bc) = (KRV * e + (1 << 13), KGU * d + KGV * e + (1 << 13), KBU * d + (1 << 13));
            for (px, &yv) in pair.as_chunks_mut::<4>().0.iter_mut().zip(ys) {
                let l = KY * (yv as i32 - 16);
                px[0] = ((l + bc) >> 14).clamp(0, 255) as u8;
                px[1] = ((l + gc) >> 14).clamp(0, 255) as u8;
                px[2] = ((l + rc) >> 14).clamp(0, 255) as u8;
                px[3] = 0xFF;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------------------------

fn bitrate_bps(kbps: u32) -> u32 {
    kbps.saturating_mul(1000).min(i32::MAX as u32)
}

fn openh264_config(cfg: &EncoderConfig) -> OhConfig {
    let fps = cfg.fps.max(1);
    OhConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(bitrate_bps(cfg.bitrate_kbps)))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        // OpenH264 can only hold the target bitrate when it may drop frames (`Ok(None)`).
        .skip_frames(true)
        .intra_frame_period(IntraFramePeriod::from_num_frames(fps as u32 * 2))
        // screen-content mode requires these (it forces/turns them off itself with a warning)
        .scene_change_detect(true)
        .adaptive_quantization(false)
        .background_detection(false)
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        .complexity(Complexity::Medium)
        .vui(VuiConfig::bt709())
}

fn open_encoder(cfg: &EncoderConfig) -> Result<Encoder> {
    Encoder::with_api_config(OpenH264API::from_source(), openh264_config(cfg)).map_err(|e| oh_err("encoder creation", e))
}

/// SPS + PPS (Annex-B) of an access unit, if it carries both.
fn parameter_set_nals(au: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut sps, mut pps) = (false, false);
    for nal in annexb::nal_units(au) {
        match h264_nal_type(nal) {
            nal_type::H264_SPS => sps = true,
            nal_type::H264_PPS => pps = true,
            _ => continue,
        }
        push_annexb_nal(&mut out, nal);
    }
    (sps && pps).then_some(out)
}

/// OpenH264 software H.264 encoder (see module docs).
pub struct OpenH264Encoder {
    encoder: Encoder,
    cfg: EncoderConfig,
    yuv: I420Buffer,
    /// The current `encoder` has encoded (and thus initialised) at least one frame.
    initialized: bool,
    force_next_keyframe: bool,
    /// SPS + PPS of the current encoder instance (Annex-B), for IDRs that lack them.
    param_sets: Option<Vec<u8>>,
}

impl OpenH264Encoder {
    /// Creates an H.264 4:2:0 encoder; other codec/chroma requests fall back to it.
    pub fn new(mut cfg: EncoderConfig) -> Result<Self> {
        check_size(cfg.width, cfg.height)?;
        if (cfg.codec, cfg.chroma) != (VideoCodec::H264, ChromaFormat::Yuv420) {
            tracing::info!(
                requested = ?(cfg.codec, cfg.chroma),
                selected = ?(VideoCodec::H264, ChromaFormat::Yuv420),
                "OpenH264 fallback"
            );
            cfg.codec = VideoCodec::H264;
            cfg.chroma = ChromaFormat::Yuv420;
        }
        if cfg.bitrate_kbps == 0 {
            return Err(CodecError::InvalidData("bitrate must be > 0".into()));
        }
        let encoder = open_encoder(&cfg)?;
        let yuv = I420Buffer::new(cfg.width, cfg.height);
        Ok(Self { encoder, cfg, yuv, initialized: false, force_next_keyframe: true, param_sets: None })
    }

    fn reconfigure(&mut self, width: u32, height: u32) -> Result<()> {
        let mut cfg = self.cfg.clone();
        cfg.width = width;
        cfg.height = height;
        self.encoder = open_encoder(&cfg)?;
        self.cfg = cfg;
        self.yuv = I420Buffer::new(width, height);
        self.initialized = false;
        self.force_next_keyframe = true;
        self.param_sets = None;
        Ok(())
    }

    /// Sets target and maximum bitrate on the running encoder. OpenH264 rejects a layer
    /// maximum below its target, so when lowering the target goes first, when raising the
    /// maximum does.
    fn apply_bitrate(&mut self, bps: u32) -> bool {
        let lowering = bps < bitrate_bps(self.cfg.bitrate_kbps);
        let bps = bps as i32;
        let max = [(ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_ALL), (ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0)];
        let target = [(ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL)];
        let steps: Vec<_> = if lowering { target.iter().chain(&max).collect() } else { max.iter().chain(&target).collect() };
        // SAFETY: the encoder is initialised (`self.initialized`); the options only update rate
        // control targets and read the `SBitrateInfo` we pass for the duration of the call.
        unsafe {
            let api = self.encoder.raw_api();
            for &(opt, layer) in steps {
                let mut info = SBitrateInfo { iLayer: layer, iBitrate: bps };
                let rc = api.set_option(opt, (&raw mut info).cast());
                if rc != 0 {
                    tracing::debug!(opt, layer, rc, "OpenH264 SetOption(bitrate) failed");
                    return false;
                }
            }
        }
        true
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn encode(&mut self, f: &CapturedFrame, force_keyframe: bool) -> Result<Option<EncodedFrame>> {
        check_size(f.width, f.height)?;
        if (f.width, f.height) != (self.cfg.width, self.cfg.height) {
            self.reconfigure(f.width, f.height)?;
        }
        let owned;
        #[allow(irrefutable_let_patterns)] // only `Bgra` exists on Linux today
        let (data, stride): (&[u8], usize) = if let FramePixels::Bgra { stride, data } = &f.pixels {
            (data, *stride)
        } else {
            owned = f.to_bgra()?;
            (&owned.data, owned.stride)
        };
        let (w, h) = (f.width as usize, f.height as usize);
        if stride < w * 4 || data.len() < stride * (h - 1) + w * 4 {
            return Err(CodecError::InvalidData("BGRA buffer too small".into()));
        }
        bgra_to_i420(data, stride, w, h, &mut self.yuv);

        let force = force_keyframe || std::mem::take(&mut self.force_next_keyframe);
        if force && self.initialized {
            // (an uninitialised encoder starts with an IDR anyway)
            self.encoder.force_intra_frame();
        }
        let (frame_type, mut out) = {
            let bs = self
                .encoder
                .encode_at(&self.yuv, Timestamp::from_millis(f.pts_us / 1000))
                .map_err(|e| oh_err("encode", e))?;
            (bs.frame_type(), bs.to_vec())
        };
        self.initialized = true;
        if matches!(frame_type, FrameType::Skip | FrameType::Invalid) || out.is_empty() {
            self.force_next_keyframe |= force;
            return Ok(None);
        }
        let keyframe = frame_type == FrameType::IDR;
        if keyframe {
            match parameter_set_nals(&out) {
                Some(ps) => self.param_sets = Some(ps),
                None => match &self.param_sets {
                    Some(ps) => {
                        tracing::debug!("OpenH264 IDR without SPS/PPS; prepending cached parameter sets");
                        let mut with_ps = ps.clone();
                        with_ps.extend_from_slice(&out);
                        out = with_ps;
                    }
                    None => return Err(CodecError::InvalidData("OpenH264 IDR without SPS/PPS".into())),
                },
            }
        } else if force {
            tracing::warn!(?frame_type, "OpenH264 ignored a forced keyframe");
            self.force_next_keyframe = true;
        }
        Ok(Some(EncodedFrame {
            data: out,
            keyframe,
            pts_us: f.pts_us,
            codec: VideoCodec::H264,
            chroma: ChromaFormat::Yuv420,
            width: self.cfg.width,
            height: self.cfg.height,
        }))
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        if kbps == 0 {
            return Err(CodecError::InvalidData("bitrate must be > 0".into()));
        }
        if self.initialized && self.apply_bitrate(bitrate_bps(kbps)) {
            self.cfg.bitrate_kbps = kbps;
            return Ok(());
        }
        if self.initialized {
            tracing::debug!(kbps, "OpenH264 rejected a runtime bitrate change; re-creating the encoder");
        }
        let mut cfg = self.cfg.clone();
        cfg.bitrate_kbps = kbps;
        self.encoder = open_encoder(&cfg)?;
        self.cfg = cfg;
        self.force_next_keyframe |= self.initialized;
        self.initialized = false;
        self.param_sets = None;
        Ok(())
    }

    fn config(&self) -> &EncoderConfig {
        &self.cfg
    }
}

/// Codec/chroma pairs OpenH264 produces on this machine (probed once by encoding a test frame
/// and checking its SPS): `[(H264, Yuv420)]`, or empty if the probe fails.
pub fn probe_capabilities() -> Vec<(VideoCodec, ChromaFormat)> {
    static CAPS: OnceLock<Vec<(VideoCodec, ChromaFormat)>> = OnceLock::new();
    CAPS.get_or_init(|| {
        let ok = (|| -> Result<bool> {
            let mut enc = OpenH264Encoder::new(EncoderConfig {
                codec: VideoCodec::H264,
                chroma: ChromaFormat::Yuv420,
                width: 64,
                height: 64,
                fps: 30,
                bitrate_kbps: 500,
                low_latency: true,
            })?;
            let frame = CapturedFrame::from_image(0, 0, TestPattern::new(64, 64).frame(0), None);
            let Some(out) = enc.encode(&frame, true)? else { return Ok(false) };
            let chroma_idc = ParameterSets::from_annexb(VideoCodec::H264, &out.data)
                .and_then(|ps| H264Sps::parse(&ps.sps).ok())
                .map(|sps| sps.chroma_format_idc);
            Ok(out.keyframe && chroma_idc == Some(1))
        })()
        .unwrap_or_else(|e| {
            tracing::warn!("OpenH264 encoder probe failed: {e}");
            false
        });
        if ok { vec![(VideoCodec::H264, ChromaFormat::Yuv420)] } else { Vec::new() }
    })
    .clone()
}

// ---------------------------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------------------------

/// OpenH264 software H.264 decoder producing packed BGRA images.
pub struct OpenH264Decoder {
    decoder: Decoder,
    have_params: bool,
}

impl OpenH264Decoder {
    pub fn new(codec: VideoCodec) -> Result<Self> {
        if codec != VideoCodec::H264 {
            return Err(CodecError::Unsupported("the OpenH264 decoder supports H.264 only"));
        }
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
            .map_err(|e| oh_err("decoder creation", e))?;
        Ok(Self { decoder, have_params: false })
    }
}

/// Output size: the signalled size when it is the decoded size minus the encoder's odd-size
/// padding pixel, otherwise the decoded size.
fn display_dim(signalled: u32, decoded: usize) -> u32 {
    if signalled % 2 == 1 && signalled as usize + 1 == decoded { signalled } else { decoded as u32 }
}

impl VideoDecoder for OpenH264Decoder {
    fn decode(&mut self, f: &VideoFrame) -> Result<Option<BgraImage>> {
        if f.codec != VideoCodec::H264 {
            return Err(CodecError::InvalidData(format!("decoder is H264, frame is {:?}", f.codec)));
        }
        if !self.have_params {
            if ParameterSets::from_annexb(VideoCodec::H264, &f.data).is_none() {
                return Ok(None);
            }
            self.have_params = true;
        }
        let Some(yuv) = self.decoder.decode(&f.data).map_err(|e| oh_err("decode", e))? else { return Ok(None) };
        let (dw, dh) = yuv.dimensions();
        let (ys, us, vs) = yuv.strides();
        let mut img = BgraImage::new(display_dim(f.width, dw), display_dim(f.height, dh));
        if img.width == 0 || img.height == 0 {
            return Ok(None);
        }
        i420_to_bgra(yuv.y(), ys, yuv.u(), us, yuv.v(), vs, &mut img);
        Ok(Some(img))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;
    use crate::annexb::BitReader;
    use crate::{create_decoder, create_encoder, encoder_capabilities};

    fn cfg(w: u32, h: u32, kbps: u32) -> EncoderConfig {
        EncoderConfig {
            codec: VideoCodec::H264,
            chroma: ChromaFormat::Yuv420,
            width: w,
            height: h,
            fps: 30,
            bitrate_kbps: kbps,
            low_latency: true,
        }
    }

    fn frame(p: &TestPattern, i: u64) -> CapturedFrame {
        CapturedFrame::from_image(0, i * 33_333, p.frame(i), None)
    }

    /// Moving high-entropy content (hard to compress) so the rate controller is the
    /// limiting factor: noise blocks scrolled by `i` pixels per frame.
    fn busy_frame(w: u32, h: u32, i: u64) -> CapturedFrame {
        let mut img = BgraImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let (bx, by) = ((x as u64 + i * 3) / 4, y as u64 / 4);
                let mut z = bx.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ by.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
                z ^= z >> 29;
                z = z.wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z ^= z >> 32;
                let o = y as usize * img.stride + x as usize * 4;
                img.data[o..o + 4].copy_from_slice(&[z as u8, (z >> 8) as u8, (z >> 16) as u8, 0xFF]);
            }
        }
        CapturedFrame::from_image(0, i * 33_333, img, None)
    }

    fn nal_types(au: &[u8]) -> Vec<u8> {
        annexb::nal_units(au).map(h264_nal_type).collect()
    }

    /// Coded (cropped) size from an H.264 SPS. Test-only: no scaling lists.
    fn sps_size(sps: &[u8]) -> (u32, u32) {
        let rbsp = annexb::nal_to_rbsp(&sps[1..]);
        let mut r = BitReader::new(&rbsp);
        let profile = r.bits(8).unwrap();
        r.bits(16).unwrap();
        r.ue().unwrap();
        if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
            if r.ue().unwrap() == 3 {
                r.bit().unwrap();
            }
            r.ue().unwrap();
            r.ue().unwrap();
            r.bit().unwrap();
            assert_eq!(r.bit().unwrap(), 0, "scaling matrices not supported by this helper");
        }
        r.ue().unwrap();
        match r.ue().unwrap() {
            0 => {
                r.ue().unwrap();
            }
            1 => {
                r.bit().unwrap();
                r.se().unwrap();
                r.se().unwrap();
                for _ in 0..r.ue().unwrap() {
                    r.se().unwrap();
                }
            }
            _ => {}
        }
        r.ue().unwrap();
        r.bit().unwrap();
        let w_mbs = r.ue().unwrap() + 1;
        let h_units = r.ue().unwrap() + 1;
        let frame_mbs_only = r.bit().unwrap();
        if frame_mbs_only == 0 {
            r.bit().unwrap();
        }
        r.bit().unwrap();
        let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
        if r.bit().unwrap() == 1 {
            (cl, cr, ct, cb) = (r.ue().unwrap(), r.ue().unwrap(), r.ue().unwrap(), r.ue().unwrap());
        }
        let h_mul = 2 - frame_mbs_only;
        (w_mbs * 16 - 2 * (cl + cr), h_mul * h_units * 16 - 2 * h_mul * (ct + cb))
    }

    fn mae_psnr(a: &BgraImage, b: &BgraImage) -> (f64, f64) {
        assert_eq!((a.width, a.height), (b.width, b.height));
        let (mut ae, mut se, mut n) = (0f64, 0f64, 0f64);
        for y in 0..a.height {
            let (ra, rb) = (a.row(y), b.row(y));
            for x in 0..a.width as usize {
                for c in 0..3 {
                    let d = ra[x * 4 + c] as f64 - rb[x * 4 + c] as f64;
                    ae += d.abs();
                    se += d * d;
                    n += 1.0;
                }
            }
        }
        let mse = se / n;
        (ae / n, if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / mse).log10() })
    }

    #[test]
    fn capabilities_report_h264_420() {
        let caps = encoder_capabilities();
        println!("encoder capabilities: {caps:?}");
        assert_eq!(probe_capabilities(), vec![(VideoCodec::H264, ChromaFormat::Yuv420)]);
        assert!(caps.contains(&(VideoCodec::H264, ChromaFormat::Yuv420)));
        assert!(caps.contains(&(VideoCodec::Tiles, ChromaFormat::Yuv444)));
    }

    #[test]
    fn solid_colours_roundtrip_through_i420() {
        let colours: [[u8; 3]; 10] = [
            [0, 0, 0],
            [255, 255, 255],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [255, 255, 0],
            [0, 255, 255],
            [128, 128, 128],
            [30, 144, 255],
            [250, 128, 114],
        ];
        // odd size + padded stride to exercise edge replication and stride handling
        let (w, h, stride) = (17usize, 9usize, 17 * 4 + 12);
        for [r, g, b] in colours {
            let mut src = vec![0xEEu8; stride * h];
            for y in 0..h {
                for x in 0..w {
                    src[y * stride + x * 4..][..4].copy_from_slice(&[b, g, r, 0xFF]);
                }
            }
            let mut yuv = I420Buffer::new(w as u32, h as u32);
            bgra_to_i420(&src, stride, w, h, &mut yuv);
            assert_eq!((yuv.width, yuv.height), (18, 10));
            // Y against the floating-point BT.709 limited-range definition
            let yf = 16.0 + 219.0 / 255.0 * (0.2126 * r as f64 + 0.7152 * g as f64 + 0.0722 * b as f64);
            assert!((yuv.y[0] as f64 - yf).abs() <= 0.6, "Y {} vs {yf}", yuv.y[0]);
            let mut out = BgraImage::new(w as u32, h as u32);
            i420_to_bgra(&yuv.y, yuv.width, &yuv.u, yuv.width / 2, &yuv.v, yuv.width / 2, &mut out);
            for y in 0..h as u32 {
                for x in 0..w as u32 {
                    let p = out.pixel(x, y);
                    for (got, want) in p[..3].iter().zip([b, g, r]) {
                        assert!((*got as i32 - want as i32).abs() <= 3, "{:?} → {p:?} at {x},{y}", [r, g, b]);
                    }
                    assert_eq!(p[3], 0xFF);
                }
            }
        }
    }

    fn check_stream(w: u32, h: u32) {
        let mut enc = create_encoder(cfg(w, h, 4_000)).unwrap();
        let p = TestPattern::new(w, h);
        for i in 0..12u64 {
            let force = i == 6;
            let out = enc.encode(&frame(&p, i), force).unwrap().expect("no frame skipping");
            assert_eq!((out.width, out.height), (w, h));
            assert_eq!((out.codec, out.chroma), (VideoCodec::H264, ChromaFormat::Yuv420));
            let types = nal_types(&out.data);
            if i == 0 || force {
                assert!(out.keyframe, "frame {i} must be a keyframe");
                assert_eq!(&types[..2], &[nal_type::H264_SPS, nal_type::H264_PPS], "frame {i}: {types:?}");
                assert!(types.contains(&nal_type::H264_IDR), "frame {i}: {types:?}");
                let ps = ParameterSets::from_annexb(VideoCodec::H264, &out.data).unwrap();
                let sps = H264Sps::parse(&ps.sps).unwrap();
                assert_eq!(sps.chroma_format_idc, 1);
                assert_eq!(sps_size(&ps.sps), (even(w), even(h)));
                assert!(ps.codec_string().unwrap().starts_with("avc1."));
                ps.decoder_config().unwrap();
            } else {
                assert!(!out.keyframe, "frame {i} unexpectedly a keyframe");
                assert!(types.iter().all(|&t| t == 1), "frame {i} should be a non-IDR (P) slice: {types:?}");
            }
        }
    }

    #[test]
    fn keyframes_carry_parameter_sets_640x360() {
        check_stream(640, 360);
    }

    #[test]
    fn keyframes_carry_parameter_sets_odd_333x201() {
        check_stream(333, 201);
    }

    fn roundtrip(w: u32, h: u32, kbps: u32) -> (f64, f64) {
        let mut enc = create_encoder(cfg(w, h, kbps)).unwrap();
        let mut dec = create_decoder(VideoCodec::H264).unwrap();
        let p = TestPattern::new(w, h);
        let (mut worst_mae, mut min_psnr) = (0f64, f64::INFINITY);
        for i in 0..20u64 {
            let src = p.frame(i);
            let out = enc.encode(&CapturedFrame::from_image(0, i * 33_333, src.clone(), None), false).unwrap().unwrap();
            let img = dec.decode(&out.into_video_frame(0, i)).unwrap().expect("decoded image");
            assert_eq!((img.width, img.height, img.stride), (w, h, w as usize * 4));
            let (mae, psnr) = mae_psnr(&src, &img);
            worst_mae = worst_mae.max(mae);
            min_psnr = min_psnr.min(psnr);
        }
        println!("{w}x{h} @ {kbps} kbps: worst MAE {worst_mae:.2}, min PSNR {min_psnr:.1} dB");
        (worst_mae, min_psnr)
    }

    #[test]
    fn encode_decode_roundtrip_quality() {
        for (w, h) in [(640, 360), (333, 201)] {
            let (mae, psnr) = roundtrip(w, h, 20_000);
            assert!(mae < 6.0, "{w}x{h}: MAE {mae}");
            assert!(psnr > 30.0, "{w}x{h}: PSNR {psnr}");
        }
    }

    #[test]
    fn strided_input_matches_packed_input() {
        let (w, h) = (96u32, 64u32);
        let img = TestPattern::new(w, h).frame(3);
        let stride = w as usize * 4 + 64;
        let mut padded = vec![0u8; stride * h as usize];
        for y in 0..h {
            padded[y as usize * stride..][..w as usize * 4].copy_from_slice(img.row(y));
        }
        let strided = CapturedFrame {
            display_id: 0,
            width: w,
            height: h,
            pts_us: 0,
            dirty_rects: None,
            pixels: FramePixels::Bgra { stride, data: Arc::from(padded) },
        };
        let a = create_encoder(cfg(w, h, 2_000)).unwrap().encode(&strided, false).unwrap().unwrap();
        let b = create_encoder(cfg(w, h, 2_000)).unwrap().encode(&CapturedFrame::from_image(0, 0, img, None), false).unwrap().unwrap();
        assert_eq!(a.data, b.data);
    }

    #[test]
    fn resolution_change_reconfigures() {
        let mut enc = create_encoder(cfg(320, 240, 2_000)).unwrap();
        let mut dec = create_decoder(VideoCodec::H264).unwrap();
        let sizes = [(320u32, 240u32), (320, 240), (481, 273), (481, 273), (320, 240), (320, 240)];
        for (i, (w, h)) in sizes.into_iter().enumerate() {
            let f = CapturedFrame::from_image(0, i as u64 * 1000, TestPattern::new(w, h).frame(i as u64), None);
            let out = enc.encode(&f, false).unwrap().unwrap();
            assert_eq!(out.keyframe, i % 2 == 0, "frame {i}");
            assert_eq!((out.width, out.height), (w, h));
            assert_eq!(enc.config().width, w);
            let img = dec.decode(&out.into_video_frame(0, i as u64)).unwrap().unwrap();
            assert_eq!((img.width, img.height), (w, h));
        }
    }

    #[test]
    fn set_bitrate_changes_output_rate() {
        let (w, h) = (640, 360);
        let mut enc = create_encoder(cfg(w, h, 8_000)).unwrap();
        let mut sizes = Vec::new();
        for i in 0..60u64 {
            if i == 30 {
                enc.set_bitrate(250).unwrap();
                assert_eq!(enc.config().bitrate_kbps, 250);
            }
            // (a frame skipped by the rate controller counts as 0 bytes)
            // (noise content may trigger scene-change IDRs, so keyframes are checked below)
            let out = enc.encode(&busy_frame(w, h, i), false).unwrap();
            sizes.push(out.map_or(0, |o| o.data.len()));
        }
        // skip the start-up IDR and the rate controller's adaptation period
        let high = sizes[5..30].iter().sum::<usize>() as f64 / 25.0;
        let low = sizes[40..60].iter().sum::<usize>() as f64 / 20.0;
        let skipped = sizes.iter().filter(|&&n| n == 0).count();
        println!("avg bytes/frame: {high:.0} @ 8000 kbps → {low:.0} @ 250 kbps ({skipped} skipped)");
        assert!(low < high * 0.6, "bitrate change had no effect ({high} → {low})");
        // A runtime bitrate change (down and up) does not force an IDR.
        let p = TestPattern::new(w, h);
        let mut enc = create_encoder(cfg(w, h, 4_000)).unwrap();
        for i in 0..30u64 {
            match i {
                10 => enc.set_bitrate(500).unwrap(),
                20 => enc.set_bitrate(6_000).unwrap(),
                _ => {}
            }
            if let Some(out) = enc.encode(&frame(&p, i), false).unwrap() {
                assert_eq!(out.keyframe, i == 0, "bitrate change forced an IDR (frame {i})");
            }
        }
        // before the first frame: re-creates the encoder, first output still an IDR
        let mut enc = create_encoder(cfg(w, h, 1_000)).unwrap();
        enc.set_bitrate(3_000).unwrap();
        assert_eq!(enc.config().bitrate_kbps, 3_000);
        assert!(enc.encode(&frame(&p, 0), false).unwrap().unwrap().keyframe);
        assert!(enc.set_bitrate(0).is_err());
    }

    #[test]
    fn fallback_rejections_and_waiting() {
        let enc = create_encoder(EncoderConfig { codec: VideoCodec::Hevc, chroma: ChromaFormat::Yuv444, ..cfg(64, 64, 1_000) })
            .unwrap();
        assert_eq!((enc.config().codec, enc.config().chroma), (VideoCodec::H264, ChromaFormat::Yuv420));
        assert!(matches!(create_encoder(cfg(8, 64, 1_000)), Err(CodecError::InvalidData(_))));
        let mut enc = create_encoder(cfg(64, 64, 1_000)).unwrap();
        let tiny = CapturedFrame::from_image(0, 0, BgraImage::new(15, 64), None);
        assert!(matches!(enc.encode(&tiny, false), Err(CodecError::InvalidData(_))));
        assert!(matches!(create_decoder(VideoCodec::Hevc), Err(CodecError::Unsupported(_))));

        // decoder: P frame before any parameter sets → Ok(None), then the IDR decodes
        let p = TestPattern::new(64, 64);
        let first = enc.encode(&frame(&p, 0), false).unwrap().unwrap();
        let second = enc.encode(&frame(&p, 1), false).unwrap().unwrap();
        let mut dec = OpenH264Decoder::new(VideoCodec::H264).unwrap();
        assert!(dec.decode(&second.clone().into_video_frame(0, 1)).unwrap().is_none());
        assert!(dec.decode(&first.into_video_frame(0, 0)).unwrap().is_some());
        assert!(dec.decode(&second.into_video_frame(0, 1)).unwrap().is_some());
    }

    #[test]
    fn encode_1080p_timing() {
        let (w, h) = (1920, 1080);
        let mut enc = create_encoder(cfg(w, h, 20_000)).unwrap();
        let p = TestPattern::new(w, h);
        let images: Vec<_> = (0..10).map(|i| p.frame(i)).collect();
        let mut yuv = I420Buffer::new(w, h);
        let t = Instant::now();
        for img in &images {
            bgra_to_i420(&img.data, img.stride, w as usize, h as usize, &mut yuv);
        }
        let frames: Vec<_> = images.into_iter().enumerate().map(|(i, img)| CapturedFrame::from_image(0, i as u64 * 33_333, img, None)).collect();
        let convert_ms = t.elapsed().as_secs_f64() * 100.0;
        let t = Instant::now();
        for (i, f) in frames.iter().enumerate() {
            assert!(enc.encode(f, false).unwrap().unwrap().keyframe == (i == 0));
        }
        let encode_ms = t.elapsed().as_secs_f64() * 100.0;
        println!("1080p: BGRA→I420 {convert_ms:.2} ms/frame, convert+encode {encode_ms:.2} ms/frame (10 frames)");
    }
}
