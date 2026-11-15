//! Portable BGRA ↔ NV12 colour conversion (BT.709, limited/"video" range), used by the
//! Media Foundation backend on Windows (hardware and software H.264/HEVC MFTs take NV12
//! input and decoders produce NV12) and unit-tested on every OS.
//!
//! * [`bgra_to_nv12`] — luma per pixel, chroma from the 2×2 average of the RGB values.
//!   The destination may be larger than the source (encoders need even, sometimes
//!   16-aligned, sizes): the right column / bottom row are replicated into the padding.
//! * [`nv12_to_bgra`] — nearest (2×2 shared) chroma, opaque alpha, cropped to the display size.
//!
//! Arithmetic is 16.16 fixed point; the coefficient sets sum exactly to the range
//! (Y: 219/255, Cb/Cr: 0) so white / black / greys map to Y = 235 / 16, Cb = Cr = 128.

use crate::{CodecError, Result};
use aether_capture::BgraImage;

/// Layout of an NV12 picture in one contiguous buffer: `height` luma rows of `pitch` bytes,
/// followed at `uv_offset` by `height / 2` rows of interleaved `U, V` pairs (`pitch` bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nv12Layout {
    /// Picture width in pixels (even).
    pub width: u32,
    /// Picture height in pixels (even).
    pub height: u32,
    /// Bytes per row of both planes (≥ `width`).
    pub pitch: usize,
    /// Byte offset of the UV plane (≥ `pitch * height`).
    pub uv_offset: usize,
}

impl Nv12Layout {
    /// Tightly packed layout (`pitch == width`, UV plane right after the luma plane).
    /// Odd sizes are rounded up to even.
    pub fn packed(width: u32, height: u32) -> Self {
        let (w, h) = (even(width), even(height));
        Self { width: w, height: h, pitch: w as usize, uv_offset: w as usize * h as usize }
    }

    /// Layout with an explicit pitch; the UV plane follows `pitch * plane_rows` bytes
    /// (`plane_rows ≥ height`, e.g. a decoder's 16-aligned coded height).
    pub fn with_pitch(width: u32, height: u32, pitch: usize, plane_rows: u32) -> Self {
        let (w, h) = (even(width), even(height));
        Self { width: w, height: h, pitch, uv_offset: pitch * plane_rows.max(h) as usize }
    }

    /// Bytes needed for the whole picture.
    pub fn len(&self) -> usize {
        self.uv_offset + self.pitch * (self.height as usize / 2)
    }

    /// True for a zero-sized picture.
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    fn validate(&self, buf_len: usize) -> Result<()> {
        if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            return Err(CodecError::InvalidData(format!("NV12 size {}x{} is not even", self.width, self.height)));
        }
        if self.pitch < self.width as usize || self.uv_offset < self.pitch * self.height as usize {
            return Err(CodecError::InvalidData(format!("invalid NV12 layout {self:?}")));
        }
        if buf_len < self.len() {
            return Err(CodecError::InvalidData(format!("NV12 buffer {buf_len} < required {}", self.len())));
        }
        Ok(())
    }
}

/// Rounds up to the next even number.
#[inline]
pub fn even(v: u32) -> u32 {
    v.saturating_add(1) & !1
}

// RGB → YCbCr (BT.709 limited range), 16.16 fixed point.
const YR: i32 = 11966;
const YG: i32 = 40254;
const YB: i32 = 4064;
const UR: i32 = -6596;
const UG: i32 = -22189;
const UB: i32 = 28785;
const VR: i32 = 28785;
const VG: i32 = -26145;
const VB: i32 = -2640;

// YCbCr → RGB.
const KY: i32 = 76309;
const KRV: i32 = 117489;
const KGU: i32 = 13975;
const KGV: i32 = 34925;
const KBU: i32 = 138438;

#[inline(always)]
fn luma(b: i32, g: i32, r: i32) -> u8 {
    (((YR * r + YG * g + YB * b + 32768) >> 16) + 16) as u8
}

/// Chroma from the *sum* of four pixels (hence the extra `>> 2`).
#[inline(always)]
fn chroma4(sb: i32, sg: i32, sr: i32) -> (u8, u8) {
    let u = ((UR * sr + UG * sg + UB * sb + (1 << 17)) >> 18) + 128;
    let v = ((VR * sr + VG * sg + VB * sb + (1 << 17)) >> 18) + 128;
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

/// Converts a BGRA picture (`src_w × src_h`, `src_stride` bytes per row; alpha ignored) into
/// `dst` laid out as `layout`. `layout` may be larger than the source; padding replicates the
/// last column / row. Bytes of `dst` outside the picture (pitch padding) are left untouched.
pub fn bgra_to_nv12(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    src_stride: usize,
    layout: &Nv12Layout,
    dst: &mut [u8],
) -> Result<()> {
    layout.validate(dst.len())?;
    if src_w == 0 || src_h == 0 {
        return Err(CodecError::InvalidData("empty BGRA source".into()));
    }
    if layout.width < src_w || layout.height < src_h {
        return Err(CodecError::InvalidData(format!(
            "NV12 {}x{} smaller than source {src_w}x{src_h}",
            layout.width, layout.height
        )));
    }
    let row_bytes = src_w as usize * 4;
    if src_stride < row_bytes || src.len() < src_stride * (src_h as usize - 1) + row_bytes {
        return Err(CodecError::InvalidData("BGRA source buffer too small".into()));
    }
    let (w, h) = (layout.width as usize, layout.height as usize);
    let (sw, sh) = (src_w as usize, src_h as usize);
    let pitch = layout.pitch;
    let (y_plane, uv_plane) = dst.split_at_mut(layout.uv_offset);
    // Fast path columns: pairs fully inside the source.
    let inner_pairs = sw / 2;
    for yy in 0..h / 2 {
        let r0 = (2 * yy).min(sh - 1);
        let r1 = (2 * yy + 1).min(sh - 1);
        let s0 = &src[r0 * src_stride..r0 * src_stride + row_bytes];
        let s1 = &src[r1 * src_stride..r1 * src_stride + row_bytes];
        let (ya, yb) = y_plane[2 * yy * pitch..(2 * yy + 2) * pitch].split_at_mut(pitch);
        let uv = &mut uv_plane[yy * pitch..yy * pitch + w];
        for xx in 0..inner_pairs {
            let o = xx * 8;
            let (a, b) = (&s0[o..o + 8], &s1[o..o + 8]);
            let px = |p: &[u8], i: usize| (p[i] as i32, p[i + 1] as i32, p[i + 2] as i32);
            let (b00, g00, r00) = px(a, 0);
            let (b01, g01, r01) = px(a, 4);
            let (b10, g10, r10) = px(b, 0);
            let (b11, g11, r11) = px(b, 4);
            ya[2 * xx] = luma(b00, g00, r00);
            ya[2 * xx + 1] = luma(b01, g01, r01);
            yb[2 * xx] = luma(b10, g10, r10);
            yb[2 * xx + 1] = luma(b11, g11, r11);
            let (u, v) = chroma4(b00 + b01 + b10 + b11, g00 + g01 + g10 + g11, r00 + r01 + r10 + r11);
            uv[2 * xx] = u;
            uv[2 * xx + 1] = v;
        }
        // Remaining columns (odd source width and/or horizontal padding): clamp to the edge.
        for xx in inner_pairs..w / 2 {
            let c0 = (2 * xx).min(sw - 1) * 4;
            let c1 = (2 * xx + 1).min(sw - 1) * 4;
            let px = |p: &[u8], i: usize| (p[i] as i32, p[i + 1] as i32, p[i + 2] as i32);
            let (b00, g00, r00) = px(s0, c0);
            let (b01, g01, r01) = px(s0, c1);
            let (b10, g10, r10) = px(s1, c0);
            let (b11, g11, r11) = px(s1, c1);
            ya[2 * xx] = luma(b00, g00, r00);
            ya[2 * xx + 1] = luma(b01, g01, r01);
            yb[2 * xx] = luma(b10, g10, r10);
            yb[2 * xx + 1] = luma(b11, g11, r11);
            let (u, v) = chroma4(b00 + b01 + b10 + b11, g00 + g01 + g10 + g11, r00 + r01 + r10 + r11);
            uv[2 * xx] = u;
            uv[2 * xx + 1] = v;
        }
    }
    Ok(())
}

/// Converts an owned [`BgraImage`] into a freshly allocated packed NV12 buffer of `layout`.
pub fn image_to_nv12(img: &BgraImage, layout: &Nv12Layout) -> Result<Vec<u8>> {
    let mut out = vec![0u8; layout.len()];
    bgra_to_nv12(&img.data, img.width, img.height, img.stride, layout, &mut out)?;
    Ok(out)
}

#[inline(always)]
fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Converts an NV12 picture (`layout`) to a packed BGRA image of `out_w × out_h`
/// (≤ the NV12 size: decoders output 16-aligned coded pictures that are cropped here).
pub fn nv12_to_bgra(src: &[u8], layout: &Nv12Layout, out_w: u32, out_h: u32) -> Result<BgraImage> {
    layout.validate(src.len())?;
    if out_w > layout.width || out_h > layout.height {
        return Err(CodecError::InvalidData(format!(
            "crop {out_w}x{out_h} exceeds NV12 {}x{}",
            layout.width, layout.height
        )));
    }
    let mut img = BgraImage::new(out_w, out_h);
    let pitch = layout.pitch;
    let (w, stride) = (out_w as usize, img.stride);
    for y in 0..out_h as usize {
        let yrow = &src[y * pitch..y * pitch + w];
        let uvrow = &src[layout.uv_offset + (y / 2) * pitch..];
        let out = &mut img.data[y * stride..y * stride + w * 4];
        for x in 0..w {
            let yy = (yrow[x] as i32 - 16) * KY;
            let u = uvrow[x & !1] as i32 - 128;
            let v = uvrow[(x & !1) + 1] as i32 - 128;
            let o = x * 4;
            out[o] = clamp_u8((yy + KBU * u + 32768) >> 16);
            out[o + 1] = clamp_u8((yy - KGU * u - KGV * v + 32768) >> 16);
            out[o + 2] = clamp_u8((yy + KRV * v + 32768) >> 16);
            out[o + 3] = 255;
        }
    }
    Ok(img)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_capture::TestPattern;

    fn solid(w: u32, h: u32, bgra: [u8; 4]) -> BgraImage {
        let mut img = BgraImage::new(w, h);
        for p in img.data.as_chunks_mut::<4>().0.iter_mut() {
            *p = bgra;
        }
        img
    }

    fn yuv_of(bgra: [u8; 4]) -> (u8, u8, u8) {
        let img = solid(2, 2, bgra);
        let l = Nv12Layout::packed(2, 2);
        let nv = image_to_nv12(&img, &l).unwrap();
        assert!(nv[..4].iter().all(|&y| y == nv[0]));
        (nv[0], nv[4], nv[5])
    }

    #[test]
    fn reference_colours_bt709_limited() {
        assert_eq!(yuv_of([255, 255, 255, 255]), (235, 128, 128));
        assert_eq!(yuv_of([0, 0, 0, 255]), (16, 128, 128));
        assert_eq!(yuv_of([0, 0, 255, 255]), (63, 102, 240)); // red
        assert_eq!(yuv_of([0, 255, 0, 255]), (173, 42, 26)); // green
        assert_eq!(yuv_of([255, 0, 0, 255]), (32, 240, 118)); // blue
        // Greys are neutral.
        for g in [1u8, 77, 128, 200, 254] {
            let (_, u, v) = yuv_of([g, g, g, 255]);
            assert_eq!((u, v), (128, 128), "grey {g}");
        }
    }

    #[test]
    fn reference_colours_back_to_rgb() {
        for c in [[255u8, 255, 255, 255], [0, 0, 0, 255], [0, 0, 255, 255], [0, 255, 0, 255], [255, 0, 0, 255], [37, 140, 211, 255]] {
            let img = solid(4, 4, c);
            let l = Nv12Layout::packed(4, 4);
            let back = nv12_to_bgra(&image_to_nv12(&img, &l).unwrap(), &l, 4, 4).unwrap();
            for p in back.data.as_chunks::<4>().0.iter() {
                for i in 0..3 {
                    assert!((p[i] as i32 - c[i] as i32).abs() <= 2, "{c:?} -> {p:?}");
                }
                assert_eq!(p[3], 255);
            }
        }
    }

    #[test]
    fn roundtrip_error_bounds_on_test_pattern() {
        let img = TestPattern::new(320, 180).frame(7);
        let l = Nv12Layout::packed(320, 180);
        let back = nv12_to_bgra(&image_to_nv12(&img, &l).unwrap(), &l, 320, 180).unwrap();
        let (mut sum, mut max, mut n) = (0u64, 0i32, 0u64);
        for (a, b) in img.data.as_chunks::<4>().0.iter().zip(back.data.as_chunks::<4>().0.iter()) {
            for i in 0..3 {
                let d = (a[i] as i32 - b[i] as i32).abs();
                sum += d as u64;
                max = max.max(d);
                n += 1;
            }
        }
        let mean = sum as f64 / n as f64;
        // 4:2:0 subsampling smears chroma across sharp edges; on average it must stay tiny.
        assert!(mean < 4.0, "mean abs error {mean}");
        assert!(max <= 255);
        // Luma alone (grey content) must be near-lossless.
        let grey = {
            let mut g = BgraImage::new(64, 64);
            for y in 0..64u32 {
                for x in 0..64u32 {
                    let v = ((x * 4) ^ (y * 3)) as u8;
                    g.row_mut(y)[x as usize * 4..x as usize * 4 + 4].copy_from_slice(&[v, v, v, 255]);
                }
            }
            g
        };
        let l = Nv12Layout::packed(64, 64);
        let back = nv12_to_bgra(&image_to_nv12(&grey, &l).unwrap(), &l, 64, 64).unwrap();
        for (a, b) in grey.data.as_chunks::<4>().0.iter().zip(back.data.as_chunks::<4>().0.iter()) {
            assert!((a[0] as i32 - b[0] as i32).abs() <= 1, "{a:?} {b:?}");
        }
    }

    #[test]
    fn odd_dimensions_pad_by_edge_replication() {
        let (w, h) = (5u32, 3u32);
        let mut img = BgraImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let v = (x * 50 + y * 10) as u8;
                img.row_mut(y)[x as usize * 4..x as usize * 4 + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let l = Nv12Layout::packed(w, h);
        assert_eq!((l.width, l.height, l.len()), (6, 4, 36));
        let nv = image_to_nv12(&img, &l).unwrap();
        let y_at = |x: usize, y: usize| nv[y * 6 + x];
        // Padding column 5 replicates column 4; padding row 3 replicates row 2.
        for y in 0..4 {
            assert_eq!(y_at(5, y), y_at(4, y));
        }
        for x in 0..6 {
            assert_eq!(y_at(x, 3), y_at(x, 2));
        }
        let back = nv12_to_bgra(&nv, &l, w, h).unwrap();
        assert_eq!((back.width, back.height), (w, h));
        for y in 0..h {
            for x in 0..w {
                let a = img.pixel(x, y)[0] as i32;
                let b = back.pixel(x, y)[0] as i32;
                assert!((a - b).abs() <= 1, "({x},{y}) {a} vs {b}");
            }
        }
    }

    #[test]
    fn strided_layouts_and_validation() {
        let img = TestPattern::new(34, 18).frame(1);
        let packed = Nv12Layout::packed(34, 18);
        let strided = Nv12Layout::with_pitch(34, 18, 64, 32); // 16-aligned rows like a decoder
        let a = image_to_nv12(&img, &packed).unwrap();
        let mut b = vec![0u8; strided.len()];
        bgra_to_nv12(&img.data, img.width, img.height, img.stride, &strided, &mut b).unwrap();
        let ia = nv12_to_bgra(&a, &packed, 34, 18).unwrap();
        let ib = nv12_to_bgra(&b, &strided, 34, 18).unwrap();
        assert_eq!(ia, ib);
        assert!(nv12_to_bgra(&a[..10], &packed, 34, 18).is_err());
        assert!(nv12_to_bgra(&a, &packed, 36, 18).is_err());
        let small = Nv12Layout::packed(32, 18);
        let mut c = vec![0u8; small.len()];
        assert!(bgra_to_nv12(&img.data, img.width, img.height, img.stride, &small, &mut c).is_err());
    }
}
