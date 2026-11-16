//! Two real engines on localhost: QUIC + SPAKE2 password auth, video into the UI
//! frame channel, system audio (Opus datagrams), DVR, port forwarding, remote terminal,
//! VFS shares and file transfer.
//!
//! The host uses the deterministic test-pattern capturer and a sine-wave audio source so no
//! Screen Recording permission is needed; the client decodes audio without opening an output
//! device; nothing injects input or touches the clipboard.

use aether_capture::{SineAudioCapturer, TestPatternCapturer};
use aether_core::settings::Settings;
use aether_core::{ByteSink, Engine, EngineConfig, EventSink, ui_video};
use aether_protocol::{ClipboardMode, SessionPermissions};
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

async fn wait_for<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn engine(dir: &std::path::Path, events: Arc<Events>, host: bool) -> Arc<Engine> {
    let settings = Settings {
        advertise_on_lan: false,
        clipboard_mode: ClipboardMode::Disabled,
        device_name: if host { "Host di prova".into() } else { "Client".into() },
        ..Settings::default()
    };
    settings.save(&dir.join("config/settings.json")).unwrap();
    let cfg = EngineConfig {
        config_dir: dir.join("config"),
        cache_dir: dir.join("cache"),
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        download_dir: Some(dir.join("downloads")),
        capturer: Some(Arc::new(|| Ok(Box::new(TestPatternCapturer::new(640, 360)) as Box<_>))),
        // 44.1 kHz source: exercises the host-side resampler to Opus' 48 kHz.
        audio_capturer: Some(Arc::new(|| Ok(Box::new(SineAudioCapturer::new(1000.0, 44_100, 2)) as Box<_>))),
        audio_null_output: true,
    };
    std::fs::create_dir_all(dir.join("downloads")).unwrap();
    Engine::start(cfg, events).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_session_over_localhost() {
    // SAFETY: set before any engine thread starts; forces the file identity store (no keychain prompts).
    unsafe { std::env::set_var("AETHERLINK_NO_KEYCHAIN", "1") };
    let tmp = tempfile::tempdir().unwrap();
    let (host_ev, client_ev) = (Arc::new(Events::default()), Arc::new(Events::default()));
    let host = engine(&tmp.path().join("host"), host_ev.clone(), true).await;
    let client = engine(&tmp.path().join("client"), client_ev.clone(), false).await;
    let host_addr = format!("127.0.0.1:{}", host.local_addr().unwrap().port());

    // ---- wrong password is rejected
    let sink: ByteSink = Arc::new(|_| {});
    let bad = client.connect(&host_addr, Some("zzzz-zzzz".into()), sink).unwrap();
    let failed = wait_for("failed state", 15, || {
        client_ev.find("session-state", |v| v["sessionId"] == bad && v["state"] == "failed")
    })
    .await;
    assert_eq!(failed["errorCode"], "errors.wrongPassword", "{failed}");

    // ---- correct password connects; frames reach the UI sink
    let frames: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let f2 = frames.clone();
    let password = host.identity_info().password;
    let sid = client.connect(&host_addr, Some(password), Arc::new(move |m| f2.lock().push(m))).unwrap();
    wait_for("connected", 15, || client_ev.find("session-state", |v| v["sessionId"] == sid && v["state"] == "connected")).await;
    wait_for("host session listed", 5, || (!host.host_sessions().is_empty()).then_some(())).await;
    let session = client.client(sid).unwrap();

    wait_for("config + picture", 15, || {
        let f = frames.lock();
        let cfg = f.iter().any(|m| m[0] == ui_video::CONFIG);
        let pic = f.iter().any(|m| m[0] == ui_video::VIDEO || m[0] == ui_video::BASE_TILE);
        (cfg && pic).then_some(())
    })
    .await;
    wait_for("telemetry", 5, || client_ev.find("session-telemetry", |v| v["sessionId"] == sid)).await;

    // ---- least privilege: the host announces the defaults; terminal / tunnels are refused
    let perms = wait_for("permissions", 5, || client_ev.find("session-permissions", |v| v["sessionId"] == sid)).await;
    assert_eq!(perms["permissions"]["input"], true);
    assert_eq!(perms["permissions"]["terminal"], false);
    assert_eq!(perms["permissions"]["portForwarding"], false);
    let refused = session.terminal_open(100, 30, Arc::new(|_| {})).await;
    assert!(refused.is_err(), "terminal must be refused by default");
    wait_for("terminal refusal reported", 5, || {
        client_ev.find("session-state", |v| v["sessionId"] == sid && v["errorCode"] == "errors.permission.terminal")
    })
    .await;

    // ---- audio: enable → Opus datagrams reach the client, jitter buffer + decoder play them
    session.set_audio(true).await.unwrap();
    let stats = wait_for("audio decoded", 10, || {
        let s = session.audio_stats();
        (s.packets_received >= 30 && s.receiver.decoded >= 20).then_some(s)
    })
    .await;
    assert!(stats.enabled && stats.bytes_received > 0, "{stats:?}");
    assert_eq!(stats.receiver.decode_errors, 0, "{stats:?}");
    assert_eq!(stats.receiver.rejected, 0, "{stats:?}");
    wait_for("audio telemetry", 5, || {
        client_ev.find("session-telemetry", |v| v["sessionId"] == sid && v["audio"]["packetsReceived"].as_u64() > Some(0))
    })
    .await;
    session.set_audio_volume(0.4, Some(true));
    let s = session.audio_stats();
    assert!(s.muted && (s.volume - 0.4).abs() < 1e-6);
    session.set_audio(false).await.unwrap();
    assert!(!session.audio_stats().enabled);

    // ---- DVR: keyframes recorded, seek renders history, live resumes
    let index = wait_for("dvr keyframe", 10, || session.dvr_index().filter(|i| !i.keyframes.is_empty())).await;
    let before = frames.lock().len();
    session.dvr_seek(index.end_us).unwrap();
    let after_seek = frames.lock()[before..].to_vec();
    assert_eq!(after_seek[0], vec![ui_video::RESET]);
    assert!(after_seek.iter().any(|m| m[0] == ui_video::CONFIG), "seek re-sends decoder config");
    session.dvr_live().await.unwrap();

    // ---- port forwarding to a TCP echo service on the host
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = echo.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    // Grant terminal + port forwarding (to the echo port only); applies to the live session.
    let granted = Settings {
        permissions: SessionPermissions { terminal: true, port_forwarding: true, ..SessionPermissions::default() },
        tunnel_allowed_ports: vec![echo_port],
        ..host.settings()
    };
    host.set_settings(granted.clone()).await.unwrap();
    wait_for("permissions update", 5, || {
        client_ev.find("session-permissions", |v| v["sessionId"] == sid && v["permissions"]["terminal"] == true)
    })
    .await;
    let fwd = session.port_forward(echo_port, Some(0)).await.unwrap();
    let mut s = tokio::net::TcpStream::connect(&fwd.local_addr).await.unwrap();
    s.write_all(b"ciao dal tunnel").await.unwrap();
    let mut got = [0u8; 15];
    s.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"ciao dal tunnel");
    let ports = session.ports_scan().await.unwrap();
    assert!(ports.iter().all(|p| p.port == echo_port), "only allowlisted ports are advertised: {ports:?}");
    // Revoking port forwarding closes tunnels that are already open.
    host.set_settings(Settings { permissions: SessionPermissions { port_forwarding: false, ..granted.permissions }, ..granted.clone() })
        .await
        .unwrap();
    let mut buf = [0u8; 8];
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "open tunnel survived revocation");

    // ---- remote terminal
    let out: Arc<Mutex<Vec<u8>>> = Arc::default();
    let o2 = out.clone();
    let term = session.terminal_open(100, 30, Arc::new(move |b| o2.lock().extend(b))).await.unwrap();
    assert!(!term.cwd.is_empty());
    session.terminal_input(term.terminal_id, b"echo AETHER_$((6*7))\n".to_vec()).unwrap();
    wait_for("terminal output", 15, || String::from_utf8_lossy(&out.lock()).contains("AETHER_42").then_some(())).await;
    session.terminal_close(term.terminal_id);

    // ---- VFS share listing
    let shared = tmp.path().join("progetti");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("main.rs"), "fn main() {}").unwrap();
    let share = host.add_local_share(&shared, true).unwrap();
    let shares = session.vfs_list_shares().await.unwrap();
    assert!(shares.iter().any(|s| s.id == share.id && s.name == "progetti" && s.read_only));

    // ---- drag & drop file transfer lands in the host's download dir
    let src = tmp.path().join("report.txt");
    std::fs::write(&src, b"AetherLink transfer test").unwrap();
    let tid = session.send_files(vec![src]).await.unwrap();
    wait_for("transfer done", 15, || host_ev.find("transfer-progress", |v| v["transferId"] == tid && v["state"] == "done")).await;
    let landed = tmp.path().join("host/downloads/report.txt");
    assert_eq!(std::fs::read(landed).unwrap(), b"AetherLink transfer test");

    // ---- host key was pinned on the client (trusted fleet next time)
    assert!(client.trusted_devices().iter().any(|d| d.name == "Host di prova"));

    // ---- disconnect
    client.disconnect(sid).await.unwrap();
    wait_for("closed", 5, || client_ev.find("session-state", |v| v["sessionId"] == sid && v["state"] == "closed")).await;
    wait_for("host cleanup", 10, || host.host_sessions().is_empty().then_some(())).await;

    client.shutdown().await;
    host.shutdown().await;
}
