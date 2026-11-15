//! Every message family must survive encode → frame → decode unchanged.

use aether_protocol::*;
use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;

fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + Debug>(msg: T) {
    let frame = encode_frame(&msg).unwrap();
    let mut dec = FrameDecoder::new();
    dec.push(&frame);
    let back: T = dec.next_frame().unwrap().expect("complete frame");
    assert_eq!(back, msg);
    // Datagram-style (no prefix) as well.
    let bare = encode_message(&msg).unwrap();
    assert_eq!(decode_message::<T>(&bare).unwrap(), msg);
}

fn hello() -> Hello {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        device_id: DeviceId::from_public_key(&[9; 32]),
        device_name: "MacBook di Giulia".into(),
        os: OsKind::MacOs,
        os_version: "27.2".into(),
        app_version: "0.1.0".into(),
        public_key: [9; 32],
        capabilities: Capabilities { video_hevc: true, lossless_tiles: true, ..Default::default() },
        mode: ConnectMode::EphemeralId,
        locale: "it".into(),
    }
}

#[test]
fn handshake_and_auth() {
    roundtrip(hello());
    roundtrip(StreamHeader { kind: StreamKind::Control });
    roundtrip(StreamHeader {
        kind: StreamKind::PortTunnel { tunnel_id: 3, remote_port: 5173, protocol: TunnelProtocol::Tcp },
    });
    roundtrip(AuthMessage::Required { method: AuthMethod::Password });
    roundtrip(AuthMessage::Spake2 { message: vec![1, 2, 3] });
    roundtrip(AuthMessage::Confirm { mac: [5; 32] });
    roundtrip(AuthMessage::Result { accepted: false, reason: Some(AuthRejectReason::WrongPassword), persist_trust: false });
}

#[test]
fn control() {
    roundtrip(ControlMessage::SessionReady {
        displays: vec![DisplayInfo {
            id: 1,
            name: "Odyssey G9".into(),
            bounds: Rect::new(0, 0, 7680, 2160),
            scale: 1.0,
            refresh_hz: 120.0,
            primary: true,
            is_virtual: false,
        }],
        windows: vec![WindowInfo { id: 77, app_name: "Code".into(), title: "main.rs".into(), bounds: Rect::new(10, 10, 800, 600), pid: 42 }],
        active: CaptureTarget::Display { display_id: 1 },
    });
    roundtrip(ControlMessage::SetQuality(QualitySettings::default()));
    roundtrip(ControlMessage::Telemetry(TelemetrySample { rtt_ms: 0.5, fps: 120.0, ..Default::default() }));
    roundtrip(ControlMessage::Error { code: "error.capture.permission".into(), detail: "denied".into() });
}

#[test]
fn video_and_tiles() {
    roundtrip(VideoMessage::Config {
        display_id: 1,
        codec: VideoCodec::Hevc,
        chroma: ChromaFormat::Yuv444,
        width: 7680,
        height: 2160,
        fps: 120,
    });
    roundtrip(VideoMessage::Tiles(DirtyTileBatch {
        display_id: 1,
        seq: 9,
        pts_us: 123_456,
        frame_width: 1920,
        frame_height: 1080,
        tile_size: 64,
        full_refresh: false,
        refinement: true,
        tiles: vec![
            DirtyTile { x: 0, y: 0, width: 64, height: 64, hash: 1, encoding: TileEncoding::Zstd, data: vec![1; 300] },
            DirtyTile { x: 64, y: 0, width: 64, height: 64, hash: 2, encoding: TileEncoding::Solid { bgra: 0xFF0B0E14 }, data: vec![] },
            DirtyTile { x: 128, y: 0, width: 64, height: 64, hash: 3, encoding: TileEncoding::CacheRef { hash: 99 }, data: vec![] },
        ],
    }));
    roundtrip(VideoMessage::Cursor(CursorUpdate { display_id: 1, x: 0.25, y: 0.75, visible: true, shape_id: 4 }));
    roundtrip(Datagram::Input { seq: 1, event: InputEvent::MouseMove { display_id: 1, x: 0.5, y: 0.5 } });
    roundtrip(AudioPacket { seq: 1, pts_us: 2, codec: AudioCodec::Opus, sample_rate: 48_000, channels: 2, data: vec![7; 120] });
}

#[test]
fn input_events() {
    for ev in [
        InputEvent::Scroll { dx: 0.0, dy: -3.5, unit: ScrollUnit::Pixel, phase: Some(GesturePhase::Momentum) },
        InputEvent::Key { code: HidUsage::C, pressed: true, modifiers: Modifiers::META },
        InputEvent::Gesture { gesture: Gesture::Magnify { delta: 0.1 }, phase: GesturePhase::Change },
        InputEvent::Touch { id: 0, display_id: 0, x: 0.1, y: 0.2, phase: TouchPhase::Down, pressure: 1.0 },
        InputEvent::System(SystemAction::AndroidBack),
        InputEvent::ReleaseAll,
    ] {
        roundtrip(ev);
    }
}

#[test]
fn clipboard_vfs_transfer() {
    roundtrip(ClipboardMessage::Update(ClipboardPayload {
        seq: 1,
        origin_os: OsKind::Windows,
        items: vec![
            ClipboardItem { format: ClipboardFormat::Html, data: b"<b>ciao</b>".to_vec() },
            ClipboardItem { format: ClipboardFormat::PlainText, data: b"ciao".to_vec() },
        ],
        files: vec![RemoteFileRef { token: 1, name: "report.pdf".into(), size: 1024, is_dir: false }],
    }));
    roundtrip(VfsEnvelope { req_id: 5, body: VfsRequest::Read { share: 1, path: "src/main.rs".into(), offset: 0, len: 4096 } });
    roundtrip(VfsEnvelope {
        req_id: 5,
        body: VfsResponse::Entries(vec![DirEntry {
            name: "main.rs".into(),
            attr: FileAttr {
                file_id: 1,
                kind: FileKind::File,
                size: 10,
                mode: 0o644,
                mtime_ns: 1,
                ctime_ns: 1,
                read_only: false,
            },
        }]),
    });
    roundtrip(VfsEnvelope { req_id: 6, body: VfsResponse::Error { kind: VfsErrorKind::InvalidPath, message: "..".into() } });
    roundtrip(TransferMessage::Offer {
        transfer_id: 1,
        entries: vec![TransferEntry { rel_path: "a/b.txt".into(), size: 3, is_dir: false, mtime_ns: 0 }],
        total_bytes: 3,
        drop_target: None,
    });
    roundtrip(TransferMessage::EntryDone { entry: 0, blake3: [1; 32] });
}

#[test]
fn dom_tunnel_terminal_usb_dvr() {
    let node = DomNode {
        id: 1,
        parent: None,
        role: DomRole::Button,
        native_role: "AXButton".into(),
        name: "Salva".into(),
        value: None,
        description: None,
        shortcut: Some("⌘S".into()),
        bounds: Rect::new(1, 2, 3, 4),
        actions: vec![DomAction::Press],
        enabled: true,
        focused: false,
        children: vec![],
    };
    roundtrip(DomMessage::Diff(DomTreeDiff {
        base_revision: 1,
        revision: 2,
        upserted: vec![node],
        removed: vec![7],
        root: None,
        app_name: None,
        window_title: Some("Untitled".into()),
    }));
    roundtrip(PortTunnelMessage::ScanResult {
        ports: vec![ListeningPort {
            port: 5173,
            protocol: TunnelProtocol::Tcp,
            address: "127.0.0.1".into(),
            pid: Some(1),
            process_name: Some("node".into()),
            service_hint: service_hint_for_port(5173).map(Into::into),
        }],
    });
    roundtrip(PortTunnelOpen::Refused { reason: "not allowed".into() });
    roundtrip(TerminalMessage::Open { cols: 120, rows: 30, cwd: None, shell: None });
    roundtrip(TerminalMessage::Output(b"\x1b[32mok\x1b[0m".to_vec()));
    roundtrip(UsbMessage::Submit(UrbSubmit {
        urb_id: 1,
        endpoint: 0x81,
        transfer_type: UsbTransferType::Bulk,
        direction: UsbDirection::In,
        setup: None,
        data: vec![],
        length: 512,
        timeout_ms: 1000,
    }));
    roundtrip(TimeTravelIndex {
        start_us: 0,
        end_us: 180_000_000,
        keyframes: vec![0, 2_000_000],
        marks: vec![TimelineMark { pts_us: 5, kind: TimelineEventKind::Alert, label: Some("Crash".into()) }],
    });
}

#[test]
fn stream_header_is_small() {
    // The stream header is sent on every stream; keep it tiny.
    let bytes = encode_message(&StreamHeader { kind: StreamKind::Input }).unwrap();
    assert!(bytes.len() <= 8, "header is {} bytes", bytes.len());
}
