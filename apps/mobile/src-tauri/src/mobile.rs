//! Mobile-only commands (`mobile_*`) and platform wiring.
//!
//! | command | Android | iOS |
//! |---|---|---|
//! | `mobile_platform` | capabilities of this build | same |
//! | `mobile_host_start` / `_stop` | MediaProjection consent → `ScreenShareService` | broadcast picker / finish broadcast |
//! | `mobile_host_status` | encoder + viewers ([`aether_core::mobile::MobileHost`]) | same |
//! | `mobile_input_status` | accessibility service / Shizuku state | `notApplicable` (iOS forbids injection) |
//! | `mobile_open_accessibility_settings`, `mobile_request_shizuku`, `mobile_request_permissions` | Kotlin `AetherPlugin` | – |
//! | `mobile_vfs_list_dir` / `mobile_vfs_download` | remote share browser (no NFS mount on phones) | same |
//! | `mobile_hid_*` | `BluetoothHidDevice` keyboard + mouse controller | `errors.unsupported` |
//!
//! Android calls go through the Tauri mobile-plugin bridge (`AetherPlugin.kt`); the hot paths
//! (encoded frames in, input / HID reports out) use direct JNI in `android.rs`.

use crate::backend::{self, BackendResult};
use aether_core::EngineConfig;
use aether_input::hid_report::{HidConfig, HidReportBuilder};
use aether_protocol::InputEvent;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tauri::plugin::TauriPlugin;
use tauri::{AppHandle, State};
#[cfg(target_os = "android")]
use tauri::Manager;

#[cfg(target_os = "android")]
pub struct NativeHandle(pub tauri::plugin::PluginHandle<tauri::Wry>);

/// Registers the platform plugin (`AetherPlugin.kt` on Android; Swift is linked via C ABI on iOS).
pub fn native_plugin() -> TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::<tauri::Wry>::new("aether-native")
        .setup(|_app, _api| {
            #[cfg(target_os = "android")]
            {
                let handle = _api.register_android_plugin("dev.aetherlink.mobile", "AetherPlugin")?;
                _app.manage(NativeHandle(handle));
            }
            Ok(())
        })
        .build()
}

/// Engine directories inside the app sandbox.
pub fn engine_config(app: &AppHandle) -> EngineConfig {
    let (config_dir, cache_dir, received) = crate::sandbox_dirs(app);
    EngineConfig { config_dir, cache_dir, download_dir: Some(received), ..EngineConfig::default() }
}

/// Register the platform host / input sink / clipboard. Called once after the engine started.
pub fn install_platform(app: &AppHandle) {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    aether_core::clipboard::system::set_platform_clipboard(Some(std::sync::Arc::new(PluginClipboard(app.clone()))));
    #[cfg(target_os = "android")]
    crate::android::install(app);
    #[cfg(target_os = "ios")]
    crate::ios::install(app);
    flush_default_device_name();
    let _ = app;
}

#[cfg(any(target_os = "android", target_os = "ios"))]
struct PluginClipboard(AppHandle);

#[cfg(any(target_os = "android", target_os = "ios"))]
impl aether_core::clipboard::system::PlatformClipboard for PluginClipboard {
    fn read_text(&self) -> Option<String> {
        use tauri_plugin_clipboard_manager::ClipboardExt;
        self.0.clipboard().read_text().ok()
    }

    fn write_text(&self, text: &str) -> Result<(), String> {
        use tauri_plugin_clipboard_manager::ClipboardExt;
        self.0.clipboard().write_text(text.to_owned()).map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "android")]
async fn native(app: &AppHandle, cmd: &str, payload: Value) -> BackendResult<Value> {
    let handle = app.try_state::<NativeHandle>().ok_or_else(|| backend::NOT_WIRED.to_owned())?.0.clone();
    handle.run_mobile_plugin_async::<Value>(cmd, payload).await.map_err(|e| {
        tracing::warn!("native {cmd} failed: {e}");
        // Kotlin rejects with a stable i18n key (`errors.*`) as the message.
        let s = e.to_string();
        match s.find("errors.") {
            Some(i) => s[i..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '.').collect(),
            None => "errors.native".to_owned(),
        }
    })
}

const UNSUPPORTED: &str = "errors.unsupported";

fn os() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "ios") {
        "ios"
    } else {
        "desktop"
    }
}

// ---------------------------------------------------------------- platform

#[tauri::command]
pub async fn mobile_platform() -> BackendResult<Value> {
    let os = os();
    Ok(json!({
        "os": os,
        "canHost": os != "desktop",
        "hostKind": match os { "android" => "mediaProjection", "ios" => "replayKit", _ => "none" },
        "canBeControlled": os == "android",
        "hidController": os == "android",
    }))
}

// ---------------------------------------------------------------- host ("Share this device")

#[tauri::command]
pub async fn mobile_host_status(_app: AppHandle) -> BackendResult<Value> {
    let host = aether_core::external::platform_host().is_some();
    let status = platform_status();
    let mut v = serde_json::to_value(status).map_err(|e| e.to_string())?;
    v["registered"] = json!(host);
    if let Some(e) = backend::engine_handle() {
        v["sessions"] = serde_json::to_value(e.host_sessions()).unwrap_or(Value::Null);
    }
    #[cfg(target_os = "android")]
    if let Ok(n) = native(&_app, "hostStatus", json!({})).await {
        v["native"] = n;
    }
    Ok(v)
}

fn platform_status() -> Option<aether_core::mobile::MobileHostStatus> {
    #[cfg(target_os = "android")]
    return crate::android::host().map(|h| h.status());
    #[cfg(target_os = "ios")]
    return crate::ios::host().map(|h| h.status());
    #[allow(unreachable_code)]
    None
}

#[tauri::command]
pub async fn mobile_host_start(_app: AppHandle, audio: Option<bool>) -> BackendResult<Value> {
    #[cfg(target_os = "android")]
    return native(&_app, "startScreenShare", json!({ "audio": audio.unwrap_or(true) })).await;
    #[cfg(target_os = "ios")]
    {
        let _ = audio;
        crate::ios::show_broadcast_picker();
        return Ok(json!({ "started": false, "picker": true }));
    }
    #[allow(unreachable_code)]
    {
        let _ = audio;
        Err(UNSUPPORTED.into())
    }
}

#[tauri::command]
pub async fn mobile_host_stop(_app: AppHandle) -> BackendResult<Value> {
    #[cfg(target_os = "android")]
    return native(&_app, "stopScreenShare", json!({})).await;
    #[cfg(target_os = "ios")]
    {
        crate::ios::stop_broadcast();
        return Ok(json!({ "stopped": true }));
    }
    #[allow(unreachable_code)]
    Err(UNSUPPORTED.into())
}

// ---------------------------------------------------------------- input control of this device

#[tauri::command]
pub async fn mobile_input_status(_app: AppHandle) -> BackendResult<Value> {
    #[cfg(target_os = "android")]
    return native(&_app, "inputStatus", json!({})).await;
    #[allow(unreachable_code)]
    Ok(json!({ "accessibility": "notApplicable", "shizuku": "notApplicable", "backend": "none" }))
}

#[tauri::command]
pub async fn mobile_open_accessibility_settings(_app: AppHandle) -> BackendResult<Value> {
    #[cfg(target_os = "android")]
    return native(&_app, "openAccessibilitySettings", json!({})).await;
    #[allow(unreachable_code)]
    Err(UNSUPPORTED.into())
}

#[tauri::command]
pub async fn mobile_request_shizuku(_app: AppHandle) -> BackendResult<Value> {
    #[cfg(target_os = "android")]
    return native(&_app, "requestShizuku", json!({})).await;
    #[allow(unreachable_code)]
    Err(UNSUPPORTED.into())
}

/// `kinds`: any of `notifications`, `audio`, `bluetooth`.
#[tauri::command]
pub async fn mobile_request_permissions(_app: AppHandle, kinds: Vec<String>) -> BackendResult<Value> {
    #[cfg(target_os = "android")]
    return native(&_app, "requestRuntimePermissions", json!({ "kinds": kinds })).await;
    #[allow(unreachable_code)]
    {
        let _ = kinds;
        Ok(json!({}))
    }
}

// ---------------------------------------------------------------- remote file browser

#[tauri::command]
pub async fn mobile_vfs_list_dir(session_id: String, share_id: u32, path: String) -> BackendResult<Value> {
    let s = session(&session_id)?;
    let entries = s.vfs_read_dir(share_id, &path).await.map_err(|e| aether_core::engine::error_code(&e))?;
    serde_json::to_value(entries).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn mobile_vfs_download(app: AppHandle, session_id: String, share_id: u32, path: String) -> BackendResult<Value> {
    let s = session(&session_id)?;
    let (_, _, received) = crate::sandbox_dirs(&app);
    let dest = s.vfs_download(share_id, &path, &received).await.map_err(|e| aether_core::engine::error_code(&e))?;
    Ok(json!({ "path": dest.display().to_string() }))
}

fn session(id: &str) -> BackendResult<std::sync::Arc<aether_core::client::ClientSession>> {
    let id: u64 = id.trim().parse().map_err(|_| "errors.invalid".to_owned())?;
    let e = backend::engine_handle().ok_or_else(|| backend::NOT_WIRED.to_owned())?;
    e.client(id).map_err(|e| aether_core::engine::error_code(&e))
}

// ---------------------------------------------------------------- Bluetooth HID controller

/// Report builder for the phone-as-keyboard/trackpad mode (state persists across calls).
pub struct HidState(pub Mutex<HidReportBuilder>);

impl Default for HidState {
    fn default() -> Self {
        Self(Mutex::new(HidReportBuilder::new(HidConfig::default())))
    }
}

#[tauri::command]
pub async fn mobile_hid_status(_app: AppHandle) -> BackendResult<Value> {
    #[cfg(target_os = "android")]
    return native(&_app, "hidStatus", json!({})).await;
    #[allow(unreachable_code)]
    Ok(json!({ "supported": false }))
}

#[tauri::command]
pub async fn mobile_hid_start(_app: AppHandle) -> BackendResult<Value> {
    // The Kotlin service reads `aether_input::hid_report::REPORT_DESCRIPTOR` via JNI.
    #[cfg(target_os = "android")]
    return native(&_app, "hidStart", json!({})).await;
    #[allow(unreachable_code)]
    Err(UNSUPPORTED.into())
}

#[tauri::command]
pub async fn mobile_hid_stop(_app: AppHandle) -> BackendResult<Value> {
    #[cfg(target_os = "android")]
    return native(&_app, "hidStop", json!({})).await;
    #[allow(unreachable_code)]
    Err(UNSUPPORTED.into())
}

#[tauri::command]
pub async fn mobile_hid_connect(_app: AppHandle, address: String) -> BackendResult<Value> {
    #[cfg(target_os = "android")]
    return native(&_app, "hidConnect", json!({ "address": address })).await;
    #[allow(unreachable_code)]
    {
        let _ = address;
        Err(UNSUPPORTED.into())
    }
}

/// Convert an `InputEvent` into HID reports (`aether_input::hid_report`) and send them.
#[tauri::command]
pub async fn mobile_hid_send(state: State<'_, HidState>, event: InputEvent) -> BackendResult<()> {
    let reports = state.0.lock().convert(&event).map_err(|_| "errors.invalid".to_owned())?;
    #[cfg(target_os = "android")]
    for r in reports {
        crate::android::hid_send_report(r.id, &r.data)?;
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = reports;
        return Err(UNSUPPORTED.into());
    }
    #[allow(unreachable_code)]
    Ok(())
}

// ---------------------------------------------------------------- QR scanner

/// iOS: native AVFoundation QR scanner → `{content}` (empty when cancelled).
/// Android uses `tauri-plugin-barcode-scanner` from the UI and gets `errors.unsupported` here.
#[tauri::command]
pub async fn mobile_scan_qr() -> BackendResult<Value> {
    #[cfg(target_os = "ios")]
    return crate::ios::scan_qr().await.map(|content| json!({ "content": content }));
    #[allow(unreachable_code)]
    Err(UNSUPPORTED.into())
}

// ---------------------------------------------------------------- device name

static PENDING_NAME: Mutex<Option<String>> = Mutex::new(None);

/// Use the platform's device model/name instead of a generic default (`localhost`).
/// May be called before the engine started; applied once it is.
pub fn apply_default_device_name(name: String) {
    let name = name.trim().to_owned();
    if !name.is_empty() {
        *PENDING_NAME.lock() = Some(name);
    }
    flush_default_device_name();
}

fn flush_default_device_name() {
    let Some(engine) = backend::engine_handle() else { return };
    let Some(name) = PENDING_NAME.lock().take() else { return };
    tauri::async_runtime::spawn(async move {
        let mut s = engine.settings();
        if matches!(s.device_name.trim(), "" | "localhost" | "AetherLink") {
            s.device_name = name;
            if let Err(e) = engine.set_settings(s).await {
                tracing::warn!("cannot set device name: {e}");
            }
        }
    });
}
