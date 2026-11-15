//! VideoToolbox (macOS) hardware encoder and decoder.
//!
//! ## Encoder
//! `VTCompressionSession` per [`VtEncoder`], BGRA input (zero copy for ScreenCaptureKit
//! `CVPixelBuffer`s, pooled IOSurface copy for CPU frames). Settings: `RealTime`, no frame
//! reordering (no B-frames), keyframe every ≤ 2 s, `ExpectedFrameRate`, `AverageBitRate`,
//! speed over quality, BT.709 colour tags, and — when `low_latency` — the
//! `EnableLowLatencyRateControl` encoder specification.
//!
//! Variant selection (`HEVC 4:4:4 → HEVC 4:2:0 → H.264 High`): each candidate profile is
//! probed once per process by encoding a test frame and reading `chroma_format_idc` from the
//! produced SPS (VideoToolbox silently accepts some profiles it then does not honour). A
//! 4:4:4 variant without the low-latency rate controller is preferred over a 4:2:0 one with
//! it (both are real-time, reorder-free); see [`VtEncoder::uses_low_latency_rate_control`].
//!
//! Output is Annex-B; keyframes are prefixed with the parameter sets.
//!
//! ## Decoder
//! `VTDecompressionSession` built from the in-band parameter sets
//! (`CMVideoFormatDescriptionCreateFrom{H264,HEVC}ParameterSets`), BGRA output.

use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::{self, NonNull};
use std::sync::{Mutex, OnceLock};

use aether_capture::macos::PixelBuffer;
use aether_capture::{BgraImage, CapturedFrame, FramePixels, TestPattern};
use aether_protocol::{ChromaFormat, VideoCodec, VideoFrame};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, CMVideoFormatDescriptionCreateFromHEVCParameterSets,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_32BGRA,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession, VTEncodeInfoFlags, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ColorPrimaries,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
};

use crate::annexb::{self, H264Sps, HevcSps, ParameterSets, h264_nal_type, hevc_nal_type, nal_type, push_annexb_nal};
use crate::{CodecError, EncodedFrame, EncoderConfig, Result, VideoDecoder, VideoEncoder};

type OSStatus = i32;

fn check(context: &'static str, status: OSStatus) -> Result<()> {
    if status == 0 { Ok(()) } else { Err(CodecError::Os { context, status }) }
}

fn cf_dict(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
    let keys: Vec<&CFString> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<&CFType> = pairs.iter().map(|p| p.1).collect();
    CFDictionary::from_slices(&keys, &values)
}

fn us_time(us: u64) -> CMTime {
    CMTime { value: us as i64, timescale: 1_000_000, flags: CMTimeFlags::Valid, epoch: 0 }
}

fn invalid_time() -> CMTime {
    CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 }
}

unsafe extern "C-unwind" {
    fn CFArrayGetValueAtIndex(arr: &CFArray, idx: isize) -> *const c_void;
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFBooleanGetValue(b: *const c_void) -> u8;
}

// ---------------------------------------------------------------------------------------------
// Variants & probing
// ---------------------------------------------------------------------------------------------

/// One concrete encoder configuration candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Variant {
    codec: VideoCodec,
    chroma: ChromaFormat,
    low_latency: bool,
}

impl Variant {
    /// `kVTProfileLevel_*` value. The 4:4:4 profiles are not exported as SDK constants but are
    /// advertised by the hardware encoders in `VTCopySupportedPropertyDictionaryForEncoder`.
    fn profile(&self) -> &'static str {
        match (self.codec, self.chroma) {
            (VideoCodec::Hevc, ChromaFormat::Yuv444) => "HEVC_Main444_AutoLevel",
            (VideoCodec::Hevc, ChromaFormat::Yuv420) => "HEVC_Main_AutoLevel",
            (_, ChromaFormat::Yuv444) => "H264_High444Predictive_AutoLevel",
            (_, ChromaFormat::Yuv420) => "H264_High_AutoLevel",
        }
    }

    fn codec_type(&self) -> u32 {
        if self.codec == VideoCodec::Hevc { kCMVideoCodecType_HEVC } else { kCMVideoCodecType_H264 }
    }
}

fn fallback_chain(cfg: &EncoderConfig) -> Vec<Variant> {
    use ChromaFormat::*;
    use VideoCodec::*;
    let combos: &[(VideoCodec, ChromaFormat)] = match (cfg.codec, cfg.chroma) {
        (Hevc, Yuv444) => &[(Hevc, Yuv444), (Hevc, Yuv420), (H264, Yuv420)],
        (Hevc, Yuv420) => &[(Hevc, Yuv420), (H264, Yuv420)],
        (H264, Yuv444) => &[(H264, Yuv444), (H264, Yuv420)],
        _ => &[(H264, Yuv420)],
    };
    let lls: &[bool] = if cfg.low_latency { &[true, false] } else { &[false] };
    combos.iter().flat_map(|&(codec, chroma)| lls.iter().map(move |&low_latency| Variant { codec, chroma, low_latency })).collect()
}

/// Chroma format signalled by the SPS of an Annex-B keyframe.
fn bitstream_chroma(codec: VideoCodec, annexb: &[u8]) -> Option<ChromaFormat> {
    let ps = ParameterSets::from_annexb(codec, annexb)?;
    let idc = match codec {
        VideoCodec::Hevc => HevcSps::parse(&ps.sps).ok()?.chroma_format_idc,
        _ => H264Sps::parse(&ps.sps).ok()?.chroma_format_idc,
    };
    match idc {
        1 => Some(ChromaFormat::Yuv420),
        3 => Some(ChromaFormat::Yuv444),
        _ => None,
    }
}

/// True when `v` can be opened *and* produces the requested chroma format.
fn variant_works(v: Variant) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<Variant, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(r) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&v) {
        return *r;
    }
    let ok = (|| -> Result<bool> {
        let cfg = EncoderConfig {
            codec: v.codec,
            chroma: v.chroma,
            width: 640,
            height: 360,
            fps: 30,
            bitrate_kbps: 4_000,
            low_latency: v.low_latency,
        };
        let mut enc = VtEncoder::with_variant(cfg, v)?;
        let img = TestPattern::new(640, 360).frame(0);
        let frame = CapturedFrame::from_image(0, 0, img, None);
        let out = enc.encode_raw(&frame, true)?;
        Ok(out.is_some_and(|o| o.keyframe && bitstream_chroma(v.codec, &o.data) == Some(v.chroma)))
    })()
    .unwrap_or_else(|e| {
        tracing::debug!(?v, "VideoToolbox variant unavailable: {e}");
        false
    });
    cache.lock().unwrap_or_else(|e| e.into_inner()).insert(v, ok);
    ok
}

/// Codec/chroma pairs the hardware can produce (any rate-control mode), best first.
pub fn probe_capabilities() -> Vec<(VideoCodec, ChromaFormat)> {
    static CAPS: OnceLock<Vec<(VideoCodec, ChromaFormat)>> = OnceLock::new();
    CAPS.get_or_init(|| {
        [
            (VideoCodec::Hevc, ChromaFormat::Yuv444),
            (VideoCodec::Hevc, ChromaFormat::Yuv420),
            (VideoCodec::H264, ChromaFormat::Yuv444),
            (VideoCodec::H264, ChromaFormat::Yuv420),
        ]
        .into_iter()
        .filter(|&(codec, chroma)| {
            variant_works(Variant { codec, chroma, low_latency: true })
                || variant_works(Variant { codec, chroma, low_latency: false })
        })
        .collect()
    })
    .clone()
}

/// Detailed probe result: `(codec, chroma, low_latency_rate_control)` for every variant
/// that works on this machine.
pub fn probe_variants() -> Vec<(VideoCodec, ChromaFormat, bool)> {
    let mut out = Vec::new();
    for codec in [VideoCodec::Hevc, VideoCodec::H264] {
        for chroma in [ChromaFormat::Yuv444, ChromaFormat::Yuv420] {
            for ll in [true, false] {
                if variant_works(Variant { codec, chroma, low_latency: ll }) {
                    out.push((codec, chroma, ll));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------------------------

struct EncodedOut {
    data: Vec<u8>,
    keyframe: bool,
}

struct EncShared {
    codec: VideoCodec,
    out: Mutex<Vec<Result<EncodedOut>>>,
}

fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    let Some(arr) = (unsafe { sample.sample_attachments_array(false) }) else { return true };
    if arr.is_empty() {
        return true;
    }
    unsafe {
        let dict = CFArrayGetValueAtIndex(&arr, 0);
        if dict.is_null() {
            return true;
        }
        let key: *const CFString = kCMSampleAttachmentKey_NotSync;
        let v = CFDictionaryGetValue(dict, key.cast());
        v.is_null() || CFBooleanGetValue(v) == 0
    }
}

fn sample_to_annexb(sample: &CMSampleBuffer, codec: VideoCodec) -> Result<EncodedOut> {
    let keyframe = is_keyframe(sample);
    let mut out = Vec::new();
    let mut nal_len_size: i32 = 4;
    if let Some(fd) = unsafe { sample.format_description() } {
        let mut count = 0usize;
        let mut i = 0usize;
        loop {
            let mut p: *const u8 = ptr::null();
            let mut size = 0usize;
            let st = unsafe {
                if codec == VideoCodec::Hevc {
                    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(&fd, i, &mut p, &mut size, &mut count, &mut nal_len_size)
                } else {
                    CMVideoFormatDescriptionGetH264ParameterSetAtIndex(&fd, i, &mut p, &mut size, &mut count, &mut nal_len_size)
                }
            };
            if st != 0 || p.is_null() {
                break;
            }
            if keyframe {
                push_annexb_nal(&mut out, unsafe { std::slice::from_raw_parts(p, size) });
            }
            i += 1;
            if i >= count {
                break;
            }
        }
    }
    let block = unsafe { sample.data_buffer() }.ok_or_else(|| CodecError::InvalidData("sample without data".into()))?;
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    if len > 0 {
        let st = unsafe { block.copy_data_bytes(0, len, NonNull::new_unchecked(avcc.as_mut_ptr().cast())) };
        check("CMBlockBufferCopyDataBytes", st)?;
    }
    let nal_len_size = if matches!(nal_len_size, 1 | 2 | 4) { nal_len_size as usize } else { 4 };
    for nal in annexb::length_prefixed_nals(&avcc, nal_len_size)? {
        let ty = if codec == VideoCodec::Hevc { hevc_nal_type(nal) } else { h264_nal_type(nal) };
        // parameter sets were already emitted from the format description
        let is_ps = match codec {
            VideoCodec::Hevc => matches!(ty, nal_type::HEVC_VPS | nal_type::HEVC_SPS | nal_type::HEVC_PPS),
            _ => matches!(ty, nal_type::H264_SPS | nal_type::H264_PPS),
        };
        if keyframe && is_ps {
            continue;
        }
        push_annexb_nal(&mut out, nal);
    }
    Ok(EncodedOut { data: out, keyframe })
}

unsafe extern "C-unwind" fn compression_output(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: OSStatus,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: refcon is the `EncShared` boxed by the owning VtEncoder, which invalidates the
    // session before freeing it.
    let shared = unsafe { &*(refcon as *const EncShared) };
    let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if status != 0 {
            return Some(Err(CodecError::Os { context: "VTCompressionOutputCallback", status }));
        }
        if flags.contains(VTEncodeInfoFlags::FrameDropped) {
            return None;
        }
        let sample = unsafe { sample.as_ref() }?;
        Some(sample_to_annexb(sample, shared.codec))
    }));
    if let Ok(Some(r)) = res
        && let Ok(mut q) = shared.out.lock()
    {
        q.push(r);
    }
}

fn set_property(session: &CFType, key: &CFString, value: &CFType, required: bool) -> Result<()> {
    let st = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    if st != 0 {
        if required {
            return Err(CodecError::Os { context: "VTSessionSetProperty", status: st });
        }
        tracing::debug!("optional VideoToolbox property {key} rejected: {st}");
    }
    Ok(())
}

fn create_compression_session(
    v: Variant,
    cfg: &EncoderConfig,
    shared: *const EncShared,
) -> Result<CFRetained<VTCompressionSession>> {
    let yes: &CFType = CFBoolean::new(true);
    let no: &CFType = CFBoolean::new(false);
    let spec = unsafe {
        if v.low_latency {
            cf_dict(&[(kVTVideoEncoderSpecification_EnableLowLatencyRateControl, yes)])
        } else {
            cf_dict(&[(kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder, yes)])
        }
    };
    let fmt = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
    let w = CFNumber::new_i32(cfg.width as i32);
    let h = CFNumber::new_i32(cfg.height as i32);
    let iosurface = CFDictionary::<CFString, CFType>::empty();
    let src_attrs = unsafe {
        cf_dict(&[
            (kCVPixelBufferPixelFormatTypeKey, &fmt),
            (kCVPixelBufferWidthKey, &w),
            (kCVPixelBufferHeightKey, &h),
            (kCVPixelBufferIOSurfacePropertiesKey, &iosurface),
        ])
    };
    let mut raw: *mut VTCompressionSession = ptr::null_mut();
    let st = unsafe {
        VTCompressionSession::create(
            None,
            cfg.width as i32,
            cfg.height as i32,
            v.codec_type(),
            Some(spec.as_opaque()),
            Some(src_attrs.as_opaque()),
            None,
            Some(compression_output),
            shared as *mut c_void,
            NonNull::from(&mut raw),
        )
    };
    check("VTCompressionSessionCreate", st)?;
    let session = unsafe { CFRetained::from_raw(NonNull::new(raw).ok_or(CodecError::Os { context: "VTCompressionSessionCreate", status: -1 })?) };
    let s: &CFType = &session;
    let fps = cfg.fps.max(1);
    let profile = CFString::from_static_str(v.profile());
    unsafe {
        set_property(s, kVTCompressionPropertyKey_ProfileLevel, &profile, true)?;
        set_property(s, kVTCompressionPropertyKey_RealTime, yes, false)?;
        set_property(s, kVTCompressionPropertyKey_AllowFrameReordering, no, false)?;
        set_property(s, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, &CFNumber::new_f64(2.0), false)?;
        set_property(s, kVTCompressionPropertyKey_MaxKeyFrameInterval, &CFNumber::new_i32(fps as i32 * 2), false)?;
        set_property(s, kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::new_i32(fps as i32), false)?;
        set_property(s, kVTCompressionPropertyKey_AverageBitRate, &CFNumber::new_i64(cfg.bitrate_kbps as i64 * 1000), false)?;
        set_property(s, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, yes, false)?;
        set_property(s, kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2, false)?;
        set_property(s, kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2, false)?;
        set_property(s, kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2, false)?;
        let _ = session.prepare_to_encode_frames();
    }
    Ok(session)
}

/// VideoToolbox H.264 / HEVC encoder.
pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    shared: Box<EncShared>,
    cfg: EncoderConfig,
    variant: Variant,
    force_next_keyframe: bool,
    chroma_verified: bool,
}

// SAFETY: VTCompressionSession is thread-safe; the shared output queue is behind a Mutex.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    /// Opens the best working variant of the fallback chain for `cfg` (see module docs).
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        if cfg.width < 16 || cfg.height < 16 {
            return Err(CodecError::InvalidData(format!("frame size {}x{} too small", cfg.width, cfg.height)));
        }
        for v in fallback_chain(&cfg) {
            if variant_works(v) {
                let enc = Self::with_variant(cfg.clone(), v)?;
                if (v.codec, v.chroma) != (cfg.codec, cfg.chroma) {
                    tracing::info!(requested = ?(cfg.codec, cfg.chroma), selected = ?(v.codec, v.chroma), "VideoToolbox fallback");
                }
                return Ok(enc);
            }
        }
        Err(CodecError::Unsupported("no VideoToolbox H.264/HEVC encoder available"))
    }

    fn with_variant(mut cfg: EncoderConfig, v: Variant) -> Result<Self> {
        cfg.codec = v.codec;
        cfg.chroma = v.chroma;
        let shared = Box::new(EncShared { codec: v.codec, out: Mutex::new(Vec::new()) });
        let session = create_compression_session(v, &cfg, &*shared)?;
        Ok(Self { session, shared, cfg, variant: v, force_next_keyframe: true, chroma_verified: false })
    }

    /// Whether `EnableLowLatencyRateControl` is active for the selected variant.
    pub fn uses_low_latency_rate_control(&self) -> bool {
        self.variant.low_latency
    }

    /// The `kVTProfileLevel_*` value in use.
    pub fn profile_name(&self) -> &'static str {
        self.variant.profile()
    }

    fn reconfigure(&mut self, width: u32, height: u32) -> Result<()> {
        unsafe { self.session.invalidate() };
        self.shared.out.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.cfg.width = width;
        self.cfg.height = height;
        self.session = create_compression_session(self.variant, &self.cfg, &*self.shared)?;
        self.force_next_keyframe = true;
        Ok(())
    }

    fn pixel_buffer_from_bgra(&self, f: &CapturedFrame, data: &[u8], stride: usize) -> Result<CFRetained<CVPixelBuffer>> {
        let pool: CFRetained<CVPixelBufferPool> = unsafe { self.session.pixel_buffer_pool() }
            .ok_or(CodecError::Os { context: "VTCompressionSessionGetPixelBufferPool", status: -1 })?;
        let mut raw: *mut CVPixelBuffer = ptr::null_mut();
        let st = unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut raw)) };
        check("CVPixelBufferPoolCreatePixelBuffer", st)?;
        let pb = unsafe { CFRetained::from_raw(NonNull::new(raw).ok_or(CodecError::Os { context: "CVPixelBufferPoolCreatePixelBuffer", status: -1 })?) };
        let row_bytes = f.width as usize * 4;
        if stride < row_bytes || data.len() < stride * (f.height as usize - 1) + row_bytes {
            return Err(CodecError::InvalidData("BGRA buffer too small".into()));
        }
        unsafe {
            check("CVPixelBufferLockBaseAddress", CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags(0)))?;
            let base = CVPixelBufferGetBaseAddress(&pb) as *mut u8;
            let dst_stride = CVPixelBufferGetBytesPerRow(&pb);
            if !base.is_null() {
                for y in 0..f.height as usize {
                    ptr::copy_nonoverlapping(data.as_ptr().add(y * stride), base.add(y * dst_stride), row_bytes);
                }
            }
            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags(0));
            if base.is_null() {
                return Err(CodecError::InvalidData("pixel buffer has no base address".into()));
            }
        }
        Ok(pb)
    }

    fn encode_raw(&mut self, f: &CapturedFrame, force_keyframe: bool) -> Result<Option<EncodedOut>> {
        if (f.width, f.height) != (self.cfg.width, self.cfg.height) {
            self.reconfigure(f.width, f.height)?;
        }
        let force = force_keyframe || std::mem::take(&mut self.force_next_keyframe);
        let owned;
        let pb: &CVPixelBuffer = match &f.pixels {
            FramePixels::PixelBuffer(p) => p.as_cv(),
            FramePixels::Bgra { stride, data } => {
                owned = self.pixel_buffer_from_bgra(f, data, *stride)?;
                &owned
            }
        };
        let yes: &CFType = CFBoolean::new(true);
        let props = force.then(|| unsafe { cf_dict(&[(kVTEncodeFrameOptionKey_ForceKeyFrame, yes)]) });
        let pts = us_time(f.pts_us);
        let st = unsafe {
            self.session.encode_frame(
                pb,
                pts,
                invalid_time(),
                props.as_deref().map(|d| d.as_opaque()),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        check("VTCompressionSessionEncodeFrame", st)?;
        check("VTCompressionSessionCompleteFrames", unsafe { self.session.complete_frames(pts) })?;
        let outs = std::mem::take(&mut *self.shared.out.lock().unwrap_or_else(|e| e.into_inner()));
        let mut last = None;
        for o in outs {
            last = Some(o?);
        }
        if force && last.as_ref().is_some_and(|o| !o.keyframe) {
            tracing::warn!("VideoToolbox ignored a forced keyframe");
        }
        Ok(last)
    }
}

impl VideoEncoder for VtEncoder {
    fn encode(&mut self, f: &CapturedFrame, force_keyframe: bool) -> Result<Option<EncodedFrame>> {
        let Some(out) = self.encode_raw(f, force_keyframe)? else { return Ok(None) };
        if out.keyframe
            && !self.chroma_verified
            && let Some(actual) = bitstream_chroma(self.cfg.codec, &out.data)
        {
            if actual != self.cfg.chroma {
                tracing::warn!(expected = ?self.cfg.chroma, ?actual, "encoder produced a different chroma format");
                self.cfg.chroma = actual;
            }
            self.chroma_verified = true;
        }
        Ok(Some(EncodedFrame {
            data: out.data,
            keyframe: out.keyframe,
            pts_us: f.pts_us,
            codec: self.cfg.codec,
            chroma: self.cfg.chroma,
            width: self.cfg.width,
            height: self.cfg.height,
        }))
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        let s: &CFType = &self.session;
        unsafe { set_property(s, kVTCompressionPropertyKey_AverageBitRate, &CFNumber::new_i64(kbps as i64 * 1000), true)? };
        self.cfg.bitrate_kbps = kbps;
        Ok(())
    }

    fn config(&self) -> &EncoderConfig {
        &self.cfg
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // Guarantees no callback touches `shared` after it is freed.
        unsafe { self.session.invalidate() };
    }
}

// ---------------------------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------------------------

struct DecShared {
    out: Mutex<Option<Result<BgraImage>>>,
}

unsafe extern "C-unwind" fn decompression_output(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: OSStatus,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: refcon is the `DecShared` boxed by the owning VtDecoder (outlives the session).
    let shared = unsafe { &*(refcon as *const DecShared) };
    let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if status != 0 {
            return Some(Err(CodecError::Os { context: "VTDecompressionOutputCallback", status }));
        }
        let image = NonNull::new(image)?;
        let pb = PixelBuffer::new(unsafe { CFRetained::retain(image) });
        Some(pb.to_bgra().map_err(CodecError::from))
    }));
    if let Ok(Some(r)) = res
        && let Ok(mut slot) = shared.out.lock()
    {
        *slot = Some(r);
    }
}

/// VideoToolbox H.264 / HEVC decoder producing BGRA images.
pub struct VtDecoder {
    codec: VideoCodec,
    params: Option<ParameterSets>,
    format: Option<CFRetained<CMFormatDescription>>,
    session: Option<CFRetained<VTDecompressionSession>>,
    shared: Box<DecShared>,
}

// SAFETY: VTDecompressionSession is thread-safe; output slot is behind a Mutex.
unsafe impl Send for VtDecoder {}

impl VtDecoder {
    pub fn new(codec: VideoCodec) -> Result<Self> {
        if !matches!(codec, VideoCodec::H264 | VideoCodec::Hevc) {
            return Err(CodecError::Unsupported("VideoToolbox decoder supports H.264 and HEVC"));
        }
        Ok(Self { codec, params: None, format: None, session: None, shared: Box::new(DecShared { out: Mutex::new(None) }) })
    }

    fn configure(&mut self, ps: ParameterSets) -> Result<()> {
        if let Some(s) = self.session.take() {
            unsafe { s.invalidate() };
        }
        let mut sets: Vec<&[u8]> = Vec::new();
        if let Some(v) = &ps.vps {
            sets.push(v);
        }
        sets.push(&ps.sps);
        sets.push(&ps.pps);
        let mut ptrs: Vec<NonNull<u8>> = sets.iter().map(|s| NonNull::new(s.as_ptr() as *mut u8).expect("non-null")).collect();
        let mut sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
        let mut fd: *const CMFormatDescription = ptr::null();
        let st = unsafe {
            let p = NonNull::new_unchecked(ptrs.as_mut_ptr());
            let s = NonNull::new_unchecked(sizes.as_mut_ptr());
            if self.codec == VideoCodec::Hevc {
                CMVideoFormatDescriptionCreateFromHEVCParameterSets(None, sets.len(), p, s, 4, None, NonNull::from(&mut fd))
            } else {
                CMVideoFormatDescriptionCreateFromH264ParameterSets(None, sets.len(), p, s, 4, NonNull::from(&mut fd))
            }
        };
        check("CMVideoFormatDescriptionCreateFromParameterSets", st)?;
        let fd = unsafe {
            CFRetained::from_raw(NonNull::new(fd as *mut CMFormatDescription).ok_or(CodecError::Os {
                context: "CMVideoFormatDescriptionCreateFromParameterSets",
                status: -1,
            })?)
        };
        let fmt = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
        let iosurface = CFDictionary::<CFString, CFType>::empty();
        let attrs = unsafe {
            cf_dict(&[(kCVPixelBufferPixelFormatTypeKey, &fmt), (kCVPixelBufferIOSurfacePropertiesKey, &iosurface)])
        };
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(decompression_output),
            decompressionOutputRefCon: &*self.shared as *const DecShared as *mut c_void,
        };
        let mut raw: *mut VTDecompressionSession = ptr::null_mut();
        let st = unsafe {
            VTDecompressionSession::create(None, &fd, None, Some(attrs.as_opaque()), &record, NonNull::from(&mut raw))
        };
        check("VTDecompressionSessionCreate", st)?;
        self.session = Some(unsafe {
            CFRetained::from_raw(NonNull::new(raw).ok_or(CodecError::Os { context: "VTDecompressionSessionCreate", status: -1 })?)
        });
        self.format = Some(fd);
        self.params = Some(ps);
        Ok(())
    }
}

impl VideoDecoder for VtDecoder {
    fn decode(&mut self, f: &VideoFrame) -> Result<Option<BgraImage>> {
        if f.codec != self.codec {
            return Err(CodecError::InvalidData(format!("decoder is {:?}, frame is {:?}", self.codec, f.codec)));
        }
        if let Some(ps) = ParameterSets::from_annexb(self.codec, &f.data)
            && self.params.as_ref() != Some(&ps)
        {
            self.configure(ps)?;
        }
        let (Some(session), Some(fd)) = (&self.session, &self.format) else { return Ok(None) };

        let mut avcc = Vec::with_capacity(f.data.len() + 16);
        for nal in annexb::nal_units(&f.data) {
            let skip = match self.codec {
                VideoCodec::Hevc => matches!(
                    hevc_nal_type(nal),
                    nal_type::HEVC_VPS | nal_type::HEVC_SPS | nal_type::HEVC_PPS | nal_type::HEVC_AUD
                ),
                _ => matches!(h264_nal_type(nal), nal_type::H264_SPS | nal_type::H264_PPS | nal_type::H264_AUD),
            };
            if !skip {
                avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                avcc.extend_from_slice(nal);
            }
        }
        if avcc.is_empty() {
            return Ok(None);
        }
        let len = avcc.len();
        let mut bb: *mut CMBlockBuffer = ptr::null_mut();
        const ASSURE_MEMORY_NOW: u32 = 1 << 0;
        let st = unsafe {
            CMBlockBuffer::create_with_memory_block(None, ptr::null_mut(), len, None, ptr::null(), 0, len, ASSURE_MEMORY_NOW, NonNull::from(&mut bb))
        };
        check("CMBlockBufferCreateWithMemoryBlock", st)?;
        let bb = unsafe { CFRetained::from_raw(NonNull::new(bb).ok_or(CodecError::Os { context: "CMBlockBufferCreateWithMemoryBlock", status: -1 })?) };
        let st = unsafe { CMBlockBuffer::replace_data_bytes(NonNull::new_unchecked(avcc.as_mut_ptr().cast()), &bb, 0, len) };
        check("CMBlockBufferReplaceDataBytes", st)?;

        let timing = CMSampleTimingInfo { duration: invalid_time(), presentationTimeStamp: us_time(f.pts_us), decodeTimeStamp: invalid_time() };
        let mut sb: *mut CMSampleBuffer = ptr::null_mut();
        let st = unsafe { CMSampleBuffer::create_ready(None, Some(&bb), Some(fd), 1, 1, &timing, 1, &len, NonNull::from(&mut sb)) };
        check("CMSampleBufferCreateReady", st)?;
        let sb = unsafe { CFRetained::from_raw(NonNull::new(sb).ok_or(CodecError::Os { context: "CMSampleBufferCreateReady", status: -1 })?) };

        *self.shared.out.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let mut info = VTDecodeInfoFlags(0);
        let st = unsafe { session.decode_frame(&sb, VTDecodeFrameFlags(0), ptr::null_mut(), &mut info) };
        check("VTDecompressionSessionDecodeFrame", st)?;
        check("VTDecompressionSessionWaitForAsynchronousFrames", unsafe { session.wait_for_asynchronous_frames() })?;
        match self.shared.out.lock().unwrap_or_else(|e| e.into_inner()).take() {
            None => Ok(None),
            Some(r) => r.map(Some),
        }
    }
}

impl Drop for VtDecoder {
    fn drop(&mut self) {
        if let Some(s) = self.session.take() {
            unsafe { s.invalidate() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{create_decoder, create_encoder, encoder_capabilities};

    fn psnr(a: &BgraImage, b: &BgraImage) -> f64 {
        assert_eq!((a.width, a.height), (b.width, b.height));
        let mut se = 0f64;
        let mut n = 0f64;
        for y in 0..a.height {
            let (ra, rb) = (a.row(y), b.row(y));
            for x in 0..a.width as usize {
                for c in 0..3 {
                    let d = ra[x * 4 + c] as f64 - rb[x * 4 + c] as f64;
                    se += d * d;
                    n += 1.0;
                }
            }
        }
        let mse = se / n;
        if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
    }

    #[test]
    fn profile_strings_match_sdk_constants() {
        use objc2_video_toolbox::{kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel};
        unsafe {
            assert_eq!(kVTProfileLevel_HEVC_Main_AutoLevel.to_string(), "HEVC_Main_AutoLevel");
            assert_eq!(kVTProfileLevel_H264_High_AutoLevel.to_string(), "H264_High_AutoLevel");
        }
    }

    #[test]
    fn capabilities_probe() {
        let caps = encoder_capabilities();
        let variants = probe_variants();
        println!("encoder capabilities: {caps:?}");
        println!("variants (codec, chroma, low-latency RC): {variants:?}");
        assert!(caps.contains(&(VideoCodec::Tiles, ChromaFormat::Yuv444)));
        assert!(
            caps.contains(&(VideoCodec::H264, ChromaFormat::Yuv420)),
            "every Mac with VideoToolbox encodes H.264 4:2:0"
        );
    }

    fn roundtrip(codec: VideoCodec, chroma: ChromaFormat, w: u32, h: u32) {
        let cfg = EncoderConfig { codec, chroma, width: w, height: h, fps: 30, bitrate_kbps: 12_000, low_latency: true };
        let mut enc = create_encoder(cfg).unwrap();
        let actual = enc.config().clone();
        println!("requested {codec:?}/{chroma:?} → using {:?}/{:?}", actual.codec, actual.chroma);
        let mut dec = create_decoder(actual.codec).unwrap();
        let pattern = TestPattern::new(w, h);
        let mut decoded = 0;
        let mut min_psnr = f64::INFINITY;
        for i in 0..20u64 {
            let src = pattern.frame(i);
            let frame = CapturedFrame::from_image(0, i * 33_333, src.clone(), None);
            let Some(out) = enc.encode(&frame, i == 10).unwrap() else { continue };
            if i == 0 || i == 10 {
                assert!(out.keyframe, "frame {i} must be a keyframe");
                let ps = ParameterSets::from_annexb(out.codec, &out.data).expect("parameter sets on keyframe");
                let cs = ps.codec_string().unwrap();
                assert!(cs.starts_with(if out.codec == VideoCodec::Hevc { "hvc1." } else { "avc1." }), "{cs}");
                ps.decoder_config().unwrap();
                assert_eq!(bitstream_chroma(out.codec, &out.data), Some(out.chroma));
            } else {
                assert!(!out.keyframe, "frame {i} unexpectedly a keyframe");
            }
            let vf = out.into_video_frame(0, i);
            let img = dec.decode(&vf).unwrap().expect("decoded image");
            let p = psnr(&src, &img);
            min_psnr = min_psnr.min(p);
            decoded += 1;
        }
        println!("{codec:?}/{chroma:?} {w}x{h}: decoded {decoded} frames, min PSNR {min_psnr:.1} dB");
        assert!(decoded >= 18);
        assert!(min_psnr > 30.0, "PSNR {min_psnr}");
    }

    #[test]
    fn h264_encode_decode_roundtrip() {
        roundtrip(VideoCodec::H264, ChromaFormat::Yuv420, 640, 360);
    }

    #[test]
    fn hevc_encode_decode_roundtrip() {
        roundtrip(VideoCodec::Hevc, ChromaFormat::Yuv420, 1280, 720);
    }

    #[test]
    fn hevc_444_encode_decode_roundtrip() {
        roundtrip(VideoCodec::Hevc, ChromaFormat::Yuv444, 1280, 720);
    }

    #[test]
    fn resolution_change_reconfigures() {
        let mut enc = create_encoder(EncoderConfig {
            codec: VideoCodec::H264,
            chroma: ChromaFormat::Yuv420,
            width: 320,
            height: 240,
            fps: 30,
            bitrate_kbps: 2000,
            low_latency: true,
        })
        .unwrap();
        let mut dec = create_decoder(VideoCodec::H264).unwrap();
        for (i, (w, h)) in [(320u32, 240u32), (320, 240), (480, 272), (480, 272)].into_iter().enumerate() {
            let f = CapturedFrame::from_image(0, i as u64 * 1000, TestPattern::new(w, h).frame(i as u64), None);
            let out = enc.encode(&f, false).unwrap().unwrap();
            assert_eq!(out.keyframe, i % 2 == 0, "frame {i}");
            assert_eq!((out.width, out.height), (w, h));
            let img = dec.decode(&out.into_video_frame(0, i as u64)).unwrap().unwrap();
            assert_eq!((img.width, img.height), (w, h));
        }
        enc.set_bitrate(1000).unwrap();
        assert_eq!(enc.config().bitrate_kbps, 1000);
    }

    #[test]
    fn decoder_waits_for_keyframe() {
        let mut dec = VtDecoder::new(VideoCodec::Hevc).unwrap();
        let vf = VideoFrame {
            display_id: 0,
            seq: 0,
            pts_us: 0,
            codec: VideoCodec::Hevc,
            chroma: ChromaFormat::Yuv420,
            keyframe: false,
            width: 16,
            height: 16,
            data: vec![0, 0, 0, 1, 0x02, 0x01, 0xAA],
        };
        assert!(dec.decode(&vf).unwrap().is_none());
        assert!(VtDecoder::new(VideoCodec::Tiles).is_err());
    }
}
