//! Rendezvous + relay end-to-end tests: an in-process relay server and real aether-net
//! endpoints on 127.0.0.1.

use aether_net::rendezvous::RendezvousClient;
use aether_net::{
    AetherEndpoint, BoxFuture, ClientAuth, ConnectOptions, ConnectPath, HostAuthPolicy, Identity, IncomingStream,
    NetError, Session, connect_to_device_with,
};
use aether_protocol::rendezvous::{
    RendezvousErrorCode, RendezvousMessage, decode_rendezvous, encode_rendezvous, register_signing_payload,
};
use aether_protocol::{
    Capabilities, ConnectMode, ControlMessage, Datagram, DeviceId, Hello, OsKind, PROTOCOL_VERSION, StreamKind,
};
use aether_relay::{RelayConfig, RelayHandle, RelayServer};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

struct PasswordPolicy(String);

impl HostAuthPolicy for PasswordPolicy {
    fn is_trusted(&self, _peer: &Hello) -> bool {
        false
    }
    fn password(&self) -> Option<String> {
        Some(self.0.clone())
    }
    fn consent(&self, _peer: &Hello) -> BoxFuture<'static, bool> {
        Box::pin(async { false })
    }
}

fn hello(id: &Identity, mode: ConnectMode) -> Hello {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        device_id: id.device_id(),
        device_name: "dev".into(),
        os: OsKind::current(),
        os_version: "test".into(),
        app_version: "0.1.0".into(),
        public_key: id.public_key(),
        capabilities: Capabilities::default(),
        mode,
        locale: "en".into(),
    }
}

async fn relay(cfg: RelayConfig) -> RelayHandle {
    let cfg = RelayConfig { bind: "127.0.0.1:0".parse().unwrap(), ..cfg };
    RelayServer::bind(cfg).await.unwrap().spawn().unwrap()
}

fn endpoint() -> AetherEndpoint {
    AetherEndpoint::bind("127.0.0.1:0".parse().unwrap(), Identity::generate()).unwrap()
}

/// A registered host accepting password-authenticated sessions; yields sessions on a channel.
async fn spawn_host(relay_addr: SocketAddr, password: &str) -> (AetherEndpoint, RendezvousClient, mpsc::Receiver<Session>) {
    let host = endpoint();
    let rv = RendezvousClient::new(&host, relay_addr).unwrap();
    let reg = rv.register(host.identity().device_id()).await.unwrap();
    assert_eq!(reg.observed, host.local_addr().unwrap());
    let (tx, rx) = mpsc::channel(4);
    let h = host.clone();
    let policy: Arc<dyn HostAuthPolicy> = Arc::new(PasswordPolicy(password.into()));
    tokio::spawn(async move {
        let hello = hello(h.identity(), ConnectMode::EphemeralId);
        while let Some(r) = h.accept(hello.clone(), policy.clone()).await {
            match r {
                Ok(s) => {
                    if tx.send(s).await.is_err() {
                        return;
                    }
                }
                Err(e) => eprintln!("host handshake failed: {e}"),
            }
        }
    });
    (host, rv, rx)
}

#[tokio::test]
async fn register_and_lookup() {
    let relay = relay(RelayConfig::default()).await;
    let (host, _host_rv, _sessions) = spawn_host(relay.local_addr, "pw").await;

    let client = endpoint();
    let rv = RendezvousClient::new(&client, relay.local_addr).unwrap();
    let mut host_events = _host_rv.subscribe();
    let info = rv.lookup(host.identity().device_id()).await.unwrap();
    assert_eq!(info.public_key, host.identity().public_key());
    assert_eq!(info.reflexive, host.local_addr().unwrap());
    assert_eq!(info.local_addrs, vec![host.local_addr().unwrap()]);
    assert_eq!(info.observed_self, client.local_addr().unwrap());

    // The host was told to punch towards the client.
    let ev = tokio::time::timeout(Duration::from_secs(5), host_events.recv()).await.unwrap().unwrap();
    match ev {
        aether_net::rendezvous::RendezvousEvent::PunchRequested { requester_public_key, addrs } => {
            assert_eq!(requester_public_key, client.identity().public_key());
            assert!(addrs.contains(&client.local_addr().unwrap()));
        }
        other => panic!("unexpected event {other:?}"),
    }

    let missing = rv.lookup(DeviceId::new(123_456_789).unwrap()).await;
    assert!(matches!(missing, Err(NetError::Rendezvous(RendezvousErrorCode::NotFound))), "{missing:?}");

    // STUN through the endpoint socket against the relay's built-in responder.
    assert_eq!(rv.stun_binding(relay.local_addr).await.unwrap(), client.local_addr().unwrap());
    // And the standalone STUN client on a plain socket.
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    assert_eq!(aether_net::stun::binding(&sock, relay.local_addr).await.unwrap(), sock.local_addr().unwrap());
    relay.shutdown().await;
}

#[tokio::test]
async fn device_id_hijack_is_prevented() {
    let relay = relay(RelayConfig::default()).await;

    // Owner registers its key-derived ID.
    let owner = endpoint();
    let owner_rv = RendezvousClient::new(&owner, relay.local_addr).unwrap();
    let owner_id = owner.identity().device_id();
    owner_rv.register(owner_id).await.unwrap();

    // An attacker with another key cannot claim it.
    let attacker = endpoint();
    let attacker_rv = RendezvousClient::new(&attacker, relay.local_addr).unwrap();
    let r = attacker_rv.register(owner_id).await;
    assert!(matches!(r, Err(NetError::Rendezvous(RendezvousErrorCode::IdTaken))), "{r:?}");

    // Ephemeral IDs are first-come-first-served...
    let ephemeral = DeviceId::from_random(rand::random());
    attacker_rv.register(ephemeral).await.unwrap();
    let r = owner_rv.register(ephemeral).await;
    assert!(matches!(r, Err(NetError::Rendezvous(RendezvousErrorCode::IdTaken))), "{r:?}");

    // ...but a squatter on someone's derived ID is evicted by the real owner.
    let late_owner = Identity::generate();
    let late_id = late_owner.device_id();
    attacker_rv.register(late_id).await.unwrap();
    let late_ep = AetherEndpoint::bind("127.0.0.1:0".parse().unwrap(), late_owner.clone()).unwrap();
    let late_rv = RendezvousClient::new(&late_ep, relay.local_addr).unwrap();
    late_rv.register(late_id).await.unwrap();
    let looker = endpoint();
    let looker_rv = RendezvousClient::new(&looker, relay.local_addr).unwrap();
    assert_eq!(looker_rv.lookup(late_id).await.unwrap().public_key, late_owner.public_key());

    // Forged signatures and replayed nonces are rejected (raw protocol).
    let raw = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rpc = |msg: RendezvousMessage| {
        let raw = &raw;
        let to = relay.local_addr;
        async move {
            raw.send_to(&encode_rendezvous(&msg).unwrap(), to).await.unwrap();
            let mut buf = [0u8; 1500];
            let (n, _) = tokio::time::timeout(Duration::from_secs(2), raw.recv_from(&mut buf)).await.unwrap().unwrap();
            decode_rendezvous(&buf[..n]).unwrap()
        }
    };
    let RendezvousMessage::Challenge { nonce, server_time_ms, .. } = rpc(RendezvousMessage::RegisterInit { request_id: 1 }).await
    else {
        panic!()
    };
    let forger = Identity::generate();
    let victim_key = owner.identity().public_key();
    let payload = register_signing_payload(b"register", owner_id, &victim_key, server_time_ms, &nonce);
    let reply = rpc(RendezvousMessage::Register {
        request_id: 2,
        device_id: owner_id,
        public_key: victim_key,
        local_addrs: vec![],
        timestamp_ms: server_time_ms,
        nonce,
        signature: forger.sign(&payload).to_vec(),
    })
    .await;
    assert_eq!(reply, RendezvousMessage::Error { request_id: 2, code: RendezvousErrorCode::BadSignature });

    // A nonce issued to another address is useless here.
    let foreign_nonce = [0x55; 16];
    let fid = forger.device_id();
    let payload = register_signing_payload(b"register", fid, &forger.public_key(), server_time_ms, &foreign_nonce);
    let reply = rpc(RendezvousMessage::Register {
        request_id: 3,
        device_id: fid,
        public_key: forger.public_key(),
        local_addrs: vec![],
        timestamp_ms: server_time_ms,
        nonce: foreign_nonce,
        signature: forger.sign(&payload).to_vec(),
    })
    .await;
    assert_eq!(reply, RendezvousMessage::Error { request_id: 3, code: RendezvousErrorCode::BadNonce });
    relay.shutdown().await;
}

async fn exercise_session(client: &mut Session, host: &mut Session) {
    client.control_tx.send(&ControlMessage::Ping { nonce: 9, sent_us: 1 }).await.unwrap();
    let m: ControlMessage = host.control_rx.recv().await.unwrap().unwrap();
    assert_eq!(m, ControlMessage::Ping { nonce: 9, sent_us: 1 });

    // A 4 MB uni stream host → client.
    let data: Vec<u8> = (0..4_000_000u32).map(|i| (i % 253) as u8).collect();
    let mut tx = host.conn.open_uni(StreamKind::FileTransfer { transfer_id: 1 }).await.unwrap();
    let send = {
        let data = data.clone();
        tokio::spawn(async move {
            tx.send(&data).await.unwrap();
            tx.finish().unwrap();
            tx
        })
    };
    let IncomingStream::Uni { kind, mut recv } = client.conn.accept_stream().await.unwrap() else { panic!() };
    assert_eq!(kind, StreamKind::FileTransfer { transfer_id: 1 });
    let got: Vec<u8> = recv.recv().await.unwrap().unwrap();
    assert!(got == data);
    let _keep_alive = send.await.unwrap();

    let r = tokio::spawn({
        let c = host.conn.clone();
        async move { c.recv_datagram().await.unwrap() }
    });
    for i in 0..20 {
        client.conn.send_datagram(&Datagram::Ping { nonce: i, sent_us: 0 }).unwrap();
        if r.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(matches!(r.await.unwrap(), Datagram::Ping { .. }));
}

#[tokio::test]
async fn connect_by_id_direct_with_hole_punch() {
    let relay = relay(RelayConfig::default()).await;
    let (host, _host_rv, mut sessions) = spawn_host(relay.local_addr, "314159").await;
    let client = endpoint();
    let rv = RendezvousClient::new(&client, relay.local_addr).unwrap();
    let opts = ConnectOptions { try_relay: false, ..Default::default() };
    let (mut c, path) = connect_to_device_with(
        &client,
        &rv,
        host.identity().device_id(),
        hello(client.identity(), ConnectMode::EphemeralId),
        ClientAuth::Password("314159".into()),
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(path, ConnectPath::Direct(host.local_addr().unwrap()));
    let mut h = tokio::time::timeout(Duration::from_secs(10), sessions.recv()).await.unwrap().unwrap();
    exercise_session(&mut c, &mut h).await;
    assert_eq!(relay.stats.bytes_relayed.load(Ordering::Relaxed), 0);
    relay.shutdown().await;
}

#[tokio::test]
async fn connect_by_id_through_relay() {
    let relay = relay(RelayConfig::default()).await;
    let (host, _host_rv, mut sessions) = spawn_host(relay.local_addr, "271828").await;
    let client = endpoint();
    let rv = RendezvousClient::new(&client, relay.local_addr).unwrap();
    let opts = ConnectOptions { try_direct: false, ..Default::default() };
    let (mut c, path) = connect_to_device_with(
        &client,
        &rv,
        host.identity().device_id(),
        hello(client.identity(), ConnectMode::EphemeralId),
        ClientAuth::Password("271828".into()),
        &opts,
    )
    .await
    .unwrap();
    let ConnectPath::Relay(relay_leg) = path else { panic!("expected relay path, got {path:?}") };
    assert_eq!(relay_leg.ip(), relay.local_addr.ip());
    assert_ne!(relay_leg.port(), relay.local_addr.port());
    assert_eq!(c.conn.remote_address(), relay_leg);
    assert_eq!(c.conn.peer_public_key(), host.identity().public_key());

    let mut h = tokio::time::timeout(Duration::from_secs(10), sessions.recv()).await.unwrap().unwrap();
    // The host sees the relay's other leg, not the client.
    assert_eq!(h.conn.remote_address().ip(), relay.local_addr.ip());
    assert_ne!(h.conn.remote_address(), client.local_addr().unwrap());
    exercise_session(&mut c, &mut h).await;
    assert!(relay.stats.bytes_relayed.load(Ordering::Relaxed) > 4_000_000);
    assert_eq!(relay.stats.sessions_active.load(Ordering::Relaxed), 1);

    // Wrong password through the relay is still rejected end-to-end.
    let client2 = endpoint();
    let rv2 = RendezvousClient::new(&client2, relay.local_addr).unwrap();
    let r = connect_to_device_with(
        &client2,
        &rv2,
        host.identity().device_id(),
        hello(client2.identity(), ConnectMode::EphemeralId),
        ClientAuth::Password("000000".into()),
        &opts,
    )
    .await;
    assert!(matches!(r, Err(NetError::AuthRejected(_))), "{:?}", r.err());
    relay.shutdown().await;
}

#[tokio::test]
async fn relay_capacity_limit() {
    let relay = relay(RelayConfig { max_sessions: 0, ..Default::default() }).await;
    let (host, _host_rv, _sessions) = spawn_host(relay.local_addr, "x").await;
    let client = endpoint();
    let rv = RendezvousClient::new(&client, relay.local_addr).unwrap();
    let r = rv.request_relay(host.identity().device_id()).await;
    assert!(matches!(r, Err(NetError::Rendezvous(RendezvousErrorCode::RelayUnavailable))), "{r:?}");
    relay.shutdown().await;
}
