//! `AetherLink --selftest-live [--with-virtual-display] [--no-ghost]`
//!
//! Boots real subsystems on this machine and prints a structured JSON health report:
//! permissions (status only — nothing is requested), a 1 s ScreenCaptureKit probe (only if
//! already permitted), a full local host + client session over QUIC, the Ghost Mode
//! shielding-level curtain for 1 s with backlight dim + exact restore, the audio loopback
//! chain, and optionally a temporary HiDPI virtual display. Exit code 0 = every executed
//! check passed.

use aether_display::{brightness, ghost, virtual_display};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub fn run(args: &[String]) -> i32 {
    // Never touch the user's real Keychain identity from the self-test engines.
    // SAFETY: single-threaded at this point (before any runtime/thread is started).
    unsafe { std::env::set_var("AETHERLINK_NO_KEYCHAIN", "1") };
    let started = Instant::now();
    let with_vd = args.iter().any(|a| a == "--with-virtual-display");
    let no_ghost = args.iter().any(|a| a == "--no-ghost");
    let mut failures: Vec<&str> = Vec::new();

    let permissions = aether_core::selftest::permissions();
    let screen_ok = permissions.screen_recording == "granted";

    let capture = aether_core::selftest::capture_probe(Duration::from_secs(1));
    if capture.error.is_some() || (capture.ran && capture.frames == 0) {
        failures.push("capture");
    }

    let workdir = std::env::temp_dir().join(format!("aetherlink-selftest-{}", std::process::id()));
    let session = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => {
            let r = rt.block_on(aether_core::selftest::live_session(&workdir, screen_ok, Duration::from_secs(2)));
            rt.shutdown_timeout(Duration::from_secs(2));
            r
        }
        Err(e) => {
            failures.push("runtime");
            return finish(json!({ "error": e.to_string() }), &failures, started);
        }
    };
    let _ = std::fs::remove_dir_all(&workdir);
    if !session.ok {
        failures.push("session");
    }

    let ghost_report = if no_ghost { json!({ "skipped": "--no-ghost" }) } else { ghost_check(&mut failures) };
    let audio = audio_check(&mut failures);
    let vd = if with_vd { virtual_display_check(&mut failures) } else { json!({ "skipped": "pass --with-virtual-display" }) };

    let report = json!({
        "app": { "name": "AetherLink", "version": env!("CARGO_PKG_VERSION"), "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "permissions": permissions,
        "capture": capture,
        "session": session,
        "ghostMode": ghost_report,
        "audio": audio,
        "virtualDisplay": vd,
    });
    finish(report, &failures, started)
}

fn finish(mut report: Value, failures: &[&str], started: Instant) -> i32 {
    report["ok"] = json!(failures.is_empty());
    report["failed"] = json!(failures);
    report["durationMs"] = json!(started.elapsed().as_millis() as u64);
    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    if failures.is_empty() { 0 } else { 1 }
}

/// Curtain at CGShieldingWindowLevel for 1 s with backlight dimmed, then verify the exact
/// previous brightness is restored. Runs on the main thread (AppKit requirement).
fn ghost_check(failures: &mut Vec<&str>) -> Value {
    let before = brightness::read_all().ok();
    let restore_file = std::env::temp_dir().join(format!("aetherlink-selftest-brightness-{}.json", std::process::id()));
    let opts = ghost::GhostOptions { dim_backlight: true, restore_file: Some(restore_file.clone()) };
    let t = Instant::now();
    let status = match ghost::enable(&opts) {
        Ok(s) => s,
        Err(e) => {
            failures.push("ghost");
            return json!({ "error": e.to_string() });
        }
    };
    #[cfg(target_os = "macos")]
    ghost::pump_main_runloop(1.0);
    #[cfg(not(target_os = "macos"))]
    std::thread::sleep(Duration::from_secs(1));
    let during = brightness::read_all().ok();
    let disabled = ghost::disable();
    let after = brightness::read_all().ok();
    let shown_ms = t.elapsed().as_millis() as u64;

    let dimmed_to_zero = during.as_ref().is_some_and(|d| !d.levels.is_empty() && d.levels.iter().all(|(_, v)| *v < 0.02));
    let restored_exactly = match (&before, &after) {
        (Some(b), Some(a)) => b.levels.len() == a.levels.len() && b.levels.iter().zip(&a.levels).all(|((i, x), (j, y))| i == j && (x - y).abs() < 0.005),
        _ => false,
    };
    let at_shielding = status.window_level == status.shielding_level && status.curtain_windows > 0;
    if !at_shielding || disabled.is_err() || !restored_exactly || ghost::status().active || restore_file.exists() {
        failures.push("ghost");
    }
    json!({
        "curtainWindows": status.curtain_windows,
        "windowLevel": status.window_level,
        "shieldingLevel": status.shielding_level,
        "aboveMenuBarAndDock": at_shielding,
        "shownMs": shown_ms,
        "backlight": {
            "displays": before.as_ref().map(|b| b.levels.len()).unwrap_or(0),
            "before": before.as_ref().map(|b| &b.levels),
            "dimmedToZero": dimmed_to_zero,
            "restoredExactly": restored_exactly,
            "error": status.backlight_error,
        },
        "disableError": disabled.err().map(|e| e.to_string()),
    })
}

/// Opus encode → `Datagram::Audio` serialize/deserialize → jitter buffer (one lost, two
/// reordered packets) → decode, measured against the injected 1 kHz tone.
fn audio_check(failures: &mut Vec<&str>) -> Value {
    let report = aether_core::audio::loopback_selftest();
    let v = serde_json::to_value(&report).unwrap_or(Value::Null);
    if v["passed"].as_bool() != Some(true) {
        failures.push("audio");
    }
    v
}

fn virtual_display_check(failures: &mut Vec<&str>) -> Value {
    let before = virtual_display::online_display_count();
    let t = Instant::now();
    match virtual_display::VirtualDisplay::create("AetherLink Self-test", virtual_display::VirtualDisplaySpec::FHD_16_9) {
        Ok(vd) => {
            let online = virtual_display::online_display_count();
            let id = vd.display_id();
            let spec = vd.spec();
            drop(vd);
            std::thread::sleep(Duration::from_millis(300));
            let ok = online == before + 1;
            if !ok {
                failures.push("virtualDisplay");
            }
            json!({ "displayId": id, "spec": spec, "createdMs": t.elapsed().as_millis() as u64, "onlineBefore": before, "onlineWhileAttached": online, "onlineAfter": virtual_display::online_display_count() })
        }
        Err(e) => {
            failures.push("virtualDisplay");
            json!({ "error": e.to_string() })
        }
    }
}
