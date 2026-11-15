//! PipeWire video stream consumer (the data path of the ScreenCast portal).
//!
//! A dedicated thread runs a PipeWire main loop with one input stream connected to the
//! target node. Format negotiation offers BGRx/BGRA/RGBx/RGBA twice: first with a mandatory
//! `DRM_FORMAT_MOD_LINEAR` modifier (DMA-BUF, mapped read-only with `DMA_BUF_IOCTL_SYNC`),
//! then without modifier (MemFd/MemPtr shared memory). Tiled/compressed GPU modifiers would
//! need an EGL/Vulkan import and are deliberately not offered, so compositors fall back to a
//! linear copy or shared memory.
//!
//! Per buffer: the newest queued buffer wins (older ones are recycled), corrupted or empty
//! chunks are skipped, `SPA_META_VideoCrop` is honoured, `SPA_META_VideoDamage` becomes
//! `dirty_rects` (accumulated over frames dropped by the fps limiter); without damage
//! metadata the frame is compared tile-by-tile with the previous one and unchanged frames
//! are not delivered. `SPA_META_Cursor` bitmaps are composited when the portal delivers the
//! cursor as metadata.

use std::io::Cursor;
use std::os::fd::{OwnedFd, RawFd};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamFlags, StreamState};
use spa::buffer::meta::{MetaCursor, MetaVideoCrop, MetaVideoDamage};
use spa::buffer::{ChunkFlags, DataType};
use spa::param::ParamType;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::serialize::PodSerializer;
use spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

use super::{CursorImage, blend_cursor, scale_image, tile_diff};
use crate::{
    BgraImage, CaptureConfig, CaptureError, CaptureEvent, CaptureSink, CaptureTarget, CapturedFrame, Rect, Result,
    ScreenCapturer,
};

const DRM_FORMAT_MOD_LINEAR: i64 = 0;
const MAX_DAMAGE_RECTS: i32 = 16;
const CURSOR_MAX: i32 = 256;

/// Captures one PipeWire video node.
///
/// Normally created by [`PortalCapturer`](super::PortalCapturer) with the portal's remote fd;
/// [`PipeWireCapturer::for_node`] connects to the user's default PipeWire daemon instead
/// (e.g. for a virtual output or camera-like source node).
pub struct PipeWireCapturer {
    fd: Option<OwnedFd>,
    node_id: u32,
    display_id: u32,
    composite_cursor: bool,
    quit: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl PipeWireCapturer {
    /// Capture `node_id` through an already opened PipeWire remote (portal `OpenPipeWireRemote`).
    pub fn with_remote(fd: OwnedFd, node_id: u32, display_id: u32) -> Self {
        Self { fd: Some(fd), node_id, display_id, composite_cursor: false, quit: None, thread: None }
    }

    /// Capture `node_id` on the default PipeWire daemon (`$PIPEWIRE_REMOTE` / `pipewire-0`).
    pub fn for_node(node_id: u32, display_id: u32) -> Self {
        Self { fd: None, node_id, display_id, composite_cursor: false, quit: None, thread: None }
    }

    /// Composite `SPA_META_Cursor` bitmaps into the frames (portal cursor mode "metadata").
    pub fn set_composite_metadata_cursor(&mut self, on: bool) {
        self.composite_cursor = on;
    }
}

impl ScreenCapturer for PipeWireCapturer {
    fn start(&mut self, _target: CaptureTarget, cfg: CaptureConfig, sink: CaptureSink) -> Result<()> {
        if self.thread.is_some() {
            return Err(CaptureError::AlreadyRunning);
        }
        let (ready_tx, ready_rx) = mpsc::channel();
        let job = Job {
            fd: self.fd.take(),
            node_id: self.node_id,
            display_id: self.display_id,
            composite_cursor: self.composite_cursor,
            cfg,
            sink,
        };
        let thread = std::thread::Builder::new()
            .name("aether-pipewire-capture".into())
            .spawn(move || run(job, ready_tx))
            .map_err(|e| CaptureError::Os(format!("spawn PipeWire thread: {e}")))?;
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(quit)) => {
                self.quit = Some(quit);
                self.thread = Some(thread);
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(CaptureError::Timeout("PipeWire stream connection")),
        }
    }

    fn stop(&mut self) {
        if let Some(q) = self.quit.take() {
            let _ = q.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PipeWireCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Job {
    fd: Option<OwnedFd>,
    node_id: u32,
    display_id: u32,
    composite_cursor: bool,
    cfg: CaptureConfig,
    sink: CaptureSink,
}

fn pw_err(ctx: &'static str) -> impl FnOnce(pw::Error) -> CaptureError {
    move |e| CaptureError::Os(format!("PipeWire {ctx}: {e}"))
}

fn serialize(obj: Object) -> Vec<u8> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .map(|(c, _)| c.into_inner())
        .unwrap_or_default()
}

fn prop(key: u32, value: Value) -> Property {
    Property { key, flags: PropertyFlags::empty(), value }
}

/// EnumFormat offers: (1) linear DMA-BUF, (2) shared memory.
fn enum_formats(fps: u16) -> Vec<Vec<u8>> {
    let fps = fps.max(1) as u32;
    let base = |modifier: bool| {
        let mut props = vec![
            prop(FormatProperties::MediaType.as_raw(), Value::Id(Id(MediaType::Video.as_raw()))),
            prop(FormatProperties::MediaSubtype.as_raw(), Value::Id(Id(MediaSubtype::Raw.as_raw()))),
            prop(
                FormatProperties::VideoFormat.as_raw(),
                Value::Choice(ChoiceValue::Id(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: Id(VideoFormat::BGRx.as_raw()),
                        alternatives: [VideoFormat::BGRx, VideoFormat::BGRA, VideoFormat::RGBx, VideoFormat::RGBA]
                            .iter()
                            .map(|f| Id(f.as_raw()))
                            .collect(),
                    },
                ))),
            ),
        ];
        if modifier {
            props.push(Property {
                key: FormatProperties::VideoModifier.as_raw(),
                flags: PropertyFlags::MANDATORY,
                value: Value::Choice(ChoiceValue::Long(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum { default: DRM_FORMAT_MOD_LINEAR, alternatives: vec![DRM_FORMAT_MOD_LINEAR] },
                ))),
            });
        }
        props.push(prop(
            FormatProperties::VideoSize.as_raw(),
            Value::Choice(ChoiceValue::Rectangle(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Rectangle { width: 1920, height: 1080 },
                    min: Rectangle { width: 1, height: 1 },
                    max: Rectangle { width: 16384, height: 16384 },
                },
            ))),
        ));
        props.push(prop(
            FormatProperties::VideoFramerate.as_raw(),
            Value::Choice(ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Fraction { num: 0, denom: 1 },
                    min: Fraction { num: 0, denom: 1 },
                    max: Fraction { num: 1000, denom: 1 },
                },
            ))),
        ));
        props.push(prop(
            FormatProperties::VideoMaxFramerate.as_raw(),
            Value::Choice(ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Fraction { num: fps, denom: 1 },
                    min: Fraction { num: 1, denom: 1 },
                    max: Fraction { num: 1000, denom: 1 },
                },
            ))),
        ));
        serialize(Object { type_: SpaTypes::ObjectParamFormat.as_raw(), id: ParamType::EnumFormat.as_raw(), properties: props })
    };
    vec![base(true), base(false)]
}

fn int_range(default: i32, min: i32, max: i32) -> Value {
    Value::Choice(ChoiceValue::Int(Choice(ChoiceFlags::empty(), ChoiceEnum::Range { default, min, max })))
}

fn meta_param(ty: u32, size: Value) -> Vec<u8> {
    serialize(Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![prop(spa::sys::SPA_PARAM_META_type, Value::Id(Id(ty))), prop(spa::sys::SPA_PARAM_META_size, size)],
    })
}

/// Buffers + Meta params announced after the format is fixed.
fn buffer_params(dmabuf: bool) -> Vec<Vec<u8>> {
    let types: i32 = if dmabuf {
        1 << spa::sys::SPA_DATA_DmaBuf
    } else {
        (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr)
    };
    let region = std::mem::size_of::<spa::sys::spa_meta_region>() as i32;
    let cursor_size = |w: i32, h: i32| {
        (std::mem::size_of::<spa::sys::spa_meta_cursor>() + std::mem::size_of::<spa::sys::spa_meta_bitmap>()) as i32 + w * h * 4
    };
    vec![
        serialize(Object {
            type_: SpaTypes::ObjectParamBuffers.as_raw(),
            id: ParamType::Buffers.as_raw(),
            properties: vec![
                prop(spa::sys::SPA_PARAM_BUFFERS_buffers, int_range(4, 2, 16)),
                prop(
                    spa::sys::SPA_PARAM_BUFFERS_dataType,
                    Value::Choice(ChoiceValue::Int(Choice(ChoiceFlags::empty(), ChoiceEnum::Flags { default: types, flags: vec![] }))),
                ),
            ],
        }),
        meta_param(spa::sys::SPA_META_Header, Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32)),
        meta_param(spa::sys::SPA_META_VideoCrop, Value::Int(region)),
        meta_param(spa::sys::SPA_META_VideoDamage, int_range(region * MAX_DAMAGE_RECTS, region, region * MAX_DAMAGE_RECTS)),
        meta_param(spa::sys::SPA_META_Cursor, int_range(cursor_size(64, 64), cursor_size(1, 1), cursor_size(CURSOR_MAX, CURSOR_MAX))),
    ]
}

/// Whether a negotiated format pod carries a DRM modifier (→ DMA-BUF buffers).
fn format_has_modifier(param: &Pod) -> bool {
    match spa::pod::deserialize::PodDeserializer::deserialize_any_from(param.as_bytes()) {
        Ok((_, Value::Object(o))) => o.properties.iter().any(|p| p.key == FormatProperties::VideoModifier.as_raw()),
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Swizzle {
    Bgr,
    Rgb,
}

struct Negotiated {
    width: u32,
    height: u32,
    swizzle: Swizzle,
    /// BGRx/RGBx: alpha byte is padding.
    opaque: bool,
}

struct State {
    sink: CaptureSink,
    display_id: u32,
    cfg: CaptureConfig,
    composite_cursor: bool,
    format: Option<Negotiated>,
    start: Instant,
    interval: Duration,
    last_emit: Option<Instant>,
    prev: Option<BgraImage>,
    /// Damage of frames skipped by the fps limiter (None = unknown → full diff).
    pending_damage: Option<Vec<Rect>>,
    pending_any: bool,
    cursor: Option<CursorImage>,
    cursor_pos: (i32, i32),
    errored: bool,
}

fn run(job: Job, ready: mpsc::Sender<Result<pw::channel::Sender<()>>>) {
    let Job { fd, node_id, display_id, composite_cursor, cfg, sink } = job;
    pw::init();
    let setup = || -> Result<_> {
        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_err("main loop"))?;
        let context = pw::context::ContextRc::new(&mainloop, None).map_err(pw_err("context"))?;
        let core = match fd {
            Some(fd) => context.connect_fd_rc(fd, None),
            None => context.connect_rc(None),
        }
        .map_err(|e| CaptureError::Unsupported(if matches!(e, pw::Error::CreationFailed) {
            "cannot connect to PipeWire (is the pipewire daemon running?)"
        } else {
            "cannot connect to PipeWire"
        }))?;
        Ok((mainloop, context, core))
    };
    let (mainloop, _context, core) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let ml = mainloop.clone();
    let _quit = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());

    let target = node_id.to_string();
    let stream = match pw::stream::StreamBox::new(
        &core,
        "aetherlink-screen",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            // Session managers link by these properties (the connect() id alone is
            // deprecated); the portal's restricted remote only exposes the granted node.
            "target.object" => target.as_str(),
            "node.target" => target.as_str(),
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(pw_err("stream")(e)));
            return;
        }
    };
    let fps = cfg.fps;
    let state = State {
        sink,
        display_id,
        interval: Duration::from_secs_f64(1.0 / cfg.fps.max(1) as f64),
        cfg,
        composite_cursor,
        format: None,
        start: Instant::now(),
        last_emit: None,
        prev: None,
        pending_damage: Some(Vec::new()),
        pending_any: false,
        cursor: None,
        cursor_pos: (0, 0),
        errored: false,
    };
    let listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, st, _old, new| match new {
            StreamState::Error(msg) => {
                if !st.errored {
                    st.errored = true;
                    (st.sink)(CaptureEvent::Error(CaptureError::Os(format!("PipeWire stream error: {msg}"))));
                }
            }
            StreamState::Unconnected if st.format.is_some() && !st.errored => {
                st.errored = true;
                (st.sink)(CaptureEvent::Error(CaptureError::Os("PipeWire stream closed (capture revoked?)".into())));
            }
            _ => {}
        })
        .param_changed(on_param_changed)
        .process(on_process)
        .register();
    let _listener = match listener {
        Ok(l) => l,
        Err(e) => {
            let _ = ready.send(Err(pw_err("listener")(e)));
            return;
        }
    };
    let formats = enum_formats(fps);
    let mut pods: Vec<&Pod> = formats.iter().filter_map(|b| Pod::from_bytes(b)).collect();
    if let Err(e) = stream.connect(spa::utils::Direction::Input, Some(node_id), StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS, &mut pods) {
        let _ = ready.send(Err(pw_err("connect")(e)));
        return;
    }
    if ready.send(Ok(quit_tx)).is_err() {
        return; // start() timed out and gave up
    }
    mainloop.run();
    let _ = stream.disconnect();
}

fn on_param_changed(stream: &pw::stream::Stream, st: &mut State, id: u32, param: Option<&Pod>) {
    let Some(param) = param else { return };
    if id != ParamType::Format.as_raw() {
        return;
    }
    let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param) else { return };
    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return;
    }
    let mut info = VideoInfoRaw::new();
    if info.parse(param).is_err() {
        return;
    }
    let f = info.format();
    let (swizzle, opaque) = if f == VideoFormat::BGRx {
        (Swizzle::Bgr, true)
    } else if f == VideoFormat::BGRA {
        (Swizzle::Bgr, false)
    } else if f == VideoFormat::RGBx {
        (Swizzle::Rgb, true)
    } else if f == VideoFormat::RGBA {
        (Swizzle::Rgb, false)
    } else {
        (st.sink)(CaptureEvent::Error(CaptureError::Unsupported("PipeWire negotiated a non-RGB video format")));
        return;
    };
    let size = info.size();
    let dmabuf = format_has_modifier(param);
    tracing::info!(width = size.width, height = size.height, format = ?f, dmabuf, "PipeWire capture format");
    st.format = Some(Negotiated { width: size.width, height: size.height, swizzle, opaque });
    st.prev = None;
    let params = buffer_params(dmabuf);
    let mut pods: Vec<&Pod> = params.iter().filter_map(|b| Pod::from_bytes(b)).collect();
    if let Err(e) = stream.update_params(&mut pods) {
        tracing::warn!("PipeWire update_params failed: {e}");
    }
}

/// Read-only mapping of a DMA-BUF bracketed by `DMA_BUF_IOCTL_SYNC`.
struct DmaBufMap {
    fd: RawFd,
    ptr: *mut libc::c_void,
    len: usize,
}

const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_START: u64 = 0;
const DMA_BUF_SYNC_END: u64 = 4;
/// `_IOW('b', 0, struct dma_buf_sync)`.
const DMA_BUF_IOCTL_SYNC: u64 = (1 << 30) | (8 << 16) | ((b'b' as u64) << 8);

impl DmaBufMap {
    fn new(fd: RawFd, len: usize) -> Option<Self> {
        if fd < 0 || len == 0 {
            return None;
        }
        // SAFETY: mapping a dma-buf fd read-only; checked for failure; unmapped in Drop.
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        let m = Self { fd, ptr, len };
        m.sync(DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ);
        Some(m)
    }

    fn sync(&self, flags: u64) {
        let arg = flags;
        // SAFETY: struct dma_buf_sync is a single u64.
        unsafe { libc::ioctl(self.fd, DMA_BUF_IOCTL_SYNC as _, &arg as *const u64) };
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: valid read-only mapping of `len` bytes for the lifetime of self.
        unsafe { std::slice::from_raw_parts(self.ptr.cast(), self.len) }
    }
}

impl Drop for DmaBufMap {
    fn drop(&mut self) {
        self.sync(DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
        // SAFETY: ptr/len from mmap above.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

/// Copies a `width`×`height` rectangle at (`x`, `y`) out of a 32-bit RGB(x/A) buffer.
#[allow(clippy::too_many_arguments)]
fn convert(src: &[u8], offset: usize, stride: usize, x: u32, y: u32, width: u32, height: u32, fmt: &Negotiated) -> Option<BgraImage> {
    let row_bytes = width as usize * 4;
    let first = offset + y as usize * stride + x as usize * 4;
    if height == 0 || width == 0 || stride < row_bytes || src.len() < first + (height as usize - 1) * stride + row_bytes {
        return None;
    }
    let mut img = BgraImage::new(width, height);
    for r in 0..height as usize {
        let s = &src[first + r * stride..first + r * stride + row_bytes];
        let d = img.row_mut(r as u32);
        match fmt.swizzle {
            Swizzle::Bgr => d.copy_from_slice(s),
            Swizzle::Rgb => {
                for (dp, sp) in d.as_chunks_mut::<4>().0.iter_mut().zip(s.as_chunks::<4>().0.iter()) {
                    dp[0] = sp[2];
                    dp[1] = sp[1];
                    dp[2] = sp[0];
                    dp[3] = sp[3];
                }
            }
        }
        if fmt.opaque {
            d.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 0xFF);
        }
    }
    Some(img)
}

/// Converts a cursor bitmap (straight alpha, 32-bit RGB family) to premultiplied ARGB.
fn cursor_bitmap(format: VideoFormat, w: u32, h: u32, stride: usize, data: &[u8]) -> Option<Vec<u32>> {
    let idx: [usize; 4] = if format == VideoFormat::BGRA {
        [2, 1, 0, 3]
    } else if format == VideoFormat::RGBA {
        [0, 1, 2, 3]
    } else if format == VideoFormat::ARGB {
        [1, 2, 3, 0]
    } else if format == VideoFormat::ABGR {
        [3, 2, 1, 0]
    } else {
        return None;
    }; // positions of R, G, B, A
    if stride < w as usize * 4 || data.len() < stride * h.saturating_sub(1) as usize + w as usize * 4 {
        return None;
    }
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let p = &data[y * stride + x * 4..y * stride + x * 4 + 4];
            let a = p[idx[3]] as u32;
            let pm = |v: u8| (v as u32 * a + 127) / 255;
            out.push((a << 24) | (pm(p[idx[0]]) << 16) | (pm(p[idx[1]]) << 8) | pm(p[idx[2]]));
        }
    }
    Some(out)
}

fn on_process(stream: &pw::stream::Stream, st: &mut State) {
    // Use the newest buffer; older ones are recycled immediately (dropping re-queues them).
    let mut latest = None;
    while let Some(b) = stream.dequeue_buffer() {
        latest = Some(b);
    }
    let Some(mut buffer) = latest else { return };
    let Some(fmt) = st.format.as_ref() else { return };
    let (fw, fh) = (fmt.width, fmt.height);

    let mut cursor_changed = false;
    if st.composite_cursor
        && let Some(c) = buffer.find_meta::<MetaCursor>()
    {
        if c.id() == 0 {
            cursor_changed = st.cursor.take().is_some();
        } else {
            if let Some(bm) = c.bitmap()
                && let Some(data) = bm.bitmap_data()
            {
                let size = bm.size();
                if let Some(px) = cursor_bitmap(bm.format(), size.width, size.height, bm.stride().unsigned_abs() as usize, data) {
                    st.cursor = Some(CursorImage { x: 0, y: 0, width: size.width, height: size.height, argb_premultiplied: px });
                    cursor_changed = true;
                }
            }
            let (pos, hot) = (c.position(), c.hotspot());
            let p = (pos.x - hot.x, pos.y - hot.y);
            if p != st.cursor_pos {
                st.cursor_pos = p;
                cursor_changed = true;
            }
        }
    }
    let damage: Option<Vec<Rect>> = buffer.find_meta::<MetaVideoDamage>().map(|d| {
        d.iter()
            .filter(|r| r.size().width > 0 && r.size().height > 0)
            .map(|r| Rect::new(r.position().x, r.position().y, r.size().width, r.size().height))
            .collect()
    });
    let crop = buffer
        .find_meta::<MetaVideoCrop>()
        .map(|c| c.meta_region())
        .filter(|r| r.is_valid() && r.size().width > 0 && r.size().height > 0)
        .map(|r| (r.position().x.max(0) as u32, r.position().y.max(0) as u32, r.size().width, r.size().height));

    let datas = buffer.datas_mut();
    let Some(d) = datas.first_mut() else { return };
    let chunk = d.chunk();
    if chunk.flags().contains(ChunkFlags::CORRUPTED) {
        return;
    }
    let (offset, stride, size) = (chunk.offset() as usize, chunk.stride(), chunk.size() as usize);
    let stride = if stride > 0 { stride as usize } else { fw as usize * 4 };
    let (cx, cy, cw, ch) = match crop {
        Some((x, y, w, h)) if x + w <= fw && y + h <= fh => (x, y, w, h),
        _ => (0, 0, fw, fh),
    };

    let image = if size == 0 {
        // No new picture (e.g. a cursor-only update): re-emit the last frame if the cursor moved.
        if !cursor_changed {
            return;
        }
        match &st.prev {
            Some(p) => p.clone(),
            None => return,
        }
    } else {
        let converted = if d.type_() == DataType::DmaBuf {
            let raw = d.as_raw();
            let len = (raw.maxsize as usize + raw.mapoffset as usize).max(offset + stride * fh as usize);
            DmaBufMap::new(d.fd() as RawFd, len).and_then(|m| convert(m.bytes(), offset + raw.mapoffset as usize, stride, cx, cy, cw, ch, fmt))
        } else {
            d.data().and_then(|bytes| convert(bytes, offset, stride, cx, cy, cw, ch, fmt))
        };
        match converted {
            Some(i) => i,
            None => {
                tracing::debug!("PipeWire buffer could not be mapped / was too small");
                return;
            }
        }
    };

    // Accumulate damage across frames dropped by the limiter.
    match (&mut st.pending_damage, damage, crop.is_some()) {
        (Some(acc), Some(d), false) => acc.extend(d),
        _ => st.pending_damage = None,
    }
    st.pending_any = true;
    let now = Instant::now();
    if st.last_emit.is_some_and(|t| now.duration_since(t) + Duration::from_millis(2) < st.interval) {
        return;
    }

    let mut img = image;
    if let Some(c) = &st.cursor {
        let mut c = c.clone();
        c.x = st.cursor_pos.0 - cx as i32;
        c.y = st.cursor_pos.1 - cy as i32;
        blend_cursor(&mut img, &c);
    }
    let scaled = st.cfg.scale.is_finite() && st.cfg.scale > 0.0 && st.cfg.scale < 0.999;
    if scaled {
        img = scale_image(&img, st.cfg.scale);
    }
    let os_damage = st.pending_damage.replace(Vec::new());
    st.pending_any = false;
    let dirty = match &st.prev {
        Some(p) if p.width == img.width && p.height == img.height => {
            let diff = match os_damage {
                Some(d) if !scaled && !cursor_changed && st.cursor.is_none() => d,
                _ => tile_diff(p, &img),
            };
            if diff.is_empty() {
                return; // idle
            }
            Some(diff)
        }
        _ => None,
    };
    st.last_emit = Some(now);
    let pts = st.start.elapsed().as_micros() as u64;
    let frame = CapturedFrame::from_image(st.display_id, pts, img.clone(), dirty);
    st.prev = Some(img);
    (st.sink)(CaptureEvent::Frame(frame));
}

#[cfg(test)]
mod tests {
    //! The live test needs a PipeWire daemon + session manager (`pipewire` + `wireplumber`,
    //! see ci/linux-test.sh). It publishes a real `Video/Source` node from this process
    //! and captures it through [`PipeWireCapturer::for_node`].
    use super::*;

    #[test]
    fn format_offers_modifier_first() {
        let f = enum_formats(30);
        assert_eq!(f.len(), 2);
        assert!(format_has_modifier(Pod::from_bytes(&f[0]).unwrap()));
        assert!(!format_has_modifier(Pod::from_bytes(&f[1]).unwrap()));
        assert_eq!(buffer_params(false).len(), 5);
    }

    #[test]
    fn rgb_buffers_are_swizzled() {
        let fmt = Negotiated { width: 2, height: 2, swizzle: Swizzle::Rgb, opaque: true };
        // 2x2 RGBx with a 12-byte stride and 4 bytes of leading offset.
        let mut src = vec![0u8; 4 + 12 * 2];
        src[4..8].copy_from_slice(&[1, 2, 3, 0]);
        src[4 + 12 + 4..4 + 12 + 8].copy_from_slice(&[7, 8, 9, 0]);
        let img = convert(&src, 4, 12, 0, 0, 2, 2, &fmt).unwrap();
        assert_eq!(img.pixel(0, 0), [3, 2, 1, 255]);
        assert_eq!(img.pixel(1, 1), [9, 8, 7, 255]);
        let crop = convert(&src, 4, 12, 1, 1, 1, 1, &fmt).unwrap();
        assert_eq!(crop.pixel(0, 0), [9, 8, 7, 255]);
        assert!(convert(&src, 4, 12, 0, 0, 3, 2, &fmt).is_some()); // exactly fits
        assert!(convert(&src, 8, 12, 0, 0, 3, 2, &fmt).is_none()); // past the end
        assert!(convert(&src, 4, 12, 0, 0, 4, 2, &fmt).is_none()); // wider than the stride
        let cur = cursor_bitmap(VideoFormat::RGBA, 1, 1, 4, &[255, 0, 0, 128]).unwrap();
        assert_eq!(cur, vec![0x8080_0000]);
    }

    /// A real PipeWire `Video/Source` node (BGRx `w`×`h`, solid colour + a moving square).
    fn spawn_source(w: u32, h: u32) -> (u32, pw::channel::Sender<()>, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            pw::init();
            let mainloop = pw::main_loop::MainLoopRc::new(None).unwrap();
            let context = pw::context::ContextRc::new(&mainloop, None).unwrap();
            let core = context.connect_rc(None).expect("PipeWire daemon");
            let stream = pw::stream::StreamRc::new(
                core,
                "aether-test-source",
                properties! {
                    *pw::keys::MEDIA_CLASS => "Video/Source",
                    *pw::keys::NODE_NAME => "aether-test-source",
                    *pw::keys::MEDIA_TYPE => "Video",
                    *pw::keys::MEDIA_CATEGORY => "Source",
                    *pw::keys::MEDIA_ROLE => "Screen",
                },
            )
            .unwrap();
            let (quit_tx, quit_rx) = pw::channel::channel::<()>();
            let ml = mainloop.clone();
            let _q = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());
            let stride = w as i32 * 4;
            let tx2 = tx.clone();
            let _l = stream
                .add_local_listener_with_user_data(false)
                .state_changed(move |s, sent, _, new| {
                    if !*sent && matches!(new, StreamState::Paused | StreamState::Streaming) {
                        *sent = true;
                        let _ = tx2.send((s.node_id(), None));
                    }
                })
                .param_changed(move |s, _, id, p| {
                    if id != ParamType::Format.as_raw() || p.is_none() {
                        return;
                    }
                    let b = serialize(Object {
                        type_: SpaTypes::ObjectParamBuffers.as_raw(),
                        id: ParamType::Buffers.as_raw(),
                        properties: vec![
                            prop(spa::sys::SPA_PARAM_BUFFERS_buffers, int_range(4, 2, 8)),
                            prop(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
                            prop(spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(stride * h as i32)),
                            prop(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
                        ],
                    });
                    let mut pods = [Pod::from_bytes(&b).unwrap()];
                    s.update_params(&mut pods).unwrap();
                })
                .register()
                .unwrap();
            let fmt = serialize(Object {
                type_: SpaTypes::ObjectParamFormat.as_raw(),
                id: ParamType::EnumFormat.as_raw(),
                properties: vec![
                    prop(FormatProperties::MediaType.as_raw(), Value::Id(Id(MediaType::Video.as_raw()))),
                    prop(FormatProperties::MediaSubtype.as_raw(), Value::Id(Id(MediaSubtype::Raw.as_raw()))),
                    prop(FormatProperties::VideoFormat.as_raw(), Value::Id(Id(VideoFormat::BGRx.as_raw()))),
                    prop(FormatProperties::VideoSize.as_raw(), Value::Rectangle(Rectangle { width: w, height: h })),
                    prop(FormatProperties::VideoFramerate.as_raw(), Value::Fraction(Fraction { num: 30, denom: 1 })),
                ],
            });
            let mut pods = [Pod::from_bytes(&fmt).unwrap()];
            stream
                .connect(
                    spa::utils::Direction::Output,
                    None,
                    StreamFlags::DRIVER | StreamFlags::MAP_BUFFERS,
                    &mut pods,
                )
                .unwrap();
            let s2 = stream.clone();
            let frame = std::cell::Cell::new(0u32);
            let timer = mainloop.loop_().add_timer(move |_| {
                let n = frame.get();
                frame.set(n + 1);
                if let Some(mut b) = s2.dequeue_buffer() {
                    let datas = b.datas_mut();
                    let d = &mut datas[0];
                    if let Some(px) = d.data().filter(|px| px.len() >= (stride * h as i32) as usize) {
                        for y in 0..h as usize {
                            for x in 0..w as usize {
                                let o = y * stride as usize + x * 4;
                                // Background BGRx = (0x10, 0x20, 0x30); an 8×8 white square moves.
                                let sq = (x as u32).wrapping_sub(n % (w - 8)) < 8 && y < 8;
                                let v = if sq { [0xFF, 0xFF, 0xFF, 0] } else { [0x10, 0x20, 0x30, 0] };
                                px[o..o + 4].copy_from_slice(&v);
                            }
                        }
                    }
                    let c = d.chunk_mut();
                    *c.offset_mut() = 0;
                    *c.stride_mut() = stride;
                    *c.size_mut() = (stride * h as i32) as u32;
                }
                let _ = s2.trigger_process();
            });
            timer.update_timer(Some(Duration::from_millis(10)), Some(Duration::from_millis(20))).into_result().unwrap();
            let _ = tx.send((0, Some(quit_tx)));
            mainloop.run();
        });
        let mut node = None;
        let mut quit = None;
        while node.is_none() || quit.is_none() {
            match rx.recv_timeout(Duration::from_secs(10)).expect("source node did not come up") {
                (id, None) => node = Some(id),
                (_, Some(q)) => quit = Some(q),
            }
        }
        (node.unwrap(), quit.unwrap(), t)
    }

    #[test]
    #[ignore = "needs a PipeWire daemon + wireplumber (ci/linux-test.sh)"]
    fn captures_a_real_pipewire_video_node() {
        let (node, quit, t) = spawn_source(96, 64);
        let (tx, rx) = mpsc::channel();
        let mut cap = PipeWireCapturer::for_node(node, 7);
        cap.start(
            CaptureTarget::Display { display_id: 7 },
            CaptureConfig { fps: 30, ..Default::default() },
            Box::new(move |ev| {
                let _ = tx.send(ev);
            }),
        )
        .unwrap();
        let mut frames = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        while frames.len() < 3 && Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(CaptureEvent::Frame(f)) => frames.push(f),
                Ok(other) => panic!("unexpected {other:?}"),
                Err(_) => {}
            }
        }
        cap.stop();
        let _ = quit.send(());
        t.join().unwrap();
        assert!(frames.len() >= 3, "only {} frames captured", frames.len());
        let f = &frames[0];
        assert_eq!((f.display_id, f.width, f.height), (7, 96, 64));
        let img = f.to_bgra().unwrap();
        assert_eq!(img.pixel(50, 40), [0x10, 0x20, 0x30, 0xFF]);
        // Later frames: only the moving square's area is dirty.
        let later = frames.iter().skip(1).find_map(|f| f.dirty_rects.clone()).expect("dirty rects");
        assert!(later.iter().all(|r| r.y == 0), "{later:?}");
        assert!(frames.windows(2).all(|w| w[1].pts_us > w[0].pts_us));
    }
}
