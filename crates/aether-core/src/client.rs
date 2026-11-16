//! Client (controller) side of a session.

use crate::engine::Engine;
use crate::events::{self, ByteSink, TransferProgressEvent, names};
use crate::streams::{self, SharedSend};
use crate::ui_video::{self, UiVideoAdapter};
use crate::{CoreError, Result};
use aether_dom::{DomTree, SearchHit};
use aether_net::{FramedSend, IncomingStream, PeerConnection, Session, tunnel::TunnelHandle};
use aether_protocol::*;
use aether_timetravel::{Blackbox, BlackboxConfig};
use aether_vfs::{MountHandle, VfsClient};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// Pending DOM action: resolves with `(ok, error)` from the host's `ActionResult`.
type DomWaiter = oneshot::Sender<(bool, Option<String>)>;

/// Interval between synthetic DVR keyframes in tiles-only mode.
const TILE_SNAPSHOT_EVERY_US: u64 = 2_000_000;

struct VideoState {
    live: UiVideoAdapter,
    paused: bool,
    blackbox: Option<Blackbox>,
    last_config: Option<VideoMessage>,
    last_snapshot_us: u64,
    frames: u32,
    bytes: u64,
    window_start: Instant,
}

pub struct ClientSession {
    pub id: u64,
    pub peer: Hello,
    pub path: String,
    engine: Arc<Engine>,
    conn: PeerConnection,
    control: SharedSend,
    input: tokio::sync::Mutex<Option<FramedSend>>,
    input_seq: AtomicU64,
    frames: ByteSink,
    epoch: Instant,
    video: Mutex<VideoState>,
    displays: Mutex<Vec<DisplayInfo>>,
    quality: Mutex<QualitySettings>,
    dom: Mutex<DomTree>,
    dom_tx: tokio::sync::Mutex<Option<FramedSend>>,
    dom_pending: Mutex<HashMap<u64, DomWaiter>>,
    tunnels: Mutex<HashMap<u16, TunnelHandle>>,
    terminals: Mutex<HashMap<u64, mpsc::UnboundedSender<TerminalMessage>>>,
    vfs: tokio::sync::Mutex<Option<Arc<VfsClient>>>,
    mounts: tokio::sync::Mutex<HashMap<u32, MountHandle>>,
    clipboard_mode: tokio::sync::watch::Sender<ClipboardMode>,
    audio: crate::audio::ClientAudio,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardInfo {
    pub remote_port: u16,
    pub local_addr: String,
    pub bytes_up: u64,
    pub bytes_down: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInfo {
    pub terminal_id: u64,
    pub cwd: String,
    pub shell: String,
}

impl ClientSession {
    pub(crate) async fn start(engine: Arc<Engine>, id: u64, session: Session, frames: ByteSink, path: String) -> Arc<Self> {
        let Session { conn, control_tx, control_rx, remote_hello } = session;
        let settings = engine.settings();
        let blackbox = settings.dvr_enabled.then(|| {
            Blackbox::new(BlackboxConfig { window_us: settings.dvr_window_secs as u64 * 1_000_000, ..BlackboxConfig::default() })
        });
        let (cb_tx, _) = tokio::sync::watch::channel(settings.clipboard_mode);
        let me = Arc::new(Self {
            id,
            peer: remote_hello,
            path,
            engine: engine.clone(),
            conn: conn.clone(),
            control: streams::shared(control_tx),
            input: tokio::sync::Mutex::new(None),
            input_seq: AtomicU64::new(0),
            frames,
            epoch: Instant::now(),
            video: Mutex::new(VideoState {
                live: UiVideoAdapter::new(),
                paused: false,
                blackbox,
                last_config: None,
                last_snapshot_us: 0,
                frames: 0,
                bytes: 0,
                window_start: Instant::now(),
            }),
            displays: Mutex::new(Vec::new()),
            quality: Mutex::new(QualitySettings {
                max_fps: settings.max_fps,
                prefer_444: settings.prefer_444,
                lossless_text: settings.lossless_text,
                ..QualitySettings::default()
            }),
            dom: Mutex::new(DomTree::new()),
            dom_tx: tokio::sync::Mutex::new(None),
            dom_pending: Mutex::new(HashMap::new()),
            tunnels: Mutex::new(HashMap::new()),
            terminals: Mutex::new(HashMap::new()),
            vfs: tokio::sync::Mutex::new(None),
            mounts: tokio::sync::Mutex::new(HashMap::new()),
            clipboard_mode: cb_tx,
            audio: crate::audio::ClientAudio::new(engine.audio_null_output()),
            tasks: Mutex::new(Vec::new()),
        });

        let q = me.quality.lock().clone();
        let _ = me.control.lock().await.send(&ControlMessage::SetQuality(q)).await;
        if let Ok(s) = me.conn.open_uni(StreamKind::Input).await {
            *me.input.lock().await = Some(s);
        }
        let tasks = vec![
            tokio::spawn(me.clone().control_loop(control_rx)),
            tokio::spawn(me.clone().stream_loop()),
            tokio::spawn(me.clone().datagram_loop()),
            tokio::spawn(me.clone().clipboard()),
            tokio::spawn(me.clone().ping_loop()),
        ];
        me.tasks.lock().extend(tasks);
        me
    }

    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    fn emit<T: Serialize>(&self, name: &str, payload: &T) {
        events::emit(&*self.engine.events, name, payload);
    }

    // ------------------------------------------------------------------ loops

    async fn control_loop(self: Arc<Self>, rx: aether_net::FramedRecv) {
        let mut rx = streams::spawn_reader::<ControlMessage>(rx, 32);
        while let Some(msg) = rx.recv().await {
            match msg {
                ControlMessage::SessionReady { displays, windows, active } => {
                    *self.displays.lock() = displays.clone();
                    self.emit(
                        names::SESSION_DISPLAYS,
                        &serde_json::json!({ "sessionId": self.id, "displays": displays, "windows": windows, "active": active }),
                    );
                }
                ControlMessage::DisplaysChanged { displays } => {
                    *self.displays.lock() = displays.clone();
                    self.emit(names::SESSION_DISPLAYS, &serde_json::json!({ "sessionId": self.id, "displays": displays }));
                }
                ControlMessage::Telemetry(mut t) => {
                    let mut v = self.video.lock();
                    let secs = v.window_start.elapsed().as_secs_f32().max(0.001);
                    t.fps = v.frames as f32 / secs;
                    t.bitrate_kbps = (v.bytes as f32 * 8.0 / 1000.0 / secs) as u32;
                    t.rtt_ms = self.conn.rtt().as_secs_f32() * 1000.0;
                    v.frames = 0;
                    v.bytes = 0;
                    v.window_start = Instant::now();
                    t.pts_us = self.now_us();
                    if let Some(bb) = v.blackbox.as_mut() {
                        bb.push_telemetry(t);
                    }
                    drop(v);
                    let audio = self.audio.is_enabled().then(|| self.audio.stats());
                    self.emit(names::SESSION_TELEMETRY, &serde_json::json!({ "sessionId": self.id, "telemetry": t, "audio": audio }));
                }
                ControlMessage::Permissions(p) => {
                    self.emit(names::SESSION_PERMISSIONS, &serde_json::json!({ "sessionId": self.id, "permissions": p }));
                }
                ControlMessage::Error { code, detail } => {
                    self.emit(
                        names::SESSION_STATE,
                        &serde_json::json!({ "sessionId": self.id, "state": "connected", "errorCode": code, "detail": detail }),
                    );
                }
                ControlMessage::Bye => break,
                _ => {}
            }
        }
        self.closed(None);
    }

    async fn stream_loop(self: Arc<Self>) {
        while let Ok(s) = self.conn.accept_stream().await {
            match s {
                IncomingStream::Uni { kind: StreamKind::Video { .. }, mut recv } => {
                    let me = self.clone();
                    tokio::spawn(async move {
                        while let Ok(Some(msg)) = recv.recv::<VideoMessage>().await {
                            me.on_video(msg);
                        }
                    });
                }
                IncomingStream::Uni { kind: StreamKind::Audio, mut recv } => {
                    // Fallback path for audio packets too large for a datagram.
                    let me = self.clone();
                    tokio::spawn(async move {
                        while let Ok(Some(p)) = recv.recv::<AudioPacket>().await {
                            me.audio.on_packet(p);
                        }
                    });
                }
                other => tracing::debug!("client ignoring stream {:?}", other.kind()),
            }
        }
    }

    async fn datagram_loop(self: Arc<Self>) {
        while let Ok(d) = self.conn.recv_datagram().await {
            match d {
                Datagram::Cursor(c) if !self.video.lock().paused => (self.frames)(ui_video::cursor_message(&c)),
                Datagram::Audio(p) => self.audio.on_packet(p),
                _ => {}
            }
        }
    }

    async fn ping_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let mut nonce = 0;
        loop {
            tick.tick().await;
            nonce += 1;
            let _ = self.conn.send_datagram(&Datagram::Ping { nonce, sent_us: self.now_us() });
        }
    }

    async fn clipboard(self: Arc<Self>) {
        let Ok((send, recv)) = self.conn.open_bi(StreamKind::Clipboard).await else { return };
        let mode = *self.clipboard_mode.borrow();
        let me = self.clone();
        streams::clipboard_link(mode, self.clipboard_mode.subscribe(), send, recv, move || {
            me.emit(names::CLIPBOARD_SYNCED, &serde_json::json!({ "sessionId": me.id }));
        })
        .await;
    }

    fn on_video(&self, msg: VideoMessage) {
        let now = self.now_us();
        let mut guard = self.video.lock();
        let v = &mut *guard;
        v.frames += matches!(msg, VideoMessage::Frame(_) | VideoMessage::Tiles(_)) as u32;
        v.bytes += match &msg {
            VideoMessage::Frame(f) => f.data.len() as u64,
            VideoMessage::Tiles(b) => b.tiles.iter().map(|t| t.data.len() as u64).sum(),
            _ => 0,
        };
        if matches!(msg, VideoMessage::Config { .. }) {
            v.last_config = Some(msg.clone());
        }
        let ui = v.live.convert(&msg);
        // DVR: video keyframes are natural seek points; tiles-only streams get periodic
        // synthetic full-frame snapshots.
        if let Some(bb) = v.blackbox.as_mut() {
            let keyframe = matches!(&msg, VideoMessage::Frame(f) if f.keyframe);
            if !matches!(msg, VideoMessage::Config { .. } | VideoMessage::Cursor(_) | VideoMessage::CursorShape(_))
                && let Ok(bytes) = encode_message(&msg) {
                    bb.push_frame(now, keyframe, &bytes);
                }
            if v.live.is_tiles_mode() && now.saturating_sub(v.last_snapshot_us) >= TILE_SNAPSHOT_EVERY_US
                && let Some(snap) = v.live.snapshot_keyframe(0, now)
                    && let Ok(bytes) = encode_message(&snap) {
                        bb.push_frame(now, true, &bytes);
                        v.last_snapshot_us = now;
                    }
        }
        if !v.paused {
            drop(guard);
            for m in ui {
                (self.frames)(m);
            }
        }
    }

    fn closed(&self, error: Option<String>) {
        self.audio.stop();
        if let Some(c) = self.engine.clients.lock().remove(&self.id) {
            let _ = c; // dropping our registry entry
            self.emit(
                names::SESSION_STATE,
                &serde_json::json!({ "sessionId": self.id, "state": "closed", "errorCode": error.as_ref().map(|_| "errors.network"), "detail": error }),
            );
        }
    }

    /// Tear the session down.
    pub async fn disconnect(&self) {
        let _ = self.control.lock().await.send(&ControlMessage::Bye).await;
        for (_, m) in self.mounts.lock().await.drain() {
            let _ = m.unmount().await;
        }
        self.tunnels.lock().clear();
        self.conn.close("disconnect");
        for t in self.tasks.lock().drain(..) {
            t.abort();
        }
        self.closed(None);
    }

    // ------------------------------------------------------------------ input & control

    pub async fn send_input(&self, event: InputEvent) -> Result<()> {
        let unreliable = matches!(
            event,
            InputEvent::MouseMove { .. } | InputEvent::MouseMoveRelative { .. } | InputEvent::Scroll { .. }
        );
        if unreliable {
            let seq = self.input_seq.fetch_add(1, Ordering::Relaxed) + 1;
            if self.conn.send_datagram(&Datagram::Input { seq, event: event.clone() }).is_ok() {
                return Ok(());
            }
        }
        let mut guard = self.input.lock().await;
        let s = guard.as_mut().ok_or_else(|| CoreError::Net("input stream closed".into()))?;
        s.send(&event).await.map_err(|e| CoreError::Net(e.to_string()))
    }

    async fn control(&self, msg: ControlMessage) -> Result<()> {
        self.control.lock().await.send(&msg).await.map_err(|e| CoreError::Net(e.to_string()))
    }

    pub async fn select_display(&self, display_id: u32) -> Result<()> {
        self.control(ControlMessage::SelectTarget { target: CaptureTarget::Display { display_id } }).await
    }

    pub async fn select_window(&self, window_id: u64) -> Result<()> {
        self.control(ControlMessage::SelectTarget { target: CaptureTarget::Window { window_id } }).await
    }

    pub async fn set_quality(&self, f: impl FnOnce(&mut QualitySettings)) -> Result<()> {
        let q = {
            let mut q = self.quality.lock();
            f(&mut q);
            q.clone()
        };
        self.control(ControlMessage::SetQuality(q)).await
    }

    pub async fn set_ghost_mode(&self, enabled: bool) -> Result<()> {
        self.control(ControlMessage::SetGhostMode { enabled }).await
    }

    /// Ask the host for a headless virtual display (streamed as soon as it exists).
    pub async fn set_virtual_display(&self, enabled: bool, width: u32, height: u32, refresh_hz: u32) -> Result<()> {
        self.control(ControlMessage::SetVirtualDisplay { enabled, width, height, refresh_hz }).await
    }

    /// Start/stop the host's audio stream and local playback.
    pub async fn set_audio(&self, enabled: bool) -> Result<()> {
        if !enabled {
            self.audio.stop();
            return self.control(ControlMessage::SetAudio { enabled }).await;
        }
        self.audio.start()?;
        if let Err(e) = self.control(ControlMessage::SetAudio { enabled }).await {
            self.audio.stop();
            return Err(e);
        }
        Ok(())
    }

    /// Local playback gain (0–1) and mute; the host keeps streaming.
    pub fn set_audio_volume(&self, volume: f32, muted: Option<bool>) {
        self.audio.set_volume(volume, muted);
    }

    pub fn set_audio_muted(&self, muted: bool) {
        self.audio.set_muted(muted);
    }

    /// Output device for this session (`None` = system default); see [`crate::audio::list_audio_outputs`].
    pub fn set_audio_output(&self, device_id: Option<String>) -> Result<()> {
        self.audio.set_output(device_id)
    }

    pub fn audio_stats(&self) -> crate::audio::ClientAudioStats {
        self.audio.stats()
    }

    pub async fn set_clipboard_mode(&self, mode: ClipboardMode) -> Result<()> {
        let _ = self.clipboard_mode.send(mode);
        self.control(ControlMessage::SetClipboardMode { mode }).await
    }

    pub fn displays(&self) -> Vec<DisplayInfo> {
        self.displays.lock().clone()
    }

    // ------------------------------------------------------------------ semantic DOM

    pub async fn dom_subscribe(self: &Arc<Self>) -> Result<()> {
        let mut tx = self.dom_tx.lock().await;
        if tx.is_some() {
            return Ok(());
        }
        let (mut send, recv) = self.conn.open_bi(StreamKind::Dom).await.map_err(net)?;
        send.send(&DomMessage::Subscribe { max_depth: 48, interactive_only: false }).await.map_err(net)?;
        *tx = Some(send);
        let me = self.clone();
        let mut rx = streams::spawn_reader::<DomMessage>(recv, 32);
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                match m {
                    DomMessage::Snapshot(s) => {
                        me.dom.lock().apply_snapshot(s);
                    }
                    DomMessage::Diff(d) => {
                        let res = me.dom.lock().apply_diff(&d);
                        if let Err(e) = res {
                            if e.needs_resync()
                                && let Some(tx) = me.dom_tx.lock().await.as_mut() {
                                    let _ = tx.send(&DomMessage::Resync).await;
                                }
                            continue;
                        }
                    }
                    DomMessage::ActionResult { request_id, ok, error } => {
                        if let Some(waiter) = me.dom_pending.lock().remove(&request_id) {
                            let _ = waiter.send((ok, error));
                        } else if !ok {
                            me.emit(names::DOM_UPDATED, &serde_json::json!({ "sessionId": me.id, "error": error }));
                        }
                        continue;
                    }
                    _ => continue,
                }
                let (rev, app, title) = {
                    let t = me.dom.lock();
                    let s = t.to_snapshot();
                    (t.revision(), s.app_name, s.window_title)
                };
                me.emit(
                    names::DOM_UPDATED,
                    &serde_json::json!({ "sessionId": me.id, "revision": rev, "appName": app, "windowTitle": title }),
                );
            }
            *me.dom_tx.lock().await = None;
        });
        Ok(())
    }

    pub fn dom_search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        self.dom.lock().search(query, limit)
    }

    async fn dom_request(&self, build: impl FnOnce(u64) -> DomMessage) -> Result<()> {
        let request_id = self.engine.next_id();
        let (tx, rx) = oneshot::channel();
        self.dom_pending.lock().insert(request_id, tx);
        {
            let mut guard = self.dom_tx.lock().await;
            let s = guard.as_mut().ok_or_else(|| CoreError::Invalid("dom not subscribed".into()))?;
            s.send(&build(request_id)).await.map_err(net)?;
        }
        match tokio::time::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok((true, _))) => Ok(()),
            Ok(Ok((false, e))) => Err(CoreError::Other(e.unwrap_or_else(|| "action failed".into()))),
            _ => {
                self.dom_pending.lock().remove(&request_id);
                Err(CoreError::Net("dom action timed out".into()))
            }
        }
    }

    pub async fn dom_action(&self, node_id: u64, action: DomAction) -> Result<()> {
        self.dom_request(|request_id| DomMessage::PerformAction { request_id, node_id, action }).await
    }

    pub async fn dom_set_value(&self, node_id: u64, value: String) -> Result<()> {
        self.dom_request(|request_id| DomMessage::SetValue { request_id, node_id, value }).await
    }

    // ------------------------------------------------------------------ port tunnels

    pub async fn ports_scan(&self) -> Result<Vec<ListeningPort>> {
        let (mut send, mut recv) = self.conn.open_bi(StreamKind::PortControl).await.map_err(net)?;
        send.send(&PortTunnelMessage::ScanRequest).await.map_err(net)?;
        let fut = async {
            loop {
                match recv.recv::<PortTunnelMessage>().await {
                    Ok(Some(PortTunnelMessage::ScanResult { ports })) => return Ok(ports),
                    Ok(Some(_)) => continue,
                    Ok(None) => return Err(CoreError::Net("port scan stream closed".into())),
                    Err(e) => return Err(net(e)),
                }
            }
        };
        let res = tokio::time::timeout(Duration::from_secs(10), fut).await.map_err(|_| CoreError::Net("port scan timed out".into()))?;
        let _ = send.finish();
        res
    }

    /// Bind `127.0.0.1:<local_port>` (same as remote by default; falls back to an ephemeral port).
    pub async fn port_forward(&self, remote_port: u16, local_port: Option<u16>) -> Result<ForwardInfo> {
        if let Some(h) = self.tunnels.lock().get(&remote_port) {
            return Ok(forward_info(h));
        }
        let want = local_port.unwrap_or(remote_port);
        let bind: SocketAddr = ([127, 0, 0, 1], want).into();
        let handle = match aether_net::tunnel::forward_local(self.conn.clone(), bind, remote_port).await {
            Ok(h) => h,
            Err(_) if local_port.is_none() => {
                aether_net::tunnel::forward_local(self.conn.clone(), ([127, 0, 0, 1], 0).into(), remote_port).await.map_err(net)?
            }
            Err(e) => return Err(net(e)),
        };
        let info = forward_info(&handle);
        self.tunnels.lock().insert(remote_port, handle);
        Ok(info)
    }

    pub fn port_unforward(&self, remote_port: u16) {
        if let Some(h) = self.tunnels.lock().remove(&remote_port) {
            h.stop();
        }
    }

    pub fn forwards(&self) -> Vec<ForwardInfo> {
        self.tunnels.lock().values().map(forward_info).collect()
    }

    // ------------------------------------------------------------------ terminal

    pub async fn terminal_open(&self, cols: u16, rows: u16, output: ByteSink) -> Result<TerminalInfo> {
        let terminal_id = self.engine.next_id();
        let (mut send, recv) = self.conn.open_bi(StreamKind::Terminal { terminal_id }).await.map_err(net)?;
        send.send(&TerminalMessage::Open { cols, rows, cwd: None, shell: None }).await.map_err(net)?;
        let mut rx = streams::spawn_reader::<TerminalMessage>(recv, 256);
        let (cwd, shell) = match tokio::time::timeout(Duration::from_secs(15), rx.recv()).await {
            Ok(Some(TerminalMessage::Opened { cwd, shell })) => (cwd, shell),
            Ok(Some(TerminalMessage::Output(b))) => {
                return Err(CoreError::Terminal(String::from_utf8_lossy(&b).trim().to_owned()));
            }
            _ => return Err(CoreError::Terminal("remote terminal did not start".into())),
        };
        let (tx, mut wrx) = mpsc::unbounded_channel::<TerminalMessage>();
        self.terminals.lock().insert(terminal_id, tx);
        tokio::spawn(async move {
            while let Some(m) = wrx.recv().await {
                if send.send(&m).await.is_err() {
                    break;
                }
            }
            let _ = send.finish();
        });
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                match m {
                    TerminalMessage::Output(b) => output(b),
                    TerminalMessage::Exit { .. } => break,
                    _ => {}
                }
            }
            output(Vec::new()); // empty chunk = terminal closed
        });
        Ok(TerminalInfo { terminal_id, cwd, shell })
    }

    fn terminal_send(&self, id: u64, m: TerminalMessage) -> Result<()> {
        let t = self.terminals.lock();
        let tx = t.get(&id).ok_or_else(|| CoreError::Invalid("unknown terminal".into()))?;
        tx.send(m).map_err(|_| CoreError::Terminal("terminal closed".into()))
    }

    pub fn terminal_input(&self, id: u64, data: Vec<u8>) -> Result<()> {
        self.terminal_send(id, TerminalMessage::Input(data))
    }

    pub fn terminal_resize(&self, id: u64, cols: u16, rows: u16) -> Result<()> {
        self.terminal_send(id, TerminalMessage::Resize { cols, rows })
    }

    pub fn terminal_close(&self, id: u64) {
        self.terminals.lock().remove(&id);
    }

    // ------------------------------------------------------------------ VFS

    pub(crate) async fn vfs_client(&self) -> Result<Arc<VfsClient>> {
        let mut guard = self.vfs.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let (send, recv) = self.conn.open_bi(StreamKind::Vfs).await.map_err(net)?;
        let cache_dir = self.engine.cache_dir().join("vfs").join(KeyFingerprint::of(&self.peer.public_key).to_string());
        let cache = aether_vfs::ChunkCache::open(cache_dir, 2 * 1024 * 1024 * 1024).ok();
        let c = Arc::new(VfsClient::with_cache(recv, send, cache));
        *guard = Some(c.clone());
        Ok(c)
    }

    pub async fn vfs_list_shares(&self) -> Result<Vec<ShareInfo>> {
        self.vfs_client().await?.list_shares().await.map_err(|e| CoreError::Other(e.to_string()))
    }

    pub async fn vfs_mount(&self, share: u32) -> Result<PathBuf> {
        if let Some(m) = self.mounts.lock().await.get(&share) {
            return Ok(m.mount_point().to_path_buf());
        }
        let client = self.vfs_client().await?;
        let shares = client.list_shares().await.map_err(|e| CoreError::Other(e.to_string()))?;
        let name = shares.iter().find(|s| s.id == share).map(|s| s.name.clone()).unwrap_or_else(|| format!("share-{share}"));
        let label = format!("{} - {}", self.peer.device_name, name);
        let mp = match self.engine.settings().mount_root {
            Some(root) => root.join(&label),
            None => aether_vfs::default_mount_point(&label),
        };
        let handle = aether_vfs::mount(client, share, &mp).await.map_err(|e| CoreError::Other(e.to_string()))?;
        let path = handle.mount_point().to_path_buf();
        self.mounts.lock().await.insert(share, handle);
        Ok(path)
    }

    pub async fn vfs_unmount(&self, share: u32) -> Result<()> {
        if let Some(m) = self.mounts.lock().await.remove(&share) {
            m.unmount().await.map_err(|e| CoreError::Other(e.to_string()))?;
        }
        Ok(())
    }

    pub async fn mounts(&self) -> Vec<(u32, PathBuf)> {
        self.mounts.lock().await.iter().map(|(k, v)| (*k, v.mount_point().to_path_buf())).collect()
    }

    // ------------------------------------------------------------------ file transfer

    pub async fn send_files(self: &Arc<Self>, paths: Vec<PathBuf>) -> Result<u64> {
        let transfer_id = self.engine.next_id();
        let mut send = self.conn.open_uni(StreamKind::FileTransfer { transfer_id }).await.map_err(net)?;
        let me = self.clone();
        tokio::spawn(async move {
            let id = me.id;
            let progress = |p: &aether_vfs::transfer::TransferProgress| {
                me.emit(
                    names::TRANSFER_PROGRESS,
                    &TransferProgressEvent {
                        session_id: id,
                        transfer_id,
                        direction: "outgoing",
                        name: p.current.clone(),
                        bytes_done: p.bytes_done,
                        total_bytes: p.total_bytes,
                        state: "running",
                        error: None,
                    },
                )
            };
            let opts = aether_vfs::transfer::SendOptions { transfer_id, ..Default::default() };
            let res = aether_vfs::transfer::send_paths(&paths, &mut send, opts, progress).await;
            let _ = send.finish();
            let (state, error, bytes) = match res {
                Ok(b) => ("done", None, b),
                Err(e) => ("failed", Some(e.to_string()), 0),
            };
            me.emit(
                names::TRANSFER_PROGRESS,
                &TransferProgressEvent { session_id: id, transfer_id, direction: "outgoing", name: None, bytes_done: bytes, total_bytes: bytes, state, error },
            );
        });
        Ok(transfer_id)
    }

    // ------------------------------------------------------------------ DVR

    pub fn dvr_index(&self) -> Option<TimeTravelIndex> {
        self.video.lock().blackbox.as_ref().map(|b| b.index())
    }

    /// Pause live view and render the state at `pts_us` from the encrypted ring.
    pub fn dvr_seek(&self, pts_us: u64) -> Result<()> {
        let mut v = self.video.lock();
        let bb = v.blackbox.as_ref().ok_or(CoreError::Unsupported("DVR disabled in settings"))?;
        let plan = bb.seek(pts_us).map_err(|e| CoreError::Other(e.to_string()))?;
        let mut replay = UiVideoAdapter::new();
        let mut out = vec![vec![ui_video::RESET]];
        if v.live.is_tiles_mode()
            && let Some(cfg) = &v.last_config {
                out.extend(replay.convert(cfg));
            }
        for f in &plan.frames {
            if let Ok(msg) = decode_message::<VideoMessage>(&f.data) {
                out.extend(replay.convert(&msg));
            }
        }
        v.paused = true;
        drop(v);
        for m in out {
            (self.frames)(m);
        }
        Ok(())
    }

    /// Return to the live stream.
    pub async fn dvr_live(&self) -> Result<()> {
        let (msgs, display) = {
            let mut v = self.video.lock();
            v.paused = false;
            (v.live.repaint(), self.displays.lock().first().map(|d| d.id).unwrap_or(0))
        };
        for m in msgs {
            (self.frames)(m);
        }
        self.control(ControlMessage::RequestKeyframe { display_id: display }).await
    }

    /// Reconstruct the picture at `pts_us` (latest when `None`) from the DVR ring and return
    /// it as PNG — hardware-decoded for video streams, the lossless framebuffer for tiles.
    pub fn dvr_frame_png(&self, pts_us: Option<u64>) -> Result<Vec<u8>> {
        let plan = {
            let v = self.video.lock();
            let bb = v.blackbox.as_ref().ok_or(CoreError::Unsupported("DVR disabled in settings"))?;
            let pts = pts_us.unwrap_or_else(|| bb.index().end_us);
            bb.seek(pts).map_err(|e| CoreError::Other(e.to_string()))?
        };
        let mut fb = aether_codec::Framebuffer::new();
        let mut decoder: Option<Box<dyn aether_codec::VideoDecoder>> = None;
        let mut picture: Option<aether_capture::BgraImage> = None;
        for f in &plan.frames {
            match decode_message::<VideoMessage>(&f.data) {
                Ok(VideoMessage::Frame(vf)) => {
                    if decoder.is_none() {
                        decoder = aether_codec::create_decoder(vf.codec).ok();
                    }
                    if let Some(Ok(Some(img))) = decoder.as_mut().map(|d| d.decode(&vf)) {
                        picture = Some(img);
                    }
                }
                Ok(VideoMessage::Tiles(b)) if !b.refinement
                    && fb.apply(&b).is_ok() => {
                        picture = Some(fb.to_image());
                    }
                _ => {}
            }
        }
        let img = picture.ok_or_else(|| CoreError::Other("no decodable frame at this time".into()))?;
        let mut rgba = Vec::with_capacity(img.width as usize * img.height as usize * 4);
        for y in 0..img.height {
            for px in img.row(y)[..img.width as usize * 4].as_chunks::<4>().0 {
                rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
        crate::clipboard::transcode::rgba_to_png(img.width, img.height, rgba)
    }

    /// OCR of the frame at `pts_us` (on-device Vision on macOS).
    pub async fn dvr_ocr(self: &Arc<Self>, pts_us: Option<u64>) -> Result<String> {
        let me = self.clone();
        tokio::task::spawn_blocking(move || {
            let png = me.dvr_frame_png(pts_us)?;
            let lines = crate::ocr::recognize(&png, &["it-IT", "en-US"])?;
            Ok(crate::ocr::to_text(&lines))
        })
        .await
        .map_err(|e| CoreError::Other(e.to_string()))?
    }

    pub fn dvr_marks(&self) -> Vec<TimelineMark> {
        self.video.lock().blackbox.as_ref().map(|b| b.index().marks).unwrap_or_default()
    }
}

fn forward_info(h: &TunnelHandle) -> ForwardInfo {
    ForwardInfo { remote_port: h.remote_port(), local_addr: h.local_addr().to_string(), bytes_up: h.bytes_up(), bytes_down: h.bytes_down() }
}

fn net(e: impl std::fmt::Display) -> CoreError {
    CoreError::Net(e.to_string())
}
