//! The engine facade owned by the app.

use crate::client::ClientSession;
use crate::events::{self, ByteSink, EventSink, names};
use crate::host::{self, HostSessionInfo};
use crate::password::RotatingPassword;
use crate::settings::{self, Settings};
use crate::{CoreError, Result};
use aether_net::discovery::{LanAdvertisement, LanAdvertiser, LanBrowser, LanEvent, LanPeer};
use aether_net::rendezvous::RendezvousClient;
use aether_net::{AetherEndpoint, ClientAuth, ConnectOptions, Identity, Session, TrustStore, TrustedPeer};
use aether_protocol::*;
use aether_vfs::VfsHost;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;

const KEYCHAIN_SERVICE: &str = "dev.aetherlink.identity";

/// Produces the host's screen capturer (overridable for test-pattern / demo sources).
pub type CapturerFactory = Arc<dyn Fn() -> aether_capture::Result<Box<dyn aether_capture::ScreenCapturer>> + Send + Sync>;

#[derive(Clone)]
pub struct EngineConfig {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// QUIC bind address; falls back to an ephemeral port if taken.
    pub bind: SocketAddr,
    /// Where incoming drag-and-drop files land (default: the OS Downloads folder).
    pub download_dir: Option<PathBuf>,
    /// Screen source for hosted sessions (default: the native OS capturer).
    pub capturer: Option<CapturerFactory>,
    /// System-audio source for hosted sessions (default: the native OS loopback capturer).
    pub audio_capturer: Option<crate::audio::AudioCapturerFactory>,
    /// Client sessions decode audio on a timer and discard it instead of opening an output
    /// device (tests / headless).
    pub audio_null_output: bool,
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineConfig")
            .field("config_dir", &self.config_dir)
            .field("cache_dir", &self.cache_dir)
            .field("bind", &self.bind)
            .field("download_dir", &self.download_dir)
            .field("custom_capturer", &self.capturer.is_some())
            .field("custom_audio_capturer", &self.audio_capturer.is_some())
            .field("audio_null_output", &self.audio_null_output)
            .finish()
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            config_dir: settings::config_dir(),
            cache_dir: settings::cache_dir(),
            bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)),
            download_dir: None,
            capturer: None,
            audio_capturer: None,
            audio_null_output: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityInfo {
    pub device_id: String,
    pub device_name: String,
    pub fingerprint: String,
    pub os: String,
    pub password: String,
    pub password_expires_at: u64,
    pub qr_payload: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanPeerInfo {
    pub device_id: String,
    pub name: String,
    pub os: String,
    pub addrs: Vec<String>,
    pub fingerprint: String,
    pub trusted: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedDeviceInfo {
    pub device_id: String,
    pub name: String,
    pub fingerprint: String,
    pub added_at: u64,
    pub online: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LocalShare {
    pub id: u32,
    pub name: String,
    pub path: PathBuf,
    pub read_only: bool,
}

pub struct Engine {
    cfg: EngineConfig,
    pub(crate) identity: Identity,
    settings: RwLock<Settings>,
    pub(crate) trust: Mutex<TrustStore>,
    pub(crate) password: Mutex<RotatingPassword>,
    pub(crate) endpoint: AetherEndpoint,
    pub(crate) events: Arc<dyn EventSink>,
    pub(crate) vfs_host: Arc<VfsHost>,
    shares: Mutex<Vec<LocalShare>>,
    lan_peers: Mutex<BTreeMap<DeviceId, LanPeer>>,
    rendezvous: Mutex<Option<Arc<RendezvousClient>>>,
    advertiser: Mutex<Option<LanAdvertiser>>,
    pub(crate) clients: Mutex<HashMap<u64, Arc<ClientSession>>>,
    pub(crate) hosts: Mutex<HashMap<u64, HostSessionInfo>>,
    pub(crate) pending_consent: Mutex<HashMap<u64, (Hello, oneshot::Sender<bool>)>>,
    pub(crate) ghost_active: AtomicBool,
    next_id: AtomicU64,
}

impl Engine {
    /// Load identity/settings/trust, bind the QUIC endpoint and start hosting + discovery.
    /// Must be called inside a Tokio runtime.
    pub async fn start(cfg: EngineConfig, events: Arc<dyn EventSink>) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.config_dir)?;
        let identity = Identity::load_or_create(KEYCHAIN_SERVICE, &cfg.config_dir).map_err(net)?;
        let settings = Settings::load(&cfg.config_dir.join("settings.json"))?;
        let trust = TrustStore::open(&cfg.config_dir.join("trusted-devices.json")).map_err(net)?;
        let endpoint = match AetherEndpoint::bind(cfg.bind, identity.clone()) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("cannot bind {}: {e}; using an ephemeral port", cfg.bind);
                AetherEndpoint::bind(SocketAddr::new(cfg.bind.ip(), 0), identity.clone()).map_err(net)?
            }
        };
        let shares: Vec<LocalShare> = std::fs::read(cfg.config_dir.join("shares.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let vfs_host = Arc::new(VfsHost::new(aether_vfs::ShareRegistry::new()));
        let mut live_shares = Vec::new();
        for s in shares {
            match vfs_host.add_share(s.name.clone(), &s.path, s.read_only) {
                Ok(id) => live_shares.push(LocalShare { id, ..s }),
                Err(e) => tracing::warn!("share {} not available: {e}", s.path.display()),
            }
        }
        let password = RotatingPassword::new(Duration::from_secs(settings.password_lifetime_secs.max(60)));
        let engine = Arc::new(Self {
            cfg,
            identity,
            settings: RwLock::new(settings),
            trust: Mutex::new(trust),
            password: Mutex::new(password),
            endpoint,
            events,
            vfs_host,
            shares: Mutex::new(live_shares),
            lan_peers: Mutex::new(BTreeMap::new()),
            rendezvous: Mutex::new(None),
            advertiser: Mutex::new(None),
            clients: Mutex::new(HashMap::new()),
            hosts: Mutex::new(HashMap::new()),
            pending_consent: Mutex::new(HashMap::new()),
            ghost_active: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
        });
        match aether_display::brightness::restore_pending(&engine.brightness_restore_file()) {
            Ok(true) => tracing::warn!("restored display brightness left dimmed by a previous Ghost Mode session"),
            Ok(false) => {}
            Err(e) => tracing::debug!("brightness restore check: {e}"),
        }
        tokio::spawn(host::accept_loop(engine.clone()));
        engine.apply_network_settings().await;
        engine.clone().spawn_lan_browser();
        tracing::info!(id = %engine.identity.device_id(), addr = ?engine.endpoint.local_addr().ok(), "AetherLink engine started");
        Ok(engine)
    }

    pub(crate) fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().clone()
    }

    pub(crate) fn cache_dir(&self) -> &Path {
        &self.cfg.cache_dir
    }

    pub(crate) fn download_dir(&self) -> PathBuf {
        self.cfg
            .download_dir
            .clone()
            .or_else(dirs::download_dir)
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir)
    }

    pub(crate) fn create_capturer(&self) -> aether_capture::Result<Box<dyn aether_capture::ScreenCapturer>> {
        match &self.cfg.capturer {
            Some(f) => f(),
            None => aether_capture::create_capturer(),
        }
    }

    pub(crate) fn create_audio_capturer(&self) -> aether_capture::Result<Box<dyn aether_capture::AudioCapturer>> {
        match &self.cfg.audio_capturer {
            Some(f) => f(),
            None => aether_capture::create_audio_capturer(),
        }
    }

    pub(crate) fn audio_null_output(&self) -> bool {
        self.cfg.audio_null_output
    }

    pub(crate) fn uses_native_capture(&self) -> bool {
        self.cfg.capturer.is_none()
    }

    pub(crate) fn brightness_restore_file(&self) -> PathBuf {
        self.cfg.config_dir.join("brightness-restore.json")
    }

    /// Ghost Mode on this host: system-level curtain + backlight off (macOS), exact restore on
    /// disable. Emits `ghost-mode` `{enabled, native, status}`; when `native` is false the app
    /// shell shows its own (webview) curtain instead.
    pub async fn set_ghost_mode(&self, enabled: bool) -> aether_display::ghost::GhostStatus {
        let restore = self.brightness_restore_file();
        let res = tokio::task::spawn_blocking(move || {
            if enabled {
                aether_display::ghost::enable(&aether_display::ghost::GhostOptions {
                    dim_backlight: true,
                    restore_file: Some(restore),
                })
            } else {
                aether_display::ghost::disable().map(|_| aether_display::ghost::status())
            }
        })
        .await;
        let (status, native) = match res {
            Ok(Ok(s)) => (s, true),
            Ok(Err(e)) => {
                tracing::info!("native Ghost Mode unavailable ({e}); app shell curtain will be used");
                (aether_display::ghost::status(), false)
            }
            Err(e) => {
                tracing::warn!("ghost task failed: {e}");
                (aether_display::ghost::status(), false)
            }
        };
        self.ghost_active.store(enabled, Ordering::Relaxed);
        events::emit(&*self.events, names::GHOST_MODE, &serde_json::json!({ "enabled": enabled, "native": native, "status": status }));
        status
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.endpoint.local_addr().ok()
    }

    pub fn device_id(&self) -> DeviceId {
        self.identity.device_id()
    }

    pub(crate) fn local_hello(&self, mode: ConnectMode) -> Hello {
        let caps = aether_codec::encoder_capabilities();
        let s = self.settings();
        Hello {
            protocol_version: PROTOCOL_VERSION,
            device_id: self.identity.device_id(),
            device_name: s.device_name.clone(),
            os: OsKind::current(),
            os_version: crate::sysinfo::System::os_version().unwrap_or_default(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            public_key: self.identity.public_key(),
            capabilities: Capabilities {
                video_h264: caps.iter().any(|c| c.0 == VideoCodec::H264),
                video_hevc: caps.iter().any(|c| c.0 == VideoCodec::Hevc),
                video_hevc_444: caps.contains(&(VideoCodec::Hevc, ChromaFormat::Yuv444)),
                video_av1: false,
                lossless_tiles: true,
                audio: self.cfg.audio_capturer.is_some()
                    || cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux")),
                input_injection: cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux")),
                clipboard: true,
                file_transfer: true,
                vfs_mount: cfg!(any(target_os = "macos", target_os = "linux")),
                semantic_dom: cfg!(target_os = "macos"),
                port_tunnels: true,
                terminal: true,
                usb_redirect: true,
                ghost_mode: true,
                time_travel: s.dvr_enabled,
            },
            mode,
            locale: match s.resolved_language(&sys_locale()) {
                settings::Language::It => "it".into(),
                _ => "en".into(),
            },
        }
    }

    // ------------------------------------------------------------------ identity

    pub fn identity_info(&self) -> IdentityInfo {
        let mut pw = self.password.lock();
        let password = pw.current().to_owned();
        let port = self.endpoint.local_addr().map(|a| a.port()).unwrap_or(0);
        let id = self.identity.device_id();
        let pk: String = self.identity.public_key().iter().map(|b| format!("{b:02x}")).collect();
        IdentityInfo {
            device_id: id.to_string(),
            device_name: self.settings().device_name,
            fingerprint: self.identity.fingerprint().to_string(),
            os: format!("{:?}", OsKind::current()),
            password,
            password_expires_at: pw.expires_at_ms(),
            qr_payload: format!("aetherlink://connect?id={}&pk={pk}&port={port}", id.get()),
            port,
        }
    }

    pub fn rotate_password(&self) -> IdentityInfo {
        self.password.lock().rotate();
        self.identity_info()
    }

    // ------------------------------------------------------------------ settings

    pub async fn set_settings(&self, new: Settings) -> Result<Settings> {
        new.save(&self.cfg.config_dir.join("settings.json"))?;
        let old = std::mem::replace(&mut *self.settings.write(), new.clone());
        if old.password_lifetime_secs != new.password_lifetime_secs {
            *self.password.lock() = RotatingPassword::new(Duration::from_secs(new.password_lifetime_secs.max(60)));
        }
        if old.relay_server != new.relay_server || old.advertise_on_lan != new.advertise_on_lan || old.device_name != new.device_name {
            self.apply_network_settings().await;
        }
        if old.clipboard_mode != new.clipboard_mode {
            let clients: Vec<_> = self.clients.lock().values().cloned().collect();
            for c in clients {
                let _ = c.set_clipboard_mode(new.clipboard_mode).await;
            }
        }
        Ok(new)
    }

    async fn apply_network_settings(&self) {
        let s = self.settings();
        let port = self.endpoint.local_addr().map(|a| a.port()).unwrap_or(DEFAULT_PORT);
        *self.advertiser.lock() = None;
        if s.advertise_on_lan {
            let adv = LanAdvertisement {
                device_id: self.identity.device_id(),
                device_name: s.device_name.clone(),
                os: OsKind::current(),
                public_key: self.identity.public_key(),
                port,
                version: env!("CARGO_PKG_VERSION").into(),
            };
            match LanAdvertiser::start(&adv) {
                Ok(a) => *self.advertiser.lock() = Some(a),
                Err(e) => tracing::warn!("LAN advertising unavailable: {e}"),
            }
        }
        let old = self.rendezvous.lock().take();
        if let Some(rv) = old {
            rv.stop_hosting();
        }
        if let Some(server) = s.relay_server.as_deref().filter(|s| !s.trim().is_empty()) {
            let addr = resolve(server, DEFAULT_RELAY_PORT).await;
            match addr.and_then(|a| RendezvousClient::new(&self.endpoint, a).map_err(net)) {
                Ok(rv) => {
                    rv.start_hosting(self.identity.device_id());
                    *self.rendezvous.lock() = Some(Arc::new(rv));
                }
                Err(e) => tracing::warn!("relay {server} unavailable: {e}"),
            }
        }
    }

    // ------------------------------------------------------------------ LAN discovery

    fn spawn_lan_browser(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut browser = match LanBrowser::start_excluding(Some(self.identity.device_id())) {
                Ok(b) => b,
                Err(e) => return tracing::warn!("LAN discovery unavailable: {e}"),
            };
            while let Some(ev) = browser.next_event().await {
                match ev {
                    LanEvent::Found(p) => {
                        self.lan_peers.lock().insert(p.device_id, p);
                    }
                    LanEvent::Lost(id) => {
                        self.lan_peers.lock().remove(&id);
                    }
                }
                events::emit(&*self.events, names::LAN_PEERS_CHANGED, &self.lan_peers());
            }
        });
    }

    pub fn lan_peers(&self) -> Vec<LanPeerInfo> {
        let trust = self.trust.lock();
        self.lan_peers
            .lock()
            .values()
            .map(|p| LanPeerInfo {
                device_id: p.device_id.to_string(),
                name: p.device_name.clone(),
                os: format!("{:?}", p.os),
                addrs: p.addrs.iter().map(|a| a.to_string()).collect(),
                fingerprint: KeyFingerprint::of(&p.public_key).to_string(),
                trusted: trust.is_trusted(&p.public_key),
            })
            .collect()
    }

    // ------------------------------------------------------------------ trust

    pub fn trusted_devices(&self) -> Vec<TrustedDeviceInfo> {
        let online: Vec<DeviceId> = self.lan_peers.lock().keys().copied().collect();
        self.trust
            .lock()
            .list()
            .into_iter()
            .map(|p| TrustedDeviceInfo {
                device_id: p.device_id.to_string(),
                name: p.name.clone(),
                fingerprint: p.fingerprint().to_string(),
                added_at: p.added_at,
                online: online.contains(&p.device_id),
            })
            .collect()
    }

    pub fn remove_trusted_device(&self, device_id: &str) -> Result<()> {
        let id: DeviceId = device_id.parse().map_err(|e| CoreError::Invalid(format!("{e}")))?;
        let mut trust = self.trust.lock();
        let key = trust.find_by_device_id(id).map(|p| p.public_key).ok_or_else(|| CoreError::Invalid("unknown device".into()))?;
        trust.remove(&key).map_err(net)
    }

    // ------------------------------------------------------------------ outgoing sessions

    /// Start connecting to `target` (9-digit ID or `host:port`). Progress is reported
    /// through `session-state` events; frames flow into `frames`.
    pub fn connect(self: &Arc<Self>, target: &str, password: Option<String>, frames: ByteSink) -> Result<u64> {
        let target = target.trim().to_owned();
        let parsed_id = target.parse::<DeviceId>().ok();
        if parsed_id.is_none() && !target.contains(':') {
            return Err(CoreError::Invalid("errors.invalidTarget".into()));
        }
        let session_id = self.next_id();
        let me = self.clone();
        let password = password.filter(|p| !p.trim().is_empty());
        tokio::spawn(async move {
            me.emit_state(session_id, "connecting", None, None, None);
            match me.establish(&target, parsed_id, password).await {
                Ok((session, path)) => {
                    let peer_name = session.remote_hello.device_name.clone();
                    me.remember_host(&session.remote_hello);
                    let client = ClientSession::start(me.clone(), session_id, session, frames, path.clone()).await;
                    me.clients.lock().insert(session_id, client);
                    me.emit_state(session_id, "connected", None, Some(peer_name), Some(path));
                }
                Err(e) => me.emit_state(session_id, "failed", Some(&e), None, None),
            }
        });
        Ok(session_id)
    }

    async fn establish(&self, target: &str, id: Option<DeviceId>, password: Option<String>) -> Result<(Session, String)> {
        let auth = match password {
            Some(p) => ClientAuth::Password(p),
            None => ClientAuth::Trusted,
        };
        let mode = match (&auth, id) {
            (ClientAuth::Password(_), _) => ConnectMode::EphemeralId,
            (ClientAuth::Trusted, _) => ConnectMode::TrustedFleet,
        };
        let Some(id) = id else {
            let addr = resolve(target, DEFAULT_PORT).await?;
            let s = self.endpoint.connect(addr, self.local_hello(ConnectMode::LanDirect), auth).await.map_err(net)?;
            return Ok((s, "direct".into()));
        };
        let expected = self.trust.lock().find_by_device_id(id).map(|p| p.public_key);
        let lan = self.lan_peers.lock().get(&id).cloned();
        let rv = self.rendezvous.lock().clone();
        if let Some(rv) = rv {
            let opts = ConnectOptions {
                expected_host_key: expected.or(lan.as_ref().map(|p| p.public_key)),
                extra_candidates: lan.as_ref().map(|p| p.addrs.clone()).unwrap_or_default(),
                ..ConnectOptions::default()
            };
            let (s, path) = aether_net::connect_to_device_with(&self.endpoint, &rv, id, self.local_hello(mode), auth, &opts)
                .await
                .map_err(net)?;
            return Ok((s, format!("{path:?}").to_lowercase()));
        }
        // No relay configured: LAN only.
        let peer = lan.ok_or_else(|| CoreError::Net("errors.peerNotFound".into()))?;
        let mut last = None;
        for addr in &peer.addrs {
            match self.endpoint.connect_with_key(*addr, self.local_hello(ConnectMode::LanDirect), auth.clone(), Some(peer.public_key)).await {
                Ok(s) => return Ok((s, "lan".into())),
                Err(e) => last = Some(e),
            }
        }
        Err(net(last.map(|e| e.to_string()).unwrap_or_else(|| "errors.peerNotFound".into())))
    }

    /// Pin the host key after a successful session (trust on first use for future unattended connects).
    fn remember_host(&self, hello: &Hello) {
        let mut trust = self.trust.lock();
        if !trust.is_trusted(&hello.public_key) {
            let _ = trust.add(TrustedPeer::new(hello.public_key, hello.device_id, hello.device_name.clone()));
        }
    }

    fn emit_state(&self, session_id: u64, state: &str, err: Option<&CoreError>, peer: Option<String>, path: Option<String>) {
        events::emit(
            &*self.events,
            names::SESSION_STATE,
            &serde_json::json!({
                "sessionId": session_id,
                "state": state,
                "errorCode": err.map(error_code),
                "detail": err.map(|e| e.to_string()),
                "peerName": peer,
                "path": path,
            }),
        );
    }

    pub fn client(&self, session_id: u64) -> Result<Arc<ClientSession>> {
        self.clients.lock().get(&session_id).cloned().ok_or(CoreError::SessionNotFound)
    }

    pub fn client_sessions(&self) -> Vec<Arc<ClientSession>> {
        self.clients.lock().values().cloned().collect()
    }

    pub async fn disconnect(&self, session_id: u64) -> Result<()> {
        let c = self.client(session_id)?;
        c.disconnect().await;
        Ok(())
    }

    // ------------------------------------------------------------------ incoming sessions

    pub fn host_sessions(&self) -> Vec<HostSessionInfo> {
        self.hosts.lock().values().cloned().collect()
    }

    pub(crate) fn emit_host_sessions(&self) {
        events::emit(&*self.events, names::HOST_SESSIONS_CHANGED, &self.host_sessions());
    }

    pub fn kick_session(&self, session_id: u64) {
        if let Some(conn) = self.hosts.lock().get(&session_id).and_then(|h| h.conn.clone()) {
            conn.close("closed by host");
        }
    }

    /// Mark the peer of a running incoming session as trusted (unattended access).
    pub fn trust_host_session(&self, session_id: u64) -> Result<()> {
        let info = self.hosts.lock().get(&session_id).cloned().ok_or(CoreError::SessionNotFound)?;
        let id: DeviceId = info.device_id.parse().map_err(|e| CoreError::Invalid(format!("{e}")))?;
        self.trust.lock().add(TrustedPeer::new(info.public_key, id, info.peer_name)).map_err(net)
    }

    pub fn respond_incoming(&self, request_id: u64, accept: bool, trust: bool) {
        if let Some((hello, tx)) = self.pending_consent.lock().remove(&request_id) {
            if accept && trust {
                let _ = self.trust.lock().add(TrustedPeer::new(hello.public_key, hello.device_id, hello.device_name));
            }
            let _ = tx.send(accept);
        }
    }

    // ------------------------------------------------------------------ local shares

    pub fn local_shares(&self) -> Vec<LocalShare> {
        self.shares.lock().clone()
    }

    pub fn add_local_share(&self, path: &Path, read_only: bool) -> Result<LocalShare> {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        let id = self.vfs_host.add_share(name.clone(), path, read_only).map_err(|e| CoreError::Invalid(e.to_string()))?;
        let share = LocalShare { id, name, path: path.to_path_buf(), read_only };
        self.shares.lock().push(share.clone());
        self.save_shares()?;
        Ok(share)
    }

    pub fn remove_local_share(&self, id: u32) -> Result<()> {
        self.vfs_host.remove_share(id);
        self.shares.lock().retain(|s| s.id != id);
        self.save_shares()
    }

    fn save_shares(&self) -> Result<()> {
        std::fs::write(self.cfg.config_dir.join("shares.json"), serde_json::to_vec_pretty(&*self.shares.lock())?)?;
        Ok(())
    }

    // ------------------------------------------------------------------ permissions

    pub fn permissions(&self) -> aether_input::permissions::PermissionReport {
        aether_input::permissions::report()
    }

    pub fn open_permission_settings(&self, kind: &str) -> Result<()> {
        use aether_input::permissions::PermissionKind;
        let kind = match kind {
            "accessibility" => PermissionKind::Accessibility,
            "screenRecording" => PermissionKind::ScreenRecording,
            "inputMonitoring" => PermissionKind::InputMonitoring,
            other => return Err(CoreError::Invalid(format!("unknown permission {other}"))),
        };
        aether_input::permissions::open_settings(kind).map_err(|e| CoreError::Other(e.to_string()))
    }

    /// Graceful shutdown: close sessions and stop advertising.
    pub async fn shutdown(&self) {
        let clients: Vec<_> = self.clients.lock().values().cloned().collect();
        for c in clients {
            c.disconnect().await;
        }
        *self.advertiser.lock() = None;
        if let Some(rv) = self.rendezvous.lock().take() {
            rv.stop_hosting();
        }
        self.endpoint.close();
    }
}

fn net(e: impl std::fmt::Display) -> CoreError {
    CoreError::Net(e.to_string())
}

/// Stable i18n key for an engine error; network errors carrying an `errors.*` key keep it.
pub fn error_code(e: &CoreError) -> String {
    match e {
        CoreError::Net(m) | CoreError::Invalid(m) if m.starts_with("errors.") => m.clone(),
        CoreError::Net(m) if m.contains("assword") || m.contains("WrongPassword") => "errors.wrongPassword".into(),
        CoreError::Net(m) if m.to_lowercase().contains("timed out") || m.to_lowercase().contains("timeout") => "errors.timeout".into(),
        other => other.code().into(),
    }
}

async fn resolve(target: &str, default_port: u16) -> Result<SocketAddr> {
    let t = if target.contains(':') && !target.starts_with('[') && target.matches(':').count() == 1 || target.starts_with('[') {
        target.to_owned()
    } else {
        format!("{target}:{default_port}")
    };
    tokio::task::spawn_blocking(move || t.to_socket_addrs().ok().and_then(|mut a| a.next()))
        .await
        .ok()
        .flatten()
        .ok_or_else(|| CoreError::Net("errors.resolve".into()))
}

fn sys_locale() -> String {
    std::env::var("LANG").unwrap_or_default()
}
