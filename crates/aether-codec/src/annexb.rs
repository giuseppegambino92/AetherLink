//! H.264 / HEVC bitstream helpers: Annex-B ↔ AVCC/HVCC (length-prefixed) conversion,
//! parameter-set extraction, SPS parsing, `avcC` / `hvcC` decoder configuration records
//! (WebCodecs `VideoDecoderConfig.description`) and RFC 6381 codec strings.

use crate::{CodecError, Result};
use aether_protocol::VideoCodec;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Iterator over the NAL units of an Annex-B byte stream (start codes stripped,
/// trailing zero bytes of each NAL removed).
pub struct NalIter<'a> {
    data: &'a [u8],
    pos: usize,
}

/// Finds the next start code (`00 00 01`) at or after `from`; returns (index of start code, its length).
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i + 2] > 1 {
            i += 3;
            continue;
        }
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if i > from && data[i - 1] == 0 {
                return Some((i - 1, 4));
            }
            return Some((i, 3));
        }
        i += 1;
    }
    None
}

impl<'a> Iterator for NalIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            if self.pos >= self.data.len() {
                return None;
            }
            let (sc, len) = find_start_code(self.data, self.pos)?;
            let start = sc + len;
            let end = match find_start_code(self.data, start) {
                Some((next, _)) => next,
                None => self.data.len(),
            };
            self.pos = end;
            let mut nal = &self.data[start..end];
            while let [rest @ .., 0] = nal {
                nal = rest;
            }
            if !nal.is_empty() {
                return Some(nal);
            }
        }
    }
}

/// Splits an Annex-B stream into NAL units.
pub fn nal_units(annexb: &[u8]) -> NalIter<'_> {
    NalIter { data: annexb, pos: 0 }
}

/// Converts Annex-B to length-prefixed (AVCC/HVCC) format with 4-byte big-endian lengths.
pub fn annexb_to_avcc(annexb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(annexb.len() + 16);
    for nal in nal_units(annexb) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Converts length-prefixed NAL units (`nal_length_size` ∈ {1, 2, 4}) to Annex-B with 4-byte start codes.
pub fn avcc_to_annexb(avcc: &[u8], nal_length_size: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(avcc.len() + 16);
    for nal in length_prefixed_nals(avcc, nal_length_size)? {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    Ok(out)
}

/// Splits length-prefixed NAL units.
pub fn length_prefixed_nals(data: &[u8], nal_length_size: usize) -> Result<Vec<&[u8]>> {
    if !matches!(nal_length_size, 1 | 2 | 4) {
        return Err(CodecError::InvalidData(format!("invalid NAL length size {nal_length_size}")));
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if i + nal_length_size > data.len() {
            return Err(CodecError::InvalidData("truncated NAL length".into()));
        }
        let mut len = 0usize;
        for b in &data[i..i + nal_length_size] {
            len = (len << 8) | *b as usize;
        }
        i += nal_length_size;
        if i + len > data.len() {
            return Err(CodecError::InvalidData(format!("NAL length {len} exceeds buffer")));
        }
        out.push(&data[i..i + len]);
        i += len;
    }
    Ok(out)
}

/// Appends `nal` to `out` with a 4-byte start code.
pub fn push_annexb_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(nal);
}

/// H.264 `nal_unit_type`.
#[inline]
pub fn h264_nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1F)
}

/// HEVC `nal_unit_type`.
#[inline]
pub fn hevc_nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| (b >> 1) & 0x3F)
}

pub mod nal_type {
    pub const H264_IDR: u8 = 5;
    pub const H264_SEI: u8 = 6;
    pub const H264_SPS: u8 = 7;
    pub const H264_PPS: u8 = 8;
    pub const H264_AUD: u8 = 9;
    pub const HEVC_IDR_W_RADL: u8 = 19;
    pub const HEVC_IDR_N_LP: u8 = 20;
    pub const HEVC_CRA: u8 = 21;
    pub const HEVC_VPS: u8 = 32;
    pub const HEVC_SPS: u8 = 33;
    pub const HEVC_PPS: u8 = 34;
    pub const HEVC_AUD: u8 = 35;
}

/// Parameter sets of an H.264 (`vps == None`) or HEVC stream, NAL headers included,
/// emulation-prevention bytes retained (exactly as they appear in the bitstream).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParameterSets {
    pub vps: Option<Vec<u8>>,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

impl ParameterSets {
    /// Extracts the first SPS/PPS (and VPS for HEVC) of an Annex-B access unit.
    pub fn from_annexb(codec: VideoCodec, annexb: &[u8]) -> Option<Self> {
        let mut ps = ParameterSets::default();
        let (mut have_sps, mut have_pps) = (false, false);
        for nal in nal_units(annexb) {
            match codec {
                VideoCodec::H264 => match h264_nal_type(nal) {
                    nal_type::H264_SPS if !have_sps => {
                        ps.sps = nal.to_vec();
                        have_sps = true;
                    }
                    nal_type::H264_PPS if !have_pps => {
                        ps.pps = nal.to_vec();
                        have_pps = true;
                    }
                    _ => {}
                },
                VideoCodec::Hevc => match hevc_nal_type(nal) {
                    nal_type::HEVC_VPS if ps.vps.is_none() => ps.vps = Some(nal.to_vec()),
                    nal_type::HEVC_SPS if !have_sps => {
                        ps.sps = nal.to_vec();
                        have_sps = true;
                    }
                    nal_type::HEVC_PPS if !have_pps => {
                        ps.pps = nal.to_vec();
                        have_pps = true;
                    }
                    _ => {}
                },
                _ => return None,
            }
        }
        let complete = have_sps && have_pps && (codec != VideoCodec::Hevc || ps.vps.is_some());
        complete.then_some(ps)
    }

    /// Builds the decoder configuration record (`avcC` or `hvcC`).
    pub fn decoder_config(&self) -> Result<Vec<u8>> {
        match &self.vps {
            Some(vps) => hevc_decoder_config(vps, &self.sps, &self.pps),
            None => avc_decoder_config(&self.sps, &self.pps),
        }
    }

    /// WebCodecs / RFC 6381 codec string.
    pub fn codec_string(&self) -> Result<String> {
        match self.vps {
            Some(_) => Ok(HevcSps::parse(&self.sps)?.codec_string()),
            None => Ok(H264Sps::parse(&self.sps)?.codec_string()),
        }
    }
}

/// Removes emulation-prevention bytes (`00 00 03` → `00 00`).
pub fn nal_to_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// MSB-first bit reader with Exp-Golomb support.
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn eof() -> CodecError {
        CodecError::InvalidData("bitstream truncated".into())
    }

    pub fn bit(&mut self) -> Result<u32> {
        let byte = *self.data.get(self.pos / 8).ok_or_else(Self::eof)?;
        let b = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Ok(b as u32)
    }

    pub fn bits(&mut self, n: u32) -> Result<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        Ok(v)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        if self.pos + n > self.data.len() * 8 {
            return Err(Self::eof());
        }
        self.pos += n;
        Ok(())
    }

    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(CodecError::InvalidData("exp-golomb overflow".into()));
            }
        }
        let rest = self.bits(zeros)? as u32;
        Ok(((1u64 << zeros) - 1) as u32 + rest)
    }

    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()? as i64;
        Ok(if k % 2 == 1 { ((k + 1) / 2) as i32 } else { -(k / 2) as i32 })
    }
}

/// Fields of an H.264 SPS needed for `avcC` and codec strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H264Sps {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
}

fn h264_has_chroma_info(profile_idc: u8) -> bool {
    matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135)
}

impl H264Sps {
    /// Parses an SPS NAL (header byte included).
    pub fn parse(sps_nal: &[u8]) -> Result<Self> {
        if sps_nal.len() < 4 || h264_nal_type(sps_nal) != nal_type::H264_SPS {
            return Err(CodecError::InvalidData("not an H.264 SPS".into()));
        }
        let rbsp = nal_to_rbsp(&sps_nal[1..]);
        let mut r = BitReader::new(&rbsp);
        let profile_idc = r.bits(8)? as u8;
        let constraint_flags = r.bits(8)? as u8;
        let level_idc = r.bits(8)? as u8;
        let _sps_id = r.ue()?;
        let (mut chroma, mut bdl, mut bdc) = (1, 0, 0);
        if h264_has_chroma_info(profile_idc) {
            chroma = r.ue()?;
            if chroma == 3 {
                r.skip(1)?; // separate_colour_plane_flag
            }
            bdl = r.ue()?;
            bdc = r.ue()?;
        }
        Ok(Self {
            profile_idc,
            constraint_flags,
            level_idc,
            chroma_format_idc: chroma,
            bit_depth_luma_minus8: bdl,
            bit_depth_chroma_minus8: bdc,
        })
    }

    /// e.g. `avc1.640028`.
    pub fn codec_string(&self) -> String {
        format!("avc1.{:02x}{:02x}{:02x}", self.profile_idc, self.constraint_flags, self.level_idc)
    }
}

/// `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15 §5.3.3.1) with 4-byte NAL lengths.
pub fn avc_decoder_config(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>> {
    let info = H264Sps::parse(sps)?;
    if sps.len() > u16::MAX as usize || pps.len() > u16::MAX as usize || pps.is_empty() {
        return Err(CodecError::InvalidData("parameter set size".into()));
    }
    let mut out = Vec::with_capacity(16 + sps.len() + pps.len());
    out.push(1); // configurationVersion
    out.push(sps[1]); // AVCProfileIndication
    out.push(sps[2]); // profile_compatibility
    out.push(sps[3]); // AVCLevelIndication
    out.push(0xFC | 3); // reserved(6) + lengthSizeMinusOne
    out.push(0xE0 | 1); // reserved(3) + numOfSequenceParameterSets
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1); // numOfPictureParameterSets
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    if h264_has_chroma_info(info.profile_idc) {
        out.push(0xFC | (info.chroma_format_idc as u8 & 3));
        out.push(0xF8 | (info.bit_depth_luma_minus8 as u8 & 7));
        out.push(0xF8 | (info.bit_depth_chroma_minus8 as u8 & 7));
        out.push(0); // numOfSequenceParameterSetExt
    }
    Ok(out)
}

/// Fields of an HEVC SPS needed for `hvcC` and codec strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HevcSps {
    pub max_sub_layers_minus1: u8,
    pub temporal_id_nesting: bool,
    pub general_profile_space: u8,
    pub general_tier_flag: bool,
    pub general_profile_idc: u8,
    pub general_profile_compatibility_flags: u32,
    /// 48 bits: progressive, interlaced, non-packed, frame-only + 44 constraint bits.
    pub general_constraint_indicator_flags: u64,
    pub general_level_idc: u8,
    pub chroma_format_idc: u32,
    pub width: u32,
    pub height: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
}

impl HevcSps {
    /// Parses an SPS NAL (2-byte NAL header included).
    pub fn parse(sps_nal: &[u8]) -> Result<Self> {
        if sps_nal.len() < 4 || hevc_nal_type(sps_nal) != nal_type::HEVC_SPS {
            return Err(CodecError::InvalidData("not an HEVC SPS".into()));
        }
        let rbsp = nal_to_rbsp(&sps_nal[2..]);
        let mut r = BitReader::new(&rbsp);
        let _vps_id = r.bits(4)?;
        let max_sub_layers_minus1 = r.bits(3)? as u8;
        let temporal_id_nesting = r.bit()? == 1;
        // profile_tier_level(1, max_sub_layers_minus1)
        let general_profile_space = r.bits(2)? as u8;
        let general_tier_flag = r.bit()? == 1;
        let general_profile_idc = r.bits(5)? as u8;
        let general_profile_compatibility_flags = r.bits(32)? as u32;
        let general_constraint_indicator_flags = r.bits(48)?;
        let general_level_idc = r.bits(8)? as u8;
        let mut sub_profile = [false; 8];
        let mut sub_level = [false; 8];
        for i in 0..max_sub_layers_minus1 as usize {
            sub_profile[i] = r.bit()? == 1;
            sub_level[i] = r.bit()? == 1;
        }
        if max_sub_layers_minus1 > 0 {
            for _ in max_sub_layers_minus1..8 {
                r.skip(2)?;
            }
        }
        for i in 0..max_sub_layers_minus1 as usize {
            if sub_profile[i] {
                r.skip(88)?;
            }
            if sub_level[i] {
                r.skip(8)?;
            }
        }
        let _sps_id = r.ue()?;
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.skip(1)?; // separate_colour_plane_flag
        }
        let mut width = r.ue()?;
        let mut height = r.ue()?;
        if r.bit()? == 1 {
            let (l, rt, t, b) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
            let (sub_w, sub_h) = match chroma_format_idc {
                1 => (2, 2),
                2 => (2, 1),
                _ => (1, 1),
            };
            width = width.saturating_sub(sub_w * (l + rt));
            height = height.saturating_sub(sub_h * (t + b));
        }
        let bit_depth_luma_minus8 = r.ue()?;
        let bit_depth_chroma_minus8 = r.ue()?;
        Ok(Self {
            max_sub_layers_minus1,
            temporal_id_nesting,
            general_profile_space,
            general_tier_flag,
            general_profile_idc,
            general_profile_compatibility_flags,
            general_constraint_indicator_flags,
            general_level_idc,
            chroma_format_idc,
            width,
            height,
            bit_depth_luma_minus8,
            bit_depth_chroma_minus8,
        })
    }

    /// ISO/IEC 14496-15 Annex E codec string, e.g. `hvc1.1.6.L93.B0` (Main) or
    /// `hvc1.4.10.L120.9E.08` (Range Extensions / 4:4:4).
    pub fn codec_string(&self) -> String {
        let space = match self.general_profile_space {
            1 => "A",
            2 => "B",
            3 => "C",
            _ => "",
        };
        let compat = self.general_profile_compatibility_flags.reverse_bits();
        let tier = if self.general_tier_flag { 'H' } else { 'L' };
        let mut s = format!("hvc1.{space}{}.{compat:X}.{tier}{}", self.general_profile_idc, self.general_level_idc);
        let bytes = self.general_constraint_indicator_flags.to_be_bytes(); // 8 bytes, top 2 unused
        let constraint = &bytes[2..];
        let last = constraint.iter().rposition(|b| *b != 0);
        if let Some(last) = last {
            for b in &constraint[..=last] {
                s.push_str(&format!(".{b:02X}"));
            }
        }
        s
    }
}

/// `HEVCDecoderConfigurationRecord` (ISO/IEC 14496-15 §8.3.3.1) with 4-byte NAL lengths and
/// one array each for VPS, SPS and PPS (`array_completeness = 1`).
pub fn hevc_decoder_config(vps: &[u8], sps: &[u8], pps: &[u8]) -> Result<Vec<u8>> {
    let info = HevcSps::parse(sps)?;
    if hevc_nal_type(vps) != nal_type::HEVC_VPS || hevc_nal_type(pps) != nal_type::HEVC_PPS {
        return Err(CodecError::InvalidData("expected HEVC VPS / PPS".into()));
    }
    let mut out = Vec::with_capacity(23 + 15 + vps.len() + sps.len() + pps.len());
    out.push(1); // configurationVersion
    out.push((info.general_profile_space << 6) | ((info.general_tier_flag as u8) << 5) | (info.general_profile_idc & 0x1F));
    out.extend_from_slice(&info.general_profile_compatibility_flags.to_be_bytes());
    out.extend_from_slice(&info.general_constraint_indicator_flags.to_be_bytes()[2..]);
    out.push(info.general_level_idc);
    out.extend_from_slice(&0xF000u16.to_be_bytes()); // reserved + min_spatial_segmentation_idc = 0
    out.push(0xFC); // reserved + parallelismType = 0 (unknown)
    out.push(0xFC | (info.chroma_format_idc as u8 & 3));
    out.push(0xF8 | (info.bit_depth_luma_minus8 as u8 & 7));
    out.push(0xF8 | (info.bit_depth_chroma_minus8 as u8 & 7));
    out.extend_from_slice(&0u16.to_be_bytes()); // avgFrameRate
    // constantFrameRate(2)=0 numTemporalLayers(3) temporalIdNested(1) lengthSizeMinusOne(2)=3
    out.push((((info.max_sub_layers_minus1 + 1) & 7) << 3) | ((info.temporal_id_nesting as u8) << 2) | 3);
    out.push(3); // numOfArrays
    for (ty, nal) in [(nal_type::HEVC_VPS, vps), (nal_type::HEVC_SPS, sps), (nal_type::HEVC_PPS, pps)] {
        if nal.len() > u16::MAX as usize {
            return Err(CodecError::InvalidData("parameter set too large".into()));
        }
        out.push(0x80 | ty); // array_completeness = 1, reserved = 0
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(nal);
    }
    Ok(out)
}

/// Parses an `avcC` / `hvcC` record back into parameter sets (first of each kind).
pub fn parameter_sets_from_config(codec: VideoCodec, record: &[u8]) -> Result<ParameterSets> {
    let bad = || CodecError::InvalidData("malformed decoder configuration record".into());
    let read_u16 = |at: usize| -> Result<usize> {
        record.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize).ok_or_else(bad)
    };
    let mut ps = ParameterSets::default();
    match codec {
        VideoCodec::H264 => {
            let mut i = 5;
            let nsps = (*record.get(i).ok_or_else(bad)? & 0x1F) as usize;
            i += 1;
            for k in 0..nsps {
                let len = read_u16(i)?;
                let nal = record.get(i + 2..i + 2 + len).ok_or_else(bad)?;
                if k == 0 {
                    ps.sps = nal.to_vec();
                }
                i += 2 + len;
            }
            let npps = *record.get(i).ok_or_else(bad)? as usize;
            i += 1;
            for k in 0..npps {
                let len = read_u16(i)?;
                let nal = record.get(i + 2..i + 2 + len).ok_or_else(bad)?;
                if k == 0 {
                    ps.pps = nal.to_vec();
                }
                i += 2 + len;
            }
        }
        VideoCodec::Hevc => {
            let mut i = 22;
            let arrays = *record.get(i).ok_or_else(bad)?;
            i += 1;
            for _ in 0..arrays {
                let ty = *record.get(i).ok_or_else(bad)? & 0x3F;
                let n = read_u16(i + 1)?;
                i += 3;
                for k in 0..n {
                    let len = read_u16(i)?;
                    let nal = record.get(i + 2..i + 2 + len).ok_or_else(bad)?.to_vec();
                    if k == 0 {
                        match ty {
                            nal_type::HEVC_VPS => ps.vps = Some(nal),
                            nal_type::HEVC_SPS => ps.sps = nal,
                            nal_type::HEVC_PPS => ps.pps = nal,
                            _ => {}
                        }
                    }
                    i += 2 + len;
                }
            }
        }
        _ => return Err(CodecError::Unsupported("decoder configuration records exist only for H.264/HEVC")),
    }
    if ps.sps.is_empty() || ps.pps.is_empty() {
        return Err(bad());
    }
    Ok(ps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // Reference vectors produced by ffmpeg (libx264 / libx265, 320x240 testsrc) and the avcC /
    // hvcC boxes ffmpeg's MP4 muxer wrote for the same streams.
    const AVC_SPS: &str = "6764100dacb8283f60220000030002000003006408";
    const AVC_PPS: &str = "68ee0f2c8b";
    const AVC_C: &str = "0164100dffe100156764100dacb8283f6022000003000200000300640801000568ee0f2c8bfdf8f800";

    const HEVC_VPS: &str = "40010c01ffff01600000030090000003000003003c959809";
    const HEVC_SPS: &str = "42010101600000030090000003000003003ca00a080f165959a4932bc05a020000030002000003003210";
    const HEVC_PPS: &str = "4401c172b46240";
    const HVC_C: &str = "0101600000009000000000003cf000fcfdf8f800000f03a00001001840010c01ffff01600000030090000003000003003c959809a10001002a42010101600000030090000003000003003ca00a080f165959a4932bc05a020000030002000003003210a2000100074401c172b46240";

    const HEVC444_VPS: &str = "40010c01ffff0408000003009e0800000300003c959809";
    const HEVC444_SPS: &str = "4201010408000003009e0800000300003c90014101e2cb2b349265780b4040000003004000000642";
    const HEVC444_PPS: &str = "4401c172860c4624";
    const HVC_C_444: &str = "0104080000009e08000000003cf000fcfff8f800000f03a00001001740010c01ffff0408000003009e0800000300003c959809a1000100284201010408000003009e0800000300003c90014101e2cb2b349265780b4040000003004000000642a2000100084401c172860c4624";

    #[test]
    fn annexb_split_mixed_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 0, 0, 3, 1, 9];
        let nals: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 0, 0, 3, 1, 9][..]]);
    }

    #[test]
    fn annexb_avcc_roundtrip() {
        let data = [0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 0xAA];
        let avcc = annexb_to_avcc(&data);
        assert_eq!(avcc, vec![0, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 2, 0x68, 3, 0, 0, 0, 2, 0x65, 0xAA]);
        let back = avcc_to_annexb(&avcc, 4).unwrap();
        assert_eq!(back, vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 0xAA]);
        assert_eq!(annexb_to_avcc(&back), avcc);
        // 2-byte lengths
        assert_eq!(avcc_to_annexb(&[0, 1, 0x41, 0, 2, 0x42, 0x43], 2).unwrap(), vec![0, 0, 0, 1, 0x41, 0, 0, 0, 1, 0x42, 0x43]);
        assert!(avcc_to_annexb(&[0, 0, 0, 9, 1], 4).is_err());
        assert!(avcc_to_annexb(&[0, 0, 0], 4).is_err());
        assert!(avcc_to_annexb(&[], 3).is_err());
    }

    #[test]
    fn rbsp_and_exp_golomb() {
        assert_eq!(nal_to_rbsp(&[0, 0, 3, 1, 0, 0, 3, 0, 3]), vec![0, 0, 1, 0, 0, 0, 3]);
        // bits: 1 | 010 | 011 | 00100 | 00101 → ue 0,1,2,3, se(4) = -2
        let data = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 0);
        assert_eq!(r.ue().unwrap(), 1);
        assert_eq!(r.ue().unwrap(), 2);
        assert_eq!(r.ue().unwrap(), 3);
        assert_eq!(r.se().unwrap(), -2);
    }

    #[test]
    fn avcc_record_matches_ffmpeg() {
        let rec = avc_decoder_config(&hex(AVC_SPS), &hex(AVC_PPS)).unwrap();
        assert_eq!(rec, hex(AVC_C));
        let sps = H264Sps::parse(&hex(AVC_SPS)).unwrap();
        assert_eq!((sps.profile_idc, sps.level_idc, sps.chroma_format_idc), (100, 13, 1));
        assert_eq!(sps.codec_string(), "avc1.64100d");
        let ps = parameter_sets_from_config(VideoCodec::H264, &rec).unwrap();
        assert_eq!(ps.sps, hex(AVC_SPS));
        assert_eq!(ps.pps, hex(AVC_PPS));
    }

    #[test]
    fn hvcc_record_matches_ffmpeg() {
        let rec = hevc_decoder_config(&hex(HEVC_VPS), &hex(HEVC_SPS), &hex(HEVC_PPS)).unwrap();
        assert_eq!(rec, hex(HVC_C));
        let sps = HevcSps::parse(&hex(HEVC_SPS)).unwrap();
        assert_eq!((sps.width, sps.height, sps.chroma_format_idc), (320, 240, 1));
        assert_eq!(sps.general_profile_idc, 1);
        assert_eq!(sps.codec_string(), "hvc1.1.6.L60.90");

        let rec = hevc_decoder_config(&hex(HEVC444_VPS), &hex(HEVC444_SPS), &hex(HEVC444_PPS)).unwrap();
        assert_eq!(rec, hex(HVC_C_444));
        let sps = HevcSps::parse(&hex(HEVC444_SPS)).unwrap();
        assert_eq!((sps.width, sps.height, sps.chroma_format_idc), (320, 240, 3));
        assert_eq!(sps.codec_string(), "hvc1.4.10.L60.9E.08");

        let ps = parameter_sets_from_config(VideoCodec::Hevc, &rec).unwrap();
        assert_eq!(ps.vps.as_deref(), Some(&hex(HEVC444_VPS)[..]));
        assert_eq!(ps.decoder_config().unwrap(), rec);
    }

    #[test]
    fn extract_parameter_sets_from_annexb() {
        let mut au = Vec::new();
        for n in [HEVC_VPS, HEVC_SPS, HEVC_PPS] {
            push_annexb_nal(&mut au, &hex(n));
        }
        push_annexb_nal(&mut au, &[0x26, 0x01, 0xAF]); // IDR_W_RADL slice
        let ps = ParameterSets::from_annexb(VideoCodec::Hevc, &au).unwrap();
        assert_eq!(ps.sps, hex(HEVC_SPS));
        assert_eq!(ps.codec_string().unwrap(), "hvc1.1.6.L60.90");
        assert_eq!(hevc_nal_type(&[0x26, 0x01]), nal_type::HEVC_IDR_W_RADL);
        assert!(ParameterSets::from_annexb(VideoCodec::H264, &au).is_none());

        let mut au = Vec::new();
        push_annexb_nal(&mut au, &hex(AVC_SPS));
        push_annexb_nal(&mut au, &hex(AVC_PPS));
        let ps = ParameterSets::from_annexb(VideoCodec::H264, &au).unwrap();
        assert_eq!(ps.decoder_config().unwrap(), hex(AVC_C));
    }

    #[test]
    fn malformed_inputs_are_errors() {
        assert!(H264Sps::parse(&[0x67, 0x64]).is_err());
        assert!(HevcSps::parse(&hex(HEVC_SPS)[..6]).is_err());
        assert!(hevc_decoder_config(&hex(HEVC_SPS), &hex(HEVC_SPS), &hex(HEVC_PPS)).is_err());
        assert!(parameter_sets_from_config(VideoCodec::Hevc, &[1, 2, 3]).is_err());
    }
}
