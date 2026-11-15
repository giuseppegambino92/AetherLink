//! Deterministic animated BGRA source for tests and benchmarks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{BgraImage, CaptureConfig, CaptureError, CaptureEvent, CaptureSink, CaptureTarget, CapturedFrame, Rect, Result, ScreenCapturer};

/// Deterministic synthetic screen content:
///
/// * a static diagonal gradient background,
/// * a static "text" region (rows of pseudo-random 6×9 glyphs on white — sharp, high-contrast
///   edges like real text),
/// * a textured block moving horizontally across the lower half (`speed` px / frame),
/// * a small "ticker" square in the top-right corner whose solid colour changes every 10 frames.
///
/// `frame(i)` depends only on `(width, height, i)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestPattern {
    pub width: u32,
    pub height: u32,
    /// Horizontal block movement in pixels per frame.
    pub speed: u32,
}

const GLYPH_W: u32 = 8;
const GLYPH_H: u32 = 12;

#[inline]
fn mix(mut x: u64) -> u64 {
    // splitmix64 finalizer
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

impl TestPattern {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, speed: 7 }
    }

    fn block_size(&self) -> u32 {
        (self.width.min(self.height) / 6).max(8).min(self.width).min(self.height)
    }

    /// Rectangle covered by the moving block in frame `index`.
    pub fn block_rect(&self, index: u64) -> Rect {
        let b = self.block_size();
        let travel = (self.width - b).max(1) as u64;
        let x = (index * self.speed as u64) % travel;
        let y = (self.height * 5 / 8).min(self.height - b);
        Rect::new(x as i32, y as i32, b, b)
    }

    /// Rectangle of the ticker square.
    pub fn ticker_rect(&self) -> Rect {
        let s = (self.width.min(self.height) / 16).max(4).min(self.width).min(self.height);
        let x = self.width - s - (self.width / 64).min(self.width - s);
        Rect::new(x as i32, (self.height / 64) as i32, s, s)
    }

    /// Rectangle of the static text region.
    pub fn text_rect(&self) -> Rect {
        let x0 = self.width / 16;
        let y0 = self.height / 16;
        Rect::new(x0 as i32, y0 as i32, self.width / 2 - x0, self.height / 2 - y0)
    }

    fn ticker_color(index: u64) -> [u8; 4] {
        let h = mix(index / 10);
        [(h & 0xFF) as u8, ((h >> 8) & 0xFF) as u8, ((h >> 16) & 0xFF) as u8, 0xFF]
    }

    /// Renders frame `index` into a new packed image.
    pub fn frame(&self, index: u64) -> BgraImage {
        let mut img = BgraImage::new(self.width, self.height);
        self.render_into(&mut img, index);
        img
    }

    /// Renders frame `index` into `img` (which must have this pattern's dimensions).
    pub fn render_into(&self, img: &mut BgraImage, index: u64) {
        assert_eq!((img.width, img.height), (self.width, self.height), "image size mismatch");
        let (w, h) = (self.width, self.height);
        let text = self.text_rect();
        let block = self.block_rect(index);
        let ticker = self.ticker_rect();
        let tcol = Self::ticker_color(index);
        for y in 0..h {
            let row = img.row_mut(y);
            for x in 0..w {
                let px = if ticker.contains(x as i32, y as i32) {
                    tcol
                } else if block.contains(x as i32, y as i32) {
                    let bx = x - block.x as u32;
                    let by = y - block.y as u32;
                    // textured block: checker + gradient, moves with the block
                    let c = ((bx / 4 + by / 4) & 1) as u8;
                    [(bx * 255 / block.width) as u8, 40 + c * 120, (by * 255 / block.height) as u8, 0xFF]
                } else if text.contains(x as i32, y as i32) {
                    Self::text_pixel(x - text.x as u32, y - text.y as u32)
                } else {
                    [(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, 128, 0xFF]
                };
                row[x as usize * 4..x as usize * 4 + 4].copy_from_slice(&px);
            }
        }
    }

    #[inline]
    fn text_pixel(tx: u32, ty: u32) -> [u8; 4] {
        const PAPER: [u8; 4] = [0xFA, 0xFA, 0xFA, 0xFF];
        const INK: [u8; 4] = [0x22, 0x1E, 0x1A, 0xFF];
        let (col, row) = (tx / GLYPH_W, ty / GLYPH_H);
        let (gx, gy) = (tx % GLYPH_W, ty % GLYPH_H);
        // 6x9 glyph area inside an 8x12 cell, ~1/6 of cells are spaces, every 4th line is blank
        if gx >= 6 || gy >= 9 || row % 4 == 3 {
            return PAPER;
        }
        let g = mix(((row as u64) << 32) | col as u64);
        if g.is_multiple_of(6) {
            return PAPER;
        }
        let bit = gy * 6 + gx; // 54 bits
        if (g >> (bit + 4)) & 1 == 1 && (g >> ((bit * 7) % 60)) & 1 == 1 { INK } else { PAPER }
    }

    /// Regions that differ between frame `index - 1` and `index` (full frame for index 0).
    pub fn dirty_rects(&self, index: u64) -> Vec<Rect> {
        if index == 0 {
            return vec![Rect::new(0, 0, self.width, self.height)];
        }
        let mut out = Vec::new();
        let (a, b) = (self.block_rect(index - 1), self.block_rect(index));
        if a != b {
            out.push(a);
            out.push(b);
        }
        if index.is_multiple_of(10) {
            out.push(self.ticker_rect());
        }
        out
    }
}

/// A [`ScreenCapturer`] that emits [`TestPattern`] frames from a background thread at the
/// configured fps. Frame `n` has `pts_us = n * 1e6 / fps` and accurate `dirty_rects`.
pub struct TestPatternCapturer {
    pattern: TestPattern,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TestPatternCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        Self::with_pattern(TestPattern::new(width, height))
    }

    pub fn with_pattern(pattern: TestPattern) -> Self {
        Self { pattern, stop: Arc::new(AtomicBool::new(false)), thread: None }
    }

    pub fn pattern(&self) -> TestPattern {
        self.pattern
    }
}

impl ScreenCapturer for TestPatternCapturer {
    fn start(&mut self, target: CaptureTarget, cfg: CaptureConfig, mut sink: CaptureSink) -> Result<()> {
        if self.thread.is_some() {
            return Err(CaptureError::AlreadyRunning);
        }
        if cfg.fps == 0 {
            return Err(CaptureError::InvalidFrame("fps must be > 0".into()));
        }
        let display_id = match target {
            CaptureTarget::Display { display_id } => display_id,
            CaptureTarget::Window { .. } => 0,
        };
        let pattern = self.pattern;
        self.stop.store(false, Ordering::SeqCst);
        let stop = self.stop.clone();
        let period = Duration::from_secs_f64(1.0 / cfg.fps as f64);
        let fps = cfg.fps as u64;
        let handle = std::thread::Builder::new()
            .name("aether-testpattern".into())
            .spawn(move || {
                let start = Instant::now();
                let mut n: u64 = 0;
                while !stop.load(Ordering::SeqCst) {
                    let img = pattern.frame(n);
                    let dirty = pattern.dirty_rects(n);
                    let pts = n * 1_000_000 / fps;
                    sink(CaptureEvent::Frame(CapturedFrame::from_image(display_id, pts, img, Some(dirty))));
                    n += 1;
                    let deadline = start + period * n as u32;
                    let now = Instant::now();
                    if deadline > now {
                        std::thread::sleep(deadline - now);
                    }
                }
            })
            .map_err(|e| CaptureError::Os(e.to_string()))?;
        self.thread = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TestPatternCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn deterministic() {
        let p = TestPattern::new(320, 200);
        assert_eq!(p.frame(5), p.frame(5));
        assert_ne!(p.frame(5), p.frame(6));
    }

    #[test]
    fn dirty_rects_cover_all_changes() {
        let p = TestPattern::new(257, 143);
        for i in 1..40u64 {
            let (a, b) = (p.frame(i - 1), p.frame(i));
            let rects = p.dirty_rects(i);
            for y in 0..p.height {
                for x in 0..p.width {
                    if a.pixel(x, y) != b.pixel(x, y) {
                        assert!(rects.iter().any(|r| r.contains(x as i32, y as i32)), "frame {i}: ({x},{y}) not covered");
                    }
                }
            }
        }
    }

    #[test]
    fn text_region_has_ink_and_paper() {
        let p = TestPattern::new(640, 480);
        let img = p.frame(0);
        let t = p.text_rect();
        let mut ink = 0;
        for y in t.y as u32..(t.y as u32 + t.height) {
            for x in t.x as u32..(t.x as u32 + t.width) {
                if img.pixel(x, y)[0] < 0x80 {
                    ink += 1;
                }
            }
        }
        let frac = ink as f64 / t.area() as f64;
        assert!(frac > 0.03 && frac < 0.5, "ink fraction {frac}");
    }

    #[test]
    fn tiny_sizes_do_not_panic() {
        for (w, h) in [(1, 1), (8, 8), (9, 3), (64, 64)] {
            let p = TestPattern::new(w, h);
            for i in 0..12 {
                let _ = p.frame(i);
                let _ = p.dirty_rects(i);
            }
        }
    }

    #[test]
    fn capturer_emits_frames() {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let f2 = frames.clone();
        let mut cap = TestPatternCapturer::new(64, 48);
        cap.start(
            CaptureTarget::Display { display_id: 7 },
            CaptureConfig { fps: 200, ..Default::default() },
            Box::new(move |ev| {
                if let CaptureEvent::Frame(f) = ev {
                    f2.lock().unwrap().push((f.display_id, f.pts_us));
                }
            }),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(60));
        cap.stop();
        let got = frames.lock().unwrap().clone();
        assert!(got.len() >= 3, "only {} frames", got.len());
        assert!(got.iter().all(|(d, _)| *d == 7));
        assert_eq!(got[1].1, 5_000);
        let n = got.len();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(frames.lock().unwrap().len(), n, "frames after stop");
    }
}
