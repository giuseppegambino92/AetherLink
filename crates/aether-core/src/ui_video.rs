//! Client side: converts wire `VideoMessage`s into the compact binary messages
//! the UI renders (WebCodecs for video, `putImageData` for lossless tiles).
//!
//! UI frame format — first byte is the kind:
//!
//! | kind | payload |
//! |------|---------|
//! | 1 `CONFIG`     | UTF-8 JSON `{displayId, mode:"video"\|"tiles", codec?, description?(base64 avcC/hvcC), width, height, chroma}` |
//! | 2 `VIDEO`      | `[u8 keyframe][u64 LE pts_us]` + length-prefixed (AVCC, 4-byte) access unit |
//! | 3 `BASE_TILE`  | `[u32 LE x][y][w][h]` + RGBA8 — lossless base layer (tiles-only mode) |
//! | 4 `CURSOR`     | UTF-8 JSON `{x, y, visible}` (normalized) |
//! | 5 `RESET`      | – clear both layers (seek / resync) |
//! | 6 `OVERLAY`    | `[u32 LE x][y][w][h]` + RGBA8 — lossless refinement over the video layer |
//! | 7 `INVALIDATE` | `[u32 LE n]` + n × `[x][y][w][h]` — drop overlay in these rects |
//!
//! The same adapter replays DVR history (fed from a fresh instance).

use aether_codec::{Framebuffer, ParameterSets, TileEncoder, annexb_to_avcc};
use aether_protocol::{CursorUpdate, Rect, VideoCodec, VideoMessage};
use base64::Engine as _;

pub const CONFIG: u8 = 1;
pub const VIDEO: u8 = 2;
pub const BASE_TILE: u8 = 3;
pub const CURSOR: u8 = 4;
pub const RESET: u8 = 5;
pub const OVERLAY: u8 = 6;
pub const INVALIDATE: u8 = 7;

/// Tile patches above this many pixels are split so a single UI message stays small.
const MAX_PATCH_PIXELS: u64 = 512 * 512;

#[derive(Default)]
pub struct UiVideoAdapter {
    fb: Framebuffer,
    mode_tiles: bool,
    last_config: Option<String>,
    codec: Option<VideoCodec>,
    display_id: u32,
}

impl UiVideoAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The lossless framebuffer (base layer in tiles-only mode, overlay layer in hybrid mode).
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.fb
    }

    pub fn is_tiles_mode(&self) -> bool {
        self.mode_tiles
    }

    /// Convert one wire message. Returns UI messages in display order.
    pub fn convert(&mut self, msg: &VideoMessage) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        match msg {
            VideoMessage::Config { display_id, codec, width, height, chroma, .. } => {
                self.display_id = *display_id;
                self.codec = Some(*codec);
                self.mode_tiles = *codec == VideoCodec::Tiles;
                self.last_config = None;
                self.fb = Framebuffer::new();
                out.push(vec![RESET]);
                if self.mode_tiles {
                    let json = serde_json::json!({
                        "displayId": display_id, "mode": "tiles", "width": width, "height": height,
                        "chroma": format!("{chroma:?}"),
                    });
                    self.push_config(&mut out, json.to_string());
                }
            }
            VideoMessage::Frame(f) => {
                if f.keyframe
                    && let Some(ps) = ParameterSets::from_annexb(f.codec, &f.data)
                        && let (Ok(desc), Ok(codec)) = (ps.decoder_config(), ps.codec_string()) {
                            let json = serde_json::json!({
                                "displayId": f.display_id, "mode": "video", "codec": codec,
                                "description": base64::engine::general_purpose::STANDARD.encode(desc),
                                "width": f.width, "height": f.height, "chroma": format!("{:?}", f.chroma),
                            });
                            self.push_config(&mut out, json.to_string());
                        }
                if self.last_config.is_none() {
                    return out; // WebCodecs cannot start before a keyframe with parameter sets
                }
                let avcc = annexb_to_avcc(&f.data);
                let mut m = Vec::with_capacity(10 + avcc.len());
                m.push(VIDEO);
                m.push(f.keyframe as u8);
                m.extend_from_slice(&f.pts_us.to_le_bytes());
                m.extend_from_slice(&avcc);
                out.push(m);
            }
            VideoMessage::Tiles(b) => {
                if b.full_refresh && self.mode_tiles {
                    let json = serde_json::json!({
                        "displayId": b.display_id, "mode": "tiles", "width": b.frame_width, "height": b.frame_height,
                        "chroma": "Yuv444",
                    });
                    self.push_config(&mut out, json.to_string());
                }
                match self.fb.apply(b) {
                    Ok(rects) => {
                        let kind = if self.mode_tiles { BASE_TILE } else { OVERLAY };
                        for r in rects {
                            for part in split(r) {
                                out.push(self.patch(kind, part));
                            }
                        }
                    }
                    Err(e) => tracing::warn!("tile batch rejected: {e}"),
                }
            }
            VideoMessage::InvalidateRefinement { rects, .. } => {
                if !rects.is_empty() {
                    let mut m = Vec::with_capacity(5 + rects.len() * 16);
                    m.push(INVALIDATE);
                    m.extend_from_slice(&(rects.len() as u32).to_le_bytes());
                    for r in rects {
                        push_rect(&mut m, r);
                    }
                    out.push(m);
                }
            }
            VideoMessage::Cursor(c) => out.push(cursor_message(c)),
            VideoMessage::CursorShape(_) => {}
        }
        out
    }

    fn push_config(&mut self, out: &mut Vec<Vec<u8>>, json: String) {
        if self.last_config.as_deref() == Some(json.as_str()) {
            return;
        }
        let mut m = Vec::with_capacity(1 + json.len());
        m.push(CONFIG);
        m.extend_from_slice(json.as_bytes());
        self.last_config = Some(json);
        out.push(m);
    }

    fn patch(&self, kind: u8, r: Rect) -> Vec<u8> {
        let rgba = self.fb.patch_rgba(r);
        let mut m = Vec::with_capacity(17 + rgba.len());
        m.push(kind);
        push_rect(&mut m, &r);
        m.extend_from_slice(&rgba);
        m
    }

    /// Messages that repaint the current lossless state from scratch (used when returning
    /// to LIVE from the DVR in tiles-only mode).
    pub fn repaint(&mut self) -> Vec<Vec<u8>> {
        let mut out = vec![vec![RESET]];
        if let Some(cfg) = &self.last_config {
            let mut m = vec![CONFIG];
            m.extend_from_slice(cfg.as_bytes());
            out.push(m);
        }
        let (w, h) = (self.fb.width(), self.fb.height());
        if w > 0 && h > 0 {
            let kind = if self.mode_tiles { BASE_TILE } else { OVERLAY };
            for part in split(Rect::new(0, 0, w, h)) {
                out.push(self.patch(kind, part));
            }
        }
        out
    }

    /// A self-contained DVR keyframe for tiles-only streams: the whole framebuffer as one
    /// `full_refresh` batch (serialized `VideoMessage`).
    pub fn snapshot_keyframe(&self, seq: u64, pts_us: u64) -> Option<VideoMessage> {
        if !self.mode_tiles || self.fb.width() == 0 {
            return None;
        }
        let mut enc = TileEncoder::new(aether_codec::DEFAULT_TILE_SIZE);
        enc.encode(&self.fb.to_image(), self.display_id, seq, pts_us, true).map(VideoMessage::Tiles)
    }
}

pub fn cursor_message(c: &CursorUpdate) -> Vec<u8> {
    let json = serde_json::json!({ "x": c.x, "y": c.y, "visible": c.visible });
    let mut m = vec![CURSOR];
    m.extend_from_slice(json.to_string().as_bytes());
    m
}

fn push_rect(m: &mut Vec<u8>, r: &Rect) {
    m.extend_from_slice(&(r.x.max(0) as u32).to_le_bytes());
    m.extend_from_slice(&(r.y.max(0) as u32).to_le_bytes());
    m.extend_from_slice(&r.width.to_le_bytes());
    m.extend_from_slice(&r.height.to_le_bytes());
}

/// Split a rect into horizontal bands of at most `MAX_PATCH_PIXELS`.
fn split(r: Rect) -> Vec<Rect> {
    if r.area() <= MAX_PATCH_PIXELS || r.width == 0 {
        return vec![r];
    }
    let band = (MAX_PATCH_PIXELS / r.width as u64).max(1) as u32;
    let mut out = Vec::new();
    let mut y = 0;
    while y < r.height {
        let h = band.min(r.height - y);
        out.push(Rect::new(r.x, r.y + y as i32, r.width, h));
        y += h;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_capture::TestPattern;
    use aether_protocol::ChromaFormat;

    fn rgba_at(msgs: &[Vec<u8>], kind: u8) -> usize {
        msgs.iter().filter(|m| m[0] == kind).map(|m| m.len() - 17).sum()
    }

    #[test]
    fn tiles_stream_becomes_config_and_patches_covering_the_frame() {
        let pat = TestPattern::new(1000, 700);
        let mut enc = TileEncoder::new(64);
        let mut ad = UiVideoAdapter::new();
        let cfg = VideoMessage::Config {
            display_id: 0,
            codec: VideoCodec::Tiles,
            chroma: ChromaFormat::Yuv444,
            width: 1000,
            height: 700,
            fps: 60,
        };
        let first = ad.convert(&cfg);
        assert_eq!(first[0], vec![RESET]);
        assert_eq!(first[1][0], CONFIG);
        let b = enc.encode(&pat.frame(0), 0, 1, 0, false).unwrap();
        let msgs = ad.convert(&VideoMessage::Tiles(b));
        assert_eq!(rgba_at(&msgs, BASE_TILE), 1000 * 700 * 4, "full refresh covers every pixel once");
        assert!(msgs.iter().all(|m| m.len() <= 17 + (MAX_PATCH_PIXELS as usize) * 4));

        // DVR keyframe snapshot replays into an identical framebuffer.
        let snap = ad.snapshot_keyframe(2, 10).unwrap();
        let mut replay = UiVideoAdapter::new();
        replay.convert(&cfg);
        replay.convert(&snap);
        assert_eq!(replay.framebuffer().pixels(), ad.framebuffer().pixels());
        assert!(ad.repaint().iter().any(|m| m[0] == BASE_TILE));
    }

    #[test]
    fn invalidate_and_split() {
        let mut ad = UiVideoAdapter::new();
        let m = ad.convert(&VideoMessage::InvalidateRefinement { display_id: 0, rects: vec![Rect::new(0, 0, 64, 64)] });
        assert_eq!(m[0][0], INVALIDATE);
        assert_eq!(m[0].len(), 1 + 4 + 16);
        let parts = split(Rect::new(0, 0, 3840, 2160));
        assert_eq!(parts.iter().map(|p| p.area()).sum::<u64>(), 3840 * 2160);
        assert!(parts.iter().all(|p| p.area() <= MAX_PATCH_PIXELS));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn video_frames_wait_for_keyframe_config() {
        use aether_capture::CapturedFrame;
        use aether_codec::{EncoderConfig, create_encoder};
        let mut enc = match create_encoder(EncoderConfig {
            codec: VideoCodec::H264,
            chroma: ChromaFormat::Yuv420,
            width: 320,
            height: 180,
            fps: 30,
            bitrate_kbps: 2000,
            low_latency: true,
        }) {
            Ok(e) => e,
            Err(e) => return eprintln!("no encoder: {e}"),
        };
        let pat = TestPattern::new(320, 180);
        let mut ad = UiVideoAdapter::new();
        let mut saw_config = false;
        let mut saw_video = false;
        for i in 0..10u64 {
            let f = CapturedFrame::from_image(0, i * 33_000, pat.frame(i), None);
            if let Some(ef) = enc.encode(&f, i == 0).unwrap() {
                for m in ad.convert(&VideoMessage::Frame(ef.into_video_frame(0, i))) {
                    if m[0] == CONFIG {
                        let v: serde_json::Value = serde_json::from_slice(&m[1..]).unwrap();
                        assert!(v["codec"].as_str().unwrap().starts_with("avc1."));
                        saw_config = true;
                    }
                    if m[0] == VIDEO {
                        assert!(saw_config, "config precedes video");
                        saw_video = true;
                    }
                }
            }
        }
        assert!(saw_config && saw_video);
    }
}
