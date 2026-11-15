//! Engine boundary: the single place where the desktop shell talks to
//! `aether_core::Engine`. Errors are returned as stable i18n keys (`errors.*`)
//! so the UI can localize them; IDs cross the bridge as strings.

use aether_core::settings::Settings;
use aether_core::{ByteSink, CoreError, Engine, EngineConfig, EventSink};
use aether_protocol::{DomAction, InputEvent};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter};

/// Error string returned before the engine finished starting.
pub const NOT_WIRED: &str = "engine-not-wired";

pub type BackendResult<T> = Result<T, String>;

static ENGINE: OnceLock<Arc<Engine>> = OnceLock::new();

fn engine() -> BackendResult<&'static Arc<Engine>> {
    ENGINE.get().ok_or_else(|| NOT_WIRED.to_owned())
}

fn err(e: CoreError) -> String {
    let code = aether_core::engine::error_code(&e);
    tracing::debug!("command failed: {e} ({code})");
    code
}

fn to_json<T: serde::Serialize>(v: T) -> BackendResult<Value> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

fn id(s: &str) -> BackendResult<u64> {
    s.trim().parse().map_err(|_| "errors.invalid".to_owned())
}

fn session(s: &str) -> BackendResult<Arc<aether_core::client::ClientSession>> {
    engine()?.client(id(s)?).map_err(err)
}

fn channel_sink(ch: Channel<InvokeResponseBody>) -> ByteSink {
    Arc::new(move |bytes: Vec<u8>| {
        let _ = ch.send(InvokeResponseBody::Raw(bytes));
    })
}

/// Forwards engine events to every webview; drives the Ghost Mode curtain.
struct TauriEvents(AppHandle);

impl EventSink for TauriEvents {
    fn emit(&self, name: &str, payload: Value) {
        if name == "ghost-mode" {
            let enabled = payload["enabled"].as_bool().unwrap_or(false);
            let native = payload["native"].as_bool().unwrap_or(false);
            // The engine raises the native shielding-level curtain itself (macOS); the webview
            // curtain is only the fallback where that is unavailable.
            crate::shell::set_privacy_curtain(&self.0, enabled && !native);
            crate::shell::set_ghost_panic_hotkey(&self.0, enabled);
        }
        let _ = self.0.emit(name, payload);
    }
}

/// Start the engine inside Tauri's Tokio runtime. Called once from `setup`.
pub fn start(app: &AppHandle) -> Result<(), String> {
    start_with(app, EngineConfig::default())
}

/// Like [`start`] with explicit directories (the mobile app passes its sandbox paths).
/// This file is also compiled into `apps/mobile/src-tauri` (`#[path]` include).
pub fn start_with(app: &AppHandle, cfg: EngineConfig) -> Result<(), String> {
    let events: Arc<dyn EventSink> = Arc::new(TauriEvents(app.clone()));
    let engine = tauri::async_runtime::block_on(Engine::start(cfg, events)).map_err(|e| e.to_string())?;
    ENGINE.set(engine).map_err(|_| "engine already started".to_owned())
}

/// The running engine, if started.
pub fn engine_handle() -> Option<Arc<Engine>> {
    ENGINE.get().cloned()
}

pub async fn shutdown() {
    if let Some(e) = ENGINE.get() {
        e.shutdown().await;
    }
}

/// Disconnect every client session (session window closed).
pub async fn disconnect_all() {
    if let Some(e) = ENGINE.get() {
        for c in e.client_sessions() {
            c.disconnect().await;
        }
    }
}

// ---------------------------------------------------------------- identity / settings

pub async fn get_identity() -> BackendResult<Value> {
    to_json(engine()?.identity_info())
}

pub async fn rotate_password() -> BackendResult<Value> {
    to_json(engine()?.rotate_password())
}

pub async fn get_settings() -> BackendResult<Value> {
    to_json(engine()?.settings())
}

/// Accepts a full or partial `Settings` object (camelCase) and merges it.
pub async fn set_settings(patch: Value) -> BackendResult<Value> {
    let e = engine()?;
    let mut current = serde_json::to_value(e.settings()).map_err(|e| e.to_string())?;
    if let (Some(cur), Some(p)) = (current.as_object_mut(), patch.as_object()) {
        for (k, v) in p {
            cur.insert(k.clone(), v.clone());
        }
    }
    let mut new: Settings = serde_json::from_value(current).map_err(|_| "errors.invalid".to_owned())?;
    if new.relay_server.as_deref().is_some_and(|s| s.trim().is_empty()) {
        new.relay_server = None;
    }
    to_json(e.set_settings(new).await.map_err(err)?)
}

pub async fn get_permissions() -> BackendResult<Value> {
    to_json(engine()?.permissions())
}

pub async fn open_permission_settings(kind: String) -> BackendResult<()> {
    engine()?.open_permission_settings(&kind).map_err(err)
}

// ---------------------------------------------------------------- discovery / trust

pub async fn list_lan_peers() -> BackendResult<Value> {
    to_json(engine()?.lan_peers())
}

pub async fn list_trusted_devices() -> BackendResult<Value> {
    to_json(engine()?.trusted_devices())
}

pub async fn remove_trusted_device(device_id: String) -> BackendResult<Value> {
    let e = engine()?;
    e.remove_trusted_device(&device_id).map_err(err)?;
    to_json(e.trusted_devices())
}

// ---------------------------------------------------------------- client sessions

pub async fn connect(target: String, password: Option<String>, frames: Channel<InvokeResponseBody>) -> BackendResult<Value> {
    let sid = engine()?.connect(&target, password, channel_sink(frames)).map_err(err)?;
    Ok(json!({ "sessionId": sid.to_string() }))
}

pub async fn disconnect(session_id: String) -> BackendResult<()> {
    engine()?.disconnect(id(&session_id)?).await.map_err(err)
}

pub async fn send_input(session_id: String, event: InputEvent) -> BackendResult<()> {
    session(&session_id)?.send_input(event).await.map_err(err)
}

pub async fn select_display(session_id: String, display_id: u32) -> BackendResult<()> {
    session(&session_id)?.select_display(display_id).await.map_err(err)
}

pub async fn set_ghost_mode(session_id: String, enabled: bool) -> BackendResult<()> {
    session(&session_id)?.set_ghost_mode(enabled).await.map_err(err)
}

pub async fn set_lossless_text(session_id: String, enabled: bool) -> BackendResult<()> {
    session(&session_id)?.set_quality(|q| q.lossless_text = enabled).await.map_err(err)
}

/// Panic hotkey / local escape: lift Ghost Mode on this host immediately.
pub async fn local_ghost_off() -> BackendResult<()> {
    engine()?.set_ghost_mode(false).await;
    Ok(())
}

/// Ask the remote host to create (or remove) a headless virtual display and stream it.
pub async fn set_virtual_display(session_id: String, enabled: bool, width: Option<u32>, height: Option<u32>) -> BackendResult<()> {
    session(&session_id)?
        .set_virtual_display(enabled, width.unwrap_or(1920), height.unwrap_or(1080), 60)
        .await
        .map_err(err)
}

// ---------------------------------------------------------------- control iPhone / iPad (HID)

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn hid_backends() -> BackendResult<Value> {
    to_json(tauri::async_runtime::spawn_blocking(aether_core::hid_bridge::backends).await.map_err(|e| e.to_string())?)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn hid_start(backend: String, width: Option<u32>, height: Option<u32>) -> BackendResult<Value> {
    let st = tauri::async_runtime::spawn_blocking(move || aether_core::hid_bridge::start(&backend, width.unwrap_or(1179), height.unwrap_or(2556)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)?;
    to_json(st)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn hid_stop() -> BackendResult<()> {
    tauri::async_runtime::spawn_blocking(aether_core::hid_bridge::stop).await.map_err(|e| e.to_string())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn hid_status() -> BackendResult<Value> {
    to_json(aether_core::hid_bridge::status())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn hid_paired_devices(backend: String) -> BackendResult<Value> {
    let list = tauri::async_runtime::spawn_blocking(move || aether_core::hid_bridge::paired_devices(&backend))
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)?;
    to_json(list)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn hid_connect(address: String) -> BackendResult<()> {
    tauri::async_runtime::spawn_blocking(move || aether_core::hid_bridge::connect(&address))
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn hid_send_input(event: InputEvent) -> BackendResult<()> {
    aether_core::hid_bridge::send(event).map_err(err)
}

pub async fn set_audio(session_id: String, enabled: bool) -> BackendResult<()> {
    session(&session_id)?.set_audio(enabled).await.map_err(err)
}

/// Local playback gain 0–1 and optional mute (the host keeps streaming).
pub async fn set_audio_volume(session_id: String, volume: f32, muted: Option<bool>) -> BackendResult<()> {
    session(&session_id)?.set_audio_volume(volume, muted);
    Ok(())
}

/// `[{id, name, isDefault}]` of the local output devices (default first).
pub async fn list_audio_outputs() -> BackendResult<Value> {
    let list = tauri::async_runtime::spawn_blocking(aether_core::audio::list_audio_outputs)
        .await
        .map_err(|e| e.to_string())?;
    to_json(list.map_err(err)?)
}

/// `device_id` = an id from `list_audio_outputs`; `null`/empty = system default.
pub async fn set_audio_output(session_id: String, device_id: Option<String>) -> BackendResult<()> {
    let s = session(&session_id)?;
    tauri::async_runtime::spawn_blocking(move || s.set_audio_output(device_id))
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)
}

/// `quality` is a partial `QualitySettings` in camelCase (`maxFps`, `bitrateKbps`, `prefer444`, `losslessText`, `scale`).
pub async fn set_quality(session_id: String, quality: Value) -> BackendResult<()> {
    session(&session_id)?
        .set_quality(|q| {
            if let Some(v) = quality["maxFps"].as_u64() {
                q.max_fps = v.clamp(1, 240) as u16;
            }
            if let Some(v) = quality.get("bitrateKbps") {
                q.bitrate_kbps = v.as_u64().map(|b| b as u32);
            }
            if let Some(v) = quality["prefer444"].as_bool() {
                q.prefer_444 = v;
            }
            if let Some(v) = quality["losslessText"].as_bool() {
                q.lossless_text = v;
            }
            if let Some(v) = quality["scale"].as_f64() {
                q.scale = v.clamp(0.25, 1.0) as f32;
            }
        })
        .await
        .map_err(err)
}

// ---------------------------------------------------------------- semantic DOM

pub async fn dom_subscribe(session_id: String, _interactive_only: bool) -> BackendResult<()> {
    session(&session_id)?.dom_subscribe().await.map_err(err)
}

pub async fn dom_search(session_id: String, query: String, limit: u32) -> BackendResult<Value> {
    to_json(session(&session_id)?.dom_search(&query, limit.clamp(1, 200) as usize))
}

pub async fn dom_action(session_id: String, node_id: u64, action: DomAction) -> BackendResult<()> {
    session(&session_id)?.dom_action(node_id, action).await.map_err(err)
}

pub async fn dom_set_value(session_id: String, node_id: u64, value: String) -> BackendResult<()> {
    session(&session_id)?.dom_set_value(node_id, value).await.map_err(err)
}

// ---------------------------------------------------------------- port tunnels

pub async fn ports_scan(session_id: String) -> BackendResult<Value> {
    to_json(session(&session_id)?.ports_scan().await.map_err(err)?)
}

pub async fn port_forward(session_id: String, remote_port: u16, local_port: Option<u16>) -> BackendResult<Value> {
    to_json(session(&session_id)?.port_forward(remote_port, local_port).await.map_err(err)?)
}

pub async fn port_unforward(session_id: String, remote_port: u16) -> BackendResult<()> {
    session(&session_id)?.port_unforward(remote_port);
    Ok(())
}

// ---------------------------------------------------------------- terminal

pub async fn terminal_open(session_id: String, cols: u16, rows: u16, output: Channel<InvokeResponseBody>) -> BackendResult<Value> {
    let info = session(&session_id)?.terminal_open(cols, rows, channel_sink(output)).await.map_err(err)?;
    Ok(json!({ "terminalId": info.terminal_id.to_string(), "cwd": info.cwd, "shell": info.shell }))
}

pub async fn terminal_input(session_id: String, terminal_id: String, data: String) -> BackendResult<()> {
    session(&session_id)?.terminal_input(id(&terminal_id)?, data.into_bytes()).map_err(err)
}

pub async fn terminal_resize(session_id: String, terminal_id: String, cols: u16, rows: u16) -> BackendResult<()> {
    session(&session_id)?.terminal_resize(id(&terminal_id)?, cols, rows).map_err(err)
}

pub async fn terminal_close(session_id: String, terminal_id: String) -> BackendResult<()> {
    session(&session_id)?.terminal_close(id(&terminal_id)?);
    Ok(())
}

// ---------------------------------------------------------------- virtual drives

pub async fn vfs_list_shares(session_id: String) -> BackendResult<Value> {
    to_json(session(&session_id)?.vfs_list_shares().await.map_err(err)?)
}

pub async fn vfs_mount(session_id: String, share_id: u32) -> BackendResult<Value> {
    let mp = session(&session_id)?.vfs_mount(share_id).await.map_err(err)?;
    Ok(json!({ "mountPoint": mp.display().to_string() }))
}

pub async fn vfs_unmount(mount_point: String) -> BackendResult<()> {
    let e = engine()?;
    for c in e.client_sessions() {
        for (share, mp) in c.mounts().await {
            if mp.display().to_string() == mount_point {
                return c.vfs_unmount(share).await.map_err(err);
            }
        }
    }
    Err("errors.invalid".into())
}

pub async fn list_mounts() -> BackendResult<Value> {
    let e = engine()?;
    let mut out = Vec::new();
    for c in e.client_sessions() {
        for (share, mp) in c.mounts().await {
            out.push(json!({
                "mountPoint": mp.display().to_string(),
                "shareId": share,
                "peerName": c.peer.device_name,
                "sessionId": c.id.to_string(),
            }));
        }
    }
    Ok(Value::Array(out))
}

pub async fn list_local_shares() -> BackendResult<Value> {
    to_json(engine()?.local_shares())
}

pub async fn add_local_share(path: String, read_only: bool) -> BackendResult<Value> {
    let e = engine()?;
    e.add_local_share(&PathBuf::from(path), read_only).map_err(err)?;
    to_json(e.local_shares())
}

pub async fn remove_local_share(share_id: u32) -> BackendResult<Value> {
    let e = engine()?;
    e.remove_local_share(share_id).map_err(err)?;
    to_json(e.local_shares())
}

// ---------------------------------------------------------------- transfers

pub async fn send_files(session_id: String, paths: Vec<String>) -> BackendResult<Value> {
    let paths = paths.into_iter().map(PathBuf::from).collect();
    let tid = session(&session_id)?.send_files(paths).await.map_err(err)?;
    Ok(json!({ "transferId": tid.to_string() }))
}

// ---------------------------------------------------------------- time-travel DVR

pub async fn dvr_index(session_id: String) -> BackendResult<Value> {
    to_json(session(&session_id)?.dvr_index())
}

pub async fn dvr_seek(session_id: String, pts_us: u64) -> BackendResult<()> {
    session(&session_id)?.dvr_seek(pts_us).map_err(err)
}

pub async fn dvr_live(session_id: String) -> BackendResult<()> {
    session(&session_id)?.dvr_live().await.map_err(err)
}

pub async fn dvr_ocr(session_id: String, pts_us: Option<u64>) -> BackendResult<String> {
    session(&session_id)?.dvr_ocr(pts_us).await.map_err(err)
}

// ---------------------------------------------------------------- host side

pub async fn host_status() -> BackendResult<Value> {
    let e = engine()?;
    Ok(json!({
        "listening": e.local_addr().is_some(),
        "port": e.local_addr().map(|a| a.port()),
        "sessions": e.host_sessions(),
    }))
}

pub async fn kick_session(session_id: String) -> BackendResult<()> {
    engine()?.kick_session(id(&session_id)?);
    Ok(())
}

/// `decision` is `accept | acceptAndTrust | deny`.
pub async fn respond_incoming(request_id: String, decision: String) -> BackendResult<()> {
    let (accept, trust) = match decision.as_str() {
        "accept" => (true, false),
        "acceptAndTrust" => (true, true),
        "deny" => (false, false),
        _ => return Err("invalid-decision".into()),
    };
    engine()?.respond_incoming(id(&request_id)?, accept, trust);
    Ok(())
}
