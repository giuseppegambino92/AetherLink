//! Live self-test building blocks (`AetherLink --selftest-live`).
//!
//! Everything here runs real code paths on the local machine: two complete engines over
//! localhost QUIC, the real ScreenCaptureKit capturer when permission is already granted
//! (never prompting), the encoder chosen by the hybrid pipeline, the DVR ring.

use crate::events::EventSink;
use crate::settings::Settings;
use crate::ui_video;
use crate::{Engine, EngineConfig};
use aether_protocol::ClipboardMode;
use parking_lot::Mutex;
use serde::Serialize;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionStatus {
    pub screen_recording: String,
    pub accessibility: String,
    pub input_monitoring: String,
}

pub fn permissions() -> PermissionStatus {
    let r = aether_input::permissions::report();
    let s = |v: &dyn std::fmt::Debug| format!("{v:?}").to_lowercase();
    PermissionStatus {
        screen_recording: s(&r.screen_recording),
        accessibility: s(&r.accessibility),
        input_monitoring: s(&r.input_monitoring),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureProbe {
    pub ran: bool,
    pub skipped_reason: Option<String>,
    pub displays: usize,
    pub frames: u32,
    pub width: u32,
    pub height: u32,
    pub zero_copy: bool,
    pub error: Option<String>,
}

/// Capture the main display for `duration` — only when permission is already granted.
pub fn capture_probe(duration: Duration) -> CaptureProbe {
    let mut p = CaptureProbe { ran: false, skipped_reason: None, displays: 0, frames: 0, width: 0, height: 0, zero_copy: false, error: None };
    if !aether_capture::permission_granted() {
        p.skipped_reason = Some("screen recording permission not granted (not requested by the self-test)".into());
        return p;
    }
    let displays = match aether_capture::list_displays() {
        Ok(d) => d,
        Err(e) => {
            p.error = Some(e.to_string());
            return p;
        }
    };
    p.displays = displays.len();
    let Some(main) = displays.iter().find(|d| d.primary).or(displays.first()).cloned() else {
        p.error = Some("no displays".into());
        return p;
    };
    let mut cap = match aether_capture::create_capturer() {
        Ok(c) => c,
        Err(e) => {
            p.error = Some(e.to_string());
            return p;
        }
    };
    let stats = Arc::new(Mutex::new((0u32, 0u32, 0u32, false)));
    let st = stats.clone();
    let started = cap.start(
        aether_protocol::CaptureTarget::Display { display_id: main.id },
        aether_capture::CaptureConfig { fps: 60, ..Default::default() },
        Box::new(move |ev| {
            if let aether_capture::CaptureEvent::Frame(f) = ev {
                let mut s = st.lock();
                s.0 += 1;
                s.1 = f.width;
                s.2 = f.height;
                s.3 = !matches!(f.pixels, aether_capture::FramePixels::Bgra { .. });
            }
        }),
    );
    if let Err(e) = started {
        p.error = Some(e.to_string());
        return p;
    }
    std::thread::sleep(duration);
    cap.stop();
    let s = *stats.lock();
    p.ran = true;
    (p.frames, p.width, p.height, p.zero_copy) = s;
    p
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSelftest {
    pub ok: bool,
    pub capture_source: &'static str,
    pub wrong_password_rejected: bool,
    pub connect_ms: Option<u64>,
    pub first_picture_ms: Option<u64>,
    pub mode: Option<String>,
    pub codec: Option<String>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub video_access_units: u32,
    pub lossless_tile_patches: u32,
    pub dvr_keyframes: usize,
    pub host_telemetry_received: bool,
    pub error: Option<String>,
}

/// A UI frame message with its arrival time.
type TimedMessage = (Instant, Vec<u8>);

#[derive(Default)]
struct Events(Mutex<Vec<(String, serde_json::Value)>>);

impl EventSink for Events {
    fn emit(&self, name: &str, payload: serde_json::Value) {
        self.0.lock().push((name.to_owned(), payload));
    }
}

impl Events {
    fn find(&self, name: &str, pred: impl Fn(&serde_json::Value) -> bool) -> Option<serde_json::Value> {
        self.0.lock().iter().find(|(n, v)| n == name && pred(v)).map(|(_, v)| v.clone())
    }
}

async fn wait_for<T>(secs: u64, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn local_engine(dir: &Path, events: Arc<Events>, native_capture: bool, name: &str) -> crate::Result<Arc<Engine>> {
    let settings = Settings {
        advertise_on_lan: false,
        clipboard_mode: ClipboardMode::Disabled,
        device_name: name.into(),
        ..Settings::default()
    };
    settings.save(&dir.join("config/settings.json"))?;
    std::fs::create_dir_all(dir.join("downloads"))?;
    let capturer: Option<crate::engine::CapturerFactory> = if native_capture {
        None
    } else {
        Some(Arc::new(|| Ok(Box::new(aether_capture::TestPatternCapturer::new(1280, 720)) as Box<_>)))
    };
    Engine::start(
        EngineConfig {
            config_dir: dir.join("config"),
            cache_dir: dir.join("cache"),
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            download_dir: Some(dir.join("downloads")),
            capturer,
            ..EngineConfig::default()
        },
        events,
    )
    .await
}

/// Boot a real host + client on localhost and stream for `stream_for`.
/// Callers must set `AETHERLINK_NO_KEYCHAIN=1` so the user's identity is not touched.
pub async fn live_session(workdir: &Path, native_capture: bool, stream_for: Duration) -> SessionSelftest {
    let mut r = SessionSelftest {
        ok: false,
        capture_source: if native_capture { "screencapturekit" } else { "test-pattern" },
        wrong_password_rejected: false,
        connect_ms: None,
        first_picture_ms: None,
        mode: None,
        codec: None,
        width: None,
        height: None,
        video_access_units: 0,
        lossless_tile_patches: 0,
        dvr_keyframes: 0,
        host_telemetry_received: false,
        error: None,
    };
    let (hev, cev) = (Arc::new(Events::default()), Arc::new(Events::default()));
    let host = match local_engine(&workdir.join("host"), hev, native_capture, "Self-test host").await {
        Ok(e) => e,
        Err(e) => {
            r.error = Some(format!("host engine: {e}"));
            return r;
        }
    };
    let client = match local_engine(&workdir.join("client"), cev.clone(), false, "Self-test client").await {
        Ok(e) => e,
        Err(e) => {
            r.error = Some(format!("client engine: {e}"));
            return r;
        }
    };
    let addr = format!("127.0.0.1:{}", host.local_addr().map(|a| a.port()).unwrap_or(0));

    // Wrong password must be rejected.
    if let Ok(bad) = client.connect(&addr, Some("zzzz-zzzz".into()), Arc::new(|_| {})) {
        r.wrong_password_rejected = wait_for(10, || {
            cev.find("session-state", |v| v["sessionId"] == bad && v["state"] == "failed")
        })
        .await
        .is_some_and(|v| v["errorCode"] == "errors.wrongPassword");
    }

    let frames: Arc<Mutex<Vec<TimedMessage>>> = Arc::default();
    let f2 = frames.clone();
    let t0 = Instant::now();
    let password = host.identity_info().password;
    let sid = match client.connect(&addr, Some(password), Arc::new(move |m| f2.lock().push((Instant::now(), m)))) {
        Ok(s) => s,
        Err(e) => {
            r.error = Some(e.to_string());
            return r;
        }
    };
    let connected = wait_for(15, || cev.find("session-state", |v| v["sessionId"] == sid && v["state"] != "connecting")).await;
    match connected {
        Some(v) if v["state"] == "connected" => r.connect_ms = Some(t0.elapsed().as_millis() as u64),
        Some(v) => {
            r.error = Some(format!("session {}: {}", v["state"], v["detail"]));
            return r;
        }
        None => {
            r.error = Some("timed out connecting".into());
            return r;
        }
    }
    let picture = wait_for(15, || {
        frames
            .lock()
            .iter()
            .find(|(_, m)| m[0] == ui_video::VIDEO || m[0] == ui_video::BASE_TILE)
            .map(|(t, _)| t.duration_since(t0).as_millis() as u64)
    })
    .await;
    r.first_picture_ms = picture;
    tokio::time::sleep(stream_for).await;
    r.host_telemetry_received = cev.find("session-telemetry", |v| v["sessionId"] == sid).is_some();
    {
        let f = frames.lock();
        for (_, m) in f.iter() {
            match m[0] {
                ui_video::CONFIG => {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&m[1..]) {
                        r.mode = v["mode"].as_str().map(Into::into);
                        r.codec = v["codec"].as_str().map(Into::into).or(r.codec.take());
                        r.width = v["width"].as_u64();
                        r.height = v["height"].as_u64();
                    }
                }
                ui_video::VIDEO => r.video_access_units += 1,
                ui_video::BASE_TILE | ui_video::OVERLAY => r.lossless_tile_patches += 1,
                _ => {}
            }
        }
    }
    if let Ok(s) = client.client(sid) {
        r.dvr_keyframes = s.dvr_index().map(|i| i.keyframes.len()).unwrap_or(0);
    }
    let _ = client.disconnect(sid).await;
    client.shutdown().await;
    host.shutdown().await;
    r.ok = r.wrong_password_rejected && r.connect_ms.is_some() && r.first_picture_ms.is_some() && r.dvr_keyframes > 0;
    r
}
