//! End-to-end tests over real QUIC on 127.0.0.1.

use aether_net::tunnel::{TunnelPolicy, forward_local, serve_tunnel};
use aether_net::{
    AetherEndpoint, BoxFuture, ClientAuth, HostAuthPolicy, Identity, IncomingStream, NetError, Session,
};
use aether_protocol::{
    AuthRejectReason, Capabilities, ClipboardMessage, ConnectMode, ControlMessage, Datagram, Hello, OsKind,
    PROTOCOL_VERSION, StreamKind, VideoMessage,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct TestPolicy {
    trusted: Vec<[u8; 32]>,
    password: Option<String>,
    consent: bool,
}

impl HostAuthPolicy for TestPolicy {
    fn is_trusted(&self, peer: &Hello) -> bool {
        self.trusted.contains(&peer.public_key)
    }
    fn password(&self) -> Option<String> {
        self.password.clone()
    }
    fn consent(&self, _peer: &Hello) -> BoxFuture<'static, bool> {
        let c = self.consent;
        Box::pin(async move { c })
    }
}

fn hello(id: &Identity, name: &str) -> Hello {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        device_id: id.device_id(),
        device_name: name.into(),
        os: OsKind::current(),
        os_version: "test".into(),
        app_version: "0.1.0".into(),
        public_key: id.public_key(),
        capabilities: Capabilities { port_tunnels: true, ..Default::default() },
        mode: ConnectMode::LanDirect,
        locale: "en".into(),
    }
}

fn endpoint() -> AetherEndpoint {
    AetherEndpoint::bind("127.0.0.1:0".parse().unwrap(), Identity::generate()).unwrap()
}

/// Connect a client to a host with `policy`; returns (client result, host result).
async fn pair_on(
    host: &AetherEndpoint,
    client: &AetherEndpoint,
    policy: TestPolicy,
    auth: ClientAuth,
) -> (aether_net::Result<Session>, aether_net::Result<Session>) {
    let host_hello = hello(host.identity(), "host");
    let h = host.clone();
    let host_task = tokio::spawn(async move { h.accept(host_hello, Arc::new(policy)).await.unwrap() });
    let addr = host.local_addr().unwrap();
    let c = client.connect(addr, hello(client.identity(), "client"), auth).await;
    let h = tokio::time::timeout(Duration::from_secs(20), host_task).await.unwrap().unwrap();
    (c, h)
}

async fn connected_pair() -> (Session, Session, AetherEndpoint, AetherEndpoint) {
    let host = endpoint();
    let client = endpoint();
    let policy = TestPolicy { trusted: vec![client.identity().public_key()], password: None, consent: false };
    let (c, h) = pair_on(&host, &client, policy, ClientAuth::Trusted).await;
    (c.unwrap(), h.unwrap(), host, client)
}

#[tokio::test]
async fn trusted_key_auth_and_control_stream() {
    let (mut c, mut h, host, client) = connected_pair().await;
    assert_eq!(c.remote_hello.device_name, "host");
    assert_eq!(h.remote_hello.device_name, "client");
    assert_eq!(c.conn.peer_public_key(), host.identity().public_key());
    assert_eq!(h.conn.peer_public_key(), client.identity().public_key());

    c.control_tx.send(&ControlMessage::Ping { nonce: 1, sent_us: 2 }).await.unwrap();
    let got: ControlMessage = h.control_rx.recv().await.unwrap().unwrap();
    assert_eq!(got, ControlMessage::Ping { nonce: 1, sent_us: 2 });
    h.control_tx.send(&ControlMessage::Pong { nonce: 1, sent_us: 2 }).await.unwrap();
    let got: ControlMessage = c.control_rx.recv().await.unwrap().unwrap();
    assert_eq!(got, ControlMessage::Pong { nonce: 1, sent_us: 2 });
    // Both sides derive the same TLS exporter values.
    assert_eq!(
        c.conn.export_keying_material(b"test", b"ctx").unwrap(),
        h.conn.export_keying_material(b"test", b"ctx").unwrap()
    );
}

#[tokio::test]
async fn password_auth_success() {
    let host = endpoint();
    let client = endpoint();
    let policy = TestPolicy { trusted: vec![], password: Some("482913".into()), consent: false };
    let (c, h) = pair_on(&host, &client, policy, ClientAuth::Password("482913".into())).await;
    let (c, h) = (c.unwrap(), h.unwrap());
    assert_eq!(c.conn.peer_public_key(), host.identity().public_key());
    assert_eq!(h.remote_hello.public_key, client.identity().public_key());
}

#[tokio::test]
async fn wrong_password_rejected_then_rate_limited() {
    let host = endpoint();
    for attempt in 0..5 {
        let client = endpoint();
        let policy = TestPolicy { trusted: vec![], password: Some("482913".into()), consent: false };
        let (c, h) = pair_on(&host, &client, policy, ClientAuth::Password("000000".into())).await;
        assert!(
            matches!(c, Err(NetError::AuthRejected(AuthRejectReason::WrongPassword))),
            "attempt {attempt}: {c:?}"
        );
        assert!(matches!(h, Err(NetError::AuthRejected(AuthRejectReason::WrongPassword))));
    }
    // Now even the right password is refused from this source.
    let client = endpoint();
    let policy = TestPolicy { trusted: vec![], password: Some("482913".into()), consent: false };
    let (c, _h) = pair_on(&host, &client, policy, ClientAuth::Password("482913".into())).await;
    assert!(matches!(c, Err(NetError::AuthRejected(AuthRejectReason::TooManyAttempts))), "{c:?}");
}

#[tokio::test]
async fn password_required_but_client_has_none() {
    let host = endpoint();
    let client = endpoint();
    let policy = TestPolicy { trusted: vec![], password: Some("1".into()), consent: true };
    let (c, h) = pair_on(&host, &client, policy, ClientAuth::Trusted).await;
    assert!(matches!(c, Err(NetError::Auth(_))), "{c:?}");
    assert!(h.is_err());
}

#[tokio::test]
async fn interactive_consent() {
    let host = endpoint();
    let client = endpoint();
    let (c, h) =
        pair_on(&host, &client, TestPolicy { trusted: vec![], password: None, consent: true }, ClientAuth::Trusted).await;
    assert!(c.is_ok() && h.is_ok());

    let client = endpoint();
    let (c, _) =
        pair_on(&host, &client, TestPolicy { trusted: vec![], password: None, consent: false }, ClientAuth::Trusted).await;
    assert!(matches!(c, Err(NetError::AuthRejected(AuthRejectReason::DeniedByUser))), "{c:?}");
}

#[tokio::test]
async fn pinned_host_key_mismatch_is_rejected() {
    let host = endpoint();
    let client = endpoint();
    let h = host.clone();
    let policy = Arc::new(TestPolicy { trusted: vec![client.identity().public_key()], password: None, consent: false });
    let host_hello = hello(host.identity(), "host");
    tokio::spawn(async move {
        let _ = h.accept(host_hello, policy).await;
    });
    let wrong = Identity::generate().public_key();
    let r = client
        .connect_with_key(host.local_addr().unwrap(), hello(client.identity(), "c"), ClientAuth::Trusted, Some(wrong))
        .await;
    assert!(matches!(r, Err(NetError::Auth(_))), "{r:?}");
}

#[tokio::test]
async fn streams_with_headers() {
    let (c, h, _host, _client) = connected_pair().await;

    // Client opens a bi stream; host accepts and sees its kind.
    let (mut tx, mut rx) = c.conn.open_bi(StreamKind::Clipboard).await.unwrap();
    tx.send(&ClipboardMessage::FetchFile { token: 1, transfer_id: 2 }).await.unwrap();
    let incoming = h.conn.accept_stream().await.unwrap();
    let IncomingStream::Bi { kind, mut send, mut recv } = incoming else { panic!("expected bi") };
    assert_eq!(kind, StreamKind::Clipboard);
    let m: ClipboardMessage = recv.recv().await.unwrap().unwrap();
    assert_eq!(m, ClipboardMessage::FetchFile { token: 1, transfer_id: 2 });
    send.send(&ClipboardMessage::FetchFile { token: 3, transfer_id: 4 }).await.unwrap();
    send.finish().unwrap();
    let m: ClipboardMessage = rx.recv().await.unwrap().unwrap();
    assert_eq!(m, ClipboardMessage::FetchFile { token: 3, transfer_id: 4 });
    assert!(rx.recv::<ClipboardMessage>().await.unwrap().is_none(), "EOF after finish");

    // Host opens a uni video stream with a large frame.
    let mut vtx = h.conn.open_uni(StreamKind::Video { display_id: 3 }).await.unwrap();
    let frame = VideoMessage::Frame(aether_protocol::VideoFrame {
        display_id: 3,
        seq: 1,
        pts_us: 10,
        codec: aether_protocol::VideoCodec::Hevc,
        chroma: aether_protocol::ChromaFormat::Yuv444,
        keyframe: true,
        width: 3840,
        height: 2160,
        data: (0..2_000_000u32).map(|i| (i % 251) as u8).collect(),
    });
    vtx.send(&frame).await.unwrap();
    vtx.finish().unwrap();
    let IncomingStream::Uni { kind, mut recv } = c.conn.accept_stream().await.unwrap() else { panic!("expected uni") };
    assert_eq!(kind, StreamKind::Video { display_id: 3 });
    let got: VideoMessage = recv.recv().await.unwrap().unwrap();
    assert_eq!(got, frame);
}

#[tokio::test]
async fn datagrams() {
    let (c, h, _host, _client) = connected_pair().await;
    assert!(c.conn.max_datagram_size().unwrap() >= 1000);
    // Datagrams are unreliable; on loopback they arrive, but retry a few times to be robust.
    let recv = tokio::spawn({
        let conn = h.conn.clone();
        async move { conn.recv_datagram().await.unwrap() }
    });
    for i in 0..10u64 {
        c.conn.send_datagram(&Datagram::Ping { nonce: 42, sent_us: i }).unwrap();
        if recv.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let got = tokio::time::timeout(Duration::from_secs(5), recv).await.unwrap().unwrap();
    assert!(matches!(got, Datagram::Ping { nonce: 42, .. }));
    assert!(c.conn.rtt() < Duration::from_secs(1));
}

async fn spawn_echo_server() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
                let _ = w.shutdown().await;
            });
        }
    });
    port
}

fn spawn_tunnel_host(h: &Session, policy: TunnelPolicy) {
    let conn = h.conn.clone();
    tokio::spawn(async move {
        while let Ok(stream) = conn.accept_stream().await {
            if let IncomingStream::Bi { kind: StreamKind::PortTunnel { remote_port, .. }, send, recv } = stream {
                let policy = policy.clone();
                tokio::spawn(async move {
                    let _ = serve_tunnel(remote_port, send, recv, &policy).await;
                });
            }
        }
    });
}

#[tokio::test]
async fn port_forwarding_end_to_end() {
    let echo_port = spawn_echo_server().await;
    let (c, h, _host, _client) = connected_pair().await;
    spawn_tunnel_host(&h, TunnelPolicy::only([echo_port]));

    let tunnel = forward_local(c.conn.clone(), "127.0.0.1:0".parse().unwrap(), echo_port).await.unwrap();
    let payload: Vec<u8> = (0..1_000_000u32).map(|i| (i * 7 % 256) as u8).collect();

    // Two concurrent TCP connections through the same tunnel.
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let addr = tunnel.local_addr();
        let payload = payload.clone();
        tasks.push(tokio::spawn(async move {
            let s = tokio::net::TcpStream::connect(addr).await.unwrap();
            let (mut r, mut w) = s.into_split();
            let writer = tokio::spawn(async move {
                w.write_all(&payload).await.unwrap();
                w.shutdown().await.unwrap();
            });
            let mut back = Vec::new();
            r.read_to_end(&mut back).await.unwrap();
            writer.await.unwrap();
            back
        }));
    }
    for t in tasks {
        let back = tokio::time::timeout(Duration::from_secs(30), t).await.unwrap().unwrap();
        assert_eq!(back.len(), payload.len());
        assert!(back == payload);
    }
    assert_eq!(tunnel.bytes_up(), 2 * payload.len() as u64);
    assert_eq!(tunnel.bytes_down(), 2 * payload.len() as u64);

    // A port outside the host policy is refused: the local TCP connection just closes.
    let refused = forward_local(c.conn.clone(), "127.0.0.1:0".parse().unwrap(), echo_port.wrapping_add(1).max(1))
        .await
        .unwrap();
    let mut s = tokio::net::TcpStream::connect(refused.local_addr()).await.unwrap();
    let mut buf = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut buf)).await.unwrap().unwrap_or(0);
    assert_eq!(n, 0);

    tunnel.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(tokio::net::TcpStream::connect(tunnel.local_addr()).await.is_err(), "listener closed after stop()");
}
