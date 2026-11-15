//! Hybrid zero-latency scheduling (pure logic, no clocks or I/O).
//!
//! Per display, the scheduler tracks for every tile when it last changed and whether its
//! current content has been delivered losslessly. The host pipeline is:
//!
//! ```text
//! capture ─► TileEncoder::diff ─► HybridScheduler::on_frame(now, grid, changed) ─► FramePlan
//!   plan.encode_video   → VideoEncoder::encode(frame, plan.force_keyframe)       (VideoMessage::Frame)
//!   plan.send_tiles     → TileEncoder::encode_tiles(img, …, refinement = false)  (Tiles-only mode)
//!   plan.refine_tiles   → TileEncoder::encode_tiles(img, …, refinement = true)   (lossless refinement)
//!   plan.invalidated    → tell the client to drop its lossless overlay on these tiles
//! timer (ScreenCaptureKit sends no frames while idle):
//!   HybridScheduler::poll(now) at next_deadline_us() → refinements of the last image
//! after sending: report_sent(bytes, now) to drive the refinement bandwidth budget
//! ```
//!
//! Modes:
//! * [`StreamMode::Hybrid`] — changes go to the video encoder; a tile static for at least
//!   `refine_after_us` (80 ms) whose content was not yet sent losslessly is scheduled for
//!   lossless refinement (oldest first, limited by `max_refine_tiles` and the byte budget).
//! * [`StreamMode::TilesOnly`] — every change is sent as lossless tiles immediately; no video.
//! * [`StreamMode::VideoOnly`] — video only, no refinement.

use crate::tiles::TileGrid;

/// Streaming mode of one display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    Hybrid,
    TilesOnly,
    VideoOnly,
}

/// Scheduler parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct SchedulerConfig {
    pub mode: StreamMode,
    /// A tile must be unchanged for this long before it is refined losslessly.
    pub refine_after_us: u64,
    /// Upper bound of refinement tiles per plan (0 = unlimited).
    pub max_refine_tiles: usize,
    /// Refinement byte budget (token bucket refill rate); `None` = unlimited.
    pub refine_budget_bytes_per_sec: Option<u64>,
    /// Token bucket depth in bytes.
    pub refine_burst_bytes: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            mode: StreamMode::Hybrid,
            refine_after_us: 80_000,
            max_refine_tiles: 0,
            refine_budget_bytes_per_sec: None,
            refine_burst_bytes: 4 * 1024 * 1024,
        }
    }
}

/// What to send for one frame / timer tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FramePlan {
    /// Encode and send the captured frame with the video encoder.
    pub encode_video: bool,
    /// Request an IDR (first frame, resize, explicit request).
    pub force_keyframe: bool,
    /// Tiles-only mode: tiles to send losslessly with `refinement = false`.
    pub send_tiles: Vec<u32>,
    /// Tiles-only mode: the client framebuffer must be rebuilt (`send_tiles` covers all tiles).
    pub full_refresh: bool,
    /// Hybrid mode: static tiles to send losslessly with `refinement = true`.
    pub refine_tiles: Vec<u32>,
    /// Hybrid mode: previously refined tiles that changed again; the client must stop
    /// overlaying its lossless copy there (the video frame is authoritative again).
    pub invalidated: Vec<u32>,
}

impl FramePlan {
    pub fn is_idle(&self) -> bool {
        !self.encode_video && self.send_tiles.is_empty() && self.refine_tiles.is_empty() && self.invalidated.is_empty()
    }
}

#[derive(Debug, Clone, Copy)]
struct TileState {
    last_change_us: u64,
    /// Current content delivered losslessly (refined, or sent as tiles).
    lossless: bool,
}

/// Per-display hybrid scheduler. All times are caller-supplied microseconds (monotonic).
#[derive(Debug, Clone)]
pub struct HybridScheduler {
    cfg: SchedulerConfig,
    grid: Option<TileGrid>,
    tiles: Vec<TileState>,
    keyframe_requested: bool,
    full_refresh_requested: bool,
    tokens: i64,
    last_refill_us: Option<u64>,
}

impl HybridScheduler {
    pub fn new(cfg: SchedulerConfig) -> Self {
        let tokens = cfg.refine_burst_bytes as i64;
        Self {
            cfg,
            grid: None,
            tiles: Vec::new(),
            keyframe_requested: false,
            full_refresh_requested: false,
            tokens,
            last_refill_us: None,
        }
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.cfg
    }

    pub fn mode(&self) -> StreamMode {
        self.cfg.mode
    }

    /// Switches mode. Entering Tiles-only forces a full lossless refresh; entering Hybrid
    /// refines everything once it is static; entering Video-only forces a keyframe.
    pub fn set_mode(&mut self, mode: StreamMode, now_us: u64) {
        if mode == self.cfg.mode {
            return;
        }
        self.cfg.mode = mode;
        match mode {
            StreamMode::TilesOnly => self.full_refresh_requested = true,
            StreamMode::Hybrid => {
                self.tiles.fill(TileState { last_change_us: now_us, lossless: false });
            }
            StreamMode::VideoOnly => self.keyframe_requested = true,
        }
    }

    pub fn set_refine_after_us(&mut self, us: u64) {
        self.cfg.refine_after_us = us;
    }

    /// Bandwidth hook: sets the refinement budget (`None` = unlimited).
    pub fn set_refine_budget(&mut self, bytes_per_sec: Option<u64>, burst_bytes: u64) {
        self.cfg.refine_budget_bytes_per_sec = bytes_per_sec;
        self.cfg.refine_burst_bytes = burst_bytes;
        self.tokens = self.tokens.min(burst_bytes as i64);
    }

    pub fn set_max_refine_tiles(&mut self, n: usize) {
        self.cfg.max_refine_tiles = n;
    }

    /// Bandwidth hook: account bytes actually sent for refinement / tiles.
    pub fn report_sent(&mut self, bytes: usize, now_us: u64) {
        self.refill(now_us);
        if self.cfg.refine_budget_bytes_per_sec.is_some() {
            self.tokens -= bytes as i64;
        }
    }

    /// Next IDR (client request / decoder error) or next full tile refresh (Tiles-only).
    pub fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
        self.full_refresh_requested = true;
    }

    /// Forget all state (e.g. a new client attached): next frame is treated as the first.
    pub fn reset(&mut self) {
        self.grid = None;
        self.tiles.clear();
    }

    fn refill(&mut self, now_us: u64) {
        let Some(rate) = self.cfg.refine_budget_bytes_per_sec else {
            self.last_refill_us = Some(now_us);
            return;
        };
        if let Some(last) = self.last_refill_us
            && now_us > last
        {
            let add = (now_us - last) as u128 * rate as u128 / 1_000_000;
            self.tokens = (self.tokens as i128 + add as i128).min(self.cfg.refine_burst_bytes as i128) as i64;
        }
        self.last_refill_us = Some(now_us.max(self.last_refill_us.unwrap_or(0)));
    }

    /// Plans a newly captured frame. `changed` are the tile indices that differ from the
    /// previous captured frame (from `TileEncoder::diff`).
    pub fn on_frame(&mut self, now_us: u64, grid: TileGrid, changed: &[u32]) -> FramePlan {
        let mut plan = FramePlan::default();
        let resized = self.grid != Some(grid);
        if resized {
            self.grid = Some(grid);
            self.tiles = vec![TileState { last_change_us: now_us, lossless: false }; grid.len()];
            self.keyframe_requested = true;
            self.full_refresh_requested = true;
        }
        match self.cfg.mode {
            StreamMode::TilesOnly => {
                self.keyframe_requested = false;
                if self.full_refresh_requested {
                    self.full_refresh_requested = false;
                    plan.full_refresh = true;
                    plan.send_tiles = (0..grid.len() as u32).collect();
                } else {
                    plan.send_tiles = changed.iter().copied().filter(|i| (*i as usize) < self.tiles.len()).collect();
                    plan.send_tiles.sort_unstable();
                    plan.send_tiles.dedup();
                }
                for &i in &plan.send_tiles {
                    self.tiles[i as usize] = TileState { last_change_us: now_us, lossless: true };
                }
            }
            StreamMode::Hybrid | StreamMode::VideoOnly => {
                self.full_refresh_requested = false;
                let mut any_change = resized;
                for &i in changed {
                    let Some(t) = self.tiles.get_mut(i as usize) else { continue };
                    any_change = true;
                    if t.lossless && self.cfg.mode == StreamMode::Hybrid {
                        plan.invalidated.push(i);
                    }
                    *t = TileState { last_change_us: now_us, lossless: false };
                }
                plan.invalidated.sort_unstable();
                plan.invalidated.dedup();
                plan.force_keyframe = std::mem::take(&mut self.keyframe_requested);
                plan.encode_video = any_change || plan.force_keyframe;
                if self.cfg.mode == StreamMode::Hybrid {
                    plan.refine_tiles = self.due_refinements(now_us);
                }
            }
        }
        plan
    }

    /// Timer-driven planning when no new frame arrived: refinements only (plus a pending
    /// keyframe request, which needs the caller to re-encode the last frame).
    pub fn poll(&mut self, now_us: u64) -> FramePlan {
        let mut plan = FramePlan::default();
        if self.grid.is_none() {
            return plan;
        }
        match self.cfg.mode {
            StreamMode::Hybrid => {
                if self.keyframe_requested {
                    self.keyframe_requested = false;
                    plan.encode_video = true;
                    plan.force_keyframe = true;
                }
                plan.refine_tiles = self.due_refinements(now_us);
            }
            StreamMode::VideoOnly => {
                if std::mem::take(&mut self.keyframe_requested) {
                    plan.encode_video = true;
                    plan.force_keyframe = true;
                }
            }
            StreamMode::TilesOnly => {
                if std::mem::take(&mut self.full_refresh_requested) {
                    plan.full_refresh = true;
                    plan.send_tiles = (0..self.tiles.len() as u32).collect();
                    for t in &mut self.tiles {
                        t.lossless = true;
                    }
                }
                self.keyframe_requested = false;
            }
        }
        plan
    }

    /// Earliest time at which `poll` will produce a refinement (Hybrid mode), if any.
    pub fn next_deadline_us(&self) -> Option<u64> {
        if self.cfg.mode != StreamMode::Hybrid {
            return None;
        }
        self.tiles.iter().filter(|t| !t.lossless).map(|t| t.last_change_us + self.cfg.refine_after_us).min()
    }

    /// Number of tiles whose current content has not been delivered losslessly.
    pub fn pending_lossless(&self) -> usize {
        self.tiles.iter().filter(|t| !t.lossless).count()
    }

    fn due_refinements(&mut self, now_us: u64) -> Vec<u32> {
        self.refill(now_us);
        if self.cfg.refine_budget_bytes_per_sec.is_some() && self.tokens <= 0 {
            return Vec::new();
        }
        let threshold = self.cfg.refine_after_us;
        let mut due: Vec<(u64, u32)> = self
            .tiles
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.lossless && now_us.saturating_sub(t.last_change_us) >= threshold)
            .map(|(i, t)| (t.last_change_us, i as u32))
            .collect();
        due.sort_unstable();
        if self.cfg.max_refine_tiles > 0 {
            due.truncate(self.cfg.max_refine_tiles);
        }
        let mut out: Vec<u32> = due.into_iter().map(|(_, i)| i).collect();
        for &i in &out {
            self.tiles[i as usize].lossless = true;
        }
        out.sort_unstable();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1000;

    fn grid() -> TileGrid {
        TileGrid::new(256, 128, 64) // 4 x 2 tiles
    }

    #[test]
    fn first_frame_is_keyframe_then_refines_after_80ms() {
        let mut s = HybridScheduler::new(SchedulerConfig::default());
        let p = s.on_frame(0, grid(), &[]);
        assert!(p.encode_video && p.force_keyframe);
        assert!(p.refine_tiles.is_empty());
        assert_eq!(s.next_deadline_us(), Some(80 * MS));
        assert!(s.poll(79 * MS).refine_tiles.is_empty());
        let p = s.poll(80 * MS);
        assert_eq!(p.refine_tiles, (0..8).collect::<Vec<_>>());
        assert!(!p.encode_video);
        assert!(s.poll(500 * MS).is_idle(), "refined only once");
        assert_eq!(s.next_deadline_us(), None);
    }

    #[test]
    fn changing_tiles_go_to_video_and_refine_only_when_static() {
        let mut s = HybridScheduler::new(SchedulerConfig::default());
        s.on_frame(0, grid(), &[]);
        s.poll(100 * MS); // everything refined
        // tile 2 changes every 16 ms until t=200ms
        let mut t = 104 * MS;
        let mut first = true;
        while t <= 200 * MS {
            let p = s.on_frame(t, grid(), &[2]);
            assert!(p.encode_video && !p.force_keyframe);
            assert!(!p.refine_tiles.contains(&2));
            assert_eq!(p.invalidated, if first { vec![2] } else { vec![] });
            first = false;
            t += 16 * MS;
        }
        let last_change = t - 16 * MS;
        // static frames (no change) → no video, no refinement before 80 ms
        let p = s.on_frame(last_change + 79 * MS, grid(), &[]);
        assert!(!p.encode_video && p.refine_tiles.is_empty());
        assert_eq!(s.next_deadline_us(), Some(last_change + 80 * MS));
        let p = s.poll(last_change + 80 * MS);
        assert_eq!(p.refine_tiles, vec![2]);
        assert!(s.poll(last_change + 1000 * MS).is_idle());
    }

    #[test]
    fn configurable_threshold_and_oldest_first_limit() {
        let mut s = HybridScheduler::new(SchedulerConfig { refine_after_us: 30 * MS, max_refine_tiles: 2, ..Default::default() });
        s.on_frame(0, grid(), &[]);
        s.poll(40 * MS);
        s.poll(41 * MS);
        s.poll(42 * MS);
        s.poll(43 * MS); // all 8 refined, 2 per poll
        assert_eq!(s.pending_lossless(), 0);
        s.on_frame(100 * MS, grid(), &[7]);
        s.on_frame(110 * MS, grid(), &[1]);
        s.on_frame(120 * MS, grid(), &[4]);
        let p = s.poll(150 * MS);
        assert_eq!(p.refine_tiles, vec![1, 7], "two oldest");
        let p = s.poll(150 * MS);
        assert_eq!(p.refine_tiles, vec![4]);
    }

    #[test]
    fn bandwidth_budget_throttles_refinement() {
        let mut s = HybridScheduler::new(SchedulerConfig {
            refine_budget_bytes_per_sec: Some(100_000),
            refine_burst_bytes: 50_000,
            max_refine_tiles: 1,
            ..Default::default()
        });
        s.on_frame(0, grid(), &[]);
        let p = s.poll(80 * MS);
        assert_eq!(p.refine_tiles.len(), 1);
        s.report_sent(120_000, 80 * MS); // bucket: 50k - 120k = -70k
        assert!(s.poll(100 * MS).refine_tiles.is_empty()); // +2k
        assert!(s.poll(700 * MS).refine_tiles.is_empty()); // -70k + 62k = -8k
        assert_eq!(s.poll(800 * MS).refine_tiles.len(), 1); // +10k → 2k > 0
        s.set_refine_budget(None, 0);
        s.report_sent(10_000_000, 800 * MS);
        assert_eq!(s.poll(801 * MS).refine_tiles.len(), 1);
    }

    #[test]
    fn tiles_only_mode() {
        let mut s = HybridScheduler::new(SchedulerConfig { mode: StreamMode::TilesOnly, ..Default::default() });
        let p = s.on_frame(0, grid(), &[]);
        assert!(p.full_refresh && !p.encode_video);
        assert_eq!(p.send_tiles.len(), 8);
        let p = s.on_frame(16 * MS, grid(), &[3, 1, 3]);
        assert!(!p.full_refresh && !p.encode_video);
        assert_eq!(p.send_tiles, vec![1, 3]);
        assert!(p.refine_tiles.is_empty());
        assert!(s.poll(1000 * MS).is_idle());
        assert_eq!(s.next_deadline_us(), None);
        s.request_keyframe();
        let p = s.poll(1001 * MS);
        assert!(p.full_refresh && p.send_tiles.len() == 8);
    }

    #[test]
    fn resize_resets_and_forces_keyframe() {
        let mut s = HybridScheduler::new(SchedulerConfig::default());
        s.on_frame(0, grid(), &[]);
        s.poll(100 * MS);
        let g2 = TileGrid::new(640, 480, 64);
        let p = s.on_frame(200 * MS, g2, &[]);
        assert!(p.encode_video && p.force_keyframe);
        assert_eq!(s.pending_lossless(), g2.len());
        assert_eq!(s.poll(280 * MS).refine_tiles.len(), g2.len());
    }

    #[test]
    fn mode_switches() {
        let mut s = HybridScheduler::new(SchedulerConfig::default());
        s.on_frame(0, grid(), &[]);
        s.poll(80 * MS);
        s.set_mode(StreamMode::TilesOnly, 90 * MS);
        let p = s.on_frame(100 * MS, grid(), &[]);
        assert!(p.full_refresh && p.send_tiles.len() == 8 && !p.encode_video);
        s.set_mode(StreamMode::Hybrid, 200 * MS);
        assert!(s.poll(279 * MS).refine_tiles.is_empty());
        assert_eq!(s.poll(280 * MS).refine_tiles.len(), 8);
        s.set_mode(StreamMode::VideoOnly, 300 * MS);
        let p = s.poll(301 * MS);
        assert!(p.encode_video && p.force_keyframe && p.refine_tiles.is_empty());
        let p = s.on_frame(400 * MS, grid(), &[0]);
        assert!(p.encode_video && p.invalidated.is_empty() && p.refine_tiles.is_empty());
        assert!(s.poll(10_000 * MS).is_idle());
    }

    #[test]
    fn keyframe_request_in_hybrid() {
        let mut s = HybridScheduler::new(SchedulerConfig::default());
        s.on_frame(0, grid(), &[]);
        s.request_keyframe();
        let p = s.on_frame(10 * MS, grid(), &[]);
        assert!(p.encode_video && p.force_keyframe);
        let p = s.on_frame(20 * MS, grid(), &[]);
        assert!(!p.encode_video && !p.force_keyframe);
    }
}
