//! Media Foundation (Windows) H.264 / HEVC encoder and decoder.
//!
//! ## Encoder ([`MfEncoder`])
//! * Candidates come from `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER)` filtered on NV12 input and
//!   the requested output subtype: **hardware MFTs first** (`MFT_ENUM_FLAG_HARDWARE` —
//!   NVIDIA NVENC, AMD AMF, Intel Quick Sync), then the **software** synchronous MFTs
//!   (Microsoft "H264 Encoder MFT", which ships with every desktop Windows, so H.264 is always
//!   available; the HEVC software encoder only exists with the HEVC Video Extensions).
//!   Each candidate is activated and configured in order until one accepts the media types.
//!   A requested HEVC stream falls back to H.264 when no HEVC encoder works;
//!   [`VideoEncoder::config`] reports what was selected. Media Foundation encoders are
//!   4:2:0 only, so `chroma` is always reported as [`ChromaFormat::Yuv420`].
//! * Low latency: `CODECAPI_AVLowLatencyMode`, CBR (`CODECAPI_AVEncCommonRateControlMode`,
//!   `CODECAPI_AVEncCommonMeanBitRate`), GOP = 2 s (`CODECAPI_AVEncMPVGOPSize`), no
//!   B-frames (`CODECAPI_AVEncMPVDefaultBPictureCount = 0`); unsupported properties are
//!   skipped individually. Keyframes on demand via `CODECAPI_AVEncVideoForceKeyFrame`
//!   (falls back to re-creating the MFT, whose first frame is always an IDR).
//! * Input: BGRA frames converted to NV12 on the CPU
//!   ([`crate::mediafoundation_convert`], BT.709 limited range); odd sizes are padded to
//!   even by edge replication, so the bitstream size may exceed the source by one pixel.
//! * Output: Annex-B. Length-prefixed output is converted; keyframes without in-band
//!   parameter sets get the SPS/PPS (VPS/SPS/PPS) of `MF_MT_MPEG_SEQUENCE_HEADER` prepended.
//! * Synchronous MFTs are driven with `ProcessInput` / `ProcessOutput`
//!   (`MF_E_TRANSFORM_NEED_MORE_INPUT`, `MF_E_TRANSFORM_STREAM_CHANGE`, `MF_E_NOTACCEPTING`).
//!   Asynchronous (hardware) MFTs are unlocked with `MF_TRANSFORM_ASYNC_UNLOCK` and driven
//!   through their `IMFMediaEventGenerator` (`METransformNeedInput` / `METransformHaveOutput`)
//!   synchronously inside [`VideoEncoder::encode`], waiting at most ~2 frame intervals; an
//!   access unit that is still in flight is returned by a later call (never dropped, so the
//!   reference chain stays intact).
//!
//! ## Decoder ([`MfDecoder`])
//! The in-box Microsoft H.264 decoder (`CLSID_MSH264DecoderMFT`) / HEVC decoder
//! (`CLSID_MSH265DecoderMFT`, HEVC Video Extensions), else any synchronous decoder MFT from
//! `MFTEnumEx`. Software decoding (no D3D manager), `MF_LOW_LATENCY` set, NV12 output
//! renegotiated on `MF_E_TRANSFORM_STREAM_CHANGE` and cropped to the minimum display
//! aperture, converted to BGRA. The decoder is (re)created whenever new parameter sets
//! arrive and returns `Ok(None)` until the first access unit carrying them.
//!
//! ## Threading
//! Every entry point joins the calling thread to the COM multithreaded apartment
//! (`CoInitializeEx(COINIT_MULTITHREADED)`, never uninitialised) and `MFStartup` runs once per
//! process. MFTs are free-threaded in-proc objects, so [`MfEncoder`] / [`MfDecoder`] are
//! `Send` (moved between threads, used by one thread at a time through `&mut self`).
//!
//! Compile-verified by cross-compiling for `x86_64-pc-windows-gnu`; not run on real Windows
//! from the macOS development machine.

use std::cell::Cell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use aether_capture::{BgraImage, CapturedFrame, FramePixels};
use aether_protocol::{ChromaFormat, VideoCodec, VideoFrame};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface};

use crate::annexb::{
    H264Sps, HevcSps, ParameterSets, avcc_to_annexb, h264_nal_type, hevc_nal_type, nal_type, nal_units,
    parameter_sets_from_config, push_annexb_nal,
};
use crate::mediafoundation_convert::{Nv12Layout, bgra_to_nv12, nv12_to_bgra};
use crate::{CodecError, EncodedFrame, EncoderConfig, Result, VideoDecoder, VideoEncoder};

// ---------------------------------------------------------------------------------------------
// Runtime helpers
// ---------------------------------------------------------------------------------------------

fn os(context: &'static str) -> impl FnOnce(windows::core::Error) -> CodecError {
    move |e| CodecError::Os { context, status: e.code().0 }
}

/// Joins this thread to the MTA (once per thread; intentionally never uninitialised so COM
/// objects shared with other threads stay valid).
fn ensure_com() {
    thread_local!(static INIT: Cell<bool> = const { Cell::new(false) });
    INIT.with(|i| {
        if !i.get() {
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if hr.is_err() {
                // RPC_E_CHANGED_MODE: the thread is already STA; MFTs are free-threaded so the
                // calls still work (through the implicit MTA).
                tracing::debug!("CoInitializeEx(MTA) returned {hr:?}");
            }
            i.set(true);
        }
    });
}

/// `MFStartup` once per process (Media Foundation stays started for the process lifetime).
fn startup() -> Result<()> {
    static STARTED: OnceLock<std::result::Result<(), i32>> = OnceLock::new();
    ensure_com();
    match STARTED.get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(|e| e.code().0)) {
        Ok(()) => Ok(()),
        Err(status) => Err(CodecError::Os { context: "MFStartup", status: *status }),
    }
}

fn var_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

fn var_bool(v: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { boolVal: VARIANT_BOOL(if v { -1 } else { 0 }) },
            }),
        },
    }
}

/// Sets one `ICodecAPI` property; unsupported properties (`E_NOTIMPL`, `E_INVALIDARG`, …) are
/// skipped. Returns whether the MFT accepted it.
fn set_codec_api(api: &ICodecAPI, key: &GUID, value: &VARIANT, what: &str) -> bool {
    match unsafe { api.SetValue(key, value) } {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!("ICodecAPI {what} not applied: {e}");
            false
        }
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn subtype(codec: VideoCodec) -> GUID {
    if codec == VideoCodec::Hevc { MFVideoFormat_HEVC } else { MFVideoFormat_H264 }
}

fn friendly_name(a: &IMFAttributes) -> String {
    unsafe {
        let Ok(len) = a.GetStringLength(&MFT_FRIENDLY_NAME_Attribute) else { return String::new() };
        let mut buf = vec![0u16; len as usize + 1];
        if a.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None).is_ok() {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            String::new()
        }
    }
}

/// `MFTEnumEx` wrapper returning the activation objects (in the enumeration's order).
fn enum_mfts(category: GUID, flags: MFT_ENUM_FLAG, input: Option<GUID>, output: Option<GUID>) -> Vec<IMFActivate> {
    let in_info = input.map(|s| MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: s });
    let out_info = output.map(|s| MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: s });
    let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    let res = unsafe {
        MFTEnumEx(
            category,
            flags,
            in_info.as_ref().map(|p| p as *const _),
            out_info.as_ref().map(|p| p as *const _),
            &mut pp,
            &mut n,
        )
    };
    if let Err(e) = res {
        tracing::debug!("MFTEnumEx failed: {e}");
        return Vec::new();
    }
    if pp.is_null() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        // Take ownership of each reference, leaving `None` behind, then free the array.
        if let Some(a) = unsafe { (*pp.add(i)).take() } {
            out.push(a);
        }
    }
    unsafe { CoTaskMemFree(Some(pp as *const c_void)) };
    out
}

fn stream_ids(t: &IMFTransform) -> (u32, u32) {
    let mut i = [0u32];
    let mut o = [0u32];
    // E_NOTIMPL means the stream ids are the zero-based indices.
    match unsafe { t.GetStreamIDs(&mut i, &mut o) } {
        Ok(()) => (i[0], o[0]),
        Err(_) => (0, 0),
    }
}

fn start_streaming(t: &IMFTransform) -> Result<()> {
    unsafe {
        t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(os("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING"))?;
        t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(os("MFT_MESSAGE_NOTIFY_START_OF_STREAM"))?;
    }
    Ok(())
}

fn shutdown_transform(t: &IMFTransform, activate: Option<&IMFActivate>) {
    unsafe {
        let _ = t.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        let _ = t.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        if let Ok(s) = t.cast::<IMFShutdown>() {
            let _ = s.Shutdown();
        }
        if let Some(a) = activate {
            let _ = a.ShutdownObject();
        }
    }
}

/// Copies the (contiguous) payload of a sample.
fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buf = sample.ConvertToContiguousBuffer().map_err(os("IMFSample::ConvertToContiguousBuffer"))?;
        let mut p: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buf.Lock(&mut p, None, Some(&mut len)).map_err(os("IMFMediaBuffer::Lock"))?;
        let data = if p.is_null() { Vec::new() } else { std::slice::from_raw_parts(p, len as usize).to_vec() };
        let _ = buf.Unlock();
        Ok(data)
    }
}

fn new_sample(capacity: u32) -> Result<IMFSample> {
    unsafe {
        let sample = MFCreateSample().map_err(os("MFCreateSample"))?;
        let buf = MFCreateMemoryBuffer(capacity).map_err(os("MFCreateMemoryBuffer"))?;
        sample.AddBuffer(&buf).map_err(os("IMFSample::AddBuffer"))?;
        Ok(sample)
    }
}

fn starts_with_start_code(d: &[u8]) -> bool {
    d.starts_with(&[0, 0, 1]) || d.starts_with(&[0, 0, 0, 1])
}

/// Normalises an `MF_MT_MPEG_SEQUENCE_HEADER` blob (Annex-B, or an `avcC` / `hvcC` record)
/// to Annex-B parameter sets.
fn normalize_sequence_header(codec: VideoCodec, blob: &[u8]) -> Option<Vec<u8>> {
    if blob.is_empty() {
        return None;
    }
    if starts_with_start_code(blob) {
        return ParameterSets::from_annexb(codec, blob).is_some().then(|| blob.to_vec());
    }
    let ps = parameter_sets_from_config(codec, blob).ok()?;
    let mut out = Vec::new();
    if let Some(v) = &ps.vps {
        push_annexb_nal(&mut out, v);
    }
    push_annexb_nal(&mut out, &ps.sps);
    push_annexb_nal(&mut out, &ps.pps);
    Some(out)
}

fn read_sequence_header(t: &IMFTransform, out_id: u32, codec: VideoCodec) -> Option<Vec<u8>> {
    unsafe {
        let ty = t.GetOutputCurrentType(out_id).ok()?;
        let size = ty.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
        let mut blob = vec![0u8; size as usize];
        ty.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None).ok()?;
        normalize_sequence_header(codec, &blob)
    }
}

/// True when the access unit contains an IDR / IRAP picture.
fn contains_random_access(codec: VideoCodec, annexb: &[u8]) -> bool {
    nal_units(annexb).any(|n| match codec {
        VideoCodec::Hevc => (16..=nal_type::HEVC_CRA).contains(&hevc_nal_type(n)),
        _ => h264_nal_type(n) == nal_type::H264_IDR,
    })
}

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

enum OutStatus {
    Produced,
    NeedMoreInput,
    StreamChanged,
}

/// One `ProcessOutput` call; allocates the output sample unless the MFT provides its own.
/// Returns the produced sample (if any) and the status.
fn process_output_once(
    t: &IMFTransform,
    out_id: u32,
    info: &MFT_OUTPUT_STREAM_INFO,
    fallback_size: u32,
) -> Result<(OutStatus, Option<IMFSample>)> {
    let provides =
        info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32 != 0;
    let sample = if provides { None } else { Some(new_sample(info.cbSize.max(fallback_size))?) };
    let mut bufs = [MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: out_id,
        pSample: ManuallyDrop::new(sample),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
    }];
    let mut status = 0u32;
    let res = unsafe { t.ProcessOutput(0, &mut bufs, &mut status) };
    let sample = unsafe { ManuallyDrop::take(&mut bufs[0].pSample) };
    unsafe { ManuallyDrop::drop(&mut bufs[0].pEvents) };
    let no_sample = bufs[0].dwStatus & MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32 != 0;
    match res {
        Ok(()) if no_sample => Ok((OutStatus::Produced, None)),
        Ok(()) => Ok((OutStatus::Produced, sample)),
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok((OutStatus::NeedMoreInput, None)),
        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok((OutStatus::StreamChanged, None)),
        Err(e) => Err(os("IMFTransform::ProcessOutput")(e)),
    }
}

// ---------------------------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------------------------

struct EncodedOut {
    data: Vec<u8>,
    keyframe: bool,
    pts_us: u64,
}

struct Candidate {
    activate: IMFActivate,
    name: String,
    hardware: bool,
}

/// Hardware encoders first, then software (synchronous) ones.
fn encoder_candidates(codec: VideoCodec) -> Vec<Candidate> {
    let sub = subtype(codec);
    let mut out = Vec::new();
    for (flags, hardware) in [
        (MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, true),
        (MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER, false),
    ] {
        for activate in enum_mfts(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(MFVideoFormat_NV12), Some(sub)) {
            let name = friendly_name(&activate);
            out.push(Candidate { activate, name, hardware });
        }
    }
    out
}

/// One configured encoder MFT.
struct EncSession {
    activate: IMFActivate,
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    /// Event generator of an asynchronous (hardware) MFT.
    events: Option<IMFMediaEventGenerator>,
    in_id: u32,
    out_id: u32,
    out_info: MFT_OUTPUT_STREAM_INFO,
    layout: Nv12Layout,
    codec: VideoCodec,
    name: String,
    hardware: bool,
    frame_duration_hns: i64,
    seq_header: Option<Vec<u8>>,
    /// `METransformNeedInput` credits not consumed yet (async MFTs).
    need_input: u32,
    /// Inputs waiting for a `METransformNeedInput` (async MFTs): (sample, force keyframe).
    pending: VecDeque<(IMFSample, bool)>,
    in_flight: u32,
    ready: VecDeque<EncodedOut>,
}

impl EncSession {
    fn open(c: &Candidate, codec: VideoCodec, cfg: &EncoderConfig) -> Result<Self> {
        let transform: IMFTransform =
            unsafe { c.activate.ActivateObject() }.map_err(os("IMFActivate::ActivateObject"))?;
        match Self::configure(c, codec, cfg, transform.clone()) {
            Ok(s) => Ok(s),
            Err(e) => {
                shutdown_transform(&transform, Some(&c.activate));
                Err(e)
            }
        }
    }

    fn configure(c: &Candidate, codec: VideoCodec, cfg: &EncoderConfig, transform: IMFTransform) -> Result<Self> {
        let fps = cfg.fps.max(1) as u32;
        let bitrate = cfg.bitrate_kbps.max(100).saturating_mul(1000);
        let layout = Nv12Layout::packed(cfg.width, cfg.height);
        if layout.is_empty() {
            return Err(CodecError::InvalidData(format!("invalid encoder size {}x{}", cfg.width, cfg.height)));
        }
        let mut is_async = false;
        unsafe {
            if let Ok(attrs) = transform.GetAttributes() {
                is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
                if is_async {
                    attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(os("MF_TRANSFORM_ASYNC_UNLOCK"))?;
                }
                if cfg.low_latency {
                    let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
                }
            }
        }
        let (in_id, out_id) = stream_ids(&transform);
        let codec_api = transform.cast::<ICodecAPI>().ok();
        // Several encoders (incl. Microsoft's) only honour rate control set before the types.
        if let Some(api) = &codec_api {
            if cfg.low_latency {
                set_codec_api(api, &CODECAPI_AVLowLatencyMode, &var_bool(true), "AVLowLatencyMode");
            }
            set_codec_api(
                api,
                &CODECAPI_AVEncCommonRateControlMode,
                &var_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
                "AVEncCommonRateControlMode=CBR",
            );
            set_codec_api(api, &CODECAPI_AVEncCommonMeanBitRate, &var_u32(bitrate), "AVEncCommonMeanBitRate");
            set_codec_api(api, &CODECAPI_AVEncMPVGOPSize, &var_u32(fps * 2), "AVEncMPVGOPSize");
            set_codec_api(api, &CODECAPI_AVEncMPVDefaultBPictureCount, &var_u32(0), "AVEncMPVDefaultBPictureCount");
        }
        unsafe {
            // Output type first (encoders derive the accepted input types from it).
            let out = MFCreateMediaType().map_err(os("MFCreateMediaType"))?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(os("MF_MT_MAJOR_TYPE"))?;
            out.SetGUID(&MF_MT_SUBTYPE, &subtype(codec)).map_err(os("MF_MT_SUBTYPE"))?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate).map_err(os("MF_MT_AVG_BITRATE"))?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(layout.width, layout.height)).map_err(os("MF_MT_FRAME_SIZE"))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(os("MF_MT_FRAME_RATE"))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(os("MF_MT_PIXEL_ASPECT_RATIO"))?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(os("MF_MT_INTERLACE_MODE"))?;
            let profile = match codec {
                VideoCodec::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
                _ => eAVEncH264VProfile_High.0 as u32,
            };
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile).map_err(os("MF_MT_MPEG2_PROFILE"))?;
            transform.SetOutputType(out_id, &out, 0).map_err(os("IMFTransform::SetOutputType"))?;

            let inp = MFCreateMediaType().map_err(os("MFCreateMediaType"))?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(os("MF_MT_MAJOR_TYPE"))?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(os("MF_MT_SUBTYPE"))?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(layout.width, layout.height)).map_err(os("MF_MT_FRAME_SIZE"))?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(os("MF_MT_FRAME_RATE"))?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(os("MF_MT_PIXEL_ASPECT_RATIO"))?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(os("MF_MT_INTERLACE_MODE"))?;
            inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, layout.pitch as u32).map_err(os("MF_MT_DEFAULT_STRIDE"))?;
            inp.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32).map_err(os("MF_MT_YUV_MATRIX"))?;
            inp.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
                .map_err(os("MF_MT_VIDEO_NOMINAL_RANGE"))?;
            transform.SetInputType(in_id, &inp, 0).map_err(os("IMFTransform::SetInputType"))?;
        }
        // Some MFTs reset the bitrate when the types are applied.
        if let Some(api) = &codec_api {
            set_codec_api(api, &CODECAPI_AVEncCommonMeanBitRate, &var_u32(bitrate), "AVEncCommonMeanBitRate");
        }
        let out_info = unsafe { transform.GetOutputStreamInfo(out_id) }.map_err(os("GetOutputStreamInfo"))?;
        let events = if is_async {
            Some(transform.cast::<IMFMediaEventGenerator>().map_err(os("IMFMediaEventGenerator"))?)
        } else {
            None
        };
        start_streaming(&transform)?;
        let seq_header = read_sequence_header(&transform, out_id, codec);
        tracing::info!(
            encoder = %c.name,
            hardware = c.hardware,
            asynchronous = is_async,
            ?codec,
            width = layout.width,
            height = layout.height,
            "Media Foundation encoder ready"
        );
        Ok(Self {
            activate: c.activate.clone(),
            transform,
            codec_api,
            events,
            in_id,
            out_id,
            out_info,
            layout,
            codec,
            name: c.name.clone(),
            hardware: c.hardware,
            frame_duration_hns: 10_000_000 / fps as i64,
            seq_header,
            need_input: 0,
            pending: VecDeque::new(),
            in_flight: 0,
            ready: VecDeque::new(),
        })
    }

    fn input_sample(&self, f: &CapturedFrame) -> Result<IMFSample> {
        let owned;
        let (data, stride): (&[u8], usize) = match &f.pixels {
            FramePixels::Bgra { stride, data } => (data, *stride),
            #[allow(unreachable_patterns)]
            _ => {
                owned = f.to_bgra()?;
                (&owned.data, owned.stride)
            }
        };
        let len = self.layout.len();
        unsafe {
            let buf = MFCreateMemoryBuffer(len as u32).map_err(os("MFCreateMemoryBuffer"))?;
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut max = 0u32;
            buf.Lock(&mut p, Some(&mut max), None).map_err(os("IMFMediaBuffer::Lock"))?;
            if p.is_null() || (max as usize) < len {
                let _ = buf.Unlock();
                return Err(CodecError::InvalidData("media buffer too small".into()));
            }
            let dst = std::slice::from_raw_parts_mut(p, len);
            let conv = bgra_to_nv12(data, f.width, f.height, stride, &self.layout, dst);
            let _ = buf.Unlock();
            conv?;
            buf.SetCurrentLength(len as u32).map_err(os("IMFMediaBuffer::SetCurrentLength"))?;
            let sample = MFCreateSample().map_err(os("MFCreateSample"))?;
            sample.AddBuffer(&buf).map_err(os("IMFSample::AddBuffer"))?;
            sample.SetSampleTime(f.pts_us as i64 * 10).map_err(os("IMFSample::SetSampleTime"))?;
            sample.SetSampleDuration(self.frame_duration_hns).map_err(os("IMFSample::SetSampleDuration"))?;
            Ok(sample)
        }
    }

    /// Requests an IDR for the next input. False when the MFT does not support it.
    fn force_keyframe(&self) -> bool {
        self.codec_api
            .as_ref()
            .is_some_and(|api| set_codec_api(api, &CODECAPI_AVEncVideoForceKeyFrame, &var_u32(1), "AVEncVideoForceKeyFrame"))
    }

    fn set_bitrate(&self, bps: u32) -> bool {
        self.codec_api
            .as_ref()
            .is_some_and(|api| set_codec_api(api, &CODECAPI_AVEncCommonMeanBitRate, &var_u32(bps), "AVEncCommonMeanBitRate"))
    }

    fn renegotiate_output(&mut self) -> Result<()> {
        unsafe {
            let ty = self.transform.GetOutputAvailableType(self.out_id, 0).map_err(os("GetOutputAvailableType"))?;
            self.transform.SetOutputType(self.out_id, &ty, 0).map_err(os("IMFTransform::SetOutputType"))?;
            self.out_info = self.transform.GetOutputStreamInfo(self.out_id).map_err(os("GetOutputStreamInfo"))?;
        }
        if let Some(sh) = read_sequence_header(&self.transform, self.out_id, self.codec) {
            self.seq_header = Some(sh);
        }
        Ok(())
    }

    /// One `ProcessOutput`; returns the status (stream changes are handled here).
    fn pull_output(&mut self) -> Result<OutStatus> {
        let fallback = (self.layout.len() as u32).max(64 * 1024);
        let (status, sample) = process_output_once(&self.transform, self.out_id, &self.out_info, fallback)?;
        match status {
            OutStatus::StreamChanged => self.renegotiate_output()?,
            OutStatus::Produced => {
                if let Some(s) = sample {
                    self.in_flight = self.in_flight.saturating_sub(1);
                    let out = self.sample_to_annexb(&s)?;
                    if !out.data.is_empty() {
                        self.ready.push_back(out);
                    }
                }
            }
            OutStatus::NeedMoreInput => {}
        }
        Ok(status)
    }

    fn sample_to_annexb(&mut self, s: &IMFSample) -> Result<EncodedOut> {
        let raw = sample_bytes(s)?;
        let clean = unsafe { s.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
        let pts_us = unsafe { s.GetSampleTime() }.map(|t| (t.max(0) / 10) as u64).unwrap_or(0);
        let mut data = if starts_with_start_code(&raw) {
            raw
        } else {
            match avcc_to_annexb(&raw, 4) {
                Ok(v) => v,
                Err(_) => raw,
            }
        };
        let keyframe = clean || contains_random_access(self.codec, &data);
        if keyframe && ParameterSets::from_annexb(self.codec, &data).is_none() {
            if self.seq_header.is_none() {
                self.seq_header = read_sequence_header(&self.transform, self.out_id, self.codec);
            }
            match &self.seq_header {
                Some(sh) => {
                    let mut v = Vec::with_capacity(sh.len() + data.len());
                    v.extend_from_slice(sh);
                    v.extend_from_slice(&data);
                    data = v;
                }
                None => tracing::warn!(encoder = %self.name, "keyframe without parameter sets"),
            }
        }
        Ok(EncodedOut { data, keyframe, pts_us })
    }

    fn feed(&mut self, sample: &IMFSample, force: bool) -> Result<()> {
        if force && !self.force_keyframe() {
            return Err(CodecError::Unsupported("encoder MFT cannot force keyframes"));
        }
        unsafe { self.transform.ProcessInput(self.in_id, sample, 0) }.map_err(os("IMFTransform::ProcessInput"))?;
        self.in_flight += 1;
        Ok(())
    }

    fn submit_sync(&mut self, sample: IMFSample, force: bool) -> Result<()> {
        for _ in 0..4 {
            if force && !self.force_keyframe() {
                return Err(CodecError::Unsupported("encoder MFT cannot force keyframes"));
            }
            match unsafe { self.transform.ProcessInput(self.in_id, &sample, 0) } {
                Ok(()) => {
                    self.in_flight += 1;
                    return self.drain_sync();
                }
                Err(e) if e.code() == MF_E_NOTACCEPTING => self.drain_sync()?,
                Err(e) => return Err(os("IMFTransform::ProcessInput")(e)),
            }
        }
        Err(CodecError::Os { context: "IMFTransform::ProcessInput", status: MF_E_NOTACCEPTING.0 })
    }

    fn drain_sync(&mut self) -> Result<()> {
        for _ in 0..64 {
            if let OutStatus::NeedMoreInput = self.pull_output()? {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Async MFT: queue the input and pump events until the encoder caught up or `wait` ran out.
    fn submit_async(&mut self, sample: IMFSample, force: bool, wait: Duration) -> Result<()> {
        let Some(events) = self.events.clone() else { return self.submit_sync(sample, force) };
        self.pending.push_back((sample, force));
        while self.pending.len() > 2 {
            // The encoder is not keeping up: skip the oldest raw input (never an encoded AU).
            if let Some((_, f)) = self.pending.pop_front()
                && f
                && let Some(next) = self.pending.front_mut()
            {
                next.1 = true;
            }
            tracing::debug!(encoder = %self.name, "encoder busy, input frame skipped");
        }
        let start = Instant::now();
        loop {
            while self.need_input > 0 {
                let Some((s, f)) = self.pending.pop_front() else { break };
                self.need_input -= 1;
                self.feed(&s, f)?;
            }
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => {
                    let ty = unsafe { ev.GetType() }.map_err(os("IMFMediaEvent::GetType"))?;
                    if ty == METransformNeedInput.0 as u32 {
                        self.need_input += 1;
                    } else if ty == METransformHaveOutput.0 as u32 {
                        // A format change is followed by the actual output on the next call.
                        for _ in 0..3 {
                            if !matches!(self.pull_output()?, OutStatus::StreamChanged) {
                                break;
                            }
                        }
                    } else if ty == MEError.0 as u32 {
                        let status = unsafe { ev.GetStatus() }.map(|h| h.0).unwrap_or(-1);
                        return Err(CodecError::Os { context: "async encoder MFT (MEError)", status });
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if (self.pending.is_empty() && self.in_flight == 0) || start.elapsed() >= wait {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_micros(250));
                }
                Err(e) => return Err(os("IMFMediaEventGenerator::GetEvent")(e)),
            }
        }
    }
}

impl Drop for EncSession {
    fn drop(&mut self) {
        shutdown_transform(&self.transform, Some(&self.activate));
    }
}

/// Media Foundation H.264 / HEVC encoder (see the module docs).
pub struct MfEncoder {
    cfg: EncoderConfig,
    /// Fall back to H.264 when the requested HEVC is unavailable.
    allow_fallback: bool,
    session: Option<EncSession>,
    chroma_verified: bool,
}

// SAFETY: the MFT and its helper interfaces are free-threaded in-proc COM objects living in
// the MTA (every entry point calls `ensure_com`); `&mut self` serialises all use.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    /// Opens the best encoder for `cfg` (hardware first; HEVC → H.264 fallback).
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        Self::with_fallback(cfg, true)
    }

    /// Like [`new`](Self::new); with `allow_codec_fallback = false` only `cfg.codec` is tried.
    pub fn with_fallback(mut cfg: EncoderConfig, allow_codec_fallback: bool) -> Result<Self> {
        if !matches!(cfg.codec, VideoCodec::H264 | VideoCodec::Hevc) {
            return Err(CodecError::Unsupported("Media Foundation encoder supports H.264 and HEVC"));
        }
        startup()?;
        cfg.chroma = ChromaFormat::Yuv420;
        let mut enc =
            Self { cfg, allow_fallback: allow_codec_fallback, session: None, chroma_verified: false };
        enc.open()?;
        Ok(enc)
    }

    fn open(&mut self) -> Result<()> {
        self.session = None;
        let codecs: &[VideoCodec] = match (self.cfg.codec, self.allow_fallback) {
            (VideoCodec::Hevc, true) => &[VideoCodec::Hevc, VideoCodec::H264],
            (VideoCodec::Hevc, false) => &[VideoCodec::Hevc],
            _ => &[VideoCodec::H264],
        };
        let mut last_err = None;
        for &codec in codecs {
            for c in encoder_candidates(codec) {
                match EncSession::open(&c, codec, &self.cfg) {
                    Ok(s) => {
                        if codec != self.cfg.codec {
                            tracing::info!(requested = ?self.cfg.codec, ?codec, "encoder codec fallback");
                        }
                        self.cfg.codec = codec;
                        self.session = Some(s);
                        return Ok(());
                    }
                    Err(e) => {
                        tracing::debug!(encoder = %c.name, hardware = c.hardware, ?codec, "encoder MFT rejected: {e}");
                        last_err = Some(e);
                    }
                }
            }
        }
        Err(last_err.unwrap_or(CodecError::Unsupported("no Media Foundation encoder MFT for the requested codec")))
    }

    /// Friendly name of the active MFT (e.g. "NVIDIA H.264 Encoder MFT").
    pub fn encoder_name(&self) -> &str {
        self.session.as_ref().map_or("", |s| s.name.as_str())
    }

    /// True when the active MFT is a hardware encoder.
    pub fn is_hardware(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.hardware)
    }

    fn wait_budget(&self) -> Duration {
        Duration::from_millis((2000 / self.cfg.fps.max(1) as u64).clamp(20, 100))
    }
}

impl VideoEncoder for MfEncoder {
    fn encode(&mut self, f: &CapturedFrame, force_keyframe: bool) -> Result<Option<EncodedFrame>> {
        ensure_com();
        let mut force = force_keyframe;
        if (f.width, f.height) != (self.cfg.width, self.cfg.height) || self.session.is_none() {
            // A freshly created MFT always starts with an IDR.
            self.cfg.width = f.width;
            self.cfg.height = f.height;
            self.open()?;
            force = false;
        }
        let wait = self.wait_budget();
        if force && self.session.as_ref().is_some_and(|s| s.codec_api.is_none()) {
            // No ICodecAPI to force an IDR: re-create the MFT instead.
            self.open()?;
            force = false;
        }
        let Some(session) = self.session.as_mut() else { return Ok(None) };
        let sample = session.input_sample(f)?;
        let res = if session.events.is_some() {
            session.submit_async(sample, force, wait)
        } else {
            session.submit_sync(sample, force)
        };
        match res {
            Ok(()) => {}
            Err(CodecError::Unsupported(_)) if force => {
                // ForceKeyFrame not supported by this MFT: re-create it (first frame is an IDR).
                self.open()?;
                let Some(session) = self.session.as_mut() else { return Ok(None) };
                let sample = session.input_sample(f)?;
                if session.events.is_some() {
                    session.submit_async(sample, false, wait)?;
                } else {
                    session.submit_sync(sample, false)?;
                }
            }
            Err(e) => return Err(e),
        }
        let Some(session) = self.session.as_mut() else { return Ok(None) };
        let Some(out) = session.ready.pop_front() else { return Ok(None) };
        let (width, height) = (session.layout.width, session.layout.height);
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
            pts_us: out.pts_us,
            codec: self.cfg.codec,
            chroma: self.cfg.chroma,
            width,
            height,
        }))
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        ensure_com();
        let kbps = kbps.max(100);
        if let Some(s) = &self.session
            && !s.set_bitrate(kbps.saturating_mul(1000))
        {
            // Not adjustable at runtime: re-create with the new rate.
            // (the new MFT starts with an IDR).
            self.cfg.bitrate_kbps = kbps;
            self.open()?;
            return Ok(());
        }
        self.cfg.bitrate_kbps = kbps;
        Ok(())
    }

    fn config(&self) -> &EncoderConfig {
        &self.cfg
    }
}

/// Codec/chroma pairs a Media Foundation encoder can actually produce on this machine
/// (probed once by encoding a test frame). The software H.264 MFT makes `(H264, Yuv420)`
/// available on every desktop Windows installation.
pub fn probe_capabilities() -> Vec<(VideoCodec, ChromaFormat)> {
    static CAPS: OnceLock<Vec<(VideoCodec, ChromaFormat)>> = OnceLock::new();
    CAPS.get_or_init(|| {
        [VideoCodec::Hevc, VideoCodec::H264]
            .into_iter()
            .filter(|&c| probe_codec(c))
            .map(|c| (c, ChromaFormat::Yuv420))
            .collect()
    })
    .clone()
}

fn probe_codec(codec: VideoCodec) -> bool {
    let cfg = EncoderConfig {
        codec,
        chroma: ChromaFormat::Yuv420,
        width: 640,
        height: 360,
        fps: 30,
        bitrate_kbps: 2_000,
        low_latency: true,
    };
    let mut enc = match MfEncoder::with_fallback(cfg, false) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(?codec, "no Media Foundation encoder: {e}");
            return false;
        }
    };
    let pattern = aether_capture::TestPattern::new(640, 360);
    for i in 0..8u64 {
        let frame = CapturedFrame::from_image(0, i * 33_333, pattern.frame(i), None);
        match enc.encode(&frame, i == 0) {
            Ok(Some(out)) => return out.keyframe && ParameterSets::from_annexb(codec, &out.data).is_some(),
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(?codec, "encoder probe failed: {e}");
                return false;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------------------------

/// Negotiated NV12 output of the decoder.
#[derive(Debug, Clone, Copy)]
struct OutFormat {
    coded_w: u32,
    coded_h: u32,
    stride: usize,
    crop_w: u32,
    crop_h: u32,
}

fn read_out_format(t: &IMFMediaType) -> Result<OutFormat> {
    unsafe {
        let size = t.GetUINT64(&MF_MT_FRAME_SIZE).map_err(os("MF_MT_FRAME_SIZE"))?;
        let (coded_w, coded_h) = ((size >> 32) as u32, size as u32);
        let stride = t
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .ok()
            .map(|s| s as i32)
            .filter(|&s| s >= coded_w as i32)
            .map_or(coded_w as usize, |s| s as usize);
        let mut crop = (coded_w, coded_h);
        for key in [&MF_MT_MINIMUM_DISPLAY_APERTURE, &MF_MT_GEOMETRIC_APERTURE] {
            let mut raw = [0u8; std::mem::size_of::<MFVideoArea>()];
            let mut got = 0u32;
            if t.GetBlob(key, &mut raw, Some(&mut got)).is_ok() && got as usize == raw.len() {
                let area: MFVideoArea = std::ptr::read_unaligned(raw.as_ptr().cast());
                let (w, h) = (area.Area.cx, area.Area.cy);
                if area.OffsetX.value == 0 && area.OffsetY.value == 0 && w > 0 && h > 0 {
                    crop = ((w as u32).min(coded_w), (h as u32).min(coded_h));
                    break;
                }
            }
        }
        Ok(OutFormat { coded_w, coded_h, stride, crop_w: crop.0, crop_h: crop.1 })
    }
}

/// Picks (and sets) the NV12 output type; returns its format.
fn select_nv12_output(t: &IMFTransform, out_id: u32) -> Result<OutFormat> {
    for i in 0.. {
        let ty = match unsafe { t.GetOutputAvailableType(out_id, i) } {
            Ok(ty) => ty,
            Err(_) => break,
        };
        if unsafe { ty.GetGUID(&MF_MT_SUBTYPE) }.ok() == Some(MFVideoFormat_NV12) {
            unsafe { t.SetOutputType(out_id, &ty, 0) }.map_err(os("IMFTransform::SetOutputType"))?;
            let current = unsafe { t.GetOutputCurrentType(out_id) }.unwrap_or(ty);
            return read_out_format(&current);
        }
    }
    Err(CodecError::Unsupported("decoder MFT offers no NV12 output"))
}

struct DecSession {
    transform: IMFTransform,
    activate: Option<IMFActivate>,
    in_id: u32,
    out_id: u32,
    out_info: MFT_OUTPUT_STREAM_INFO,
    fmt: OutFormat,
}

impl DecSession {
    fn open(codec: VideoCodec, ps: &ParameterSets) -> Result<Self> {
        let clsid = if codec == VideoCodec::Hevc { CLSID_MSH265DecoderMFT } else { CLSID_MSH264DecoderMFT };
        let mut found: Option<(IMFTransform, Option<IMFActivate>)> =
            unsafe { CoCreateInstance::<_, IMFTransform>(&clsid, None, CLSCTX_INPROC_SERVER) }.ok().map(|t| (t, None));
        if found.is_none() {
            for a in enum_mfts(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(subtype(codec)),
                Some(MFVideoFormat_NV12),
            ) {
                if let Ok(t) = unsafe { a.ActivateObject::<IMFTransform>() } {
                    found = Some((t, Some(a)));
                    break;
                }
            }
        }
        let Some((transform, activate)) = found else {
            return Err(CodecError::Unsupported("no Media Foundation decoder MFT for this codec"));
        };
        match Self::configure(codec, ps, &transform) {
            Ok((in_id, out_id, out_info, fmt)) => Ok(Self { transform, activate, in_id, out_id, out_info, fmt }),
            Err(e) => {
                shutdown_transform(&transform, activate.as_ref());
                Err(e)
            }
        }
    }

    fn configure(
        codec: VideoCodec,
        ps: &ParameterSets,
        t: &IMFTransform,
    ) -> Result<(u32, u32, MFT_OUTPUT_STREAM_INFO, OutFormat)> {
        unsafe {
            if let Ok(attrs) = t.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            if let Ok(api) = t.cast::<ICodecAPI>() {
                set_codec_api(&api, &CODECAPI_AVLowLatencyMode, &var_bool(true), "AVLowLatencyMode");
            }
            let (in_id, out_id) = stream_ids(t);
            let inp = MFCreateMediaType().map_err(os("MFCreateMediaType"))?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(os("MF_MT_MAJOR_TYPE"))?;
            inp.SetGUID(&MF_MT_SUBTYPE, &subtype(codec)).map_err(os("MF_MT_SUBTYPE"))?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(os("MF_MT_INTERLACE_MODE"))?;
            if codec == VideoCodec::Hevc
                && let Ok(sps) = HevcSps::parse(&ps.sps)
            {
                inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(sps.width, sps.height)).map_err(os("MF_MT_FRAME_SIZE"))?;
            }
            t.SetInputType(in_id, &inp, 0).map_err(os("IMFTransform::SetInputType"))?;
            let fmt = select_nv12_output(t, out_id)?;
            let info = t.GetOutputStreamInfo(out_id).map_err(os("GetOutputStreamInfo"))?;
            start_streaming(t)?;
            Ok((in_id, out_id, info, fmt))
        }
    }

    fn to_bgra(&self, sample: &IMFSample) -> Result<BgraImage> {
        let f = self.fmt;
        unsafe {
            let buf = sample.GetBufferByIndex(0).map_err(os("IMFSample::GetBufferByIndex"))?;
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                let mut p: *mut u8 = std::ptr::null_mut();
                let mut pitch = 0i32;
                b2.Lock2D(&mut p, &mut pitch).map_err(os("IMF2DBuffer::Lock2D"))?;
                let res = if p.is_null() || pitch < f.coded_w as i32 {
                    Err(CodecError::InvalidData(format!("unexpected NV12 pitch {pitch}")))
                } else {
                    let layout = Nv12Layout::with_pitch(f.coded_w, f.coded_h, pitch as usize, f.coded_h);
                    let src = std::slice::from_raw_parts(p, layout.len());
                    nv12_to_bgra(src, &layout, f.crop_w, f.crop_h)
                };
                let _ = b2.Unlock2D();
                return res;
            }
            let data = sample_bytes(sample)?;
            let layout = Nv12Layout::with_pitch(f.coded_w, f.coded_h, f.stride, f.coded_h);
            nv12_to_bgra(&data, &layout, f.crop_w, f.crop_h)
        }
    }

    /// Pulls every available picture; returns the newest.
    fn drain(&mut self) -> Result<Option<BgraImage>> {
        let mut last = None;
        for _ in 0..64 {
            let fallback = (self.fmt.stride * self.fmt.coded_h as usize * 3 / 2).max(4096) as u32;
            let (status, sample) = process_output_once(&self.transform, self.out_id, &self.out_info, fallback)?;
            match status {
                OutStatus::NeedMoreInput => break,
                OutStatus::StreamChanged => {
                    self.fmt = select_nv12_output(&self.transform, self.out_id)?;
                    self.out_info =
                        unsafe { self.transform.GetOutputStreamInfo(self.out_id) }.map_err(os("GetOutputStreamInfo"))?;
                }
                OutStatus::Produced => {
                    if let Some(s) = sample {
                        last = Some(self.to_bgra(&s)?);
                    }
                }
            }
        }
        Ok(last)
    }

    fn decode(&mut self, f: &VideoFrame) -> Result<Option<BgraImage>> {
        let sample = new_sample(f.data.len() as u32)?;
        unsafe {
            let buf = sample.GetBufferByIndex(0).map_err(os("IMFSample::GetBufferByIndex"))?;
            let mut p: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut p, None, None).map_err(os("IMFMediaBuffer::Lock"))?;
            std::ptr::copy_nonoverlapping(f.data.as_ptr(), p, f.data.len());
            let _ = buf.Unlock();
            buf.SetCurrentLength(f.data.len() as u32).map_err(os("IMFMediaBuffer::SetCurrentLength"))?;
            sample.SetSampleTime(f.pts_us as i64 * 10).map_err(os("IMFSample::SetSampleTime"))?;
            if f.keyframe {
                let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
            }
        }
        let mut last = None;
        for _ in 0..4 {
            match unsafe { self.transform.ProcessInput(self.in_id, &sample, 0) } {
                Ok(()) => {
                    if let Some(img) = self.drain()? {
                        last = Some(img);
                    }
                    return Ok(last);
                }
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    if let Some(img) = self.drain()? {
                        last = Some(img);
                    }
                }
                Err(e) => return Err(os("IMFTransform::ProcessInput")(e)),
            }
        }
        Err(CodecError::Os { context: "IMFTransform::ProcessInput", status: MF_E_NOTACCEPTING.0 })
    }
}

impl Drop for DecSession {
    fn drop(&mut self) {
        shutdown_transform(&self.transform, self.activate.as_ref());
    }
}

/// Media Foundation H.264 / HEVC decoder producing BGRA (see the module docs).
pub struct MfDecoder {
    codec: VideoCodec,
    params: Option<ParameterSets>,
    session: Option<DecSession>,
}

// SAFETY: see `MfEncoder` — free-threaded MTA objects, serialised by `&mut self`.
unsafe impl Send for MfDecoder {}

impl MfDecoder {
    pub fn new(codec: VideoCodec) -> Result<Self> {
        if !matches!(codec, VideoCodec::H264 | VideoCodec::Hevc) {
            return Err(CodecError::Unsupported("Media Foundation decoder supports H.264 and HEVC"));
        }
        startup()?;
        Ok(Self { codec, params: None, session: None })
    }
}

impl VideoDecoder for MfDecoder {
    fn decode(&mut self, f: &VideoFrame) -> Result<Option<BgraImage>> {
        if f.codec != self.codec {
            return Err(CodecError::InvalidData(format!("decoder is {:?}, frame is {:?}", self.codec, f.codec)));
        }
        ensure_com();
        if let Some(ps) = ParameterSets::from_annexb(self.codec, &f.data)
            && self.params.as_ref() != Some(&ps)
        {
            // New stream (or resolution change): start a fresh decoder on this access unit.
            self.session = None;
            self.params = None;
            self.session = Some(DecSession::open(self.codec, &ps)?);
            self.params = Some(ps);
        }
        let Some(session) = self.session.as_mut() else { return Ok(None) };
        session.decode(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_capture::TestPattern;

    #[test]
    fn sequence_header_normalisation() {
        // Annex-B passes through; avcC records are converted.
        let sps = [0x67u8, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50];
        let pps = [0x68u8, 0xeb, 0xe3, 0xcb];
        let mut annexb = Vec::new();
        push_annexb_nal(&mut annexb, &sps);
        push_annexb_nal(&mut annexb, &pps);
        assert_eq!(normalize_sequence_header(VideoCodec::H264, &annexb), Some(annexb.clone()));
        assert_eq!(normalize_sequence_header(VideoCodec::H264, &[]), None);
        assert!(contains_random_access(VideoCodec::H264, &[0, 0, 0, 1, 0x65, 0x88]));
        assert!(!contains_random_access(VideoCodec::H264, &[0, 0, 0, 1, 0x41, 0x9a]));
    }

    #[test]
    #[ignore = "needs Windows Media Foundation (software H.264 MFT)"]
    fn h264_encode_decode_roundtrip() {
        let cfg = EncoderConfig {
            codec: VideoCodec::H264,
            chroma: ChromaFormat::Yuv420,
            width: 640,
            height: 360,
            fps: 30,
            bitrate_kbps: 4_000,
            low_latency: true,
        };
        let mut enc = MfEncoder::new(cfg).expect("encoder");
        let mut dec = MfDecoder::new(VideoCodec::H264).expect("decoder");
        let pattern = TestPattern::new(640, 360);
        let mut decoded = 0;
        let mut keyframes = 0;
        for i in 0..30u64 {
            let frame = CapturedFrame::from_image(0, i * 33_333, pattern.frame(i), None);
            if let Some(out) = enc.encode(&frame, i == 15).expect("encode") {
                assert!(starts_with_start_code(&out.data));
                if out.keyframe {
                    keyframes += 1;
                    assert!(ParameterSets::from_annexb(VideoCodec::H264, &out.data).is_some());
                }
                if let Some(img) = dec.decode(&out.into_video_frame(0, i)).expect("decode") {
                    assert_eq!((img.width, img.height), (640, 360));
                    decoded += 1;
                }
            }
        }
        assert!(keyframes >= 2, "forced keyframe missing");
        assert!(decoded > 20, "decoded {decoded}");
        assert!(probe_capabilities().contains(&(VideoCodec::H264, ChromaFormat::Yuv420)));
    }
}
