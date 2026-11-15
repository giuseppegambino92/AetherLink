//! Video frames, lossless dirty tiles and cursor updates.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
    /// Pure dirty-tile mode (no video codec) — lossless, ideal for text/IDE work on LAN.
    Tiles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChromaFormat {
    Yuv420,
    Yuv444,
}

/// One compressed access unit. `data` is Annex-B (start-code delimited NAL units)
/// for H.264/HEVC, or an OBU temporal unit for AV1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFrame {
    pub display_id: u32,
    pub seq: u64,
    /// Presentation timestamp in microseconds since session start.
    pub pts_us: u64,
    pub codec: VideoCodec,
    pub chroma: ChromaFormat,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Encoding of a single tile. Pixel format is always BGRA8 premultiplied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TileEncoding {
    /// Uncompressed BGRA rows, `w*h*4` bytes.
    Raw,
    /// Zstd-compressed BGRA rows.
    Zstd,
    /// Whole tile is one colour (BGRA packed little-endian); `data` empty.
    Solid { bgra: u32 },
    /// Tile identical to a previously sent tile with this content hash (client-side tile cache).
    CacheRef { hash: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyTile {
    pub x: u32,
    pub y: u32,
    pub width: u16,
    pub height: u16,
    /// xxh3/blake3-derived content hash; lets the client cache tiles.
    pub hash: u64,
    pub encoding: TileEncoding,
    pub data: Vec<u8>,
}

/// A set of lossless tile patches applied atomically on the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyTileBatch {
    pub display_id: u32,
    pub seq: u64,
    pub pts_us: u64,
    pub frame_width: u32,
    pub frame_height: u32,
    pub tile_size: u16,
    /// When true the client must clear its framebuffer first (resolution change / resync).
    pub full_refresh: bool,
    /// True when sent as lossless refinement over a lossy video frame (hybrid mode).
    pub refinement: bool,
    pub tiles: Vec<DirtyTile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorShape {
    /// Stable id so the client can cache shapes.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    /// BGRA8, empty if the shape was sent before with the same id.
    pub bgra: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CursorUpdate {
    pub display_id: u32,
    /// Normalized 0..1 position on the display.
    pub x: f32,
    pub y: f32,
    pub visible: bool,
    pub shape_id: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VideoMessage {
    /// Stream (re)configuration — always precedes the first frame and any resolution change.
    Config { display_id: u32, codec: VideoCodec, chroma: ChromaFormat, width: u32, height: u32, fps: u16 },
    Frame(VideoFrame),
    Tiles(DirtyTileBatch),
    CursorShape(CursorShape),
    Cursor(CursorUpdate),
    /// Hybrid mode: these previously refined (lossless) regions changed again; the
    /// client must drop its lossless overlay there and show the video layer.
    InvalidateRefinement { display_id: u32, rects: Vec<crate::Rect> },
}
