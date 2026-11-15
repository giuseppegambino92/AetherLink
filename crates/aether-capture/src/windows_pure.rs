//! OS-independent helpers of the Windows capture backends: DXGI device-name parsing,
//! dirty-rect normalisation, cursor-shape compositing (monochrome / color / masked-color
//! pointer shapes as returned by `IDXGIOutputDuplication::GetFramePointerShape`), CPU
//! area downscaling, quarter-turn rotation and WASAPI sample-format conversion.
//!
//! Compiled on Windows and in tests on every OS, so the pixel/sample math is verified on
//! the macOS development machine too.

use crate::BgraImage;
use aether_protocol::Rect;

/// Above this many rects the list is collapsed into its bounding box (cheaper downstream).
pub(crate) const MAX_DIRTY_RECTS: usize = 64;

/// `\\.\DISPLAY3` → `3`. The number is stable across reboots for a given connector and is
/// what Windows itself shows in Display Settings, so it is used as `DisplayInfo::id`.
pub(crate) fn parse_display_number(device_name: &str) -> Option<u32> {
    let upper = device_name.trim().to_ascii_uppercase();
    let idx = upper.rfind("DISPLAY")?;
    let digits = &upper[idx + "DISPLAY".len()..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// UTF-16 buffer (NUL terminated or full) → `String`.
pub(crate) fn wide_to_string(w: &[u16]) -> String {
    let len = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..len])
}

/// Win32 `RECT` (left, top, right, bottom; exclusive) → [`Rect`], `None` when empty.
pub(crate) fn rect_from_ltrb(l: i32, t: i32, r: i32, b: i32) -> Option<Rect> {
    (r > l && b > t).then(|| Rect::new(l, t, (r as i64 - l as i64) as u32, (b as i64 - t as i64) as u32))
}

/// Intersection of `r` with `0..w × 0..h`.
pub(crate) fn clamp_rect(r: Rect, w: u32, h: u32) -> Option<Rect> {
    let x0 = r.x.max(0) as i64;
    let y0 = r.y.max(0) as i64;
    let x1 = (r.x as i64 + r.width as i64).min(w as i64);
    let y1 = (r.y as i64 + r.height as i64).min(h as i64);
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0 as i32, y0 as i32, (x1 - x0) as u32, (y1 - y0) as u32))
}

fn contains_rect(outer: &Rect, inner: &Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x as i64 + inner.width as i64 <= outer.x as i64 + outer.width as i64
        && inner.y as i64 + inner.height as i64 <= outer.y as i64 + outer.height as i64
}

/// Bounding box of a non-empty set of rects.
pub(crate) fn bounding_box(rects: &[Rect]) -> Option<Rect> {
    let first = rects.first()?;
    let (mut x0, mut y0) = (first.x as i64, first.y as i64);
    let (mut x1, mut y1) = (x0 + first.width as i64, y0 + first.height as i64);
    for r in &rects[1..] {
        x0 = x0.min(r.x as i64);
        y0 = y0.min(r.y as i64);
        x1 = x1.max(r.x as i64 + r.width as i64);
        y1 = y1.max(r.y as i64 + r.height as i64);
    }
    Some(Rect::new(x0 as i32, y0 as i32, (x1 - x0) as u32, (y1 - y0) as u32))
}

/// Clamps to the frame, drops empty and fully contained duplicates and collapses very long
/// lists into their bounding box.
pub(crate) fn normalize_dirty(rects: impl IntoIterator<Item = Rect>, w: u32, h: u32) -> Vec<Rect> {
    let mut clamped: Vec<Rect> = rects.into_iter().filter_map(|r| clamp_rect(r, w, h)).collect();
    // Largest first so containment checks remove the small ones.
    clamped.sort_by_key(|r| std::cmp::Reverse(r.area()));
    let mut out: Vec<Rect> = Vec::with_capacity(clamped.len());
    for r in clamped {
        if !out.iter().any(|o| contains_rect(o, &r)) {
            out.push(r);
        }
    }
    if out.len() > MAX_DIRTY_RECTS {
        return bounding_box(&out).into_iter().collect();
    }
    out
}

/// Rects of a `w × h` frame mapped to a `dw × dh` downscaled frame, rounded outwards.
pub(crate) fn scale_rects(rects: &[Rect], w: u32, h: u32, dw: u32, dh: u32) -> Vec<Rect> {
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let sx = dw as f64 / w as f64;
    let sy = dh as f64 / h as f64;
    let mapped = rects.iter().map(|r| {
        let x0 = (r.x as f64 * sx).floor() as i64;
        let y0 = (r.y as f64 * sy).floor() as i64;
        let x1 = ((r.x as f64 + r.width as f64) * sx).ceil() as i64;
        let y1 = ((r.y as f64 + r.height as f64) * sy).ceil() as i64;
        Rect::new(x0 as i32, y0 as i32, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
    });
    normalize_dirty(mapped, dw, dh)
}

/// Output size for a capture `scale` (clamped to `0.1..=1.0`; even dimensions for 4:2:0
/// encoders when actually scaling, native size otherwise).
pub(crate) fn scaled_size(w: u32, h: u32, scale: f32) -> (u32, u32) {
    let s = if scale.is_finite() { scale.clamp(0.1, 1.0) } else { 1.0 };
    if s >= 0.999 {
        return (w, h);
    }
    let even = |v: u32| {
        let x = ((v as f64 * s as f64).round() as u32) & !1;
        x.max(2)
    };
    (even(w).min(w), even(h).min(h))
}

/// Area-average (box filter) downscale of `src` to `dw × dh` (each ≤ the source size).
pub(crate) fn downscale(src: &BgraImage, dw: u32, dh: u32) -> BgraImage {
    let (sw, sh) = (src.width as usize, src.height as usize);
    let (dw_u, dh_u) = (dw.max(1) as usize, dh.max(1) as usize);
    let mut out = BgraImage::new(dw_u as u32, dh_u as u32);
    if sw == 0 || sh == 0 {
        return out;
    }
    // Precompute horizontal source spans.
    let xspan: Vec<(usize, usize)> = (0..dw_u)
        .map(|ox| {
            let a = ox * sw / dw_u;
            let b = ((ox + 1) * sw / dw_u).max(a + 1).min(sw);
            (a, b)
        })
        .collect();
    let mut acc = vec![0u32; dw_u * 4];
    for oy in 0..dh_u {
        let y0 = oy * sh / dh_u;
        let y1 = ((oy + 1) * sh / dh_u).max(y0 + 1).min(sh);
        acc.iter_mut().for_each(|a| *a = 0);
        for y in y0..y1 {
            let row = src.row(y as u32);
            for (ox, &(a, b)) in xspan.iter().enumerate() {
                let o = ox * 4;
                for px in row[a * 4..b * 4].as_chunks::<4>().0 {
                    acc[o] += px[0] as u32;
                    acc[o + 1] += px[1] as u32;
                    acc[o + 2] += px[2] as u32;
                    acc[o + 3] += px[3] as u32;
                }
            }
        }
        let rows = (y1 - y0) as u32;
        let drow = out.row_mut(oy as u32);
        for (ox, &(a, b)) in xspan.iter().enumerate() {
            let n = rows * (b - a) as u32;
            for c in 0..4 {
                drow[ox * 4 + c] = ((acc[ox * 4 + c] + n / 2) / n) as u8;
            }
        }
    }
    out
}

/// Rotates an image clockwise by `quarter_turns × 90°` (DXGI hands out the desktop of a
/// rotated output in its unrotated orientation).
pub(crate) fn rotate_cw(src: &BgraImage, quarter_turns: u8) -> BgraImage {
    let q = quarter_turns % 4;
    if q == 0 {
        return src.to_packed();
    }
    let (w, h) = (src.width, src.height);
    let (dw, dh) = if q % 2 == 1 { (h, w) } else { (w, h) };
    let mut out = BgraImage::new(dw, dh);
    for y in 0..h {
        let row = src.row(y);
        for x in 0..w {
            let (dx, dy) = match q {
                1 => (h - 1 - y, x),
                2 => (w - 1 - x, h - 1 - y),
                _ => (y, w - 1 - x),
            };
            let o = dy as usize * out.stride + dx as usize * 4;
            out.data[o..o + 4].copy_from_slice(&row[x as usize * 4..x as usize * 4 + 4]);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------------------------

/// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorKind {
    /// 1 bpp AND mask (top half) + XOR mask (bottom half); `height` is the doubled height.
    Monochrome,
    /// 32 bpp BGRA, alpha blended.
    Color,
    /// 32 bpp BGRX; the alpha byte selects replace (0) or XOR with the screen (0xFF).
    MaskedColor,
}

impl CursorKind {
    pub(crate) fn from_dxgi(t: u32) -> Option<Self> {
        match t {
            1 => Some(Self::Monochrome),
            2 => Some(Self::Color),
            4 => Some(Self::MaskedColor),
            _ => None,
        }
    }
}

/// A pointer shape as returned by `GetFramePointerShape`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CursorShape {
    pub kind: CursorKind,
    pub width: u32,
    /// Height of the buffer as reported by DXGI (twice the visible height for monochrome).
    pub height: u32,
    pub pitch: u32,
    pub data: Vec<u8>,
}

impl CursorShape {
    /// Visible height in pixels.
    pub(crate) fn visible_height(&self) -> u32 {
        if self.kind == CursorKind::Monochrome { self.height / 2 } else { self.height }
    }

    fn valid(&self) -> bool {
        let row = match self.kind {
            CursorKind::Monochrome => self.width.div_ceil(8),
            _ => self.width * 4,
        } as usize;
        self.pitch as usize >= row && self.data.len() >= self.pitch as usize * self.height as usize
    }
}

/// Draws `shape` with its top-left corner at `(x, y)` (frame coordinates, may be partially
/// off-frame). Returns the frame region touched.
pub(crate) fn composite_cursor(img: &mut BgraImage, shape: &CursorShape, x: i32, y: i32) -> Option<Rect> {
    if !shape.valid() {
        return None;
    }
    let vis_h = shape.visible_height();
    let area = clamp_rect(Rect::new(x, y, shape.width, vis_h), img.width, img.height)?;
    let pitch = shape.pitch as usize;
    for fy in area.y..area.y + area.height as i32 {
        let sy = (fy - y) as usize;
        let start = fy as usize * img.stride;
        let row = &mut img.data[start..start + img.width as usize * 4];
        for fx in area.x..area.x + area.width as i32 {
            let sx = (fx - x) as usize;
            let d = &mut row[fx as usize * 4..fx as usize * 4 + 4];
            match shape.kind {
                CursorKind::Monochrome => {
                    let bit = 0x80u8 >> (sx % 8);
                    let and = shape.data[sy * pitch + sx / 8] & bit != 0;
                    let xor = shape.data[(sy + vis_h as usize) * pitch + sx / 8] & bit != 0;
                    match (and, xor) {
                        (false, false) => d[..3].copy_from_slice(&[0, 0, 0]),
                        (false, true) => d[..3].copy_from_slice(&[0xFF, 0xFF, 0xFF]),
                        (true, false) => {}
                        (true, true) => {
                            d[0] ^= 0xFF;
                            d[1] ^= 0xFF;
                            d[2] ^= 0xFF;
                        }
                    }
                    if !and {
                        d[3] = 0xFF;
                    }
                }
                CursorKind::Color => {
                    let s = &shape.data[sy * pitch + sx * 4..sy * pitch + sx * 4 + 4];
                    let a = s[3] as u32;
                    if a == 0xFF {
                        d[..3].copy_from_slice(&s[..3]);
                        d[3] = 0xFF;
                    } else if a != 0 {
                        for c in 0..3 {
                            d[c] = ((s[c] as u32 * a + d[c] as u32 * (255 - a) + 127) / 255) as u8;
                        }
                        d[3] = 0xFF;
                    }
                }
                CursorKind::MaskedColor => {
                    let s = &shape.data[sy * pitch + sx * 4..sy * pitch + sx * 4 + 4];
                    if s[3] == 0 {
                        d[..3].copy_from_slice(&s[..3]);
                        d[3] = 0xFF;
                    } else {
                        d[0] ^= s[0];
                        d[1] ^= s[1];
                        d[2] ^= s[2];
                    }
                }
            }
        }
    }
    Some(area)
}

// ---------------------------------------------------------------------------------------------
// WASAPI samples
// ---------------------------------------------------------------------------------------------

/// Sample encodings a WASAPI shared-mode mix format can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleFormat {
    F32,
    F64,
    /// Signed integer PCM in a container of `container_bytes` (1–4), `valid_bits` significant
    /// bits left-aligned in the container (e.g. 24-in-32).
    Int { container_bytes: u8, valid_bits: u8 },
}

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Decodes a `WAVEFORMATEX`'s tag / bits (and, for `WAVE_FORMAT_EXTENSIBLE`, the first field
/// of the sub-format GUID plus `wValidBitsPerSample`).
pub(crate) fn sample_format(tag: u16, bits: u16, ext_subformat_data1: Option<u32>, ext_valid_bits: Option<u16>) -> Option<SampleFormat> {
    let kind = match tag {
        WAVE_FORMAT_EXTENSIBLE => match ext_subformat_data1? {
            1 => WAVE_FORMAT_PCM,
            3 => WAVE_FORMAT_IEEE_FLOAT,
            _ => return None,
        },
        t => t,
    };
    match (kind, bits) {
        (WAVE_FORMAT_IEEE_FLOAT, 32) => Some(SampleFormat::F32),
        (WAVE_FORMAT_IEEE_FLOAT, 64) => Some(SampleFormat::F64),
        (WAVE_FORMAT_PCM, 8 | 16 | 24 | 32) => {
            let valid = ext_valid_bits.filter(|&v| v > 0 && v <= bits).unwrap_or(bits);
            Some(SampleFormat::Int { container_bytes: (bits / 8) as u8, valid_bits: valid as u8 })
        }
        _ => None,
    }
}

impl SampleFormat {
    pub(crate) fn bytes(self) -> usize {
        match self {
            SampleFormat::F32 => 4,
            SampleFormat::F64 => 8,
            SampleFormat::Int { container_bytes, .. } => container_bytes as usize,
        }
    }
}

/// Converts interleaved samples to interleaved `f32` little-endian bytes, appending to `out`.
pub(crate) fn samples_to_f32le(src: &[u8], fmt: SampleFormat, out: &mut Vec<u8>) {
    let n = fmt.bytes();
    out.reserve(src.len() / n * 4);
    for s in src.chunks_exact(n) {
        let v: f32 = match fmt {
            SampleFormat::F32 => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
            SampleFormat::F64 => f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]) as f32,
            SampleFormat::Int { container_bytes: 1, .. } => (s[0] as f32 - 128.0) / 128.0,
            SampleFormat::Int { container_bytes, .. } => {
                // Left-align into an i32: low (unused) bits stay zero.
                let mut raw = [0u8; 4];
                let c = container_bytes as usize;
                raw[4 - c..].copy_from_slice(&s[..c]);
                i32::from_le_bytes(raw) as f32 / 2_147_483_648.0
            }
        };
        out.extend_from_slice(&v.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_numbers() {
        assert_eq!(parse_display_number(r"\\.\DISPLAY1"), Some(1));
        assert_eq!(parse_display_number(r"\\.\DISPLAY12"), Some(12));
        assert_eq!(parse_display_number(r"\\.\display3"), Some(3));
        assert_eq!(parse_display_number(r"\\.\DISPLAY"), None);
        assert_eq!(parse_display_number(r"\\.\DISPLAY1\Monitor0"), None);
        assert_eq!(parse_display_number("garbage"), None);
        let w: Vec<u16> = "\\\\.\\DISPLAY2\0junk".encode_utf16().collect();
        assert_eq!(wide_to_string(&w), r"\\.\DISPLAY2");
    }

    #[test]
    fn rect_helpers() {
        assert_eq!(rect_from_ltrb(10, 20, 30, 25), Some(Rect::new(10, 20, 20, 5)));
        assert_eq!(rect_from_ltrb(10, 20, 10, 25), None);
        assert_eq!(clamp_rect(Rect::new(-5, -5, 10, 10), 100, 100), Some(Rect::new(0, 0, 5, 5)));
        assert_eq!(clamp_rect(Rect::new(95, 98, 10, 10), 100, 100), Some(Rect::new(95, 98, 5, 2)));
        assert_eq!(clamp_rect(Rect::new(200, 0, 10, 10), 100, 100), None);
        let n = normalize_dirty(
            [Rect::new(0, 0, 50, 50), Rect::new(10, 10, 5, 5), Rect::new(-10, 90, 30, 30), Rect::new(0, 0, 0, 9)],
            100,
            100,
        );
        assert_eq!(n, vec![Rect::new(0, 0, 50, 50), Rect::new(0, 90, 20, 10)]);
        let many: Vec<Rect> = (0..100).map(|i| Rect::new(i, i, 1, 1)).collect();
        assert_eq!(normalize_dirty(many, 1000, 1000), vec![Rect::new(0, 0, 100, 100)]);
        assert_eq!(scale_rects(&[Rect::new(1, 1, 3, 3)], 100, 100, 50, 50), vec![Rect::new(0, 0, 2, 2)]);
    }

    #[test]
    fn scaled_sizes() {
        assert_eq!(scaled_size(1920, 1080, 1.0), (1920, 1080));
        assert_eq!(scaled_size(1921, 1081, 1.5), (1921, 1081));
        assert_eq!(scaled_size(1920, 1080, 0.5), (960, 540));
        assert_eq!(scaled_size(1366, 768, 0.33), (450, 252));
        assert_eq!(scaled_size(10, 10, f32::NAN), (10, 10));
    }

    #[test]
    fn downscale_averages() {
        let mut img = BgraImage::new(4, 2);
        for y in 0..2 {
            for x in 0..4u32 {
                let v = (x * 40 + y * 10) as u8;
                img.row_mut(y)[x as usize * 4..x as usize * 4 + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let d = downscale(&img, 2, 1);
        // Pixel 0 averages x 0..2, y 0..2: (0+40+10+50)/4 = 25.
        assert_eq!(d.pixel(0, 0), [25, 25, 25, 255]);
        assert_eq!(d.pixel(1, 0), [105, 105, 105, 255]);
        let same = downscale(&img, 4, 2);
        assert_eq!(same, img);
    }

    #[test]
    fn rotation() {
        let mut img = BgraImage::new(3, 2);
        for y in 0..2 {
            for x in 0..3u32 {
                img.row_mut(y)[x as usize * 4] = (y * 3 + x) as u8;
            }
        }
        let r = rotate_cw(&img, 1);
        assert_eq!((r.width, r.height), (2, 3));
        // Top-left of the rotated image is the bottom-left of the source.
        assert_eq!(r.pixel(0, 0)[0], 3);
        assert_eq!(r.pixel(1, 0)[0], 0);
        assert_eq!(rotate_cw(&rotate_cw(&img, 1), 3), img);
        assert_eq!(rotate_cw(&rotate_cw(&img, 2), 2), img);
    }

    #[test]
    fn monochrome_cursor() {
        // 8×1 visible: AND row then XOR row, pitch 1.
        // bits (MSB first): px0 and=0 xor=0 → black; px1 and=0 xor=1 → white;
        // px2 and=1 xor=0 → transparent; px3 and=1 xor=1 → invert.
        let shape = CursorShape {
            kind: CursorKind::Monochrome,
            width: 8,
            height: 2,
            pitch: 1,
            data: vec![0b0011_1111, 0b0101_0000],
        };
        let mut img = BgraImage::new(4, 1);
        for x in 0..4 {
            img.row_mut(0)[x * 4..x * 4 + 4].copy_from_slice(&[10, 20, 30, 255]);
        }
        let r = composite_cursor(&mut img, &shape, 0, 0).unwrap();
        assert_eq!(r, Rect::new(0, 0, 4, 1));
        assert_eq!(img.pixel(0, 0), [0, 0, 0, 255]);
        assert_eq!(img.pixel(1, 0), [255, 255, 255, 255]);
        assert_eq!(img.pixel(2, 0), [10, 20, 30, 255]);
        assert_eq!(img.pixel(3, 0), [245, 235, 225, 255]);
    }

    #[test]
    fn color_and_masked_cursor() {
        let color = CursorShape {
            kind: CursorKind::Color,
            width: 2,
            height: 1,
            pitch: 8,
            data: vec![200, 100, 50, 255, 255, 255, 255, 0],
        };
        let mut img = BgraImage::new(3, 3);
        img.data.iter_mut().for_each(|b| *b = 40);
        // Partially off-frame at (-1, 2): only the second (transparent) pixel lands at (0, 2).
        assert_eq!(composite_cursor(&mut img, &color, -1, 2), Some(Rect::new(0, 2, 1, 1)));
        assert_eq!(img.pixel(0, 2), [40, 40, 40, 40]);
        assert!(composite_cursor(&mut img, &color, 5, 5).is_none());
        composite_cursor(&mut img, &color, 0, 0);
        assert_eq!(img.pixel(0, 0), [200, 100, 50, 255]);

        let masked = CursorShape {
            kind: CursorKind::MaskedColor,
            width: 2,
            height: 1,
            pitch: 8,
            data: vec![1, 2, 3, 0, 0xFF, 0x0F, 0, 0xFF],
        };
        let mut img = BgraImage::new(2, 1);
        img.data.iter_mut().for_each(|b| *b = 0xF0);
        composite_cursor(&mut img, &masked, 0, 0);
        assert_eq!(img.pixel(0, 0), [1, 2, 3, 255]);
        assert_eq!(img.pixel(1, 0), [0x0F, 0xFF, 0xF0, 0xF0]);
        // Malformed buffer is ignored rather than panicking.
        let bad = CursorShape { kind: CursorKind::Color, width: 4, height: 4, pitch: 16, data: vec![0; 3] };
        assert!(composite_cursor(&mut img, &bad, 0, 0).is_none());
    }

    #[test]
    fn sample_formats() {
        assert_eq!(sample_format(3, 32, None, None), Some(SampleFormat::F32));
        assert_eq!(sample_format(0xFFFE, 32, Some(3), Some(32)), Some(SampleFormat::F32));
        assert_eq!(
            sample_format(0xFFFE, 32, Some(1), Some(24)),
            Some(SampleFormat::Int { container_bytes: 4, valid_bits: 24 })
        );
        assert_eq!(sample_format(1, 16, None, None), Some(SampleFormat::Int { container_bytes: 2, valid_bits: 16 }));
        assert_eq!(sample_format(0xFFFE, 32, Some(0x92), None), None);
        assert_eq!(sample_format(2, 16, None, None), None);
    }

    fn conv(src: &[u8], fmt: SampleFormat) -> Vec<f32> {
        let mut out = Vec::new();
        samples_to_f32le(src, fmt, &mut out);
        out.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
    }

    #[test]
    fn sample_conversion() {
        let f: Vec<u8> = [0.5f32, -1.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(conv(&f, SampleFormat::F32), vec![0.5, -1.0]);
        let i16s: Vec<u8> = [16384i16, -32768, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(conv(&i16s, SampleFormat::Int { container_bytes: 2, valid_bits: 16 }), vec![0.5, -1.0, 0.0]);
        // 24-bit packed: 0x400000 = 0.5, 0x800000 = -1.0
        let i24 = [0x00, 0x00, 0x40, 0x00, 0x00, 0x80];
        assert_eq!(conv(&i24, SampleFormat::Int { container_bytes: 3, valid_bits: 24 }), vec![0.5, -1.0]);
        let i32s: Vec<u8> = (i32::MIN / 4).to_le_bytes().to_vec();
        assert_eq!(conv(&i32s, SampleFormat::Int { container_bytes: 4, valid_bits: 24 }), vec![-0.25]);
        assert_eq!(conv(&[128, 0, 192], SampleFormat::Int { container_bytes: 1, valid_bits: 8 }), vec![0.0, -1.0, 0.5]);
        let f64s: Vec<u8> = 0.25f64.to_le_bytes().to_vec();
        assert_eq!(conv(&f64s, SampleFormat::F64), vec![0.25]);
    }
}
