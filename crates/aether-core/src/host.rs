//! Host side: authorization policy and per-session service loop.

use crate::engine::Engine;
use crate::events::{self, names, TransferProgressEvent};
use crate::media::{HostVideoPipeline, PipelineCommand};
use crate::streams::{self, SharedSend};
use crate::telemetry::HostMonitor;
use crate::terminal::PtySession;
use aether_net::{BoxFuture, FramedRecv, FramedSend, HostAuthPolicy, IncomingStream, PeerConnection, Session, tunnel};
use aether_protocol::*;
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{mpsc, oneshot};

/// Authorization decisions backed by the trust store, the rotating password and UI consent.
pub(crate) struct HostPolicy {
    pub engine: Arc<Engine>,
}

impl HostAuthPolicy for HostPolicy {
    fn is_trusted(&self, peer: &Hello) -> bool {
        self.engine.settings().allow_unattended && self.engine.trust.lock().is_trusted(&peer.public_key)
    }

    fn password(&self) -> Option<String> {
        if self.engine.settings().require_consent {
            None
        } else {
            Some(self.engine.password.lock().current().to_owned())
        }
    }

    fn consent(&self, peer: &Hello) -> BoxFuture<'static, bool> {
        let (tx, rx) = oneshot::channel();
        let request_id = self.engine.next_id();
        self.engine.pending_consent.lock().insert(request_id, (peer.clone(), tx));
        events::emit(
            &*self.engine.events,
            names::INCOMING_REQUEST,
            &serde_json::json!({
                "requestId": request_id,
                "peerName": peer.device_name,
                "deviceId": peer.device_id.to_string(),
                "fingerprint": KeyFingerprint::of(&peer.public_key).to_string(),
                "os": format!("{:?}", peer.os),
            }),
        );
        let engine = self.engine.clone();
        Box::pin(async move {
            let ok = rx.await.unwrap_or(false);
            engine.pending_consent.lock().remove(&request_id);
            ok
        })
    }

    /// `require_biometric`: Touch ID / Windows Hello / account password of the person at this
    /// computer before a trusted device gets in. Fail-closed: errors, timeouts, a prompt already
    /// open for another device and unsupported platforms all reject.
    fn verify_trusted(&self, peer: &Hello) -> BoxFuture<'static, bool> {
        let engine = self.engine.clone();
        let peer_name = peer.device_name.clone();
        Box::pin(async move {
            let settings = engine.settings();
            if !settings.require_biometric {
                return true;
            }
            let Ok(_prompt) = engine.owner_prompt.try_lock() else {
                tracing::warn!(peer = %peer_name, "owner verification already in progress; rejecting");
                return false;
            };
            let reason = match settings.resolved_language(&std::env::var("LANG").unwrap_or_default()) {
                crate::settings::Language::It => format!("consentire a «{peer_name}» di connettersi a questo computer"),
                _ => format!("allow “{peer_name}” to connect to this computer"),
            };
            match tokio::task::spawn_blocking(move || crate::local_auth::verify(&reason)).await {
                Ok(Ok(())) => true,
                Ok(Err(e)) => {
                    tracing::info!(peer = %peer_name, "owner verification failed: {e}");
                    false
                }
                Err(e) => {
                    tracing::warn!("owner verification task failed: {e}");
                    false
                }
            }
        })
    }
}

/// Summary of an incoming session shown on the host's dashboard.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSessionInfo {
    pub session_id: u64,
    pub peer_name: String,
    pub device_id: String,
    pub fingerprint: String,
    pub os: String,
    pub since_ms: u64,
    pub remote_addr: String,
    #[serde(skip)]
    pub public_key: [u8; 32],
    #[serde(skip)]
    pub conn: Option<PeerConnection>,
}

/// Where a host session's video comes from.
enum VideoSource {
    /// Desktop: native capture → hybrid scheduler → VideoToolbox / tiles.
    Pipeline(HostVideoPipeline),
    /// Mobile: platform-encoded stream (MediaCodec / ReplayKit), see [`crate::external`].
    External(Box<dyn crate::external::ExternalStream>),
}

impl VideoSource {
    fn command(&self, c: PipelineCommand) {
        match (self, c) {
            (VideoSource::Pipeline(p), c) => p.command(c),
            (VideoSource::External(s), PipelineCommand::RequestKeyframe) => s.request_keyframe(),
            (VideoSource::External(_), PipelineCommand::SetQuality(_)) => {}
        }
    }

    fn stop(&mut self) {
        match self {
            VideoSource::Pipeline(p) => p.stop(),
            VideoSource::External(s) => s.stop(),
        }
    }
}

pub(crate) async fn accept_loop(engine: Arc<Engine>) {
    while let Some(incoming) = engine.endpoint.accept_connection().await {
        let engine = engine.clone();
        tokio::spawn(async move {
            let addr = incoming.remote_address();
            let hello = engine.local_hello(ConnectMode::EphemeralId);
            let policy = Arc::new(HostPolicy { engine: engine.clone() });
            match incoming.authenticate(hello, policy).await {
                Ok(session) => run_host_session(engine, session).await,
                Err(e) => tracing::info!("incoming connection from {addr} rejected: {e}"),
            }
        });
    }
}

struct HostCtx {
    engine: Arc<Engine>,
    conn: PeerConnection,
    control: SharedSend,
    quality: Mutex<QualitySettings>,
    video: Mutex<Option<VideoSource>>,
    /// Headless display created for this session (Ghost Mode without a monitor).
    virtual_display: Mutex<Option<aether_display::virtual_display::VirtualDisplay>>,
    input_tx: std::sync::mpsc::Sender<InputEvent>,
    peer_name: String,
    session_id: u64,
    clipboard_mode: tokio::sync::watch::Sender<ClipboardMode>,
    audio: Mutex<Option<crate::audio::HostAudio>>,
    limits: StreamLimits,
}

/// What a capability check refers to; the name is the suffix of the
/// `errors.permission.<name>` code sent to the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capability {
    Input,
    Clipboard,
    FileTransfer,
    Drives,
    SemanticUi,
    Audio,
    Terminal,
    PortForwarding,
    Usb,
    GhostMode,
    VirtualDisplay,
}

impl Capability {
    fn allowed(self, p: &SessionPermissions) -> bool {
        match self {
            Capability::Input => p.input,
            Capability::Clipboard => p.clipboard,
            Capability::FileTransfer => p.file_transfer,
            Capability::Drives => p.drives,
            // ⌘K presses buttons and fills fields: it is a form of input.
            Capability::SemanticUi => p.semantic_ui && p.input,
            Capability::Audio => p.audio,
            Capability::Terminal => p.terminal,
            Capability::PortForwarding => p.port_forwarding,
            Capability::Usb => p.usb,
            Capability::GhostMode => p.ghost_mode,
            Capability::VirtualDisplay => p.virtual_display,
        }
    }

    fn error_code(self) -> &'static str {
        match self {
            Capability::Input => "errors.permission.input",
            Capability::Clipboard => "errors.permission.clipboard",
            Capability::FileTransfer => "errors.permission.fileTransfer",
            Capability::Drives => "errors.permission.drives",
            Capability::SemanticUi => "errors.permission.semanticUi",
            Capability::Audio => "errors.permission.audio",
            Capability::Terminal => "errors.permission.terminal",
            Capability::PortForwarding => "errors.permission.portForwarding",
            Capability::Usb => "errors.permission.usb",
            Capability::GhostMode => "errors.permission.ghostMode",
            Capability::VirtualDisplay => "errors.permission.virtualDisplay",
        }
    }
}

/// Per-session caps on concurrently running services, so an authenticated peer cannot open
/// unbounded shells, tunnels, USB claims or file streams. (QUIC itself also caps open streams.)
struct StreamLimits {
    total: Arc<tokio::sync::Semaphore>,
    input: Arc<tokio::sync::Semaphore>,
    file_transfer: Arc<tokio::sync::Semaphore>,
    vfs: Arc<tokio::sync::Semaphore>,
    clipboard: Arc<tokio::sync::Semaphore>,
    dom: Arc<tokio::sync::Semaphore>,
    port_control: Arc<tokio::sync::Semaphore>,
    tunnel: Arc<tokio::sync::Semaphore>,
    terminal: Arc<tokio::sync::Semaphore>,
    usb: Arc<tokio::sync::Semaphore>,
}

impl StreamLimits {
    const TOTAL: usize = 96;

    fn new() -> Self {
        let s = |n| Arc::new(tokio::sync::Semaphore::new(n));
        Self {
            total: s(Self::TOTAL),
            input: s(2),
            file_transfer: s(4),
            vfs: s(8),
            clipboard: s(2),
            dom: s(2),
            port_control: s(2),
            tunnel: s(64),
            terminal: s(4),
            usb: s(4),
        }
    }

    /// A permit for one more service of `kind`, or `None` when the session is at its limit.
    fn acquire(&self, kind: &StreamKind) -> Option<(tokio::sync::OwnedSemaphorePermit, tokio::sync::OwnedSemaphorePermit)> {
        let class = match kind {
            StreamKind::Input => &self.input,
            StreamKind::FileTransfer { .. } => &self.file_transfer,
            StreamKind::Vfs => &self.vfs,
            StreamKind::Clipboard => &self.clipboard,
            StreamKind::Dom => &self.dom,
            StreamKind::PortControl => &self.port_control,
            StreamKind::PortTunnel { .. } => &self.tunnel,
            StreamKind::Terminal { .. } => &self.terminal,
            StreamKind::Usb { .. } => &self.usb,
            _ => &self.total,
        };
        let total = self.total.clone().try_acquire_owned().ok()?;
        let one = class.clone().try_acquire_owned().ok()?;
        Some((total, one))
    }
}

impl HostCtx {
    fn permissions(&self) -> SessionPermissions {
        *self.engine.permissions.borrow()
    }

    fn allowed(&self, cap: Capability) -> bool {
        cap.allowed(&self.permissions())
    }

    /// `true` if allowed; otherwise tells the controller why the request was refused.
    async fn check(&self, cap: Capability) -> bool {
        if self.allowed(cap) {
            return true;
        }
        tracing::info!(peer = %self.peer_name, ?cap, "request refused by session permissions");
        send_error(self, cap.error_code(), "").await;
        false
    }

    /// Resolves when `cap` is revoked (settings changed mid-session).
    async fn revoked(&self, cap: Capability) {
        let mut rx = self.engine.permissions.subscribe();
        loop {
            if !cap.allowed(&rx.borrow_and_update()) {
                return;
            }
            if rx.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }
}

pub(crate) async fn run_host_session(engine: Arc<Engine>, session: Session) {
    let Session { conn, control_tx, control_rx, remote_hello } = session;
    let session_id = engine.next_id();
    let info = HostSessionInfo {
        session_id,
        peer_name: remote_hello.device_name.clone(),
        device_id: remote_hello.device_id.to_string(),
        fingerprint: KeyFingerprint::of(&remote_hello.public_key).to_string(),
        os: format!("{:?}", remote_hello.os),
        since_ms: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
        remote_addr: conn.remote_address().to_string(),
        public_key: remote_hello.public_key,
        conn: Some(conn.clone()),
    };
    tracing::info!(peer = %info.peer_name, addr = %info.remote_addr, "host session started");
    engine.hosts.lock().insert(session_id, info);
    engine.emit_host_sessions();

    let platform = crate::external::platform_host();
    let mut auto_virtual = None;
    if platform.is_none()
        && engine.uses_native_capture()
        && aether_display::virtual_display::is_available()
        && aether_display::virtual_display::online_display_count() == 0
    {
        let spec = aether_display::virtual_display::VirtualDisplaySpec::FHD_16_9;
        match tokio::task::spawn_blocking(move || aether_display::virtual_display::VirtualDisplay::create("AetherLink", spec)).await {
            Ok(Ok(vd)) => {
                tracing::info!(id = vd.display_id(), "no physical display: created a headless virtual display");
                auto_virtual = Some(vd);
            }
            Ok(Err(e)) => tracing::warn!("headless host and no virtual display: {e}"),
            Err(e) => tracing::warn!("virtual display task failed: {e}"),
        }
    }
    let displays = match &platform {
        Some(p) => p.displays(),
        None => tokio::task::spawn_blocking(aether_capture::list_displays).await.ok().and_then(|r| r.ok()).unwrap_or_default(),
    };
    let windows = tokio::task::spawn_blocking(aether_capture::list_windows).await.ok().and_then(|r| r.ok()).unwrap_or_default();
    let active = displays.iter().find(|d| d.primary).or(displays.first()).map(|d| d.id).unwrap_or(0);

    let control = streams::shared(control_tx);
    let (input_tx, input_rx) = std::sync::mpsc::channel::<InputEvent>();
    spawn_input_thread(input_rx, displays.clone(), control.clone());
    let initial_clipboard =
        if engine.settings().permissions.clipboard { engine.settings().clipboard_mode } else { ClipboardMode::Disabled };
    let (cb_tx, _) = tokio::sync::watch::channel(initial_clipboard);
    let ctx = Arc::new(HostCtx {
        engine: engine.clone(),
        conn: conn.clone(),
        control: control.clone(),
        quality: Mutex::new(QualitySettings { max_fps: engine.settings().max_fps, ..QualitySettings::default() }),
        video: Mutex::new(None),
        virtual_display: Mutex::new(auto_virtual),
        input_tx,
        peer_name: remote_hello.device_name.clone(),
        session_id,
        clipboard_mode: cb_tx,
        audio: Mutex::new(None),
        limits: StreamLimits::new(),
    });

    let ready = ControlMessage::SessionReady { displays, windows, active: CaptureTarget::Display { display_id: active } };
    if control.lock().await.send(&ready).await.is_err()
        || control.lock().await.send(&ControlMessage::Permissions(ctx.permissions())).await.is_err()
    {
        finish(&ctx);
        return;
    }
    if engine.uses_native_capture() && !aether_capture::permission_granted() {
        send_error(&ctx, "errors.capturePermission", "Screen Recording permission is not granted on the host").await;
    }
    ctx.start_video(CaptureTarget::Display { display_id: active }, active).await;

    let datagrams = tokio::spawn(datagram_loop(ctx.clone()));
    let acceptor = tokio::spawn(stream_acceptor(ctx.clone()));
    let telemetry = tokio::spawn(telemetry_loop(ctx.clone()));
    let permissions = tokio::spawn(permissions_loop(ctx.clone()));

    control_loop(ctx.clone(), control_rx).await;

    datagrams.abort();
    acceptor.abort();
    telemetry.abort();
    permissions.abort();
    if let Some(a) = ctx.audio.lock().take() {
        tokio::task::spawn_blocking(move || a.stop());
    }
    if let Some(mut v) = ctx.video.lock().take() {
        tokio::task::spawn_blocking(move || v.stop());
    }
    conn.close("session ended");
    finish(&ctx);
}

fn finish(ctx: &HostCtx) {
    let engine = &ctx.engine;
    engine.hosts.lock().remove(&ctx.session_id);
    engine.emit_host_sessions();
    if engine.hosts.lock().is_empty() && engine.ghost_active.load(Ordering::Relaxed) {
        // Last controller left: lift the curtain and restore the backlight.
        let engine = engine.clone();
        tokio::spawn(async move {
            engine.set_ghost_mode(false).await;
        });
    }
    tracing::info!(peer = %ctx.peer_name, "host session ended");
}

async fn send_error(ctx: &HostCtx, code: &str, detail: &str) {
    let _ = ctx.control.lock().await.send(&ControlMessage::Error { code: code.into(), detail: detail.into() }).await;
}

fn spawn_input_thread(rx: std::sync::mpsc::Receiver<InputEvent>, displays: Vec<DisplayInfo>, control: SharedSend) {
    let rt = tokio::runtime::Handle::current();
    let _ = std::thread::Builder::new().name("aether-input".into()).spawn(move || {
        let mut injector = None;
        let mut warned = false;
        while let Ok(ev) = rx.recv() {
            if injector.is_none() {
                match aether_input::create_injector(displays.clone()) {
                    Ok(i) => injector = Some(i),
                    Err(e) => {
                        if !warned {
                            warned = true;
                            let msg = ControlMessage::Error { code: "errors.inputPermission".into(), detail: e.to_string() };
                            let control = control.clone();
                            rt.spawn(async move {
                                let _ = control.lock().await.send(&msg).await;
                            });
                        }
                        continue;
                    }
                }
            }
            if let Some(inj) = injector.as_mut()
                && let Err(e) = inj.inject(&ev) {
                    tracing::debug!("inject failed: {e}");
                }
        }
    });
}

impl HostCtx {
    /// Create (or remove) a headless virtual display and switch the stream to it.
    async fn set_virtual_display(self: &Arc<Self>, enabled: bool, width: u32, height: u32, refresh_hz: u32) {
        use aether_display::virtual_display::{VirtualDisplay, VirtualDisplaySpec};
        // Replacing the value detaches the previous virtual display (if any).
        *self.virtual_display.lock() = None;
        if enabled {
            let spec = VirtualDisplaySpec {
                width: if width == 0 { 1920 } else { width },
                height: if height == 0 { 1080 } else { height },
                refresh_hz: if refresh_hz == 0 { 60.0 } else { refresh_hz as f64 },
                // HiDPI up to 2560 points wide; larger (e.g. 32:9 5120×1440) at 1×.
                hidpi: width <= 2560,
                ppi: if width <= 2560 { 220.0 } else { 110.0 },
            };
            match tokio::task::spawn_blocking(move || VirtualDisplay::create("AetherLink", spec)).await {
                Ok(Ok(vd)) => {
                    let id = vd.display_id();
                    *self.virtual_display.lock() = Some(vd);
                    self.announce_displays().await;
                    self.start_video(CaptureTarget::Display { display_id: id }, id).await;
                }
                Ok(Err(e)) => send_error(self, "errors.virtualDisplayUnsupported", &e.to_string()).await,
                Err(e) => send_error(self, "errors.virtualDisplayUnsupported", &e.to_string()).await,
            }
        } else {
            let displays = self.announce_displays().await;
            if let Some(d) = displays.iter().find(|d| d.primary).or(displays.first()) {
                let id = d.id;
                self.start_video(CaptureTarget::Display { display_id: id }, id).await;
            }
        }
    }

    /// Re-enumerate displays (marking our virtual one) and tell the controller.
    async fn announce_displays(&self) -> Vec<DisplayInfo> {
        // Give the window server a moment to settle the new layout.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut displays = tokio::task::spawn_blocking(aether_capture::list_displays).await.ok().and_then(|r| r.ok()).unwrap_or_default();
        let vid = self.virtual_display.lock().as_ref().map(|v| v.display_id());
        for d in &mut displays {
            d.is_virtual = Some(d.id) == vid;
        }
        let _ = self.control.lock().await.send(&ControlMessage::DisplaysChanged { displays: displays.clone() }).await;
        displays
    }

    async fn start_video(self: &Arc<Self>, target: CaptureTarget, display_id: u32) {
        let previous = self.video.lock().take();
        if let Some(mut old) = previous {
            let _ = tokio::task::spawn_blocking(move || old.stop()).await;
        }
        let send = match self.conn.open_uni(StreamKind::Video { display_id }).await {
            Ok(s) => s,
            Err(e) => return tracing::warn!("cannot open video stream: {e}"),
        };
        let (tx, rx) = mpsc::channel::<VideoMessage>(8);
        if let Some(platform) = crate::external::platform_host() {
            tokio::spawn(video_writer(send, rx));
            match platform.start_video(display_id, tx) {
                Ok(s) => *self.video.lock() = Some(VideoSource::External(s)),
                Err(e) => send_error(self, "errors.captureUnavailable", &e.to_string()).await,
            }
            return;
        }
        let capturer = match self.engine.create_capturer() {
            Ok(c) => c,
            Err(e) => return send_error(self, "errors.captureUnavailable", &e.to_string()).await,
        };
        tokio::spawn(video_writer(send, rx));
        let quality = self.quality.lock().clone();
        // Exclude our own process so the Ghost Mode curtain never appears in the stream.
        let excluded = vec![std::process::id()];
        match HostVideoPipeline::start(capturer, target, display_id, quality, excluded, tx) {
            Ok(p) => *self.video.lock() = Some(VideoSource::Pipeline(p)),
            Err(e) => send_error(self, "errors.captureUnavailable", &e.to_string()).await,
        }
    }
}

/// Start / stop system-audio streaming (Opus over datagrams, see [`crate::audio`]).
async fn set_audio(ctx: &Arc<HostCtx>, enabled: bool) {
    if enabled == ctx.audio.lock().is_some() {
        return;
    }
    if !enabled {
        let running = ctx.audio.lock().take();
        if let Some(a) = running {
            let _ = tokio::task::spawn_blocking(move || a.stop()).await;
        }
        return;
    }
    let capturer = match ctx.engine.create_audio_capturer() {
        Ok(c) => c,
        Err(e) => return send_error(ctx, "errors.audioUnavailable", &e.to_string()).await,
    };
    let conn = ctx.conn.clone();
    match tokio::task::spawn_blocking(move || crate::audio::HostAudio::start(capturer, conn)).await {
        Ok(Ok(a)) => *ctx.audio.lock() = Some(a),
        Ok(Err((code, detail))) => send_error(ctx, code, &detail).await,
        Err(e) => send_error(ctx, "errors.audioUnavailable", &e.to_string()).await,
    }
}

async fn video_writer(mut send: FramedSend, mut rx: mpsc::Receiver<VideoMessage>) {
    send.set_priority(10);
    while let Some(m) = rx.recv().await {
        if send.send(&m).await.is_err() {
            break;
        }
    }
    let _ = send.finish();
}

async fn control_loop(ctx: Arc<HostCtx>, control_rx: FramedRecv) {
    let mut rx = streams::spawn_reader::<ControlMessage>(control_rx, 32);
    while let Some(msg) = rx.recv().await {
        match msg {
            ControlMessage::SelectTarget { target } => {
                let display_id = match &target {
                    CaptureTarget::Display { display_id } => *display_id,
                    CaptureTarget::Window { .. } => 0,
                };
                ctx.start_video(target, display_id).await;
            }
            ControlMessage::SetQuality(q) => {
                *ctx.quality.lock() = q.clone();
                if let Some(v) = ctx.video.lock().as_ref() {
                    v.command(PipelineCommand::SetQuality(q));
                }
            }
            ControlMessage::RequestKeyframe { .. } => {
                if let Some(v) = ctx.video.lock().as_ref() {
                    v.command(PipelineCommand::RequestKeyframe);
                }
            }
            ControlMessage::SetGhostMode { enabled } => {
                // Turning it off is always allowed.
                if enabled && !ctx.check(Capability::GhostMode).await {
                    continue;
                }
                let outcome = ctx.engine.set_ghost_mode(enabled).await;
                if enabled && !outcome.effective {
                    send_error(&ctx, "errors.ghostUnavailable", "no privacy curtain could be raised on the host").await;
                }
            }
            ControlMessage::SetVirtualDisplay { enabled, width, height, refresh_hz } => {
                if enabled && !ctx.check(Capability::VirtualDisplay).await {
                    continue;
                }
                ctx.set_virtual_display(enabled, width, height, refresh_hz).await;
            }
            ControlMessage::SetClipboardMode { mode } => {
                if mode != ClipboardMode::Disabled && !ctx.check(Capability::Clipboard).await {
                    continue;
                }
                let _ = ctx.clipboard_mode.send(mode);
            }
            ControlMessage::SetAudio { enabled } => {
                if enabled && !ctx.check(Capability::Audio).await {
                    continue;
                }
                set_audio(&ctx, enabled).await
            }
            ControlMessage::Ping { nonce, sent_us } => {
                let _ = ctx.control.lock().await.send(&ControlMessage::Pong { nonce, sent_us }).await;
            }
            ControlMessage::Bye => break,
            _ => {}
        }
    }
}

async fn datagram_loop(ctx: Arc<HostCtx>) {
    let mut last_seq = 0u64;
    while let Ok(d) = ctx.conn.recv_datagram().await {
        match d {
            Datagram::Input { seq, event } => {
                if !ctx.allowed(Capability::Input) {
                    continue;
                }
                // Datagrams can reorder; superseded pointer motion is simply dropped.
                if seq > last_seq || matches!(event, InputEvent::Scroll { .. }) {
                    last_seq = last_seq.max(seq);
                    let _ = ctx.input_tx.send(event);
                }
            }
            Datagram::Ping { nonce, sent_us } => {
                let _ = ctx.conn.send_datagram(&Datagram::Pong { nonce, sent_us });
            }
            _ => {}
        }
    }
}

async fn telemetry_loop(ctx: Arc<HostCtx>) {
    let mut monitor = HostMonitor::new();
    let epoch = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let s = monitor.sample();
        let stats = ctx.conn.stats();
        let sample = TelemetrySample {
            pts_us: epoch.elapsed().as_micros() as u64,
            rtt_ms: ctx.conn.rtt().as_secs_f32() * 1000.0,
            host_cpu_percent: s.cpu_percent,
            host_mem_percent: s.mem_percent,
            host_net_tx_kbps: s.net_tx_kbps,
            host_net_rx_kbps: s.net_rx_kbps,
            packet_loss: if stats.sent_packets == 0 { 0.0 } else { stats.lost_packets as f32 / stats.sent_packets as f32 },
            ..TelemetrySample::default()
        };
        if ctx.control.lock().await.send(&ControlMessage::Telemetry(sample)).await.is_err() {
            break;
        }
    }
}

/// Push permission changes to the controller and stop services that lost their permission.
async fn permissions_loop(ctx: Arc<HostCtx>) {
    let mut rx = ctx.engine.permissions.subscribe();
    while rx.changed().await.is_ok() {
        let p = *rx.borrow_and_update();
        if !p.clipboard {
            let _ = ctx.clipboard_mode.send(ClipboardMode::Disabled);
        }
        if !p.audio {
            set_audio(&ctx, false).await;
        }
        if !p.ghost_mode && ctx.engine.ghost_active.load(Ordering::Relaxed) {
            ctx.engine.set_ghost_mode(false).await;
        }
        if ctx.control.lock().await.send(&ControlMessage::Permissions(p)).await.is_err() {
            break;
        }
    }
}

/// The capability a service stream needs (`None`: not a service the peer may open).
fn stream_capability(kind: &StreamKind) -> Option<Capability> {
    Some(match kind {
        StreamKind::Input => Capability::Input,
        StreamKind::FileTransfer { .. } => Capability::FileTransfer,
        StreamKind::Vfs => Capability::Drives,
        StreamKind::Clipboard => Capability::Clipboard,
        StreamKind::Dom => Capability::SemanticUi,
        StreamKind::PortControl | StreamKind::PortTunnel { .. } => Capability::PortForwarding,
        StreamKind::Terminal { .. } => Capability::Terminal,
        StreamKind::Usb { .. } => Capability::Usb,
        _ => return None,
    })
}

async fn stream_acceptor(ctx: Arc<HostCtx>) {
    while let Ok(stream) = ctx.conn.accept_stream().await {
        let kind = stream.kind().clone();
        let Some(cap) = stream_capability(&kind) else {
            tracing::debug!("ignoring unexpected stream {kind:?}");
            continue;
        };
        if !ctx.check(cap).await {
            continue; // dropping the stream resets it
        }
        let Some(permits) = ctx.limits.acquire(&kind) else {
            tracing::warn!(peer = %ctx.peer_name, "too many concurrent {kind:?} streams; refusing");
            send_error(&ctx, "errors.tooManyStreams", "").await;
            continue;
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let _permits = permits;
            // Revoking the permission mid-session drops the service (closing its streams).
            tokio::select! {
                _ = serve_stream(&ctx, stream) => {}
                _ = ctx.revoked(cap) => tracing::info!(?cap, "service stopped: permission revoked"),
            }
        });
    }
}

async fn serve_stream(ctx: &Arc<HostCtx>, stream: IncomingStream) {
    match stream {
        IncomingStream::Uni { kind: StreamKind::Input, mut recv } => {
            while let Ok(Some(ev)) = recv.recv::<InputEvent>().await {
                if ctx.input_tx.send(ev).is_err() {
                    break;
                }
            }
        }
        IncomingStream::Uni { kind: StreamKind::FileTransfer { transfer_id }, mut recv } => {
            receive_files(ctx, transfer_id, &mut recv).await;
        }
        IncomingStream::Bi { kind: StreamKind::Vfs, send, recv } => {
            if let Err(e) = aether_vfs::serve(ctx.engine.vfs_host.clone(), recv, send).await {
                tracing::debug!("vfs stream ended: {e}");
            }
        }
        IncomingStream::Bi { kind: StreamKind::Clipboard, send, recv } => {
            let mode = *ctx.clipboard_mode.borrow();
            streams::clipboard_link(mode, ctx.clipboard_mode.subscribe(), send, recv, || {}).await;
        }
        IncomingStream::Bi { kind: StreamKind::Dom, send, recv } => dom_service(send, recv).await,
        IncomingStream::Bi { kind: StreamKind::PortControl, send, recv } => port_control(ctx, send, recv).await,
        IncomingStream::Bi { kind: StreamKind::PortTunnel { remote_port, .. }, send, recv } => {
            // Explicit allowlist only: an empty list tunnels nothing.
            let policy = tunnel::TunnelPolicy::only(ctx.engine.settings().tunnel_allowed_ports);
            if let Err(e) = tunnel::serve_tunnel(remote_port, send, recv, &policy).await {
                tracing::debug!("tunnel to :{remote_port} ended: {e}");
            }
        }
        IncomingStream::Bi { kind: StreamKind::Terminal { .. }, send, recv } => terminal_service(send, recv).await,
        IncomingStream::Bi { kind: StreamKind::Usb { .. }, send, recv } => usb_service(send, recv).await,
        other => tracing::debug!("ignoring unexpected stream {:?}", other.kind()),
    }
}

async fn receive_files(ctx: &Arc<HostCtx>, transfer_id: u64, recv: &mut FramedRecv) {
    let dest = ctx.engine.download_dir();
    let engine = ctx.engine.clone();
    let session_id = ctx.session_id;
    let progress = |p: &aether_vfs::transfer::TransferProgress| {
        events::emit(
            &*engine.events,
            names::TRANSFER_PROGRESS,
            &TransferProgressEvent {
                session_id,
                transfer_id,
                direction: "incoming",
                name: p.current.clone(),
                bytes_done: p.bytes_done,
                total_bytes: p.total_bytes,
                state: "running",
                error: None,
            },
        )
    };
    let res = aether_vfs::transfer::receive(recv, &dest, Default::default(), progress).await;
    let (state, error, bytes) = match res {
        Ok(s) => ("done", None, s.bytes),
        Err(e) => ("failed", Some(e.to_string()), 0),
    };
    events::emit(
        &*ctx.engine.events,
        names::TRANSFER_PROGRESS,
        &TransferProgressEvent { session_id, transfer_id, direction: "incoming", name: None, bytes_done: bytes, total_bytes: bytes, state, error },
    );
}

async fn port_control(ctx: &Arc<HostCtx>, mut send: FramedSend, mut recv: FramedRecv) {
    while let Ok(Some(msg)) = recv.recv::<PortTunnelMessage>().await {
        if msg == PortTunnelMessage::ScanRequest {
            let allowed = ctx.engine.settings().tunnel_allowed_ports;
            // Only advertise services the session may actually reach.
            let ports: Vec<_> = tokio::task::spawn_blocking(tunnel::scan_listening_ports)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|p| allowed.contains(&p.port))
                .collect();
            if send.send(&PortTunnelMessage::Policy { allowed_ports: allowed }).await.is_err()
                || send.send(&PortTunnelMessage::ScanResult { ports }).await.is_err()
            {
                break;
            }
        }
    }
}

/// Streams the focused window's accessibility tree as snapshots/diffs and runs actions.
async fn dom_service(mut send: FramedSend, recv: FramedRecv) {
    let created = tokio::task::spawn_blocking(aether_dom::create_provider)
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()));
    let provider = match created {
        Ok(p) => Arc::new(std::sync::Mutex::new(p)),
        Err(detail) => {
            // request_id 0 = unsolicited provider error (permission missing / unsupported OS).
            let _ = send.send(&DomMessage::ActionResult { request_id: 0, ok: false, error: Some(detail) }).await;
            return;
        }
    };
    let mut rx = streams::spawn_reader::<DomMessage>(recv, 16);
    let mut subscribed: Option<(u16, bool)> = None;
    let mut last: Option<DomSnapshot> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(400));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    DomMessage::Subscribe { max_depth, interactive_only } => { subscribed = Some((max_depth, interactive_only)); last = None; }
                    DomMessage::Unsubscribe => subscribed = None,
                    DomMessage::Resync => last = None,
                    DomMessage::PerformAction { request_id, node_id, action } => {
                        let p = provider.clone();
                        let r = tokio::task::spawn_blocking(move || p.lock().unwrap().perform(node_id, action)).await;
                        let (ok, error) = flatten(r);
                        if send.send(&DomMessage::ActionResult { request_id, ok, error }).await.is_err() { break; }
                    }
                    DomMessage::SetValue { request_id, node_id, value } => {
                        let p = provider.clone();
                        let r = tokio::task::spawn_blocking(move || p.lock().unwrap().set_value(node_id, &value)).await;
                        let (ok, error) = flatten(r);
                        if send.send(&DomMessage::ActionResult { request_id, ok, error }).await.is_err() { break; }
                    }
                    _ => {}
                }
            }
            _ = tick.tick(), if subscribed.is_some() => {
                let (depth, interactive) = subscribed.unwrap();
                let p = provider.clone();
                let Ok(Ok(snap)) = tokio::task::spawn_blocking(move || p.lock().unwrap().snapshot(depth, interactive)).await else { continue };
                let msg = match &last {
                    Some(old) if !aether_dom::needs_snapshot(old, &snap) => {
                        let d = aether_dom::diff(old, &snap);
                        if aether_dom::is_noop(&d) { last = Some(snap); continue; }
                        DomMessage::Diff(d)
                    }
                    _ => DomMessage::Snapshot(snap.clone()),
                };
                last = Some(snap);
                if send.send(&msg).await.is_err() { break; }
            }
        }
    }
}

fn flatten<E: std::fmt::Display>(r: Result<Result<(), E>, tokio::task::JoinError>) -> (bool, Option<String>) {
    match r {
        Ok(Ok(())) => (true, None),
        Ok(Err(e)) => (false, Some(e.to_string())),
        Err(e) => (false, Some(e.to_string())),
    }
}

/// Output chunks (≤ 16 KiB each) buffered between the PTY reader and the network.
const TERMINAL_OUTPUT_QUEUE: usize = 32;

/// Remote PTY in the working directory of the frontmost Finder/Explorer/IDE window.
async fn terminal_service(mut send: FramedSend, recv: FramedRecv) {
    let mut rx = streams::spawn_reader::<TerminalMessage>(recv, 64);
    let Some(TerminalMessage::Open { cols, rows, cwd, shell }) = rx.recv().await else { return };
    let cwd = match cwd {
        Some(c) => Some(c.into()),
        // Detection may ask Finder/Terminal via AppleScript, which blocks while an Automation
        // consent prompt is pending — never let that delay the terminal.
        None => tokio::time::timeout(
            Duration::from_millis(1500),
            tokio::task::spawn_blocking(aether_dom::context::active_working_directory),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten(),
    };
    // Bounded: when the network is slower than the shell, the PTY reader thread blocks on
    // `blocking_send`, the kernel PTY buffer fills and the shell itself is paused (backpressure)
    // instead of output piling up in memory.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(TERMINAL_OUTPUT_QUEUE);
    let pty = match PtySession::spawn(
        cols,
        rows,
        cwd,
        shell,
        move |b| {
            let _ = out_tx.blocking_send(b);
        },
        // The reader thread drops `out_tx` when it ends, which is how the loop below sees EOF.
        || {},
    ) {
        Ok(p) => p,
        Err(e) => {
            let _ = send.send(&TerminalMessage::Output(format!("\r\n{e}\r\n").into_bytes())).await;
            let _ = send.send(&TerminalMessage::Exit { code: None }).await;
            return;
        }
    };
    let opened = TerminalMessage::Opened { cwd: pty.cwd().display().to_string(), shell: pty.shell().to_owned() };
    if send.send(&opened).await.is_err() {
        return;
    }
    let pty = Arc::new(Mutex::new(pty));
    loop {
        tokio::select! {
            out = out_rx.recv() => match out {
                Some(bytes) => {
                    if send.send(&TerminalMessage::Output(bytes)).await.is_err() { break; }
                }
                // Reader thread done (PTY closed): everything it read has been forwarded.
                None => {
                    let code = pty.lock().try_exit_code();
                    let _ = send.send(&TerminalMessage::Exit { code }).await;
                    break;
                }
            },
            msg = rx.recv() => match msg {
                Some(TerminalMessage::Input(data)) => { let _ = pty.lock().write(&data); }
                Some(TerminalMessage::Resize { cols, rows }) => { let _ = pty.lock().resize(cols, rows); }
                Some(_) => {}
                None => break,
            }
        }
    }
    pty.lock().kill();
}

async fn usb_service(send: FramedSend, recv: FramedRecv) {
    match aether_usb::UsbHost::new() {
        Ok(host) => {
            if let Err(e) = aether_usb::remote::serve_usb_stream(Arc::new(host), recv, send).await {
                tracing::debug!("usb stream ended: {e}");
            }
        }
        Err(e) => tracing::warn!("USB redirection unavailable: {e}"),
    }
}
