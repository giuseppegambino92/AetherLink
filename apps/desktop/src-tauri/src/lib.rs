//! AetherLink desktop shell (Tauri 2).
//!
//! * `commands` — the Tauri command contract (docs/ARCHITECTURE.md).
//! * `backend`  — engine boundary; `// WIRE: aether-core` stubs until the facade lands.
//! * `shell`    — tray, locale, windows, HUD hotkey, clipboard (UI-only, implemented).

pub mod backend;
pub mod commands;
pub mod selftest;
pub mod shell;

#[cfg(test)]
mod contract_tests;

#[cfg(target_os = "macos")]
use tauri::RunEvent;
use tauri::{Manager, WindowEvent};

pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(
            tauri_plugin_window_state::Builder::new()
                // Only the dashboard remembers its geometry; session windows size to the remote display.
                .with_filter(|label| label == "main")
                .build(),
        )
        .manage(shell::UiState::default())
        .setup(|app| {
            shell::create_tray(app.handle())?;
            // The engine forwards its events (session-state, session-telemetry, …) via `app.emit`.
            if let Err(e) = backend::start(app.handle()) {
                tracing::error!("AetherLink engine failed to start: {e}");
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            let label = window.label();
            match event {
                // Closing the dashboard keeps AetherLink reachable from the menu bar.
                WindowEvent::CloseRequested { api, .. } if label == "main" => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                WindowEvent::Focused(focused) if label.starts_with("session-") => {
                    shell::on_session_focus(window.app_handle(), *focused);
                }
                WindowEvent::Destroyed if label.starts_with("session-") => {
                    shell::on_session_window_closed(window.app_handle(), label);
                    // The webview's `beforeunload` → `disconnect` covers the normal path; if this
                    // was the last session window, make sure no orphaned session keeps streaming.
                    let others = window.app_handle().webview_windows().keys().any(|l| l.starts_with("session-") && l != label);
                    if !others {
                        tauri::async_runtime::spawn(backend::disconnect_all());
                    }
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_identity,
            commands::rotate_password,
            commands::get_settings,
            commands::set_settings,
            commands::get_permissions,
            commands::owner_verification_available,
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
            commands::set_audio_volume,
            commands::list_audio_outputs,
            commands::set_audio_output,
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
            commands::local_ghost_off,
            commands::set_virtual_display,
            commands::hid_backends,
            commands::hid_start,
            commands::hid_stop,
            commands::hid_status,
            commands::hid_paired_devices,
            commands::hid_connect,
            commands::hid_send_input,
            commands::ui_set_locale,
            commands::ui_tray_state,
            commands::ui_open_session_window,
            commands::ui_take_pending_connect,
            commands::ui_copy_text,
            commands::ui_set_hud_hotkey,
            commands::ui_show_main,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build the AetherLink desktop app");

    app.run(|_app, _event| {
        if let tauri::RunEvent::ExitRequested { .. } = _event {
            tauri::async_runtime::block_on(backend::shutdown());
        }
        // macOS: clicking the Dock icon re-opens the hidden dashboard.
        #[cfg(target_os = "macos")]
        if let RunEvent::Reopen { has_visible_windows: false, .. } = &_event {
            shell::show_main(_app);
        }
    });
}
