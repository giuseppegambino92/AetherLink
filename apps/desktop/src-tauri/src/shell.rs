//! Window shell: menu-bar status item, locale, session windows, HUD hotkey,
//! clipboard. Nothing here talks to the engine or invents engine data — the
//! tray only shows what the frontend pushed via `ui_tray_state`.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

pub const TRAY_ID: &str = "aetherlink-tray";
pub const DEFAULT_HUD_HOTKEY: &str = "CmdOrCtrl+Shift+Period";
pub const SUPPORTED_LOCALES: [&str; 2] = ["en", "it"];

const LOCALE_EN: &str = include_str!("../../ui/locales/en.json");
const LOCALE_IT: &str = include_str!("../../ui/locales/it.json");

/// What the tray menu displays; pushed by the main window.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TrayState {
    /// Formatted local ID (`123 456 789`) once the engine reported it.
    pub device_id: Option<String>,
    pub active_connections: u32,
    pub ghost_mode: bool,
}

/// Connect parameters handed from the dashboard to a freshly opened session window.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingConnect {
    pub target: String,
    pub password: Option<String>,
    pub title: String,
}

pub struct UiState {
    locale: Mutex<String>,
    tray: Mutex<TrayState>,
    pending: Mutex<HashMap<String, PendingConnect>>,
    hud_hotkey: Mutex<String>,
    next_session: AtomicU32,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            locale: Mutex::new("en".into()),
            tray: Mutex::new(TrayState::default()),
            pending: Mutex::new(HashMap::new()),
            hud_hotkey: Mutex::new(DEFAULT_HUD_HOTKEY.into()),
            next_session: AtomicU32::new(1),
        }
    }
}

impl UiState {
    pub fn take_pending(&self, label: &str) -> Option<PendingConnect> {
        self.pending.lock().remove(label)
    }
}

// ---------------------------------------------------------------- i18n (tray only)

fn locale_table(locale: &str) -> Value {
    let raw = if locale == "it" { LOCALE_IT } else { LOCALE_EN };
    serde_json::from_str(raw).unwrap_or(Value::Null)
}

fn lookup(table: &Value, key: &str) -> Option<String> {
    let mut cur = table;
    for part in key.split('.') {
        cur = cur.get(part)?;
    }
    cur.as_str().map(str::to_owned)
}

fn tr(locale: &str, key: &str, params: &[(&str, &str)]) -> String {
    let mut s = lookup(&locale_table(locale), key)
        .or_else(|| lookup(&locale_table("en"), key))
        .unwrap_or_else(|| key.to_owned());
    for (k, v) in params {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

// ---------------------------------------------------------------- tray

fn build_tray_menu(app: &AppHandle, locale: &str, st: &TrayState) -> tauri::Result<Menu<tauri::Wry>> {
    let id_text = match &st.device_id {
        Some(id) => tr(locale, "tray.idLabel", &[("id", id)]),
        None => tr(locale, "tray.idUnknown", &[]),
    };
    let id_item = MenuItem::with_id(app, "tray-id", id_text, false, None::<&str>)?;
    let copy = MenuItem::with_id(app, "tray-copy-id", tr(locale, "tray.copyId", &[]), st.device_id.is_some(), None::<&str>)?;
    let n = st.active_connections.to_string();
    let conn_key = if st.active_connections == 1 { "tray.connections.one" } else { "tray.connections.other" };
    let conns = MenuItem::with_id(app, "tray-connections", tr(locale, conn_key, &[("count", &n)]), false, None::<&str>)?;
    let quick = MenuItem::with_id(app, "tray-quick-connect", tr(locale, "tray.quickConnect", &[]), true, None::<&str>)?;
    let ghost = CheckMenuItem::with_id(app, "tray-ghost", tr(locale, "tray.ghostMode", &[]), true, st.ghost_mode, None::<&str>)?;
    let en = CheckMenuItem::with_id(app, "tray-lang-en", tr(locale, "language.en", &[]), true, locale == "en", None::<&str>)?;
    let it = CheckMenuItem::with_id(app, "tray-lang-it", tr(locale, "language.it", &[]), true, locale == "it", None::<&str>)?;
    let lang = Submenu::with_id_and_items(app, "tray-lang", tr(locale, "tray.language", &[]), true, &[&en, &it])?;
    let show = MenuItem::with_id(app, "tray-show", tr(locale, "tray.show", &[]), true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "tray-quit", tr(locale, "tray.quit", &[]), true, Some("CmdOrCtrl+Q"))?;
    Menu::with_items(
        app,
        &[
            &id_item,
            &copy,
            &PredefinedMenuItem::separator(app)?,
            &conns,
            &quick,
            &ghost,
            &PredefinedMenuItem::separator(app)?,
            &lang,
            &show,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )
}

pub fn create_tray(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let state = app.state::<UiState>();
    let locale = state.locale.lock().clone();
    let tray_state = state.tray.lock().clone();
    let menu = build_tray_menu(app, &locale, &tray_state)?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(include_bytes!("../icons/tray-template@2x.png"))?)
        .icon_as_template(true)
        .tooltip(tr(&locale, "app.name", &[]))
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(on_tray_menu)
        .build(app)
}

fn refresh_tray(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<UiState>();
    let locale = state.locale.lock().clone();
    let tray_state = state.tray.lock().clone();
    let menu = build_tray_menu(app, &locale, &tray_state).map_err(|e| e.to_string())?;
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
        tray.set_tooltip(Some(tr(&locale, "app.name", &[]))).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn on_tray_menu(app: &AppHandle, event: MenuEvent) {
    let state = app.state::<UiState>();
    match event.id().as_ref() {
        "tray-copy-id" => {
            let id = state.tray.lock().device_id.clone();
            if let Some(id) = id {
                let _ = copy_text(app, id.replace(' ', ""));
            }
        }
        "tray-quick-connect" => {
            show_main(app);
            let _ = app.emit_to("main", "open-quick-connect", ());
        }
        "tray-ghost" => {
            let enabled = {
                let mut t = state.tray.lock();
                t.ghost_mode = !t.ghost_mode;
                t.ghost_mode
            };
            let _ = app.emit("tray-ghost-mode", serde_json::json!({ "enabled": enabled }));
            let _ = refresh_tray(app);
        }
        "tray-lang-en" => {
            let _ = set_locale(app, &state, "en");
        }
        "tray-lang-it" => {
            let _ = set_locale(app, &state, "it");
        }
        "tray-show" => show_main(app),
        "tray-quit" => app.exit(0),
        _ => {}
    }
}

pub fn set_tray_state(app: &AppHandle, state: &UiState, tray: TrayState) -> Result<(), String> {
    *state.tray.lock() = tray;
    refresh_tray(app)
}

pub fn set_locale(app: &AppHandle, state: &UiState, locale: &str) -> Result<(), String> {
    if !SUPPORTED_LOCALES.contains(&locale) {
        return Err("unsupported-locale".into());
    }
    let changed = {
        let mut cur = state.locale.lock();
        let changed = *cur != locale;
        *cur = locale.to_owned();
        changed
    };
    refresh_tray(app)?;
    if changed {
        app.emit("locale-changed", serde_json::json!({ "locale": locale })).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------- windows

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

pub fn open_session_window(
    app: &AppHandle,
    state: &UiState,
    target: String,
    password: Option<String>,
    title: String,
) -> Result<String, String> {
    let n = state.next_session.fetch_add(1, Ordering::Relaxed);
    let label = format!("session-{n}");
    state
        .pending
        .lock()
        .insert(label.clone(), PendingConnect { target, password, title: title.clone() });

    // The frontend routes to the session view based on the `session-` label prefix.
    let builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::App("index.html".into()))
        .title(&title)
        .inner_size(1440.0, 900.0)
        .min_inner_size(640.0, 400.0)
        .background_color(tauri::window::Color(5, 6, 9, 255));
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(16.0, 18.0));
    if let Err(e) = builder.build() {
        state.pending.lock().remove(&label);
        return Err(e.to_string());
    }
    Ok(label)
}

/// Drop any connect parameters a session window never claimed.
pub fn on_session_window_closed(app: &AppHandle, label: &str) {
    app.state::<UiState>().pending.lock().remove(label);
    on_session_focus(app, false);
}

// ---------------------------------------------------------------- HUD hotkey

/// Registers the HUD hotkey only while a session window has focus, so the
/// shortcut never steals the key combination from other applications.
pub fn on_session_focus(app: &AppHandle, focused: bool) {
    let state = app.state::<UiState>();
    let accel = state.hud_hotkey.lock().clone();
    let gs = app.global_shortcut();
    if focused {
        if !gs.is_registered(accel.as_str()) {
            let _ = gs.on_shortcut(accel.as_str(), |app, _shortcut, event| {
                if event.state() == ShortcutState::Pressed {
                    emit_hud_toggle(app);
                }
            });
        }
    } else {
        let any_session_focused = app
            .webview_windows()
            .iter()
            .any(|(label, w)| label.starts_with("session-") && w.is_focused().unwrap_or(false));
        if !any_session_focused {
            let _ = gs.unregister(accel.as_str());
        }
    }
}

fn emit_hud_toggle(app: &AppHandle) {
    for (label, w) in app.webview_windows() {
        if label.starts_with("session-") && w.is_focused().unwrap_or(false) {
            let _ = app.emit_to(label.as_str(), "hud-toggle", ());
        }
    }
}

pub fn set_hud_hotkey(app: &AppHandle, state: &UiState, accelerator: &str) -> Result<(), String> {
    // Validate before swapping so a typo never leaves the user without a hotkey.
    let parsed = accelerator
        .parse::<tauri_plugin_global_shortcut::Shortcut>()
        .map_err(|e| e.to_string())?;
    let gs = app.global_shortcut();
    let old = std::mem::replace(&mut *state.hud_hotkey.lock(), accelerator.to_owned());
    let was_registered = gs.is_registered(old.as_str());
    if was_registered {
        let _ = gs.unregister(old.as_str());
        gs.on_shortcut(parsed, |app, _s, event| {
            if event.state() == ShortcutState::Pressed {
                emit_hud_toggle(app);
            }
        })
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------- clipboard

pub fn copy_text(app: &AppHandle, text: String) -> Result<(), String> {
    app.clipboard().write_text(text).map_err(|e| e.to_string())
}

/// Ghost Mode privacy curtain: an opaque, borderless, always-on-top window on every
/// physical monitor of the *host*. AetherLink's own process is excluded from screen
/// capture, so the remote controller keeps seeing the real desktop while people in
/// front of the machine only see the curtain.
pub fn set_privacy_curtain(app: &AppHandle, enabled: bool) -> bool {
    let app = app.clone();
    let handle = app.clone();
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    let scheduled = handle.run_on_main_thread(move || {
        for (label, w) in app.webview_windows() {
            if label.starts_with("curtain-") {
                let _ = w.destroy();
            }
        }
        if !enabled {
            let _ = tx.send(true);
            return;
        }
        let monitors = app.available_monitors().unwrap_or_default();
        let mut raised = 0usize;
        for (i, m) in monitors.iter().enumerate() {
            let scale = m.scale_factor();
            let pos = m.position().to_logical::<f64>(scale);
            let size = m.size().to_logical::<f64>(scale);
            let built = WebviewWindowBuilder::new(&app, format!("curtain-{i}"), WebviewUrl::App("curtain.html".into()))
                .decorations(false)
                .resizable(false)
                .always_on_top(true)
                .visible_on_all_workspaces(true)
                .skip_taskbar(true)
                .focused(false)
                .shadow(false)
                .position(pos.x, pos.y)
                .inner_size(size.width, size.height)
                .build();
            match built {
                Ok(_) => raised += 1,
                Err(e) => tracing::warn!("privacy curtain on monitor {i} failed: {e}"),
            }
        }
        // Protection only counts when every monitor is covered.
        let _ = tx.send(!monitors.is_empty() && raised == monitors.len());
    });
    if scheduled.is_err() {
        return !enabled;
    }
    // Called from a blocking thread (never the main thread), so waiting here cannot deadlock.
    rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or(false)
}

/// Ctrl+Opt+Cmd+Esc lifts Ghost Mode locally (registered only while the curtain is up), so
/// someone at the host can always get their screen back.
pub const GHOST_PANIC_HOTKEY: &str = "Control+Alt+Super+Escape";

pub fn set_ghost_panic_hotkey(app: &AppHandle, enabled: bool) {
    let gs = app.global_shortcut();
    let registered = gs.is_registered(GHOST_PANIC_HOTKEY);
    if enabled && !registered {
        let res = gs.on_shortcut(GHOST_PANIC_HOTKEY, |_app, _s, event| {
            if event.state() == ShortcutState::Pressed {
                tauri::async_runtime::spawn(async {
                    let _ = crate::backend::local_ghost_off().await;
                });
            }
        });
        if let Err(e) = res {
            tracing::warn!("cannot register the Ghost Mode panic hotkey: {e}");
        }
    } else if !enabled && registered {
        let _ = gs.unregister(GHOST_PANIC_HOTKEY);
    }
}
