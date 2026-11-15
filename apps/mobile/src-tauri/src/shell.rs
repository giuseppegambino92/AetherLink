//! Mobile counterpart of `apps/desktop/src-tauri/src/shell.rs`.
//!
//! The shared `commands.rs` / `backend.rs` (compiled from the desktop crate) call into
//! `crate::shell`; on a phone there is one full-screen webview, no tray, no global
//! hotkeys and no privacy curtain, so those calls are adapted or are explicit no-ops.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::{AppHandle, Emitter};
use tauri_plugin_clipboard_manager::ClipboardExt;

pub const SUPPORTED_LOCALES: [&str; 2] = ["en", "it"];

/// Accepted for API compatibility (the desktop tray state); nothing to show on mobile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TrayState {
    pub device_id: Option<String>,
    pub active_connections: u32,
    pub ghost_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingConnect {
    pub target: String,
    pub password: Option<String>,
    pub title: String,
}

pub struct UiState {
    locale: Mutex<String>,
    pending: Mutex<HashMap<String, PendingConnect>>,
}

impl Default for UiState {
    fn default() -> Self {
        Self { locale: Mutex::new("en".into()), pending: Mutex::new(HashMap::new()) }
    }
}

impl UiState {
    pub fn take_pending(&self, label: &str) -> Option<PendingConnect> {
        self.pending.lock().remove(label)
    }

    pub fn locale(&self) -> String {
        self.locale.lock().clone()
    }
}

pub fn set_tray_state(_app: &AppHandle, _state: &UiState, _tray: TrayState) -> Result<(), String> {
    Ok(())
}

pub fn set_locale(app: &AppHandle, state: &UiState, locale: &str) -> Result<(), String> {
    if !SUPPORTED_LOCALES.contains(&locale) {
        return Err("unsupported-locale".into());
    }
    *state.locale.lock() = locale.to_owned();
    let _ = app.emit("locale-changed", serde_json::json!({ "locale": locale }));
    Ok(())
}

pub fn show_main(_app: &AppHandle) {}

/// Single-webview app: the session view is a route of the main webview, so the pending
/// parameters are parked under `main` and the UI navigates itself.
pub fn open_session_window(
    _app: &AppHandle,
    state: &UiState,
    target: String,
    password: Option<String>,
    title: String,
) -> Result<String, String> {
    state.pending.lock().insert("main".into(), PendingConnect { target, password, title });
    Ok("main".into())
}

pub fn copy_text(app: &AppHandle, text: String) -> Result<(), String> {
    app.clipboard().write_text(text).map_err(|e| e.to_string())
}

/// No global shortcuts on mobile.
pub fn set_hud_hotkey(_app: &AppHandle, _state: &UiState, _accelerator: &str) -> Result<(), String> {
    Ok(())
}

/// Ghost Mode's privacy curtain is a desktop overlay window; a phone host cannot draw
/// above other apps without `SYSTEM_ALERT_WINDOW`, so the request is only logged.
pub fn set_privacy_curtain(_app: &AppHandle, enabled: bool) {
    tracing::info!(enabled, "ghost mode requested (no privacy curtain on mobile)");
}

/// Desktop registers Ctrl+Opt+Cmd+Esc to lift Ghost Mode; phones have no global hotkeys
/// (and do not host Ghost Mode), so this is intentionally a no-op on mobile.
pub fn set_ghost_panic_hotkey(_app: &AppHandle, _enabled: bool) {}
