//! Headless viewer probe: start a desktop engine, connect to a host by `host:port` +
//! one-time password, watch the stream for a while and print a JSON report.
//!
//! Used by `apps/mobile/scripts/e2e-android.sh` (Android emulator host, UDP redirected
//! to 127.0.0.1), but works against any AetherLink host.
//!
//! ```text
//! cargo run -p aether-core --example connect_probe -- \
//!     --target 127.0.0.1:21118 --password abcd-efgh [--secs 10] [--tap 0.5,0.95] [--taps 1]
//! ```
//!
//! `--tap x,y` (normalized 0..1 on the first display) sends a mouse move + left click after
//! the stream is up (the Android host turns it into an accessibility tap); `--taps N` repeats it
//! every 2 s (for idempotent targets on a slow emulator, where a janky WebView can miss one).
//! Exit code: 0 when connected and at least one keyframe arrived, 1 otherwise.

use aether_core::settings::Settings;
use aether_core::{ByteSink, Engine, EngineConfig, EventSink, ui_video};
use aether_protocol::{ClipboardMode, InputEvent, MouseButton};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Events(Mutex<Vec<(Instant, String, Value)>>);

impl EventSink for Events {
    fn emit(&self, name: &str, payload: Value) {
        self.0
            .lock()
            .push((Instant::now(), name.to_owned(), payload));
    }
}

#[derive(Default)]
struct Video {
    configs: Vec<Value>,
    access_units: u64,
    keyframes: u64,
    bytes: u64,
    first_au: Option<Instant>,
    first_key: Option<Instant>,
    last_au: Option<Instant>,
    resets: u64,
}

struct Args {
    target: String,
    password: String,
    secs: u64,
    tap: Option<(f32, f32)>,
    taps: u32,
}

fn args() -> Args {
    let mut a = Args {
        target: String::new(),
        password: String::new(),
        secs: 10,
        tap: None,
        taps: 1,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().unwrap_or_default();
        match k.as_str() {
            "--target" => a.target = v,
            "--password" => a.password = v,
            "--secs" => a.secs = v.parse().unwrap_or(10),
            "--taps" => a.taps = v.parse().unwrap_or(1).max(1),
            "--tap" => {
                let mut p = v.split(',').map(|s| s.trim().parse::<f32>());
                if let (Some(Ok(x)), Some(Ok(y))) = (p.next(), p.next()) {
                    a.tap = Some((x, y));
                }
            }
            _ => {
                eprintln!("unknown argument {k}");
                std::process::exit(2);
            }
        }
    }
    if a.target.is_empty() || a.password.is_empty() {
        eprintln!(
            "usage: connect_probe --target host:port --password PW [--secs N] [--tap x,y] [--taps N]"
        );
        std::process::exit(2);
    }
    a
}

fn ms(a: Instant, b: Option<Instant>) -> Value {
    b.map(|b| json!(b.saturating_duration_since(a).as_millis() as u64))
        .unwrap_or(Value::Null)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let a = args();
    // SAFETY: set before any engine thread starts; file identity store, no keychain prompts.
    unsafe { std::env::set_var("AETHERLINK_NO_KEYCHAIN", "1") };
    let tmp = std::env::temp_dir().join(format!("aetherlink-probe-{}", std::process::id()));
    let settings = Settings {
        advertise_on_lan: false,
        clipboard_mode: ClipboardMode::Disabled,
        device_name: "connect_probe".into(),
        ..Settings::default()
    };
    std::fs::create_dir_all(tmp.join("config")).unwrap();
    settings.save(&tmp.join("config/settings.json")).unwrap();
    let cfg = EngineConfig {
        config_dir: tmp.join("config"),
        cache_dir: tmp.join("cache"),
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        download_dir: Some(tmp.join("downloads")),
        capturer: None,
        audio_capturer: None,
        audio_null_output: true,
    };
    let events = Arc::new(Events::default());
    let engine = Engine::start(cfg, events.clone())
        .await
        .expect("engine start");

    let video: Arc<Mutex<Video>> = Arc::default();
    let v2 = video.clone();
    let sink: ByteSink = Arc::new(move |m: Vec<u8>| {
        let mut v = v2.lock();
        let now = Instant::now();
        match m.first().copied() {
            Some(ui_video::CONFIG) => {
                if let Ok(j) = serde_json::from_slice::<Value>(&m[1..]) {
                    let mut j = j;
                    if let Some(o) = j.as_object_mut() {
                        o.remove("description");
                    }
                    v.configs.push(j);
                }
            }
            Some(ui_video::VIDEO) => {
                v.access_units += 1;
                v.bytes += m.len().saturating_sub(10) as u64;
                v.first_au.get_or_insert(now);
                v.last_au = Some(now);
                if m.get(1) == Some(&1) {
                    v.keyframes += 1;
                    v.first_key.get_or_insert(now);
                }
            }
            Some(ui_video::RESET) => v.resets += 1,
            _ => {}
        }
    });

    let t0 = Instant::now();
    let sid = engine
        .connect(&a.target, Some(a.password.clone()), sink)
        .expect("connect");
    let state = loop {
        let found = events
            .0
            .lock()
            .iter()
            .find(|(_, n, v)| {
                n == "session-state"
                    && v["sessionId"] == sid
                    && (v["state"] == "connected" || v["state"] == "failed")
            })
            .map(|(t, _, v)| (*t, v.clone()));
        if let Some(s) = found {
            break Some(s);
        }
        if t0.elapsed() > Duration::from_secs(30) {
            break None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let (connected_at, state_ev) = match state {
        Some((t, v)) => (Some(t), v),
        None => (None, json!({ "state": "timeout" })),
    };
    let connected = state_ev["state"] == "connected";

    let mut report = json!({
        "target": a.target,
        "connectMs": connected_at.map(|t| t.duration_since(t0).as_millis() as u64),
        "state": state_ev["state"],
        "errorCode": state_ev["errorCode"],
        "detail": state_ev["detail"],
        "peerName": state_ev["peerName"],
        "path": state_ev["path"],
    });

    if connected {
        let session = engine.client(sid).ok();
        let watch_start = Instant::now();
        let mut taps_report: Vec<Value> = Vec::new();
        let mut last_tap: Option<Instant> = None;
        while watch_start.elapsed() < Duration::from_secs(a.secs) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            // Tap once the picture is flowing (and the input thread had the displays).
            if (taps_report.len() as u32) < a.taps
                && last_tap.is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
                && let (Some((x, y)), Some(s)) = (a.tap, session.as_ref())
                && video.lock().keyframes > 0
                && watch_start.elapsed() > Duration::from_secs(2)
            {
                last_tap = Some(Instant::now());
                let display_id = s.displays().first().map(|d| d.id).unwrap_or(0);
                let sent_at = Instant::now();
                let mut errs = Vec::new();
                for ev in [
                    InputEvent::MouseMove { display_id, x, y },
                    InputEvent::MouseButton {
                        button: MouseButton::Left,
                        pressed: true,
                    },
                    InputEvent::MouseButton {
                        button: MouseButton::Left,
                        pressed: false,
                    },
                ] {
                    if let Err(e) = s.send_input(ev).await {
                        errs.push(e.to_string());
                    }
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                taps_report.push(json!({
                    "displayId": display_id, "x": x, "y": y,
                    "sentAtMs": sent_at.duration_since(t0).as_millis() as u64,
                    "sendErrors": errs,
                }));
            }
        }
        {
            let v = video.lock();
            let watched = watch_start.elapsed().as_secs_f64();
            let ev = events.0.lock();
            let telemetry: Vec<&Value> = ev
                .iter()
                .filter(|(_, n, v)| n == "session-telemetry" && v["sessionId"] == sid)
                .map(|(_, _, v)| v)
                .collect();
            let host_errors: Vec<Value> = ev
            .iter()
            .filter(|(_, n, v)| n == "session-state" && v["sessionId"] == sid && !v["errorCode"].is_null())
            .map(|(_, _, v)| json!({ "state": v["state"], "errorCode": v["errorCode"], "detail": v["detail"] }))
            .collect();
            let displays = session.as_ref().map(|s| s.displays()).unwrap_or_default();
            let o = report.as_object_mut().unwrap();
            o.insert(
                "displays".into(),
                json!(displays.iter().map(|d| json!({
                "id": d.id, "name": d.name, "width": d.bounds.width, "height": d.bounds.height,
                "scale": d.scale, "refreshHz": d.refresh_hz, "primary": d.primary,
            })).collect::<Vec<_>>()),
            );
            o.insert("videoConfigs".into(), json!(v.configs));
            o.insert(
            "video".into(),
            json!({
                "watchSecs": (watched * 10.0).round() / 10.0,
                "accessUnits": v.access_units,
                "keyframes": v.keyframes,
                "bytes": v.bytes,
                "avgFps": if watched > 0.0 { ((v.access_units as f64 / watched) * 10.0).round() / 10.0 } else { 0.0 },
                "kbps": if watched > 0.0 { (v.bytes as f64 * 8.0 / watched / 1000.0).round() } else { 0.0 },
                "firstAccessUnitMs": ms(t0, v.first_au),
                "firstKeyframeMs": ms(t0, v.first_key),
                "resets": v.resets,
            }),
        );
            o.insert("telemetryEvents".into(), json!(telemetry.len()));
            o.insert(
                "lastTelemetry".into(),
                telemetry
                    .last()
                    .map(|v| (*v).clone())
                    .unwrap_or(Value::Null),
            );
            o.insert("hostErrors".into(), json!(host_errors));
            o.insert("taps".into(), json!(taps_report));
        } // guards released before awaiting the disconnect
        let _ = tokio::time::timeout(Duration::from_secs(5), engine.disconnect(sid)).await;
    }
    let ok = connected && video.lock().keyframes > 0;
    report
        .as_object_mut()
        .unwrap()
        .insert("ok".into(), json!(ok));
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    let _ = tokio::time::timeout(Duration::from_secs(5), engine.shutdown()).await;
    let _ = std::fs::remove_dir_all(&tmp);
    std::process::exit(if ok { 0 } else { 1 });
}
