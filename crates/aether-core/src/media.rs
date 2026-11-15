//! Host media pipeline: capture → hybrid scheduler → VideoToolbox / lossless tiles → wire.
//!
//! Runs on a dedicated thread (encoders are synchronous). Captured frames
//! arrive through a 1-slot "latest wins" mailbox so a slow network never
//! builds latency: stale frames are dropped, never queued.

use aether_capture::{CaptureConfig, CaptureEvent, CapturedFrame, ScreenCapturer};
use aether_codec::{
    EncoderConfig, HybridScheduler, SchedulerConfig, StreamMode, TileConfig, TileEncoder, TileGrid, VideoEncoder,
    create_encoder, encoder_capabilities,
};
use aether_protocol::{CaptureTarget, ChromaFormat, QualitySettings, Rect, VideoCodec, VideoMessage};
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Commands from the session to the running pipeline.
#[derive(Debug, Clone)]
pub enum PipelineCommand {
    RequestKeyframe,
    SetQuality(QualitySettings),
}

/// One-slot mailbox: `put` replaces any unconsumed frame.
#[derive(Default)]
struct Mailbox {
    slot: Mutex<Option<CapturedFrame>>,
    cv: Condvar,
}

impl Mailbox {
    fn put(&self, f: CapturedFrame) {
        *self.slot.lock() = Some(f);
        self.cv.notify_one();
    }

    fn take_timeout(&self, timeout: Duration) -> Option<CapturedFrame> {
        let mut g = self.slot.lock();
        if g.is_none() {
            self.cv.wait_for(&mut g, timeout);
        }
        g.take()
    }
}

pub struct HostVideoPipeline {
    stop: Arc<AtomicBool>,
    capturer: Option<Box<dyn ScreenCapturer>>,
    thread: Option<std::thread::JoinHandle<()>>,
    commands: std::sync::mpsc::Sender<PipelineCommand>,
}

/// Choose the stream mode from client preferences and what this machine can encode.
pub fn choose_mode(q: &QualitySettings) -> (StreamMode, Option<(VideoCodec, ChromaFormat)>) {
    let caps = encoder_capabilities();
    let wants_tiles_only = q.preferred_codecs.first() == Some(&VideoCodec::Tiles);
    let video = q
        .preferred_codecs
        .iter()
        .filter(|c| **c != VideoCodec::Tiles)
        .find_map(|c| {
            let want = if q.prefer_444 { ChromaFormat::Yuv444 } else { ChromaFormat::Yuv420 };
            caps.iter().find(|(cc, ch)| cc == c && *ch == want).or_else(|| caps.iter().find(|(cc, _)| cc == c)).copied()
        });
    match video {
        Some(v) if !wants_tiles_only => (if q.lossless_text { StreamMode::Hybrid } else { StreamMode::VideoOnly }, Some(v)),
        _ => (StreamMode::TilesOnly, None),
    }
}

/// Reasonable default bitrate for a resolution / frame rate (≈0.07 bit per pixel per frame).
pub fn default_bitrate_kbps(width: u32, height: u32, fps: u16) -> u32 {
    let bps = width as f64 * height as f64 * fps.max(1) as f64 * 0.07;
    (bps / 1000.0).clamp(2_000.0, 80_000.0) as u32
}

impl HostVideoPipeline {
    /// Start capturing `target` and streaming `VideoMessage`s into `out`.
    pub fn start(
        mut capturer: Box<dyn ScreenCapturer>,
        target: CaptureTarget,
        display_id: u32,
        quality: QualitySettings,
        excluded_pids: Vec<u32>,
        out: mpsc::Sender<VideoMessage>,
    ) -> crate::Result<Self> {
        let mailbox = Arc::new(Mailbox::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();

        let mb = mailbox.clone();
        let err_out = out.clone();
        capturer
            .start(
                target,
                CaptureConfig {
                    fps: quality.max_fps.clamp(1, 240),
                    show_cursor: true,
                    scale: quality.scale.clamp(0.25, 1.0),
                    capture_audio: false,
                    excluded_pids,
                },
                Box::new(move |ev| match ev {
                    CaptureEvent::Frame(f) => mb.put(f),
                    CaptureEvent::Audio(_) => {}
                    CaptureEvent::Error(e) => {
                        tracing::warn!("capture error: {e}");
                        let _ = err_out.try_send(VideoMessage::InvalidateRefinement { display_id, rects: vec![] });
                    }
                }),
            )
            .map_err(|e| crate::CoreError::Other(format!("capture: {e}")))?;

        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("aether-video".into())
            .spawn(move || run(display_id, quality, mailbox, cmd_rx, out, stop2))?;
        Ok(Self { stop, capturer: Some(capturer), thread: Some(thread), commands: cmd_tx })
    }

    pub fn command(&self, c: PipelineCommand) {
        let _ = self.commands.send(c);
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.capturer.take() {
            c.stop();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for HostVideoPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

struct State {
    display_id: u32,
    quality: QualitySettings,
    mode: StreamMode,
    video: Option<(VideoCodec, ChromaFormat)>,
    encoder: Option<Box<dyn VideoEncoder>>,
    tiles: TileEncoder,
    sched: HybridScheduler,
    seq: u64,
    epoch: Instant,
    last_frame: Option<CapturedFrame>,
    last_img: Option<aether_capture::BgraImage>,
    config_sent: Option<(u32, u32, VideoCodec)>,
}

fn run(
    display_id: u32,
    quality: QualitySettings,
    mailbox: Arc<Mailbox>,
    cmds: std::sync::mpsc::Receiver<PipelineCommand>,
    out: mpsc::Sender<VideoMessage>,
    stop: Arc<AtomicBool>,
) {
    let (mode, video) = choose_mode(&quality);
    let mut st = State {
        display_id,
        sched: HybridScheduler::new(SchedulerConfig { mode, ..SchedulerConfig::default() }),
        quality,
        mode,
        video,
        encoder: None,
        tiles: TileEncoder::with_config(TileConfig::default()),
        seq: 0,
        epoch: Instant::now(),
        last_frame: None,
        last_img: None,
        config_sent: None,
    };
    tracing::info!(?mode, ?video, "video pipeline started");

    while !stop.load(Ordering::Relaxed) {
        for c in cmds.try_iter() {
            match c {
                PipelineCommand::RequestKeyframe => st.sched.request_keyframe(),
                PipelineCommand::SetQuality(q) => {
                    let (mode, video) = choose_mode(&q);
                    st.quality = q;
                    if mode != st.mode || video != st.video {
                        st.mode = mode;
                        st.video = video;
                        st.encoder = None;
                        st.config_sent = None;
                        st.tiles.reset();
                        st.sched.set_mode(mode, st.now_us());
                        st.sched.reset();
                    }
                }
            }
        }
        let now = st.now_us();
        let wait = st
            .sched
            .next_deadline_us()
            .map(|d| Duration::from_micros(d.saturating_sub(now)))
            .unwrap_or(Duration::from_millis(50))
            .min(Duration::from_millis(50));
        let msgs = match mailbox.take_timeout(wait) {
            Some(frame) => st.on_frame(frame),
            None => st.on_idle(),
        };
        for m in msgs {
            if out.blocking_send(m).is_err() {
                return; // session gone
            }
        }
    }
}

impl State {
    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn ensure_encoder(&mut self, w: u32, h: u32) -> Option<&mut Box<dyn VideoEncoder>> {
        let (codec, chroma) = self.video?;
        if self.encoder.is_none() {
            let cfg = EncoderConfig {
                codec,
                chroma,
                width: w,
                height: h,
                fps: self.quality.max_fps,
                bitrate_kbps: self.quality.bitrate_kbps.unwrap_or_else(|| default_bitrate_kbps(w, h, self.quality.max_fps)),
                low_latency: true,
            };
            match create_encoder(cfg) {
                Ok(e) => self.encoder = Some(e),
                Err(e) => {
                    tracing::warn!("hardware encoder unavailable ({e}); falling back to lossless tiles");
                    self.video = None;
                    self.mode = StreamMode::TilesOnly;
                    let now = self.now_us();
                    self.sched.set_mode(StreamMode::TilesOnly, now);
                    return None;
                }
            }
        }
        self.encoder.as_mut()
    }

    fn config_msg(&mut self, w: u32, h: u32) -> Option<VideoMessage> {
        let codec = match self.mode {
            StreamMode::TilesOnly => VideoCodec::Tiles,
            _ => self.encoder.as_ref().map(|e| e.config().codec).unwrap_or(VideoCodec::Tiles),
        };
        if self.config_sent == Some((w, h, codec)) {
            return None;
        }
        self.config_sent = Some((w, h, codec));
        let chroma = self.encoder.as_ref().map(|e| e.config().chroma).unwrap_or(ChromaFormat::Yuv444);
        Some(VideoMessage::Config { display_id: self.display_id, codec, chroma, width: w, height: h, fps: self.quality.max_fps })
    }

    fn rects(grid: &TileGrid, idx: &[u32]) -> Vec<Rect> {
        idx.iter().map(|i| grid.rect(*i)).collect()
    }

    fn on_frame(&mut self, frame: CapturedFrame) -> Vec<VideoMessage> {
        let mut out = Vec::new();
        let Ok(img) = frame.to_bgra() else { return out };
        let now = self.now_us();
        let diff = self.tiles.diff(&img, frame.dirty_rects.as_deref());
        let plan = self.sched.on_frame(now, diff.grid, &diff.changed);

        if self.mode != StreamMode::TilesOnly && plan.encode_video {
            let force = plan.force_keyframe;
            let encoded = self.ensure_encoder(img.width, img.height).map(|e| e.encode(&frame, force));
            if let Some(m) = self.config_msg(img.width, img.height) {
                out.push(m);
            }
            match encoded {
                Some(Ok(Some(ef))) => {
                    let seq = self.next_seq();
                    out.push(VideoMessage::Frame(ef.into_video_frame(self.display_id, seq)));
                }
                Some(Ok(None)) => {}
                Some(Err(e)) => {
                    tracing::warn!("encode failed: {e}");
                    self.encoder = None;
                }
                None => {}
            }
        } else if let Some(m) = self.config_msg(img.width, img.height) {
            out.push(m);
        }
        if !plan.invalidated.is_empty() {
            out.push(VideoMessage::InvalidateRefinement {
                display_id: self.display_id,
                rects: Self::rects(&diff.grid, &plan.invalidated),
            });
        }
        if !plan.send_tiles.is_empty() || plan.full_refresh {
            let seq = self.next_seq();
            if let Some(b) = self.tiles.encode_tiles(&img, &plan.send_tiles, self.display_id, seq, now, false) {
                out.push(VideoMessage::Tiles(b));
            }
        }
        if !plan.refine_tiles.is_empty() {
            let seq = self.next_seq();
            if let Some(b) = self.tiles.encode_tiles(&img, &plan.refine_tiles, self.display_id, seq, now, true) {
                self.sched.report_sent(b.tiles.iter().map(|t| t.data.len()).sum(), now);
                out.push(VideoMessage::Tiles(b));
            }
        }
        self.last_frame = Some(frame);
        self.last_img = Some(img);
        out
    }

    fn on_idle(&mut self) -> Vec<VideoMessage> {
        let mut out = Vec::new();
        let now = self.now_us();
        let plan = self.sched.poll(now);
        if plan.force_keyframe
            && let Some(f) = self.last_frame.clone()
                && let Some(Ok(Some(ef))) = self.ensure_encoder(f.width, f.height).map(|e| e.encode(&f, true)) {
                    let seq = self.next_seq();
                    out.push(VideoMessage::Frame(ef.into_video_frame(self.display_id, seq)));
                }
        if (!plan.refine_tiles.is_empty() || !plan.send_tiles.is_empty())
            && let Some(img) = self.last_img.take() {
                let refine = !plan.refine_tiles.is_empty();
                let tiles = if refine { &plan.refine_tiles } else { &plan.send_tiles };
                let seq = self.next_seq();
                if let Some(b) = self.tiles.encode_tiles(&img, tiles, self.display_id, seq, now, refine) {
                    self.sched.report_sent(b.tiles.iter().map(|t| t.data.len()).sum(), now);
                    out.push(VideoMessage::Tiles(b));
                }
                self.last_img = Some(img);
            }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_capture::TestPatternCapturer;
    use aether_codec::Framebuffer;

    fn tiles_only() -> QualitySettings {
        QualitySettings { preferred_codecs: vec![VideoCodec::Tiles], ..QualitySettings::default() }
    }

    #[test]
    fn mode_selection() {
        assert_eq!(choose_mode(&tiles_only()).0, StreamMode::TilesOnly);
        let q = QualitySettings { lossless_text: false, ..QualitySettings::default() };
        let (m, v) = choose_mode(&q);
        if v.is_some() {
            assert_eq!(m, StreamMode::VideoOnly);
        }
        assert!(default_bitrate_kbps(1920, 1080, 60) > 5_000);
    }

    /// Tiles-only end to end: the client framebuffer must equal the source frame.
    #[test]
    fn tiles_pipeline_reconstructs_source_exactly() {
        let (tx, mut rx) = mpsc::channel(64);
        let cap = Box::new(TestPatternCapturer::new(320, 200));
        let mut p = HostVideoPipeline::start(cap, CaptureTarget::Display { display_id: 0 }, 0, tiles_only(), vec![], tx).unwrap();
        let mut fb = Framebuffer::new();
        let mut batches = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while batches < 10 && Instant::now() < deadline {
            if let Ok(m) = rx.try_recv() {
                if let VideoMessage::Tiles(b) = m {
                    fb.apply(&b).unwrap();
                    batches += 1;
                }
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        p.stop();
        assert!(batches >= 10, "only {batches} batches");
        assert_eq!((fb.width(), fb.height()), (320, 200));
    }

    /// Hybrid (VideoToolbox + refinement) — needs the hardware encoder.
    #[cfg(target_os = "macos")]
    #[test]
    fn hybrid_pipeline_emits_config_video_and_refinements() {
        let (tx, mut rx) = mpsc::channel(256);
        let cap = Box::new(TestPatternCapturer::new(640, 360));
        let mut p = HostVideoPipeline::start(
            cap,
            CaptureTarget::Display { display_id: 0 },
            0,
            QualitySettings { max_fps: 30, ..QualitySettings::default() },
            vec![],
            tx,
        )
        .unwrap();
        let (mut config, mut keyframe, mut refinement) = (false, false, false);
        let deadline = Instant::now() + Duration::from_secs(8);
        while !(config && keyframe && refinement) && Instant::now() < deadline {
            match rx.try_recv() {
                Ok(VideoMessage::Config { codec, .. }) => config |= codec != VideoCodec::Tiles,
                Ok(VideoMessage::Frame(f)) => keyframe |= f.keyframe,
                Ok(VideoMessage::Tiles(b)) => refinement |= b.refinement,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        p.stop();
        assert!(config && keyframe, "config={config} keyframe={keyframe}");
        assert!(refinement, "static regions should receive lossless refinement");
    }
}
