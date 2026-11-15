//! Lossless dirty-tile codec.
//!
//! The frame is split into a grid of `tile_size`² tiles (edge tiles are smaller). Each tile's
//! content hash ([`tile_hash`], xxh3-64 seeded with the tile dimensions) is compared with the
//! previous frame; changed tiles are sent as
//!
//! * `Solid` — single colour, no payload;
//! * `CacheRef` — identical content was sent before and is still in the client tile cache;
//! * `Zstd` / `Raw` — literal BGRA rows (zstd level 3 by default; raw when zstd does not help).
//!   Horizontally adjacent literal tiles in a row are merged (up to `max_merge` tiles) into one
//!   wider [`DirtyTile`] for better compression and less per-tile overhead.
//!
//! ## Tile cache protocol (deterministic mirror)
//!
//! Encoder and [`Framebuffer`] keep an LRU of `cache_tiles` entries keyed by per-grid-tile
//! hash. Both sides apply exactly the same operations in batch order:
//!
//! * literal tile (possibly merged) → *insert* the hash of every grid tile it covers, left to
//!   right (the decoder recomputes hashes from the decoded pixels and verifies the batch hash);
//! * `CacheRef` → *touch* the hash;
//! * `Solid` → no cache operation;
//! * `full_refresh` → both sides clear the cache before processing the batch.
//!
//! The encoder therefore only emits `CacheRef` for hashes it *knows* the decoder still holds.
//! This requires the decoder to apply every batch in order (the video stream is a reliable,
//! ordered QUIC stream); after any loss of sync the host must call [`TileEncoder::reset`]
//! (next batch is a `full_refresh`). Both sides must use the same capacity
//! ([`DEFAULT_CACHE_TILES`]).

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};

use aether_capture::BgraImage;
use aether_protocol::{DirtyTile, DirtyTileBatch, Rect, TileEncoding};
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

use crate::{CodecError, Result};

/// Default tile edge in pixels.
pub const DEFAULT_TILE_SIZE: u16 = 64;
/// Default tile-cache capacity in tiles (≈ 32 MiB of 64×64 BGRA tiles on the client).
pub const DEFAULT_CACHE_TILES: usize = 2048;

/// Tile codec parameters. `tile_size` and `cache_tiles` must match on both ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileConfig {
    pub tile_size: u16,
    /// zstd level (negative = fast modes, 1..=22 regular; 3 is the default).
    pub zstd_level: i32,
    /// Shared tile-cache capacity (0 disables `CacheRef`).
    pub cache_tiles: usize,
    /// Maximum number of horizontally adjacent literal tiles merged into one `DirtyTile` (≥ 1).
    pub max_merge: u16,
    /// Worker threads for hashing / compression (0 = available parallelism, capped at 8).
    pub threads: usize,
}

impl Default for TileConfig {
    fn default() -> Self {
        Self { tile_size: DEFAULT_TILE_SIZE, zstd_level: 3, cache_tiles: DEFAULT_CACHE_TILES, max_merge: 4, threads: 0 }
    }
}

/// Geometry of the tile grid for one frame size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileGrid {
    pub width: u32,
    pub height: u32,
    pub tile: u32,
    pub cols: u32,
    pub rows: u32,
}

impl TileGrid {
    pub fn new(width: u32, height: u32, tile: u16) -> Self {
        let tile = tile.max(1) as u32;
        Self { width, height, tile, cols: width.div_ceil(tile), rows: height.div_ceil(tile) }
    }

    /// Number of tiles.
    pub fn len(&self) -> usize {
        self.cols as usize * self.rows as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Pixel rectangle of tile `idx` (row-major).
    pub fn rect(&self, idx: u32) -> Rect {
        let (c, r) = (idx % self.cols, idx / self.cols);
        let x = c * self.tile;
        let y = r * self.tile;
        Rect::new(x as i32, y as i32, self.tile.min(self.width - x), self.tile.min(self.height - y))
    }

    /// Indices of all tiles intersecting `r` (clamped to the frame).
    pub fn tiles_in_rect(&self, r: &Rect) -> impl Iterator<Item = u32> + '_ {
        let x0 = r.x.max(0) as u32;
        let y0 = r.y.max(0) as u32;
        let x1 = ((r.x as i64 + r.width as i64).clamp(0, self.width as i64)) as u32;
        let y1 = ((r.y as i64 + r.height as i64).clamp(0, self.height as i64)) as u32;
        let (c0, r0) = (x0 / self.tile, y0 / self.tile);
        let (c1, r1) = if x1 > x0 && y1 > y0 { (x1.div_ceil(self.tile), y1.div_ceil(self.tile)) } else { (c0, r0) };
        (r0..r1).flat_map(move |row| (c0..c1).map(move |col| row * self.cols + col))
    }
}

/// Content hash of a `w`×`h` BGRA region at (`x`, `y`) of a buffer with `stride` bytes per row.
pub fn tile_hash(data: &[u8], stride: usize, x: u32, y: u32, w: u32, h: u32) -> u64 {
    let mut hasher = Xxh3::with_seed(((w as u64) << 32) | h as u64);
    let row_bytes = w as usize * 4;
    for row in y..y + h {
        let o = row as usize * stride + x as usize * 4;
        hasher.update(&data[o..o + row_bytes]);
    }
    hasher.digest()
}

/// Batch-level hash of a merged literal tile: xxh3 of the covered grid-tile hashes.
fn combined_hash(hashes: &[u64]) -> u64 {
    if hashes.len() == 1 {
        return hashes[0];
    }
    let mut buf = Vec::with_capacity(hashes.len() * 8);
    for h in hashes {
        buf.extend_from_slice(&h.to_le_bytes());
    }
    xxh3_64(&buf)
}

fn solid_color(data: &[u8], stride: usize, x: u32, y: u32, w: u32, h: u32) -> Option<[u8; 4]> {
    let o = y as usize * stride + x as usize * 4;
    let first: [u8; 4] = data[o..o + 4].try_into().ok()?;
    let row_bytes = w as usize * 4;
    let first_row = &data[o..o + row_bytes];
    if !first_row.as_chunks::<4>().0.iter().all(|p| *p == first) {
        return None;
    }
    for row in y + 1..y + h {
        let o = row as usize * stride + x as usize * 4;
        if &data[o..o + row_bytes] != first_row {
            return None;
        }
    }
    Some(first)
}

fn worker_count(cfg_threads: usize) -> usize {
    if cfg_threads > 0 {
        return cfg_threads;
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8)
}

// ---------------------------------------------------------------------------------------------
// Deterministic LRU keyed by tile hash
// ---------------------------------------------------------------------------------------------

/// Identity hasher: keys are already uniformly distributed 64-bit hashes.
#[derive(Default)]
struct IdHasher(u64);
impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 << 8) | *b as u64;
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = v;
    }
}
type IdMap<V> = HashMap<u64, V, BuildHasherDefault<IdHasher>>;

struct Lru<V> {
    cap: usize,
    clock: u64,
    map: IdMap<(u64, V)>,
    order: BTreeMap<u64, u64>,
}

impl<V> Lru<V> {
    fn new(cap: usize) -> Self {
        Self { cap, clock: 0, map: IdMap::default(), order: BTreeMap::new() }
    }

    fn contains(&self, h: u64) -> bool {
        self.map.contains_key(&h)
    }

    fn touch(&mut self, h: u64) -> Option<&V> {
        let (stamp, v) = self.map.get_mut(&h)?;
        self.order.remove(stamp);
        self.clock += 1;
        *stamp = self.clock;
        self.order.insert(self.clock, h);
        Some(&*v)
    }

    fn insert(&mut self, h: u64, v: V) {
        if self.cap == 0 {
            return;
        }
        self.clock += 1;
        if let Some((stamp, old)) = self.map.get_mut(&h) {
            self.order.remove(stamp);
            *stamp = self.clock;
            *old = v;
            self.order.insert(self.clock, h);
            return;
        }
        if self.map.len() >= self.cap
            && let Some((_, victim)) = self.order.pop_first()
        {
            self.map.remove(&victim);
        }
        self.map.insert(h, (self.clock, v));
        self.order.insert(self.clock, h);
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    fn len(&self) -> usize {
        self.map.len()
    }
}

// ---------------------------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------------------------

/// Result of comparing a frame with the previous one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileDiff {
    pub grid: TileGrid,
    /// Changed tile indices, ascending.
    pub changed: Vec<u32>,
    /// True when the frame size differs from the previous frame (or it is the first frame).
    pub resized: bool,
}

enum Planned {
    Solid { idx: u32, hash: u64, bgra: [u8; 4] },
    Ref { idx: u32, hash: u64 },
    Run { first: u32, hashes: Vec<u64> },
}

/// Encodes BGRA frames into [`DirtyTileBatch`]es.
pub struct TileEncoder {
    cfg: TileConfig,
    grid: Option<TileGrid>,
    prev: Vec<u64>,
    /// Dimensions of the client framebuffer (last batch emitted).
    sent_dims: Option<(u32, u32)>,
    cache: Lru<()>,
    compressor: zstd::bulk::Compressor<'static>,
}

impl TileEncoder {
    /// Encoder with `tile`-pixel tiles and default settings.
    pub fn new(tile: u16) -> Self {
        Self::with_config(TileConfig { tile_size: tile.max(8), ..Default::default() })
    }

    pub fn with_config(mut cfg: TileConfig) -> Self {
        cfg.tile_size = cfg.tile_size.max(8);
        cfg.max_merge = cfg.max_merge.max(1);
        let compressor = zstd::bulk::Compressor::new(cfg.zstd_level).expect("zstd compressor");
        Self { cache: Lru::new(cfg.cache_tiles), cfg, grid: None, prev: Vec::new(), sent_dims: None, compressor }
    }

    pub fn config(&self) -> &TileConfig {
        &self.cfg
    }

    /// Forgets all state: the next batch is a `full_refresh` and the tile cache restarts empty.
    pub fn reset(&mut self) {
        self.grid = None;
        self.prev.clear();
        self.sent_dims = None;
        self.cache.clear();
    }

    /// Number of entries the encoder believes the client cache holds.
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Compares `img` with the previously diffed frame and remembers it.
    ///
    /// `hints` (e.g. `CapturedFrame::dirty_rects`) restrict hashing to the tiles they touch;
    /// all other tiles are assumed unchanged. Hints are ignored on the first frame and after a
    /// resize.
    pub fn diff(&mut self, img: &BgraImage, hints: Option<&[Rect]>) -> TileDiff {
        let grid = TileGrid::new(img.width, img.height, self.cfg.tile_size);
        let resized = self.grid != Some(grid);
        let candidates: Vec<u32> = match hints {
            Some(h) if !resized => {
                let mut mark = vec![false; grid.len()];
                for r in h {
                    for idx in grid.tiles_in_rect(r) {
                        mark[idx as usize] = true;
                    }
                }
                (0..grid.len() as u32).filter(|i| mark[*i as usize]).collect()
            }
            _ => (0..grid.len() as u32).collect(),
        };
        let hashes = self.hash_tiles(img, &grid, &candidates);
        if resized {
            self.grid = Some(grid);
            self.prev = vec![0; grid.len()];
        }
        let mut changed = Vec::new();
        for (idx, h) in candidates.iter().zip(hashes) {
            let slot = &mut self.prev[*idx as usize];
            if resized || *slot != h {
                changed.push(*idx);
            }
            *slot = h;
        }
        TileDiff { grid, changed, resized }
    }

    fn hash_tiles(&self, img: &BgraImage, grid: &TileGrid, idxs: &[u32]) -> Vec<u64> {
        let hash_one = |idx: &u32| {
            let r = grid.rect(*idx);
            tile_hash(&img.data, img.stride, r.x as u32, r.y as u32, r.width, r.height)
        };
        let workers = worker_count(self.cfg.threads);
        if workers <= 1 || idxs.len() < 256 {
            return idxs.iter().map(hash_one).collect();
        }
        let mut out = vec![0u64; idxs.len()];
        let chunk = idxs.len().div_ceil(workers);
        std::thread::scope(|s| {
            for (src, dst) in idxs.chunks(chunk).zip(out.chunks_mut(chunk)) {
                s.spawn(move || {
                    for (i, idx) in src.iter().enumerate() {
                        dst[i] = hash_one(idx);
                    }
                });
            }
        });
        out
    }

    /// Diffs `img` against the previous frame and encodes the changed tiles (contract API).
    ///
    /// Returns `None` when nothing changed and `force_full` is false. A size change (or the
    /// first frame, or `force_full`) produces a `full_refresh` batch containing every tile.
    pub fn encode(&mut self, img: &BgraImage, display_id: u32, seq: u64, pts_us: u64, force_full: bool) -> Option<DirtyTileBatch> {
        self.encode_hinted(img, None, display_id, seq, pts_us, force_full)
    }

    /// [`encode`](Self::encode) using OS dirty-rect hints to skip hashing untouched tiles.
    pub fn encode_hinted(
        &mut self,
        img: &BgraImage,
        hints: Option<&[Rect]>,
        display_id: u32,
        seq: u64,
        pts_us: u64,
        force_full: bool,
    ) -> Option<DirtyTileBatch> {
        let diff = self.diff(img, hints);
        let full = force_full || self.sent_dims != Some((img.width, img.height));
        let tiles: Vec<u32> = if full { (0..diff.grid.len() as u32).collect() } else { diff.changed };
        if tiles.is_empty() && !full {
            return None;
        }
        let hashes: Vec<u64> = tiles.iter().map(|i| self.prev[*i as usize]).collect();
        Some(self.build_batch(img, diff.grid, &tiles, Some(&hashes), full, false, display_id, seq, pts_us))
    }

    /// Encodes exactly the given tiles of `img` (e.g. lossless refinement of static regions in
    /// hybrid mode). Hashes are recomputed from `img`. Emits `full_refresh` (clearing the
    /// client framebuffer and cache) if the client's framebuffer has a different size.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_tiles(
        &mut self,
        img: &BgraImage,
        tiles: &[u32],
        display_id: u32,
        seq: u64,
        pts_us: u64,
        refinement: bool,
    ) -> Option<DirtyTileBatch> {
        let grid = TileGrid::new(img.width, img.height, self.cfg.tile_size);
        let mut tiles: Vec<u32> = tiles.iter().copied().filter(|t| (*t as usize) < grid.len()).collect();
        tiles.sort_unstable();
        tiles.dedup();
        let full = self.sent_dims != Some((img.width, img.height));
        if tiles.is_empty() && !full {
            return None;
        }
        Some(self.build_batch(img, grid, &tiles, None, full, refinement, display_id, seq, pts_us))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_batch(
        &mut self,
        img: &BgraImage,
        grid: TileGrid,
        tiles: &[u32],
        known_hashes: Option<&[u64]>,
        full_refresh: bool,
        refinement: bool,
        display_id: u32,
        seq: u64,
        pts_us: u64,
    ) -> DirtyTileBatch {
        if full_refresh {
            self.cache.clear();
            self.sent_dims = Some((img.width, img.height));
        }
        let hashes: Vec<u64> = match known_hashes {
            Some(h) => h.to_vec(),
            None => self.hash_tiles(img, &grid, tiles),
        };

        // Phase 1 (sequential, defines cache-operation order): classify + build runs.
        let mut plan: Vec<Planned> = Vec::with_capacity(tiles.len());
        let max_merge = self.cfg.max_merge as usize;
        for (&idx, &hash) in tiles.iter().zip(&hashes) {
            let r = grid.rect(idx);
            if let Some(bgra) = solid_color(&img.data, img.stride, r.x as u32, r.y as u32, r.width, r.height) {
                plan.push(Planned::Solid { idx, hash, bgra });
                continue;
            }
            if self.cache.contains(hash) {
                self.cache.touch(hash);
                plan.push(Planned::Ref { idx, hash });
                continue;
            }
            self.cache.insert(hash, ());
            if let Some(Planned::Run { first, hashes }) = plan.last_mut() {
                let next = *first + hashes.len() as u32;
                if next == idx && idx / grid.cols == *first / grid.cols && hashes.len() < max_merge {
                    hashes.push(hash);
                    continue;
                }
            }
            plan.push(Planned::Run { first: idx, hashes: vec![hash] });
        }

        // Phase 2: compress literal runs (parallel when there are many).
        let runs: Vec<Rect> = plan
            .iter()
            .filter_map(|p| match p {
                Planned::Run { first, hashes } => {
                    let r0 = grid.rect(*first);
                    let rl = grid.rect(*first + hashes.len() as u32 - 1);
                    Some(Rect::new(r0.x, r0.y, (rl.x as u32 + rl.width) - r0.x as u32, r0.height))
                }
                _ => None,
            })
            .collect();
        let payloads = self.compress_runs(img, &runs);

        // Phase 3: assemble in plan order.
        let mut out = Vec::with_capacity(plan.len());
        let mut payloads = payloads.into_iter();
        let mut run_rects = runs.into_iter();
        for p in plan {
            match p {
                Planned::Solid { idx, hash, bgra } => {
                    let r = grid.rect(idx);
                    out.push(DirtyTile {
                        x: r.x as u32,
                        y: r.y as u32,
                        width: r.width as u16,
                        height: r.height as u16,
                        hash,
                        encoding: TileEncoding::Solid { bgra: u32::from_le_bytes(bgra) },
                        data: Vec::new(),
                    });
                }
                Planned::Ref { idx, hash } => {
                    let r = grid.rect(idx);
                    out.push(DirtyTile {
                        x: r.x as u32,
                        y: r.y as u32,
                        width: r.width as u16,
                        height: r.height as u16,
                        hash,
                        encoding: TileEncoding::CacheRef { hash },
                        data: Vec::new(),
                    });
                }
                Planned::Run { hashes, .. } => {
                    let r = run_rects.next().expect("run rect");
                    let (encoding, data) = payloads.next().expect("run payload");
                    out.push(DirtyTile {
                        x: r.x as u32,
                        y: r.y as u32,
                        width: r.width as u16,
                        height: r.height as u16,
                        hash: combined_hash(&hashes),
                        encoding,
                        data,
                    });
                }
            }
        }
        DirtyTileBatch {
            display_id,
            seq,
            pts_us,
            frame_width: img.width,
            frame_height: img.height,
            tile_size: self.cfg.tile_size,
            full_refresh,
            refinement,
            tiles: out,
        }
    }

    fn compress_runs(&mut self, img: &BgraImage, runs: &[Rect]) -> Vec<(TileEncoding, Vec<u8>)> {
        fn one(c: &mut zstd::bulk::Compressor<'static>, img: &BgraImage, r: &Rect, scratch: &mut Vec<u8>) -> (TileEncoding, Vec<u8>) {
            scratch.clear();
            let row_bytes = r.width as usize * 4;
            for row in r.y as usize..r.y as usize + r.height as usize {
                let o = row * img.stride + r.x as usize * 4;
                scratch.extend_from_slice(&img.data[o..o + row_bytes]);
            }
            match c.compress(scratch) {
                Ok(z) if z.len() < scratch.len() => (TileEncoding::Zstd, z),
                _ => (TileEncoding::Raw, scratch.clone()),
            }
        }
        let workers = worker_count(self.cfg.threads);
        if workers <= 1 || runs.len() < 8 {
            let mut scratch = Vec::new();
            return runs.iter().map(|r| one(&mut self.compressor, img, r, &mut scratch)).collect();
        }
        let level = self.cfg.zstd_level;
        let mut out: Vec<(TileEncoding, Vec<u8>)> = vec![(TileEncoding::Raw, Vec::new()); runs.len()];
        let chunk = runs.len().div_ceil(workers);
        std::thread::scope(|s| {
            for (src, dst) in runs.chunks(chunk).zip(out.chunks_mut(chunk)) {
                s.spawn(move || {
                    let mut c = zstd::bulk::Compressor::new(level).expect("zstd compressor");
                    let mut scratch = Vec::new();
                    for (i, r) in src.iter().enumerate() {
                        dst[i] = one(&mut c, img, r, &mut scratch);
                    }
                });
            }
        });
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Decoder / client framebuffer
// ---------------------------------------------------------------------------------------------

/// Client-side BGRA framebuffer that applies [`DirtyTileBatch`]es.
pub struct Framebuffer {
    width: u32,
    height: u32,
    data: Vec<u8>,
    cache: Lru<Box<[u8]>>,
    decompressor: zstd::bulk::Decompressor<'static>,
    scratch: Vec<u8>,
    verify: bool,
}

impl Default for Framebuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framebuffer {
    /// Empty framebuffer with the default cache capacity; the first batch must be a `full_refresh`.
    pub fn new() -> Self {
        Self::with_cache_capacity(DEFAULT_CACHE_TILES)
    }

    pub fn with_cache_capacity(tiles: usize) -> Self {
        Self {
            width: 0,
            height: 0,
            data: Vec::new(),
            cache: Lru::new(tiles),
            decompressor: zstd::bulk::Decompressor::new().expect("zstd decompressor"),
            scratch: Vec::new(),
            verify: true,
        }
    }

    /// Enables/disables verifying literal tiles against their batch hash (on by default).
    pub fn set_verify(&mut self, verify: bool) {
        self.verify = verify;
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bytes per row of [`pixels`](Self::pixels).
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    /// Packed BGRA pixels.
    pub fn pixels(&self) -> &[u8] {
        &self.data
    }

    /// Copy of the framebuffer as an image.
    pub fn to_image(&self) -> BgraImage {
        BgraImage { width: self.width, height: self.height, stride: self.stride(), data: self.data.clone() }
    }

    fn blit(&mut self, x: u32, y: u32, w: u32, h: u32, src: &[u8]) {
        let row_bytes = w as usize * 4;
        let stride = self.stride();
        for row in 0..h as usize {
            let d = (y as usize + row) * stride + x as usize * 4;
            self.data[d..d + row_bytes].copy_from_slice(&src[row * row_bytes..(row + 1) * row_bytes]);
        }
    }

    fn copy_out(&self, x: u32, y: u32, w: u32, h: u32) -> Box<[u8]> {
        let row_bytes = w as usize * 4;
        let mut out = Vec::with_capacity(row_bytes * h as usize);
        for row in y..y + h {
            let o = row as usize * self.stride() + x as usize * 4;
            out.extend_from_slice(&self.data[o..o + row_bytes]);
        }
        out.into_boxed_slice()
    }

    /// Applies a batch; returns the rectangles that changed.
    pub fn apply(&mut self, b: &DirtyTileBatch) -> Result<Vec<Rect>> {
        if b.full_refresh {
            self.width = b.frame_width;
            self.height = b.frame_height;
            self.data.clear();
            self.data.resize(b.frame_width as usize * b.frame_height as usize * 4, 0);
            self.cache.clear();
        } else if (b.frame_width, b.frame_height) != (self.width, self.height) || self.width == 0 {
            return Err(CodecError::InvalidData(format!(
                "batch for {}x{} but framebuffer is {}x{} without full_refresh",
                b.frame_width, b.frame_height, self.width, self.height
            )));
        }
        let ts = b.tile_size.max(1) as u32;
        let mut changed = Vec::with_capacity(b.tiles.len());
        for t in &b.tiles {
            let (x, y, w, h) = (t.x, t.y, t.width as u32, t.height as u32);
            if w == 0 || h == 0 || x as u64 + w as u64 > self.width as u64 || y as u64 + h as u64 > self.height as u64 {
                return Err(CodecError::InvalidData(format!("tile {x},{y} {w}x{h} out of bounds")));
            }
            let n = w as usize * h as usize * 4;
            match &t.encoding {
                TileEncoding::Solid { bgra } => {
                    let px = bgra.to_le_bytes();
                    let stride = self.stride();
                    for row in y..y + h {
                        let o = row as usize * stride + x as usize * 4;
                        for p in self.data[o..o + w as usize * 4].as_chunks_mut::<4>().0 {
                            *p = px;
                        }
                    }
                }
                TileEncoding::CacheRef { hash } => {
                    let Some(px) = self.cache.touch(*hash) else {
                        return Err(CodecError::CacheMiss(*hash));
                    };
                    if px.len() != n {
                        return Err(CodecError::InvalidData(format!("cached tile {hash:#x} has a different size")));
                    }
                    let px = px.clone();
                    self.blit(x, y, w, h, &px);
                }
                TileEncoding::Raw | TileEncoding::Zstd => {
                    let mut buf = std::mem::take(&mut self.scratch);
                    let src: &[u8] = if t.encoding == TileEncoding::Zstd {
                        buf.clear();
                        buf.reserve(n);
                        let got = self
                            .decompressor
                            .decompress_to_buffer(&t.data, &mut buf)
                            .map_err(|e| CodecError::InvalidData(format!("zstd: {e}")))?;
                        if got != n {
                            return Err(CodecError::InvalidData(format!("zstd tile size {got} != {n}")));
                        }
                        &buf
                    } else {
                        &t.data
                    };
                    if src.len() != n {
                        return Err(CodecError::InvalidData(format!("raw tile size {} != {n}", src.len())));
                    }
                    self.blit(x, y, w, h, src);
                    self.scratch = buf;
                    self.insert_literal(t, ts)?;
                }
            }
            changed.push(Rect::new(x as i32, y as i32, w, h));
        }
        Ok(changed)
    }

    /// Inserts every grid tile covered by literal tile `t` into the cache (left → right) and
    /// verifies the batch hash.
    fn insert_literal(&mut self, t: &DirtyTile, ts: u32) -> Result<()> {
        let (x, y, w, h) = (t.x, t.y, t.width as u32, t.height as u32);
        let aligned = x % ts == 0 && y % ts == 0 && h <= ts;
        let mut hashes = Vec::with_capacity((w / ts + 1) as usize);
        if aligned {
            let mut cx = x;
            while cx < x + w {
                let cw = ts.min(x + w - cx);
                hashes.push((cx, cw, tile_hash(&self.data, self.stride(), cx, y, cw, h)));
                cx += cw;
            }
        } else {
            hashes.push((x, w, tile_hash(&self.data, self.stride(), x, y, w, h)));
        }
        if self.verify {
            let hs: Vec<u64> = hashes.iter().map(|h| h.2).collect();
            let got = combined_hash(&hs);
            if got != t.hash {
                return Err(CodecError::HashMismatch { expected: t.hash, actual: got });
            }
        }
        for (cx, cw, hash) in hashes {
            let px = self.copy_out(cx, y, cw, h);
            self.cache.insert(hash, px);
        }
        Ok(())
    }

    /// RGBA copy of `r` (clamped to the framebuffer) for canvas upload (`ImageData`).
    pub fn patch_rgba(&self, r: Rect) -> Vec<u8> {
        let x0 = r.x.clamp(0, self.width as i32) as u32;
        let y0 = r.y.clamp(0, self.height as i32) as u32;
        let x1 = (r.x as i64 + r.width as i64).clamp(x0 as i64, self.width as i64) as u32;
        let y1 = (r.y as i64 + r.height as i64).clamp(y0 as i64, self.height as i64) as u32;
        let mut out = Vec::with_capacity((x1 - x0) as usize * (y1 - y0) as usize * 4);
        for row in y0..y1 {
            let o = row as usize * self.stride();
            for p in self.data[o + x0 as usize * 4..o + x1 as usize * 4].as_chunks::<4>().0 {
                out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_capture::TestPattern;

    fn roundtrip(enc: &mut TileEncoder, fb: &mut Framebuffer, img: &BgraImage, seq: u64) -> Option<DirtyTileBatch> {
        let b = enc.encode(img, 1, seq, seq * 1000, false);
        if let Some(b) = &b {
            fb.apply(b).unwrap();
        }
        assert_eq!(fb.pixels(), &img.to_packed().data[..], "framebuffer differs after seq {seq}");
        b
    }

    #[test]
    fn grid_geometry() {
        let g = TileGrid::new(130, 70, 64);
        assert_eq!((g.cols, g.rows, g.len()), (3, 2, 6));
        assert_eq!(g.rect(2), Rect::new(128, 0, 2, 64));
        assert_eq!(g.rect(5), Rect::new(128, 64, 2, 6));
        let t: Vec<u32> = g.tiles_in_rect(&Rect::new(60, 10, 10, 60)).collect();
        assert_eq!(t, vec![0, 1, 3, 4]);
        assert_eq!(g.tiles_in_rect(&Rect::new(-5, -5, 3, 3)).count(), 0);
        assert_eq!(g.tiles_in_rect(&Rect::new(129, 69, 100, 100)).collect::<Vec<_>>(), vec![5]);
    }

    #[test]
    fn lossless_roundtrip_many_frames() {
        for (w, h) in [(640u32, 360u32), (333, 217)] {
            let p = TestPattern::new(w, h);
            let mut enc = TileEncoder::new(64);
            let mut fb = Framebuffer::new();
            let first = roundtrip(&mut enc, &mut fb, &p.frame(0), 0).unwrap();
            assert!(first.full_refresh);
            assert_eq!(first.tiles.iter().map(|t| t.width as u64 * t.height as u64).sum::<u64>(), w as u64 * h as u64);
            for i in 1..60 {
                let b = roundtrip(&mut enc, &mut fb, &p.frame(i), i).unwrap();
                assert!(!b.full_refresh);
                let total = TileGrid::new(w, h, 64).len();
                let covered: usize = b.tiles.iter().map(|t| (t.width as usize).div_ceil(64)).sum();
                assert!(covered < total, "frame {i}: every tile resent");
            }
            // static frame → nothing to send
            assert!(enc.encode(&p.frame(59), 1, 99, 0, false).is_none());
        }
    }

    #[test]
    fn dirty_hints_match_full_diff() {
        let p = TestPattern::new(500, 300);
        let mut a = TileEncoder::new(64);
        let mut b = TileEncoder::new(64);
        let mut fa = Framebuffer::new();
        let mut fbb = Framebuffer::new();
        for i in 0..40 {
            let img = p.frame(i);
            let hints = p.dirty_rects(i);
            let ba = a.encode(&img, 0, i, 0, false);
            let bb = b.encode_hinted(&img, Some(&hints), 0, i, 0, false);
            assert_eq!(ba, bb, "frame {i}");
            if let Some(x) = &ba {
                fa.apply(x).unwrap();
                fbb.apply(bb.as_ref().unwrap()).unwrap();
            }
            assert_eq!(fbb.pixels(), &img.data[..]);
        }
    }

    #[test]
    fn solid_tiles() {
        let mut img = BgraImage::new(128, 64);
        for p in img.data.as_chunks_mut::<4>().0 {
            *p = [1, 2, 3, 255];
        }
        let mut enc = TileEncoder::new(64);
        let b = enc.encode(&img, 0, 0, 0, false).unwrap();
        assert_eq!(b.tiles.len(), 2);
        for t in &b.tiles {
            assert_eq!(t.encoding, TileEncoding::Solid { bgra: u32::from_le_bytes([1, 2, 3, 255]) });
            assert!(t.data.is_empty());
        }
        let mut fb = Framebuffer::new();
        fb.apply(&b).unwrap();
        assert_eq!(fb.pixels(), &img.data[..]);
        assert_eq!(&fb.patch_rgba(Rect::new(0, 0, 1, 1))[..], &[3, 2, 1, 255]);
    }

    #[test]
    fn cache_refs_for_previously_seen_tiles() {
        let p = TestPattern::new(256, 128);
        let a = p.frame(0);
        let mut b = a.clone();
        // replace the left half with distinctive content, then switch back to `a`
        for y in 0..128 {
            for x in 0..128 {
                let o = y * b.stride + x * 4;
                b.data[o..o + 4].copy_from_slice(&[(x * 7) as u8, (y * 3) as u8, (x ^ y) as u8, 255]);
            }
        }
        let mut enc = TileEncoder::new(64);
        let mut fb = Framebuffer::new();
        roundtrip(&mut enc, &mut fb, &a, 0);
        roundtrip(&mut enc, &mut fb, &b, 1);
        let back = roundtrip(&mut enc, &mut fb, &a, 2).unwrap();
        assert!(!back.tiles.is_empty());
        let refs = back.tiles.iter().filter(|t| matches!(t.encoding, TileEncoding::CacheRef { .. })).count();
        let solids = back.tiles.iter().filter(|t| matches!(t.encoding, TileEncoding::Solid { .. })).count();
        assert_eq!(refs + solids, back.tiles.len(), "all restored tiles must come from cache: {back:?}");
        assert!(refs > 0);
        assert!(back.tiles.iter().all(|t| t.data.is_empty()));
    }

    #[test]
    fn cache_eviction_stays_in_sync() {
        // Tiny cache so evictions happen constantly; decoder must never miss.
        let cfg = TileConfig { cache_tiles: 5, ..Default::default() };
        let mut enc = TileEncoder::with_config(cfg);
        let mut fb = Framebuffer::with_cache_capacity(5);
        let p = TestPattern::new(320, 200);
        let frames: Vec<BgraImage> = (0..12).map(|i| p.frame(i * 13)).collect();
        for (n, i) in [0usize, 3, 1, 3, 0, 7, 2, 0, 11, 3, 3, 5, 0, 1, 2].iter().enumerate() {
            let img = &frames[*i];
            if let Some(b) = enc.encode(img, 0, n as u64, 0, false) {
                fb.apply(&b).unwrap();
            }
            assert_eq!(fb.pixels(), &img.data[..]);
        }
    }

    #[test]
    fn duplicate_tiles_within_one_frame_use_cache() {
        // Same non-solid 64x64 pattern repeated 4 times in a row.
        let mut img = BgraImage::new(256, 64);
        for y in 0..64 {
            for x in 0..256 {
                let o = y * img.stride + x * 4;
                let (lx, ly) = (x % 64, y);
                img.data[o..o + 4].copy_from_slice(&[(lx * 4) as u8, (ly * 4) as u8, (lx ^ ly) as u8, 255]);
            }
        }
        let mut enc = TileEncoder::new(64);
        let b = enc.encode(&img, 0, 0, 0, false).unwrap();
        assert_eq!(b.tiles.len(), 4);
        assert!(matches!(b.tiles[0].encoding, TileEncoding::Zstd | TileEncoding::Raw));
        assert!(b.tiles[1..].iter().all(|t| t.encoding == TileEncoding::CacheRef { hash: b.tiles[0].hash }));
        let mut fb = Framebuffer::new();
        fb.apply(&b).unwrap();
        assert_eq!(fb.pixels(), &img.data[..]);
    }

    #[test]
    fn merged_runs_and_hash_verification() {
        let p = TestPattern::new(512, 128);
        let mut enc = TileEncoder::with_config(TileConfig { max_merge: 4, cache_tiles: 0, ..Default::default() });
        let b = enc.encode(&p.frame(0), 0, 0, 0, false).unwrap();
        assert!(b.tiles.iter().any(|t| t.width == 256), "expected merged 4-tile runs");
        let mut fb = Framebuffer::with_cache_capacity(0);
        fb.apply(&b).unwrap();
        assert_eq!(fb.pixels(), &p.frame(0).data[..]);

        let mut bad = b.clone();
        let t = bad.tiles.iter_mut().find(|t| t.encoding == TileEncoding::Zstd).unwrap();
        t.hash ^= 1;
        let mut fb2 = Framebuffer::with_cache_capacity(0);
        assert!(matches!(fb2.apply(&bad), Err(CodecError::HashMismatch { .. })));
    }

    #[test]
    fn resolution_change_forces_full_refresh() {
        let mut enc = TileEncoder::new(64);
        let mut fb = Framebuffer::new();
        let a = TestPattern::new(320, 240).frame(1);
        let b = TestPattern::new(400, 300).frame(1);
        assert!(roundtrip(&mut enc, &mut fb, &a, 0).unwrap().full_refresh);
        assert!(!roundtrip(&mut enc, &mut fb, &TestPattern::new(320, 240).frame(2), 1).unwrap().full_refresh);
        let rb = roundtrip(&mut enc, &mut fb, &b, 2).unwrap();
        assert!(rb.full_refresh);
        assert_eq!((fb.width(), fb.height()), (400, 300));
        // force_full and reset also produce full refreshes
        assert!(enc.encode(&b, 0, 3, 0, true).unwrap().full_refresh);
        enc.reset();
        assert!(roundtrip(&mut enc, &mut fb, &b, 4).unwrap().full_refresh);
    }

    #[test]
    fn decoder_rejects_bad_batches() {
        let mut fb = Framebuffer::new();
        let p = TestPattern::new(128, 64);
        let mut enc = TileEncoder::new(64);
        let full = enc.encode(&p.frame(0), 0, 0, 0, false).unwrap();
        let mut not_full = full.clone();
        not_full.full_refresh = false;
        assert!(fb.apply(&not_full).is_err(), "needs full_refresh first");
        fb.apply(&full).unwrap();
        let mut oob = full.clone();
        oob.full_refresh = false;
        oob.tiles[0].x = 100;
        assert!(fb.apply(&oob).is_err());
        let miss = DirtyTileBatch {
            tiles: vec![DirtyTile { x: 0, y: 0, width: 64, height: 64, hash: 42, encoding: TileEncoding::CacheRef { hash: 42 }, data: vec![] }],
            full_refresh: false,
            ..full
        };
        assert!(matches!(fb.apply(&miss), Err(CodecError::CacheMiss(42))));
    }

    #[test]
    fn encode_tiles_refinement() {
        let p = TestPattern::new(256, 192);
        let img = p.frame(4);
        let mut enc = TileEncoder::new(64);
        let mut fb = Framebuffer::new();
        let b = enc.encode_tiles(&img, &[5, 0, 5, 99], 2, 7, 8, true).unwrap();
        assert!(b.full_refresh && b.refinement);
        let rects = fb.apply(&b).unwrap();
        assert_eq!(rects.len(), 2);
        let g = TileGrid::new(256, 192, 64);
        for idx in [0u32, 5] {
            let r = g.rect(idx);
            for y in r.y as u32..r.y as u32 + r.height {
                for x in r.x as u32..r.x as u32 + r.width {
                    let o = (y * 256 + x) as usize * 4;
                    assert_eq!(&fb.pixels()[o..o + 4], &img.pixel(x, y));
                }
            }
        }
        let b2 = enc.encode_tiles(&img, &[1], 2, 8, 9, true).unwrap();
        assert!(!b2.full_refresh);
        assert!(enc.encode_tiles(&img, &[], 2, 9, 9, true).is_none());
    }

    #[test]
    fn patch_rgba_clamps() {
        let mut enc = TileEncoder::new(64);
        let mut fb = Framebuffer::new();
        let img = TestPattern::new(100, 50).frame(0);
        fb.apply(&enc.encode(&img, 0, 0, 0, false).unwrap()).unwrap();
        let patch = fb.patch_rgba(Rect::new(90, 40, 20, 20));
        assert_eq!(patch.len(), 10 * 10 * 4);
        let px = img.pixel(90, 40);
        assert_eq!(&patch[..4], &[px[2], px[1], px[0], px[3]]);
        assert!(fb.patch_rgba(Rect::new(200, 200, 5, 5)).is_empty());
    }

    #[test]
    fn strided_input() {
        let p = TestPattern::new(200, 100);
        let packed = p.frame(3);
        let stride = 200 * 4 + 64;
        let mut data = vec![0xCD; stride * 100];
        for y in 0..100 {
            data[y * stride..y * stride + 800].copy_from_slice(packed.row(y as u32));
        }
        let strided = BgraImage::from_raw(200, 100, stride, data).unwrap();
        let mut e1 = TileEncoder::new(64);
        let mut e2 = TileEncoder::new(64);
        assert_eq!(e1.encode(&packed, 0, 0, 0, false), e2.encode(&strided, 0, 0, 0, false));
    }

    #[test]
    #[ignore = "benchmark; run with --release -- --ignored --nocapture"]
    fn bench_4k_tile_diff_throughput() {
        let p = TestPattern::new(3840, 2160);
        let frames: Vec<BgraImage> = (0..8).map(|i| p.frame(i)).collect();
        let mut enc = TileEncoder::new(64);
        let mut fb = Framebuffer::new();
        let t = std::time::Instant::now();
        let full = enc.encode(&frames[0], 0, 0, 0, false).unwrap();
        let full_ms = t.elapsed().as_secs_f64() * 1e3;
        let full_bytes: usize = full.tiles.iter().map(|t| t.data.len()).sum();
        fb.apply(&full).unwrap();
        let iters = 120;
        let t = std::time::Instant::now();
        let mut bytes = 0usize;
        for i in 1..=iters {
            let img = &frames[i % frames.len()];
            if let Some(b) = enc.encode(img, 0, i as u64, 0, false) {
                bytes += b.tiles.iter().map(|t| t.data.len()).sum::<usize>();
            }
        }
        let per = t.elapsed().as_secs_f64() * 1e3 / iters as f64;
        let t = std::time::Instant::now();
        let mut d = enc.clone_for_bench();
        for _ in 0..iters {
            let _ = d.diff(&frames[1], None);
        }
        let diff_ms = t.elapsed().as_secs_f64() * 1e3 / iters as f64;
        println!(
            "4K: full encode {full_ms:.1} ms ({:.1} MiB), incremental encode {per:.2} ms/frame ({:.1} KiB avg), hash diff {diff_ms:.2} ms/frame ({:.1} GB/s)",
            full_bytes as f64 / 1048576.0,
            bytes as f64 / iters as f64 / 1024.0,
            3840.0 * 2160.0 * 4.0 / (diff_ms / 1e3) / 1e9
        );
    }

    impl TileEncoder {
        fn clone_for_bench(&self) -> TileEncoder {
            TileEncoder::with_config(self.cfg.clone())
        }
    }
}
