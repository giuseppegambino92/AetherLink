// Desktop entry point (development only); on Android/iOS the platform runs `run()` via
// `tauri::mobile_entry_point`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    aetherlink_mobile_lib::run();
}
