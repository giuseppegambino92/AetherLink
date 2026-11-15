//! Local OCR for DVR frames ("copy text from the past"). No pixels ever leave the machine.
//!
//! * macOS: the on-device Vision framework (`VNRecognizeTextRequest`).
//! * Windows: `Windows.Media.Ocr` — engine from the requested languages, else the user-profile
//!   languages, else `en-US`; the image is decoded by `BitmapDecoder` from an
//!   `InMemoryRandomAccessStream` and scaled down to `OcrEngine::MaxImageDimension` when larger.
//!   The API exposes no confidence: lines report `1.0`.
//! * Linux: the pure-Rust [`ocrs`] engine (no system packages, works the same on every distro).
//!   Its two ONNX models (~12 MB) are fetched once, at first use, into
//!   `$XDG_CACHE_HOME/aetherlink/ocrs` (or `$AETHERLINK_OCRS_MODEL_DIR`) and verified against pinned
//!   SHA-256 digests; `AETHERLINK_OCR_NO_DOWNLOAD=1` forbids the download. When the models are
//!   unavailable (offline, download forbidden), or a non-Latin script is requested (the ocrs
//!   models are Latin-only), the `tesseract` CLI is used if installed (TSV output → lines with
//!   confidences). Otherwise `Unsupported` with an explanation. ocrs reports no confidence: `1.0`.
//!
//! Why ocrs first: it needs nothing installed and its behaviour does not depend on distro
//! packages or traineddata versions; tesseract is the better choice for CJK/Cyrillic/… and for
//! machines that are offline at first use, hence the fallback.

use crate::Result;
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
use crate::CoreError;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrLine {
    pub text: String,
    pub confidence: f32,
    /// Normalized bounding box, origin top-left: `[x, y, w, h]` in 0..1.
    pub bbox: [f32; 4],
}

/// Recognize text in an encoded image (PNG/JPEG/HEIC/TIFF…), top-to-bottom order.
pub fn recognize(image: &[u8], languages: &[&str]) -> Result<Vec<OcrLine>> {
    #[cfg(target_os = "macos")]
    {
        macos::recognize(image, languages)
    }
    #[cfg(target_os = "windows")]
    {
        windows_ocr::recognize(image, languages)
    }
    #[cfg(target_os = "linux")]
    {
        linux::recognize(image, languages)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = (image, languages);
        Err(CoreError::Unsupported("OCR backend available on macOS, Windows and Linux only"))
    }
}

/// Join lines into plain text for the clipboard.
pub fn to_text(lines: &[OcrLine]) -> String {
    lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
}

/// Top-to-bottom, then left-to-right.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows", target_os = "linux")), allow(dead_code))]
fn sort_lines(lines: &mut [OcrLine]) {
    lines.sort_by(|a, b| a.bbox[1].total_cmp(&b.bbox[1]).then(a.bbox[0].total_cmp(&b.bbox[0])));
}

/// Pixel rectangle → normalized `[x, y, w, h]` clamped to 0..1.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux", test)), allow(dead_code))]
fn normalize(x: f64, y: f64, w: f64, h: f64, img_w: f64, img_h: f64) -> [f32; 4] {
    if img_w <= 0.0 || img_h <= 0.0 {
        return [0.0; 4];
    }
    let x0 = (x / img_w).clamp(0.0, 1.0);
    let y0 = (y / img_h).clamp(0.0, 1.0);
    let x1 = ((x + w) / img_w).clamp(0.0, 1.0);
    let y1 = ((y + h) / img_h).clamp(0.0, 1.0);
    [x0 as f32, y0 as f32, (x1 - x0) as f32, (y1 - y0) as f32]
}

#[cfg(target_os = "macos")]
mod macos {
    use super::OcrLine;
    use crate::{CoreError, Result};
    use objc2::AllocAnyThread;
    use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
    use objc2_vision::{VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel};

    pub fn recognize(image: &[u8], languages: &[&str]) -> Result<Vec<OcrLine>> {
        let data = NSData::with_bytes(image);
        let handler = VNImageRequestHandler::initWithData_options(VNImageRequestHandler::alloc(), &data, &NSDictionary::new());
        let request = VNRecognizeTextRequest::new();
        request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        request.setUsesLanguageCorrection(true);
        if !languages.is_empty() {
            let langs: Vec<_> = languages.iter().map(|l| NSString::from_str(l)).collect();
            request.setRecognitionLanguages(&NSArray::from_retained_slice(&langs));
        }
        let as_request: &VNRequest = &request;
        handler
            .performRequests_error(&NSArray::from_slice(&[as_request]))
            .map_err(|e| CoreError::Other(format!("Vision: {}", e.localizedDescription())))?;

        let mut lines = Vec::new();
        if let Some(results) = request.results() {
            for obs in results.iter() {
                let candidates = obs.topCandidates(1);
                let Some(best) = candidates.firstObject() else { continue };
                // SAFETY: plain property getter on a valid observation.
                let r = unsafe { obs.boundingBox() };
                // Vision uses a bottom-left origin; flip to top-left.
                let bbox = [
                    r.origin.x as f32,
                    (1.0 - r.origin.y - r.size.height) as f32,
                    r.size.width as f32,
                    r.size.height as f32,
                ];
                lines.push(OcrLine { text: best.string().to_string(), confidence: best.confidence(), bbox });
            }
        }
        super::sort_lines(&mut lines);
        Ok(lines)
    }
}

#[cfg(target_os = "windows")]
mod windows_ocr {
    use super::{OcrLine, normalize, sort_lines};
    use crate::{CoreError, Result};
    use windows::Globalization::Language;
    use windows::Graphics::Imaging::{
        BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform, ColorManagementMode,
        ExifOrientationMode,
    };
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};
    use windows::core::HSTRING;

    fn err(ctx: &'static str) -> impl FnOnce(windows::core::Error) -> CoreError {
        move |e| CoreError::Other(format!("Windows.Media.Ocr {ctx}: {e}"))
    }

    /// Requested languages first, then the user-profile languages, then en-US.
    fn engine(languages: &[&str]) -> Result<OcrEngine> {
        for tag in languages {
            if let Ok(lang) = Language::CreateLanguage(&HSTRING::from(*tag))
                && OcrEngine::IsLanguageSupported(&lang).unwrap_or(false)
                && let Ok(e) = OcrEngine::TryCreateFromLanguage(&lang)
            {
                return Ok(e);
            }
        }
        if let Ok(e) = OcrEngine::TryCreateFromUserProfileLanguages() {
            return Ok(e);
        }
        let en = Language::CreateLanguage(&HSTRING::from("en-US")).map_err(err("Language"))?;
        OcrEngine::TryCreateFromLanguage(&en).map_err(|_| {
            CoreError::Unsupported("no Windows OCR language pack is installed (Settings → Time & language → Language)")
        })
    }

    pub fn recognize(image: &[u8], languages: &[&str]) -> Result<Vec<OcrLine>> {
        let engine = engine(languages)?;
        // Encoded bytes → InMemoryRandomAccessStream → BitmapDecoder.
        let stream = InMemoryRandomAccessStream::new().map_err(err("stream"))?;
        let writer = DataWriter::CreateDataWriter(&stream.GetOutputStreamAt(0).map_err(err("stream"))?).map_err(err("DataWriter"))?;
        writer.WriteBytes(image).map_err(err("WriteBytes"))?;
        writer.StoreAsync().map_err(err("StoreAsync"))?.join().map_err(err("StoreAsync"))?;
        writer.FlushAsync().map_err(err("FlushAsync"))?.join().map_err(err("FlushAsync"))?;
        let _ = writer.DetachStream();
        stream.Seek(0).map_err(err("Seek"))?;
        let decoder = BitmapDecoder::CreateAsync(&stream).map_err(err("BitmapDecoder"))?.join().map_err(err("decode image"))?;
        let (w, h) = (decoder.PixelWidth().map_err(err("PixelWidth"))?, decoder.PixelHeight().map_err(err("PixelHeight"))?);
        if w == 0 || h == 0 {
            return Ok(Vec::new());
        }
        // OcrEngine rejects images larger than MaxImageDimension: scale down, keep the aspect.
        let max = OcrEngine::MaxImageDimension().unwrap_or(2600).max(1);
        let scale = (max as f64 / w.max(h) as f64).min(1.0);
        let (sw, sh) = (((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1));
        let transform = BitmapTransform::new().map_err(err("BitmapTransform"))?;
        transform.SetScaledWidth(sw).map_err(err("BitmapTransform"))?;
        transform.SetScaledHeight(sh).map_err(err("BitmapTransform"))?;
        transform.SetInterpolationMode(BitmapInterpolationMode::Fant).map_err(err("BitmapTransform"))?;
        let bitmap = decoder
            .GetSoftwareBitmapTransformedAsync(
                BitmapPixelFormat::Bgra8,
                BitmapAlphaMode::Premultiplied,
                &transform,
                ExifOrientationMode::RespectExifOrientation,
                ColorManagementMode::DoNotColorManage,
            )
            .map_err(err("SoftwareBitmap"))?
            .join()
            .map_err(err("SoftwareBitmap"))?;
        let result = engine.RecognizeAsync(&bitmap).map_err(err("RecognizeAsync"))?.join().map_err(err("RecognizeAsync"))?;
        let lines = result.Lines().map_err(err("Lines"))?;
        let mut out = Vec::new();
        for i in 0..lines.Size().map_err(err("Lines"))? {
            let line = lines.GetAt(i).map_err(err("Lines"))?;
            let text = line.Text().map_err(err("Text"))?.to_string();
            let words = line.Words().map_err(err("Words"))?;
            let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for j in 0..words.Size().map_err(err("Words"))? {
                let r = words.GetAt(j).map_err(err("Words"))?.BoundingRect().map_err(err("BoundingRect"))?;
                x0 = x0.min(r.X as f64);
                y0 = y0.min(r.Y as f64);
                x1 = x1.max((r.X + r.Width) as f64);
                y1 = y1.max((r.Y + r.Height) as f64);
            }
            if text.trim().is_empty() || x1 < x0 {
                continue;
            }
            out.push(OcrLine { text, confidence: 1.0, bbox: normalize(x0, y0, x1 - x0, y1 - y0, sw as f64, sh as f64) });
        }
        sort_lines(&mut out);
        Ok(out)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{OcrLine, normalize, sort_lines};
    use crate::{CoreError, Result};
    use parking_lot::Mutex;
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    /// `(file name, URL, SHA-256)` of the ocrs models (pinned: a changed upstream file is rejected).
    pub(super) const MODELS: [(&str, &str, &str); 2] = [
        (
            "text-detection.onnx",
            "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.onnx",
            "a917b23dbd9524b465df7e922641b3ff2981623df4ded5a0234004ef2fee7cfe",
        ),
        (
            "text-recognition.onnx",
            "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.onnx",
            "86c145c2edb96c8caed5b1ebb8f44d706408922211451c309c625157dd6061c5",
        ),
    ];
    const MAX_MODEL_BYTES: u64 = 64 << 20;

    static ENGINE: Mutex<Option<Arc<ocrs::OcrEngine>>> = Mutex::new(None);

    pub fn recognize(image: &[u8], languages: &[&str]) -> Result<Vec<OcrLine>> {
        let wants_non_latin = languages.iter().any(|l| !is_latin(l));
        if wants_non_latin && let Some(t) = tesseract_binary() {
            return tesseract(&t, image, languages);
        }
        let ocrs_err = match engine() {
            Ok(e) => return recognize_ocrs(&e, image),
            Err(e) => e,
        };
        match tesseract_binary() {
            Some(t) => {
                tracing::info!("ocrs unavailable ({ocrs_err}); using tesseract");
                tesseract(&t, image, languages)
            }
            None => {
                tracing::warn!("ocrs unavailable: {ocrs_err}");
                Err(CoreError::Unsupported(
                    "Linux OCR needs the ocrs models (fetched on first use into ~/.cache/aetherlink/ocrs; that failed, see the \
                     log) or the tesseract CLI (package tesseract-ocr)",
                ))
            }
        }
    }

    /// Languages the ocrs Latin-alphabet models can read.
    pub(super) fn is_latin(tag: &str) -> bool {
        const LATIN: &[&str] = &[
            "en", "de", "fr", "it", "es", "pt", "nl", "sv", "da", "no", "nb", "nn", "fi", "is", "ga", "ca", "eu", "gl", "pl", "cs", "sk",
            "sl", "hr", "hu", "ro", "sq", "et", "lv", "lt", "tr", "id", "ms", "vi", "tl", "sw", "af", "la", "mt", "cy",
        ];
        let primary = tag.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
        LATIN.contains(&primary.as_str())
    }

    pub(super) fn model_dir() -> Option<PathBuf> {
        if let Some(d) = std::env::var_os("AETHERLINK_OCRS_MODEL_DIR").filter(|d| !d.is_empty()) {
            return Some(PathBuf::from(d));
        }
        dirs::cache_dir().map(|d| d.join("aetherlink").join("ocrs"))
    }

    pub(super) fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Reads `dir/name` if present and matching `sha256`. A corrupt file is deleted.
    pub(super) fn load_verified(dir: &Path, name: &str, sha256: &str) -> Option<Vec<u8>> {
        let p = dir.join(name);
        let bytes = std::fs::read(&p).ok()?;
        if sha256_hex(&bytes) == sha256 {
            return Some(bytes);
        }
        tracing::warn!("{} does not match its pinned SHA-256; discarding", p.display());
        let _ = std::fs::remove_file(&p);
        None
    }

    fn download(url: &str) -> std::result::Result<Vec<u8>, String> {
        let mut resp = ureq::get(url).call().map_err(|e| format!("GET {url}: {e}"))?;
        resp.body_mut().with_config().limit(MAX_MODEL_BYTES).read_to_vec().map_err(|e| format!("GET {url}: {e}"))
    }

    /// Model bytes from the cache, downloading (once, verified, atomically renamed) if allowed.
    fn model_bytes(dir: &Path, name: &str, url: &str, sha256: &str) -> std::result::Result<Vec<u8>, String> {
        if let Some(b) = load_verified(dir, name, sha256) {
            return Ok(b);
        }
        if std::env::var_os("AETHERLINK_OCR_NO_DOWNLOAD").is_some_and(|v| v != "0") {
            return Err(format!("ocrs model {name} missing in {} and downloads are disabled", dir.display()));
        }
        tracing::info!("downloading OCR model {name} into {}", dir.display());
        let bytes = download(url)?;
        let got = sha256_hex(&bytes);
        if got != sha256 {
            return Err(format!("ocrs model {name}: SHA-256 {got} does not match the pinned {sha256}"));
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = dir.join(format!(".{name}.{}.part", std::process::id()));
        std::fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, dir.join(name)).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(bytes)
    }

    fn engine() -> std::result::Result<Arc<ocrs::OcrEngine>, String> {
        let mut slot = ENGINE.lock();
        if let Some(e) = slot.as_ref() {
            return Ok(e.clone());
        }
        let dir = model_dir().ok_or("no cache directory for the ocrs models")?;
        let mut models = Vec::new();
        for (name, url, sha) in MODELS {
            let bytes = model_bytes(&dir, name, url, sha)?;
            models.push(rten::Model::load(bytes).map_err(|e| format!("load {name}: {e}"))?);
        }
        let recognition = models.pop();
        let detection = models.pop();
        let engine = ocrs::OcrEngine::new(ocrs::OcrEngineParams {
            detection_model: detection,
            recognition_model: recognition,
            ..Default::default()
        })
        .map_err(|e| format!("ocrs engine: {e}"))?;
        let engine = Arc::new(engine);
        *slot = Some(engine.clone());
        Ok(engine)
    }

    fn recognize_ocrs(engine: &ocrs::OcrEngine, image: &[u8]) -> Result<Vec<OcrLine>> {
        use ocrs::TextItem;
        let img = image::load_from_memory(image).map_err(|e| CoreError::Other(format!("decode image: {e}")))?.into_rgb8();
        let (w, h) = img.dimensions();
        if w == 0 || h == 0 {
            return Ok(Vec::new());
        }
        let ocr_err = |e: ocrs::OcrError| CoreError::Other(format!("ocrs: {e}"));
        let source = ocrs::ImageSource::from_bytes(img.as_raw(), (w, h)).map_err(|e| CoreError::Other(format!("ocrs input: {e}")))?;
        let input = engine.prepare_input(source).map_err(ocr_err)?;
        let words = engine.detect_words(&input).map_err(ocr_err)?;
        let line_rects = engine.find_text_lines(&input, &words);
        let mut out = Vec::new();
        for line in engine.recognize_text(&input, &line_rects).map_err(ocr_err)?.into_iter().flatten() {
            let text = line.to_string();
            if text.trim().is_empty() {
                continue;
            }
            let r = line.bounding_rect();
            let bbox = normalize(r.left() as f64, r.top() as f64, r.width() as f64, r.height() as f64, w as f64, h as f64);
            out.push(OcrLine { text: text.trim().to_string(), confidence: 1.0, bbox });
        }
        sort_lines(&mut out);
        Ok(out)
    }

    // ------------------------------------------------------------------ tesseract fallback

    #[cfg(test)]
    pub(super) fn tesseract_for_test(image: &[u8], languages: &[&str]) -> Result<Vec<OcrLine>> {
        let bin = tesseract_binary().ok_or(CoreError::Unsupported("tesseract is not installed"))?;
        tesseract(&bin, image, languages)
    }

    fn tesseract_binary() -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).map(|d| d.join("tesseract")).find(|p| p.is_file())
    }

    /// BCP-47 → tesseract traineddata name.
    pub(super) fn tesseract_lang(tag: &str) -> Option<&'static str> {
        let lower = tag.to_ascii_lowercase().replace('_', "-");
        if lower.starts_with("zh-hant") || lower == "zh-tw" || lower == "zh-hk" {
            return Some("chi_tra");
        }
        Some(match lower.split('-').next().unwrap_or("") {
            "en" => "eng",
            "de" => "deu",
            "fr" => "fra",
            "it" => "ita",
            "es" => "spa",
            "pt" => "por",
            "nl" => "nld",
            "sv" => "swe",
            "da" => "dan",
            "no" | "nb" => "nor",
            "fi" => "fin",
            "pl" => "pol",
            "cs" => "ces",
            "hu" => "hun",
            "ro" => "ron",
            "tr" => "tur",
            "ru" => "rus",
            "uk" => "ukr",
            "el" => "ell",
            "ar" => "ara",
            "he" => "heb",
            "hi" => "hin",
            "ja" => "jpn",
            "ko" => "kor",
            "zh" => "chi_sim",
            "th" => "tha",
            "vi" => "vie",
            _ => return None,
        })
    }

    fn tesseract(bin: &Path, image: &[u8], languages: &[&str]) -> Result<Vec<OcrLine>> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let installed: Vec<String> = Command::new(bin)
            .arg("--list-langs")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().skip(1).map(|l| l.trim().to_string()).collect())
            .unwrap_or_default();
        let mut langs: Vec<&str> =
            languages.iter().filter_map(|l| tesseract_lang(l)).filter(|l| installed.iter().any(|i| i == l)).collect();
        langs.dedup();
        if langs.is_empty() {
            langs.push(if installed.iter().any(|i| i == "eng") { "eng" } else { installed.first().map(|s| s.as_str()).unwrap_or("eng") });
        }
        let mut child = Command::new(bin)
            .args(["stdin", "stdout", "-l", &langs.join("+"), "tsv"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| CoreError::Other(format!("tesseract: {e}")))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let bytes = image.to_vec();
        let feeder = std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
        let out = child.wait_with_output().map_err(|e| CoreError::Other(format!("tesseract: {e}")))?;
        let _ = feeder.join();
        if !out.status.success() {
            return Err(CoreError::Other(format!("tesseract failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
        Ok(parse_tsv(&String::from_utf8_lossy(&out.stdout)))
    }

    /// Tesseract TSV (`level page block par line word left top width height conf text`) → lines.
    pub(super) fn parse_tsv(tsv: &str) -> Vec<OcrLine> {
        struct Acc {
            words: Vec<String>,
            conf: f64,
            rect: (f64, f64, f64, f64),
        }
        let (mut img_w, mut img_h) = (0.0, 0.0);
        let mut lines: Vec<((u32, u32, u32, u32), Acc)> = Vec::new();
        for row in tsv.lines().skip(1) {
            let f: Vec<&str> = row.split('\t').collect();
            if f.len() < 12 {
                continue;
            }
            let n = |i: usize| f[i].trim().parse::<f64>().unwrap_or(0.0);
            match f[0].trim() {
                "1" => (img_w, img_h) = (n(8), n(9)),
                "5" => {
                    let text = f[11].trim();
                    if text.is_empty() || n(10) < 0.0 {
                        continue;
                    }
                    let key = (n(1) as u32, n(2) as u32, n(3) as u32, n(4) as u32);
                    let (x0, y0, x1, y1) = (n(6), n(7), n(6) + n(8), n(7) + n(9));
                    match lines.iter_mut().find(|(k, _)| *k == key) {
                        Some((_, a)) => {
                            a.words.push(text.to_string());
                            a.conf += n(10);
                            a.rect = (a.rect.0.min(x0), a.rect.1.min(y0), a.rect.2.max(x1), a.rect.3.max(y1));
                        }
                        None => lines.push((key, Acc { words: vec![text.to_string()], conf: n(10), rect: (x0, y0, x1, y1) })),
                    }
                }
                _ => {}
            }
        }
        let mut out: Vec<OcrLine> = lines
            .into_iter()
            .map(|(_, a)| OcrLine {
                text: a.words.join(" "),
                confidence: (a.conf / a.words.len() as f64 / 100.0).clamp(0.0, 1.0) as f32,
                bbox: normalize(a.rect.0, a.rect.1, a.rect.2 - a.rect.0, a.rect.3 - a.rect.1, img_w, img_h),
            })
            .collect();
        sort_lines(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render text with a tiny built-in 5x7 bitmap font so the test has no font dependencies.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn render(text: &str, scale: usize) -> Vec<u8> {
        const GLYPHS: &[(char, [u8; 7])] = &[
            ('A', [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11]),
            ('E', [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F]),
            ('H', [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11]),
            ('I', [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E]),
            ('K', [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11]),
            ('L', [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F]),
            ('N', [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11]),
            ('R', [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11]),
            ('T', [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04]),
            (' ', [0; 7]),
        ];
        let cols = text.len() * 6 + 2;
        let (w, h) = (cols * scale, 11 * scale);
        let mut rgba = vec![255u8; w * h * 4];
        for (ci, ch) in text.chars().enumerate() {
            let rows = GLYPHS.iter().find(|g| g.0 == ch).map(|g| g.1).unwrap();
            for (ry, bits) in rows.iter().enumerate() {
                for rx in 0..5 {
                    if bits & (0x10 >> rx) != 0 {
                        for dy in 0..scale {
                            for dx in 0..scale {
                                let x = (1 + ci * 6 + rx) * scale + dx;
                                let y = (2 + ry) * scale + dy;
                                let o = (y * w + x) * 4;
                                rgba[o..o + 3].copy_from_slice(&[0, 0, 0]);
                            }
                        }
                    }
                }
            }
        }
        crate::clipboard::transcode::rgba_to_png(w as u32, h as u32, rgba).unwrap()
    }

    #[test]
    fn normalize_clamps_and_sorts() {
        assert_eq!(normalize(10.0, 20.0, 30.0, 40.0, 100.0, 200.0), [0.1, 0.1, 0.3, 0.2]);
        assert_eq!(normalize(-10.0, 0.0, 30.0, 400.0, 100.0, 200.0), [0.0, 0.0, 0.2, 1.0]);
        assert_eq!(normalize(1.0, 1.0, 1.0, 1.0, 0.0, 10.0), [0.0; 4]);
        let line = |y: f32, x: f32| OcrLine { text: format!("{x},{y}"), confidence: 1.0, bbox: [x, y, 0.1, 0.1] };
        let mut v = vec![line(0.5, 0.1), line(0.1, 0.9), line(0.1, 0.2)];
        sort_lines(&mut v);
        assert_eq!(to_text(&v), "0.2,0.1\n0.9,0.1\n0.1,0.5");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tesseract_tsv_is_grouped_into_lines() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   1\t1\t0\t0\t0\t0\t0\t0\t200\t100\t-1\t\n\
                   4\t1\t1\t1\t1\t0\t10\t10\t100\t20\t-1\t\n\
                   5\t1\t1\t1\t1\t1\t10\t10\t40\t20\t90\tHello\n\
                   5\t1\t1\t1\t1\t2\t60\t12\t50\t18\t80\tworld\n\
                   5\t1\t1\t1\t2\t1\t10\t50\t30\t20\t70\tsecond\n\
                   5\t1\t1\t1\t2\t2\t50\t50\t30\t20\t-1\t \n";
        let lines = linux::parse_tsv(tsv);
        assert_eq!(to_text(&lines), "Hello world\nsecond");
        assert!((lines[0].confidence - 0.85).abs() < 1e-6);
        assert_eq!(lines[0].bbox, [0.05, 0.1, 0.5, 0.2]);
        assert_eq!(lines[1].bbox, [0.05, 0.5, 0.15, 0.2]);
        assert!(linux::parse_tsv("").is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn language_mapping_and_model_cache_verification() {
        assert!(linux::is_latin("en-US") && linux::is_latin("de") && linux::is_latin("pt_BR"));
        assert!(!linux::is_latin("ja") && !linux::is_latin("zh-Hans") && !linux::is_latin("ru"));
        assert_eq!(linux::tesseract_lang("en-US"), Some("eng"));
        assert_eq!(linux::tesseract_lang("zh-Hant-TW"), Some("chi_tra"));
        assert_eq!(linux::tesseract_lang("zh-Hans"), Some("chi_sim"));
        assert_eq!(linux::tesseract_lang("tlh"), None);
        assert_eq!(linux::sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // Cache: a good file is returned, a tampered one is rejected and removed. No network.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.onnx"), b"abc").unwrap();
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(linux::load_verified(dir.path(), "m.onnx", good).as_deref(), Some(&b"abc"[..]));
        std::fs::write(dir.path().join("m.onnx"), b"abd").unwrap();
        assert!(linux::load_verified(dir.path(), "m.onnx", good).is_none());
        assert!(!dir.path().join("m.onnx").exists());
        assert!(linux::load_verified(dir.path(), "missing.onnx", good).is_none());
        assert!(linux::MODELS.iter().all(|(_, url, sha)| url.starts_with("https://") && sha.len() == 64));
    }

    /// Needs the ocrs models already in the cache (or `$AETHERLINK_OCRS_MODEL_DIR`); never
    /// downloads (forces `AETHERLINK_OCR_NO_DOWNLOAD`).
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs the ocrs models in the cache dir / AETHERLINK_OCRS_MODEL_DIR"]
    fn ocrs_recognizes_rendered_text() {
        // SAFETY: test-only env mutation before any OCR thread starts.
        unsafe { std::env::set_var("AETHERLINK_OCR_NO_DOWNLOAD", "1") };
        let png = render("AETHER LINK", 6);
        let lines = recognize(&png, &["en-US"]).unwrap();
        println!("{lines:?}");
        // ocrs is trained on real fonts: the blocky 5x7 test glyphs A/K/N come out as H/E/H
        // ("HETHER LIHE"), so only the unambiguous letters are checked.
        let text = to_text(&lines).to_uppercase();
        assert!(text.contains("THER") && text.contains(" LI"), "got {text:?}");
        assert_eq!(lines.len(), 1, "one line: {lines:?}");
        assert!(lines.iter().all(|l| l.bbox.iter().all(|v| (0.0..=1.0).contains(v))));
        assert!(lines[0].bbox[2] > 0.9, "line spans the image width: {:?}", lines[0].bbox);
    }

    /// Needs `tesseract` with the English traineddata on PATH.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs the tesseract CLI (tesseract-ocr + tesseract-ocr-eng)"]
    fn tesseract_recognizes_rendered_text() {
        let png = render("AETHER LINK", 6);
        let lines = linux::tesseract_for_test(&png, &["en-US"]).unwrap();
        println!("{lines:?}");
        let text = to_text(&lines).to_uppercase();
        assert!(text.contains("THER") && text.contains("LINK"), "got {text:?}");
        assert!(lines.iter().all(|l| (0.0..=1.0).contains(&l.confidence)));
    }

    /// Needs Windows with an OCR language pack (not available under wine).
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "needs Windows.Media.Ocr (a real Windows host with an OCR language pack)"]
    fn windows_ocr_recognizes_rendered_text() {
        let png = render("AETHER LINK", 6);
        let lines = recognize(&png, &["en-US"]).unwrap();
        let text = to_text(&lines).to_uppercase();
        assert!(text.contains("THER") && text.contains("LINK"), "got {text:?}");
        assert!(lines.iter().all(|l| l.bbox.iter().all(|v| (0.0..=1.0).contains(v))));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "cold-loads the Vision text model (60-130 s in a fresh process); run with --ignored"]
    fn recognizes_rendered_text() {
        let png = render("AETHER LINK", 8);
        let lines = recognize(&png, &["en-US"]).unwrap();
        let text = to_text(&lines).to_uppercase();
        assert!(text.contains("THER") && text.contains("LINK"), "got {text:?}");
        assert!(lines.iter().all(|l| l.bbox.iter().all(|v| (-0.01..=1.01).contains(v))));
    }
}
