// Prevents an additional console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--selftest-live") {
        std::process::exit(aetherlink_desktop_lib::selftest::run(&args));
    }
    aetherlink_desktop_lib::run();
}
