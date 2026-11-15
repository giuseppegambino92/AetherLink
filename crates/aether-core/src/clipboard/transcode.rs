//! In-memory clipboard format transcoding across OS boundaries.
//!
//! Wire canon: images travel as PNG, rich text as HTML (UTF-8 fragment) and/or
//! RTF, text as UTF-8. Platform formats (macOS TIFF/HEIC, Windows `CF_DIBV5`,
//! `CF_HTML`) are converted at the edges.

use crate::{CoreError, Result};
use aether_protocol::{ClipboardFormat, ClipboardItem};
use image::ImageFormat;
use std::io::Cursor;

/// Convert any supported image representation to PNG.
pub fn image_to_png(format: ClipboardFormat, data: &[u8]) -> Result<Vec<u8>> {
    match format {
        ClipboardFormat::Png => Ok(data.to_vec()),
        ClipboardFormat::Tiff => reencode(data, ImageFormat::Tiff),
        ClipboardFormat::DibV5 => dib_to_png(data),
        ClipboardFormat::Heic => heic_to_png(data),
        other => Err(CoreError::Image(format!("{other:?} is not an image format"))),
    }
}

fn reencode(data: &[u8], from: ImageFormat) -> Result<Vec<u8>> {
    let img = image::load_from_memory_with_format(data, from).map_err(|e| CoreError::Image(e.to_string()))?;
    encode_png(&img)
}

fn encode_png(img: &image::DynamicImage) -> Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).map_err(|e| CoreError::Image(e.to_string()))?;
    Ok(out.into_inner())
}

/// Decode PNG to straight RGBA8.
pub fn png_to_rgba(png: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let img = image::load_from_memory_with_format(png, ImageFormat::Png).map_err(|e| CoreError::Image(e.to_string()))?;
    let rgba = img.to_rgba8();
    Ok((rgba.width(), rgba.height(), rgba.into_raw()))
}

/// Encode straight RGBA8 to PNG.
pub fn rgba_to_png(width: u32, height: u32, rgba: Vec<u8>) -> Result<Vec<u8>> {
    let buf = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| CoreError::Image("rgba buffer size mismatch".into()))?;
    encode_png(&image::DynamicImage::ImageRgba8(buf))
}

// ---------------------------------------------------------------------------
// Windows CF_DIB / CF_DIBV5
// ---------------------------------------------------------------------------

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const LCS_SRGB: u32 = 0x7352_4742; // 'sRGB'

/// Build a `CF_DIBV5` blob (BITMAPV5HEADER followed by bottom-up 32 bpp BGRA
/// rows, BI_BITFIELDS with an alpha mask) from PNG.
pub fn png_to_dibv5(png: &[u8]) -> Result<Vec<u8>> {
    let (w, h, rgba) = png_to_rgba(png)?;
    let mut out = Vec::with_capacity(124 + rgba.len());
    let put32 = |o: &mut Vec<u8>, v: u32| o.extend_from_slice(&v.to_le_bytes());
    put32(&mut out, 124); // bV5Size
    out.extend_from_slice(&(w as i32).to_le_bytes()); // width
    out.extend_from_slice(&(h as i32).to_le_bytes()); // positive height = bottom-up
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bpp
    put32(&mut out, BI_BITFIELDS);
    put32(&mut out, w * h * 4); // size image
    put32(&mut out, 2835); // 72 DPI
    put32(&mut out, 2835);
    put32(&mut out, 0); // clr used
    put32(&mut out, 0); // clr important
    put32(&mut out, 0x00FF_0000); // red mask
    put32(&mut out, 0x0000_FF00); // green
    put32(&mut out, 0x0000_00FF); // blue
    put32(&mut out, 0xFF00_0000); // alpha
    put32(&mut out, LCS_SRGB); // CSType
    out.extend_from_slice(&[0u8; 36]); // endpoints
    put32(&mut out, 0); // gamma r/g/b
    put32(&mut out, 0);
    put32(&mut out, 0);
    put32(&mut out, 4); // intent LCS_GM_IMAGES
    put32(&mut out, 0); // profile data
    put32(&mut out, 0); // profile size
    put32(&mut out, 0); // reserved
    debug_assert_eq!(out.len(), 124);
    let row = w as usize * 4;
    for y in (0..h as usize).rev() {
        for px in rgba[y * row..(y + 1) * row].as_chunks::<4>().0 {
            out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    Ok(out)
}

/// Parse a `CF_DIB`/`CF_DIBV5` blob into PNG by synthesizing a BMP file header
/// and letting the BMP decoder handle every header variant.
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    if dib.len() < 40 {
        return Err(CoreError::Image("DIB too short".into()));
    }
    let rd32 = |o: usize| u32::from_le_bytes(dib[o..o + 4].try_into().unwrap());
    let header_size = rd32(0) as usize;
    if !(40..=124).contains(&header_size) || dib.len() < header_size {
        return Err(CoreError::Image("unsupported DIB header".into()));
    }
    let bpp = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = rd32(16);
    let clr_used = rd32(32) as usize;
    let masks = if header_size == 40 && compression == BI_BITFIELDS { 12 } else { 0 };
    let palette_entries = if bpp <= 8 && clr_used == 0 { 1usize << bpp } else { clr_used };
    let palette = palette_entries * 4;
    let off_bits = 14 + header_size + masks + palette;

    // 32bpp BI_RGB DIBs carry undefined alpha (usually 0) → force opaque, as Windows apps expect.
    let mut body = dib.to_vec();
    let height = i32::from_le_bytes(dib[8..12].try_into().unwrap());
    let width = i32::from_le_bytes(dib[4..8].try_into().unwrap());
    if bpp == 32 && compression == BI_RGB {
        let start = header_size + palette;
        let len = width.unsigned_abs() as usize * height.unsigned_abs() as usize * 4;
        if let Some(px) = body.get_mut(start..start + len)
            && px.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
                px.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 0xFF);
            }
    }

    let mut bmp = Vec::with_capacity(14 + body.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + body.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0, 0, 0, 0]);
    bmp.extend_from_slice(&(off_bits as u32).to_le_bytes());
    bmp.extend_from_slice(&body);
    reencode(&bmp, ImageFormat::Bmp)
}

// ---------------------------------------------------------------------------
// HEIC (Apple) — decoded with ImageIO, which ships HEVC image support.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn heic_to_png(data: &[u8]) -> Result<Vec<u8>> {
    imageio::convert(data, "public.png").ok_or_else(|| CoreError::Image("ImageIO could not decode HEIC".into()))
}

#[cfg(not(target_os = "macos"))]
fn heic_to_png(_data: &[u8]) -> Result<Vec<u8>> {
    Err(CoreError::Unsupported("HEIC decoding requires macOS ImageIO (Apple peers send PNG alongside HEIC)"))
}

#[cfg(target_os = "macos")]
mod imageio {
    use std::ffi::c_void;
    type CFTypeRef = *const c_void;
    type CFIndex = isize;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, len: CFIndex) -> CFTypeRef;
        fn CFDataCreateMutable(alloc: CFTypeRef, cap: CFIndex) -> CFTypeRef;
        fn CFDataGetLength(d: CFTypeRef) -> CFIndex;
        fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
        fn CFStringCreateWithBytes(alloc: CFTypeRef, b: *const u8, n: CFIndex, enc: u32, ext: u8) -> CFTypeRef;
        fn CFRelease(o: CFTypeRef);
    }
    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        fn CGImageSourceCreateWithData(data: CFTypeRef, opts: CFTypeRef) -> CFTypeRef;
        fn CGImageSourceGetCount(src: CFTypeRef) -> usize;
        fn CGImageDestinationCreateWithData(data: CFTypeRef, uti: CFTypeRef, count: usize, opts: CFTypeRef) -> CFTypeRef;
        fn CGImageDestinationAddImageFromSource(dst: CFTypeRef, src: CFTypeRef, idx: usize, props: CFTypeRef);
        fn CGImageDestinationFinalize(dst: CFTypeRef) -> bool;
    }

    /// Decode any ImageIO-supported image and re-encode as the given UTI.
    pub fn convert(input: &[u8], uti: &str) -> Option<Vec<u8>> {
        const UTF8: u32 = 0x0800_0100;
        unsafe {
            let data = CFDataCreate(std::ptr::null(), input.as_ptr(), input.len() as CFIndex);
            if data.is_null() {
                return None;
            }
            let src = CGImageSourceCreateWithData(data, std::ptr::null());
            CFRelease(data);
            if src.is_null() {
                return None;
            }
            if CGImageSourceGetCount(src) == 0 {
                CFRelease(src);
                return None;
            }
            let out = CFDataCreateMutable(std::ptr::null(), 0);
            let uti = CFStringCreateWithBytes(std::ptr::null(), uti.as_ptr(), uti.len() as CFIndex, UTF8, 0);
            let dst = CGImageDestinationCreateWithData(out, uti, 1, std::ptr::null());
            CFRelease(uti);
            let mut result = None;
            if !dst.is_null() {
                CGImageDestinationAddImageFromSource(dst, src, 0, std::ptr::null());
                if CGImageDestinationFinalize(dst) {
                    let len = CFDataGetLength(out) as usize;
                    result = Some(std::slice::from_raw_parts(CFDataGetBytePtr(out), len).to_vec());
                }
                CFRelease(dst);
            }
            CFRelease(out);
            CFRelease(src);
            result
        }
    }
}

// ---------------------------------------------------------------------------
// Windows CF_HTML ("HTML Format") envelope
// ---------------------------------------------------------------------------

/// Wrap an HTML fragment in the Windows `CF_HTML` envelope with byte offsets.
pub fn wrap_cf_html(fragment: &str) -> String {
    const START_FRAG: &str = "<!--StartFragment-->";
    const END_FRAG: &str = "<!--EndFragment-->";
    let pre = format!("<html><body>{START_FRAG}");
    let post = format!("{END_FRAG}</body></html>");
    // Header has fixed-width (10 digit) offsets so its length is known up front.
    let header_len = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n".len();
    let start_html = header_len;
    let start_frag = start_html + pre.len();
    let end_frag = start_frag + fragment.len();
    let end_html = end_frag + post.len();
    format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_frag:010}\r\nEndFragment:{end_frag:010}\r\n{pre}{fragment}{post}"
    )
}

/// Extract the fragment from a `CF_HTML` blob; falls back to the whole HTML.
pub fn unwrap_cf_html(cf: &str) -> Option<String> {
    let field = |name: &str| -> Option<usize> {
        let start = cf.find(name)? + name.len();
        let end = cf[start..].find(['\r', '\n'])? + start;
        cf[start..end].trim().parse().ok()
    };
    let (s, e) = (field("StartFragment:")?, field("EndFragment:")?);
    if s <= e && e <= cf.len() && cf.is_char_boundary(s) && cf.is_char_boundary(e) {
        return Some(cf[s..e].to_owned());
    }
    let (s, e) = (field("StartHTML:")?, field("EndHTML:")?);
    (s <= e && e <= cf.len() && cf.is_char_boundary(s) && cf.is_char_boundary(e)).then(|| cf[s..e].to_owned())
}

/// Bring locally captured items into wire canon: every image representation
/// is complemented with PNG (kept first), duplicates removed.
pub fn canonicalize(items: Vec<ClipboardItem>) -> Vec<ClipboardItem> {
    let mut out: Vec<ClipboardItem> = Vec::new();
    let has_png = items.iter().any(|i| i.format == ClipboardFormat::Png);
    for item in items {
        let platform_image = matches!(item.format, ClipboardFormat::Tiff | ClipboardFormat::Heic | ClipboardFormat::DibV5);
        if platform_image {
            // Platform-specific: receivers rebuild their native format from PNG.
            if !has_png && !out.iter().any(|o| o.format == ClipboardFormat::Png)
                && let Ok(png) = image_to_png(item.format, &item.data) {
                    out.insert(0, ClipboardItem { format: ClipboardFormat::Png, data: png });
                }
        } else if !out.iter().any(|o| o.format == item.format) {
            out.push(item);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_png(w: u32, h: u32) -> Vec<u8> {
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x * 40) as u8, (y * 60) as u8, 200, if x == 0 { 128 } else { 255 }]);
            }
        }
        rgba_to_png(w, h, rgba).unwrap()
    }

    #[test]
    fn dibv5_roundtrip_preserves_pixels_and_alpha() {
        let png = sample_png(5, 3);
        let dib = png_to_dibv5(&png).unwrap();
        assert_eq!(dib.len(), 124 + 5 * 3 * 4);
        let back = dib_to_png(&dib).unwrap();
        assert_eq!(png_to_rgba(&back).unwrap(), png_to_rgba(&png).unwrap());
    }

    #[test]
    fn legacy_dib_24bpp_bottom_up() {
        // 2x2 BITMAPINFOHEADER 24bpp, rows padded to 4 bytes, bottom-up.
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&2i32.to_le_bytes());
        dib.extend_from_slice(&2i32.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&24u16.to_le_bytes());
        dib.extend_from_slice(&[0u8; 24]);
        // bottom row: blue, green ; top row: red, white  (BGR order)
        dib.extend_from_slice(&[255, 0, 0, 0, 255, 0, 0, 0]);
        dib.extend_from_slice(&[0, 0, 255, 255, 255, 255, 0, 0]);
        let (w, h, rgba) = png_to_rgba(&dib_to_png(&dib).unwrap()).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255]); // top-left red
        assert_eq!(&rgba[4..8], &[255, 255, 255, 255]);
        assert_eq!(&rgba[8..12], &[0, 0, 255, 255]); // bottom-left blue
    }

    #[test]
    fn tiff_to_png() {
        let png = sample_png(4, 4);
        let img = image::load_from_memory(&png).unwrap();
        let mut tiff = Cursor::new(Vec::new());
        img.write_to(&mut tiff, ImageFormat::Tiff).unwrap();
        let out = image_to_png(ClipboardFormat::Tiff, tiff.get_ref()).unwrap();
        assert_eq!(png_to_rgba(&out).unwrap(), png_to_rgba(&png).unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn heic_via_imageio() {
        // Produce a real HEIC with ImageIO, then decode it back through our path.
        let png = sample_png(16, 16);
        let Some(heic) = imageio::convert(&png, "public.heic") else {
            eprintln!("HEIC encoder unavailable on this machine; skipping");
            return;
        };
        let out = image_to_png(ClipboardFormat::Heic, &heic).unwrap();
        let (w, h, _) = png_to_rgba(&out).unwrap();
        assert_eq!((w, h), (16, 16));
    }

    #[test]
    fn cf_html_roundtrip_with_utf8() {
        let frag = "<b>Città</b> — ciao 👋";
        let cf = wrap_cf_html(frag);
        assert!(cf.starts_with("Version:0.9\r\n"));
        assert_eq!(unwrap_cf_html(&cf).as_deref(), Some(frag));
        assert_eq!(unwrap_cf_html("garbage"), None);
    }

    #[test]
    fn canonicalize_prefers_png() {
        let png = sample_png(2, 2);
        let dib = png_to_dibv5(&png).unwrap();
        let items = vec![
            ClipboardItem { format: ClipboardFormat::DibV5, data: dib },
            ClipboardItem { format: ClipboardFormat::PlainText, data: b"x".to_vec() },
            ClipboardItem { format: ClipboardFormat::PlainText, data: b"dup".to_vec() },
        ];
        let out = canonicalize(items);
        assert_eq!(out.iter().map(|i| i.format).collect::<Vec<_>>(), vec![ClipboardFormat::Png, ClipboardFormat::PlainText]);
    }
}
