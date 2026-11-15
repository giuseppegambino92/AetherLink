//! AetherLink mobile shell (Tauri 2, Android + iOS).
//!
//! The engine command surface is *shared with the desktop app*: `backend.rs` and
//! `commands.rs` are compiled straight from `apps/desktop/src-tauri/src` (no copies), and
//! [`shell`] provides the small window-shell API they expect, adapted to a single
//! full-screen webview. Mobile-only features live in [`mobile`] (`mobile_*` commands)
//! with the platform bridges in `android.rs` (JNI ↔ Kotlin) and `ios.rs` (C ABI ↔ Swift).

#[path = "../../../desktop/src-tauri/src/backend.rs"]
pub mod backend;
#[path = "../../../desktop/src-tauri/src/commands.rs"]
pub mod commands;
pub mod mobile;
pub mod shell;

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "ios")]
mod ios;
mod logging;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logging::init();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(mobile::native_plugin());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());

    builder
        .manage(shell::UiState::default())
        .manage(mobile::HidState::default())
        .setup(|app| {
            let cfg = mobile::engine_config(app.handle());
            tracing::info!(?cfg, "starting AetherLink engine");
            if let Err(e) = backend::start_with(app.handle(), cfg) {
                tracing::error!("AetherLink engine failed to start: {e}");
            }
            mobile::install_platform(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // shared engine contract (docs/ARCHITECTURE.md)
            commands::get_identity,
            commands::rotate_password,
            commands::get_settings,
            commands::set_settings,
            commands::get_permissions,
            commands::open_permission_settings,
            commands::list_lan_peers,
            commands::list_trusted_devices,
            commands::remove_trusted_device,
            commands::connect,
            commands::disconnect,
            commands::send_input,
            commands::select_display,
            commands::set_ghost_mode,
            commands::set_lossless_text,
            commands::set_audio,
            commands::set_quality,
            commands::dom_subscribe,
            commands::dom_search,
            commands::dom_action,
            commands::dom_set_value,
            commands::ports_scan,
            commands::port_forward,
            commands::port_unforward,
            commands::terminal_open,
            commands::terminal_input,
            commands::terminal_resize,
            commands::terminal_close,
            commands::vfs_list_shares,
            commands::vfs_mount,
            commands::vfs_unmount,
            commands::list_mounts,
            commands::list_local_shares,
            commands::add_local_share,
            commands::remove_local_share,
            commands::send_files,
            commands::dvr_index,
            commands::dvr_seek,
            commands::dvr_live,
            commands::dvr_ocr,
            commands::host_status,
            commands::kick_session,
            commands::respond_incoming,
            commands::ui_set_locale,
            commands::ui_copy_text,
            // mobile-only
            mobile::mobile_platform,
            mobile::mobile_host_status,
            mobile::mobile_host_start,
            mobile::mobile_host_stop,
            mobile::mobile_input_status,
            mobile::mobile_open_accessibility_settings,
            mobile::mobile_request_shizuku,
            mobile::mobile_request_permissions,
            mobile::mobile_vfs_list_dir,
            mobile::mobile_vfs_download,
            mobile::mobile_hid_status,
            mobile::mobile_hid_start,
            mobile::mobile_hid_stop,
            mobile::mobile_hid_connect,
            mobile::mobile_hid_send,
            mobile::mobile_scan_qr,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run the AetherLink mobile app");
}

/// Directories inside the app sandbox (used by [`mobile::engine_config`]).
pub(crate) fn sandbox_dirs(app: &tauri::AppHandle) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let p = app.path();
    let base = p.app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("AetherLink"));
    let config = p.app_config_dir().unwrap_or_else(|_| base.join("config"));
    let cache = p.app_cache_dir().unwrap_or_else(|_| base.join("cache"));
    (config, cache, base.join("Received"))
}
