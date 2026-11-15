//! Tauri command surface.
//!
//! Engine commands mirror the contract table in `docs/ARCHITECTURE.md`
//! ("aether-core → desktop/mobile"). Rust `snake_case` parameters are exposed
//! to JavaScript as `camelCase` keys by Tauri (e.g. `session_id` ↔ `sessionId`).
//! Bodies delegate to [`crate::backend`] (`// WIRE: aether-core`).
//!
//! `ui_*` commands are window-shell helpers (tray, locale, windows, hotkey,
//! clipboard). They are fully implemented here and do not touch the engine.

use crate::backend::{self, BackendResult};
use crate::shell::{self, PendingConnect, TrayState};
use aether_protocol::{DomAction, InputEvent};
use serde_json::Value;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, State, WebviewWindow};

// ---------------------------------------------------------------- identity / settings

#[tauri::command]
pub async fn get_identity() -> BackendResult<Value> {
    backend::get_identity().await
}

#[tauri::command]
pub async fn rotate_password() -> BackendResult<Value> {
    backend::rotate_password().await
}

#[tauri::command]
pub async fn get_settings() -> BackendResult<Value> {
    backend::get_settings().await
}

#[tauri::command]
pub async fn set_settings(settings: Value) -> BackendResult<Value> {
    backend::set_settings(settings).await
}

#[tauri::command]
pub async fn get_permissions() -> BackendResult<Value> {
    backend::get_permissions().await
}

#[tauri::command]
pub async fn open_permission_settings(kind: String) -> BackendResult<()> {
    backend::open_permission_settings(kind).await
}

// ---------------------------------------------------------------- discovery / trust

#[tauri::command]
pub async fn list_lan_peers() -> BackendResult<Value> {
    backend::list_lan_peers().await
}

#[tauri::command]
pub async fn list_trusted_devices() -> BackendResult<Value> {
    backend::list_trusted_devices().await
}

#[tauri::command]
pub async fn remove_trusted_device(device_id: String) -> BackendResult<Value> {
    backend::remove_trusted_device(device_id).await
}

// ---------------------------------------------------------------- client sessions

#[tauri::command]
pub async fn connect(
    target: String,
    password: Option<String>,
    frames: Channel<InvokeResponseBody>,
) -> BackendResult<Value> {
    backend::connect(target, password, frames).await
}

#[tauri::command]
pub async fn disconnect(session_id: String) -> BackendResult<()> {
    backend::disconnect(session_id).await
}

#[tauri::command]
pub async fn send_input(session_id: String, event: InputEvent) -> BackendResult<()> {
    backend::send_input(session_id, event).await
}

#[tauri::command]
pub async fn select_display(session_id: String, display_id: u32) -> BackendResult<()> {
    backend::select_display(session_id, display_id).await
}

#[tauri::command]
pub async fn set_ghost_mode(session_id: String, enabled: bool) -> BackendResult<()> {
    backend::set_ghost_mode(session_id, enabled).await
}

#[tauri::command]
pub async fn set_lossless_text(session_id: String, enabled: bool) -> BackendResult<()> {
    backend::set_lossless_text(session_id, enabled).await
}

#[tauri::command]
pub async fn set_audio(session_id: String, enabled: bool) -> BackendResult<()> {
    backend::set_audio(session_id, enabled).await
}

#[tauri::command]
pub async fn set_audio_volume(session_id: String, volume: f32, muted: Option<bool>) -> BackendResult<()> {
    backend::set_audio_volume(session_id, volume, muted).await
}

#[tauri::command]
pub async fn list_audio_outputs() -> BackendResult<Value> {
    backend::list_audio_outputs().await
}

#[tauri::command]
pub async fn set_audio_output(session_id: String, device_id: Option<String>) -> BackendResult<()> {
    backend::set_audio_output(session_id, device_id).await
}

#[tauri::command]
pub async fn set_quality(session_id: String, quality: Value) -> BackendResult<()> {
    backend::set_quality(session_id, quality).await
}

// ---------------------------------------------------------------- semantic DOM

#[tauri::command]
pub async fn dom_subscribe(session_id: String, interactive_only: bool) -> BackendResult<()> {
    backend::dom_subscribe(session_id, interactive_only).await
}

#[tauri::command]
pub async fn dom_search(session_id: String, query: String, limit: u32) -> BackendResult<Value> {
    backend::dom_search(session_id, query, limit).await
}

#[tauri::command]
pub async fn dom_action(session_id: String, node_id: u64, action: DomAction) -> BackendResult<()> {
    backend::dom_action(session_id, node_id, action).await
}

#[tauri::command]
pub async fn dom_set_value(session_id: String, node_id: u64, value: String) -> BackendResult<()> {
    backend::dom_set_value(session_id, node_id, value).await
}

// ---------------------------------------------------------------- port tunnels

#[tauri::command]
pub async fn ports_scan(session_id: String) -> BackendResult<Value> {
    backend::ports_scan(session_id).await
}

#[tauri::command]
pub async fn port_forward(session_id: String, remote_port: u16, local_port: Option<u16>) -> BackendResult<Value> {
    backend::port_forward(session_id, remote_port, local_port).await
}

#[tauri::command]
pub async fn port_unforward(session_id: String, remote_port: u16) -> BackendResult<()> {
    backend::port_unforward(session_id, remote_port).await
}

// ---------------------------------------------------------------- terminal

#[tauri::command]
pub async fn terminal_open(
    session_id: String,
    cols: u16,
    rows: u16,
    output: Channel<InvokeResponseBody>,
) -> BackendResult<Value> {
    backend::terminal_open(session_id, cols, rows, output).await
}

#[tauri::command]
pub async fn terminal_input(session_id: String, terminal_id: String, data: String) -> BackendResult<()> {
    backend::terminal_input(session_id, terminal_id, data).await
}

#[tauri::command]
pub async fn terminal_resize(session_id: String, terminal_id: String, cols: u16, rows: u16) -> BackendResult<()> {
    backend::terminal_resize(session_id, terminal_id, cols, rows).await
}

#[tauri::command]
pub async fn terminal_close(session_id: String, terminal_id: String) -> BackendResult<()> {
    backend::terminal_close(session_id, terminal_id).await
}

// ---------------------------------------------------------------- virtual drives

#[tauri::command]
pub async fn vfs_list_shares(session_id: String) -> BackendResult<Value> {
    backend::vfs_list_shares(session_id).await
}

#[tauri::command]
pub async fn vfs_mount(session_id: String, share_id: u32) -> BackendResult<Value> {
    backend::vfs_mount(session_id, share_id).await
}

#[tauri::command]
pub async fn vfs_unmount(mount_point: String) -> BackendResult<()> {
    backend::vfs_unmount(mount_point).await
}

#[tauri::command]
pub async fn list_mounts() -> BackendResult<Value> {
    backend::list_mounts().await
}

#[tauri::command]
pub async fn list_local_shares() -> BackendResult<Value> {
    backend::list_local_shares().await
}

#[tauri::command]
pub async fn add_local_share(path: String, read_only: bool) -> BackendResult<Value> {
    backend::add_local_share(path, read_only).await
}

#[tauri::command]
pub async fn remove_local_share(share_id: u32) -> BackendResult<Value> {
    backend::remove_local_share(share_id).await
}

// ---------------------------------------------------------------- transfers

#[tauri::command]
pub async fn send_files(session_id: String, paths: Vec<String>) -> BackendResult<Value> {
    backend::send_files(session_id, paths).await
}

// ---------------------------------------------------------------- time-travel DVR

#[tauri::command]
pub async fn dvr_index(session_id: String) -> BackendResult<Value> {
    backend::dvr_index(session_id).await
}

#[tauri::command]
pub async fn dvr_seek(session_id: String, pts_us: u64) -> BackendResult<()> {
    backend::dvr_seek(session_id, pts_us).await
}

#[tauri::command]
pub async fn dvr_live(session_id: String) -> BackendResult<()> {
    backend::dvr_live(session_id).await
}

#[tauri::command]
pub async fn dvr_ocr(session_id: String, pts_us: Option<u64>) -> BackendResult<String> {
    backend::dvr_ocr(session_id, pts_us).await
}

// ---------------------------------------------------------------- host side

#[tauri::command]
pub async fn host_status() -> BackendResult<Value> {
    backend::host_status().await
}

#[tauri::command]
pub async fn kick_session(session_id: String) -> BackendResult<()> {
    backend::kick_session(session_id).await
}

#[tauri::command]
pub async fn respond_incoming(request_id: String, decision: String) -> BackendResult<()> {
    match decision.as_str() {
        "accept" | "acceptAndTrust" | "deny" => backend::respond_incoming(request_id, decision).await,
        _ => Err("invalid-decision".into()),
    }
}

// ---------------------------------------------------------------- UI shell (implemented)

/// Switch the tray/menu language; the webviews switch themselves and are notified via `locale-changed`.
#[tauri::command]
pub fn ui_set_locale(app: AppHandle, state: State<'_, shell::UiState>, locale: String) -> Result<(), String> {
    shell::set_locale(&app, &state, &locale)
}

/// Push the facts the tray shows (device ID, active connections, ghost mode).
#[tauri::command]
pub fn ui_tray_state(app: AppHandle, state: State<'_, shell::UiState>, tray: TrayState) -> Result<(), String> {
    shell::set_tray_state(&app, &state, tray)
}

/// Open a session viewport window; the connect parameters are handed over in memory.
#[tauri::command]
pub fn ui_open_session_window(
    app: AppHandle,
    state: State<'_, shell::UiState>,
    target: String,
    password: Option<String>,
    title: String,
) -> Result<String, String> {
    shell::open_session_window(&app, &state, target, password, title)
}

/// Called once by a session window to receive the parameters it was opened with.
#[tauri::command]
pub fn ui_take_pending_connect(window: WebviewWindow, state: State<'_, shell::UiState>) -> Option<PendingConnect> {
    state.take_pending(window.label())
}

#[tauri::command]
pub fn ui_copy_text(app: AppHandle, text: String) -> Result<(), String> {
    shell::copy_text(&app, text)
}

/// Re-register the global HUD hotkey (accelerator string, e.g. `CmdOrCtrl+Shift+Period`).
#[tauri::command]
pub fn ui_set_hud_hotkey(app: AppHandle, state: State<'_, shell::UiState>, accelerator: String) -> Result<(), String> {
    shell::set_hud_hotkey(&app, &state, &accelerator)
}

#[tauri::command]
pub fn ui_show_main(app: AppHandle) -> Result<(), String> {
    shell::show_main(&app);
    Ok(())
}

// ---------------------------------------------------------------- ghost / virtual display / HID

#[tauri::command]
pub async fn local_ghost_off() -> BackendResult<()> {
    backend::local_ghost_off().await
}

#[tauri::command]
pub async fn set_virtual_display(session_id: String, enabled: bool, width: Option<u32>, height: Option<u32>) -> BackendResult<()> {
    backend::set_virtual_display(session_id, enabled, width, height).await
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub async fn hid_backends() -> BackendResult<Value> {
    backend::hid_backends().await
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub async fn hid_start(backend: String, width: Option<u32>, height: Option<u32>) -> BackendResult<Value> {
    backend::hid_start(backend, width, height).await
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub async fn hid_stop() -> BackendResult<()> {
    backend::hid_stop().await
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub async fn hid_status() -> BackendResult<Value> {
    backend::hid_status().await
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub async fn hid_paired_devices(backend: String) -> BackendResult<Value> {
    backend::hid_paired_devices(backend).await
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub async fn hid_connect(address: String) -> BackendResult<()> {
    backend::hid_connect(address).await
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub async fn hid_send_input(event: InputEvent) -> BackendResult<()> {
    backend::hid_send_input(event).await
}
