//! QUIC endpoint (client + server roles on one UDP socket).

use crate::auth::{AuthRateLimiter, ClientAuth, HostAuthPolicy, client_handshake, host_handshake};
use crate::connection::PeerConnection;
use crate::socket::{AuxPacket, AuxSender, DemuxSocket};
use crate::tls::{SERVER_NAME, client_crypto, peer_key, server_crypto};
use crate::{FramedRecv, FramedSend, Identity, NetError, Result};
use aether_protocol::Hello;
use parking_lot::Mutex;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{IdleTimeout, TransportConfig, VarInt};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Timeout for the QUIC/TLS handshake itself (excludes the application handshake).
pub(crate) const QUIC_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// An authenticated session: the connection plus its Control stream.
#[derive(Debug)]
pub struct Session {
    pub conn: PeerConnection,
    pub control_tx: FramedSend,
    pub control_rx: FramedRecv,
    pub remote_hello: Hello,
}

/// Low-latency transport parameters tuned for interactive desktop streaming.
fn transport_config() -> TransportConfig {
    let mut t = TransportConfig::default();
    t.max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(30_000))));
    t.keep_alive_interval(Some(Duration::from_secs(4)));
    t.initial_rtt(Duration::from_millis(100));
    // Large windows so a 4K/60 HEVC stream (bursty keyframes) never stalls on flow control.
    t.stream_receive_window(VarInt::from_u32(16 * 1024 * 1024));
    t.receive_window(VarInt::from_u32(64 * 1024 * 1024));
    t.send_window(64 * 1024 * 1024);
    t.max_concurrent_bidi_streams(VarInt::from_u32(1024));
    t.max_concurrent_uni_streams(VarInt::from_u32(1024));
    t.datagram_receive_buffer_size(Some(4 * 1024 * 1024));
    t.datagram_send_buffer_size(4 * 1024 * 1024);
    t
}

struct EndpointInner {
    endpoint: quinn::Endpoint,
    identity: Identity,
    client_config: quinn::ClientConfig,
    aux: AuxSender,
    aux_rx: Mutex<Option<mpsc::Receiver<AuxPacket>>>,
    limiter: Arc<AuthRateLimiter>,
}

/// QUIC endpoint acting as both client (`connect`) and host (`accept`). Cheap to clone.
#[derive(Clone)]
pub struct AetherEndpoint {
    inner: Arc<EndpointInner>,
}

impl std::fmt::Debug for AetherEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AetherEndpoint")
            .field("local_addr", &self.inner.endpoint.local_addr().ok())
            .field("identity", &self.inner.identity)
            .finish()
    }
}

impl AetherEndpoint {
    /// Bind a UDP socket (dual-stack when `addr` is `[::]`) and start the endpoint.
    /// Must be called from within a tokio runtime.
    pub fn bind(addr: SocketAddr, identity: Identity) -> Result<Self> {
        use socket2::{Domain, Protocol, Socket, Type};
        let domain = if addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
        let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        if addr.is_ipv6() {
            let _ = sock.set_only_v6(false);
        }
        // Generous kernel buffers: video bursts at multi-Gbit LAN rates.
        let _ = sock.set_recv_buffer_size(4 * 1024 * 1024);
        let _ = sock.set_send_buffer_size(4 * 1024 * 1024);
        sock.bind(&addr.into())?;
        Self::from_std_socket(sock.into(), identity)
    }

    /// Start the endpoint on an existing socket (e.g. one already used for STUN). Must be called
    /// from within a tokio runtime. STUN and rendezvous traffic keep working on the same socket
    /// afterwards through the demultiplexer (see [`crate::rendezvous::RendezvousClient`]).
    pub fn from_std_socket(sock: std::net::UdpSocket, identity: Identity) -> Result<Self> {
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(NetError::Config("AetherEndpoint must be created inside a tokio runtime".into()));
        }
        sock.set_nonblocking(true)?;
        let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);
        let raw = runtime.wrap_udp_socket(sock)?;
        let (demux, aux_rx) = DemuxSocket::new(raw);
        let aux = demux.sender();

        let transport = Arc::new(transport_config());
        let server_tls = QuicServerConfig::try_from(server_crypto(&identity)?)
            .map_err(|e| NetError::Config(format!("server TLS: {e}")))?;
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_tls));
        server_config.transport_config(transport.clone());

        let client_tls = QuicClientConfig::try_from(client_crypto(&identity)?)
            .map_err(|e| NetError::Config(format!("client TLS: {e}")))?;
        let mut client_config = quinn::ClientConfig::new(Arc::new(client_tls));
        client_config.transport_config(transport);

        let mut endpoint_config = quinn::EndpointConfig::default();
        // Keep the QUIC fixed bit set so the socket can be demultiplexed with STUN/rendezvous.
        endpoint_config.grease_quic_bit(false);

        let endpoint = quinn::Endpoint::new_with_abstract_socket(endpoint_config, Some(server_config), demux, runtime)?;
        Ok(Self {
            inner: Arc::new(EndpointInner {
                endpoint,
                identity,
                client_config,
                aux,
                aux_rx: Mutex::new(Some(aux_rx)),
                limiter: Arc::new(AuthRateLimiter::default()),
            }),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.inner.endpoint.local_addr()?)
    }

    pub fn identity(&self) -> &Identity {
        &self.inner.identity
    }

    /// Failed-password limiter shared by all incoming connections of this endpoint.
    pub fn rate_limiter(&self) -> &Arc<AuthRateLimiter> {
        &self.inner.limiter
    }

    /// Connect and authenticate to a host at `addr`.
    pub async fn connect(&self, addr: SocketAddr, hello: Hello, auth: ClientAuth) -> Result<Session> {
        self.connect_with_key(addr, hello, auth, None).await
    }

    /// Like [`connect`](Self::connect), but fails unless the host's identity key equals
    /// `expected_host_key` (pinning for paired devices / rendezvous-looked-up keys).
    pub async fn connect_with_key(
        &self,
        addr: SocketAddr,
        hello: Hello,
        auth: ClientAuth,
        expected_host_key: Option<[u8; 32]>,
    ) -> Result<Session> {
        let conn = self.connect_quic(addr, QUIC_CONNECT_TIMEOUT).await?;
        self.handshake_client(conn, hello, auth, expected_host_key).await
    }

    pub(crate) async fn handshake_client(
        &self,
        conn: quinn::Connection,
        hello: Hello,
        auth: ClientAuth,
        expected_host_key: Option<[u8; 32]>,
    ) -> Result<Session> {
        self.check_hello(&hello)?;
        client_handshake(conn, hello, auth, expected_host_key).await
    }

    fn check_hello(&self, hello: &Hello) -> Result<()> {
        if hello.public_key != self.inner.identity.public_key() {
            return Err(NetError::Protocol("Hello.public_key must be this endpoint's identity key".into()));
        }
        Ok(())
    }

    /// QUIC + TLS handshake only (no application authentication).
    pub(crate) async fn connect_quic(&self, addr: SocketAddr, timeout: Duration) -> Result<quinn::Connection> {
        let addr = self.map_addr(addr);
        let connecting = self.inner.endpoint.connect_with(self.inner.client_config.clone(), addr, SERVER_NAME)?;
        tokio::time::timeout(timeout, connecting)
            .await
            .map_err(|_| NetError::Timeout("QUIC handshake"))?
            .map_err(NetError::from)
    }

    /// IPv4 targets on a dual-stack IPv6 socket must be IPv4-mapped.
    fn map_addr(&self, addr: SocketAddr) -> SocketAddr {
        match (self.inner.endpoint.local_addr(), addr) {
            (Ok(SocketAddr::V6(_)), SocketAddr::V4(v4)) => SocketAddr::new(v4.ip().to_ipv6_mapped().into(), v4.port()),
            _ => addr,
        }
    }

    /// Accept the next incoming connection and run the host-side handshake.
    /// Returns `None` once the endpoint is closed.
    pub async fn accept(&self, hello: Hello, policy: Arc<dyn HostAuthPolicy>) -> Option<Result<Session>> {
        let incoming = self.accept_connection().await?;
        Some(incoming.authenticate(hello, policy).await)
    }

    /// Accept the next incoming QUIC connection without authenticating it yet, so the caller
    /// can authenticate several connections concurrently (e.g. while a consent prompt is open).
    pub async fn accept_connection(&self) -> Option<IncomingConnection> {
        let incoming = self.inner.endpoint.accept().await?;
        Some(IncomingConnection { incoming, endpoint: self.clone() })
    }

    /// Close all connections immediately.
    pub fn close(&self) {
        self.inner.endpoint.close(0u32.into(), b"endpoint closed");
    }

    /// Wait until all connections are cleanly shut down.
    pub async fn wait_idle(&self) {
        self.inner.endpoint.wait_idle().await;
    }

    pub(crate) fn aux_sender(&self) -> AuxSender {
        self.inner.aux.clone()
    }

    pub(crate) fn take_aux_receiver(&self) -> Option<mpsc::Receiver<AuxPacket>> {
        self.inner.aux_rx.lock().take()
    }
}

/// An incoming QUIC connection that has not been authenticated yet.
pub struct IncomingConnection {
    incoming: quinn::Incoming,
    endpoint: AetherEndpoint,
}

impl std::fmt::Debug for IncomingConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncomingConnection").field("remote", &self.incoming.remote_address()).finish()
    }
}

impl IncomingConnection {
    pub fn remote_address(&self) -> SocketAddr {
        self.incoming.remote_address()
    }

    /// Refuse the connection without completing the handshake (e.g. host busy).
    pub fn refuse(self) {
        self.incoming.refuse();
    }

    /// Complete the QUIC handshake and run the host-side authentication.
    pub async fn authenticate(self, hello: Hello, policy: Arc<dyn HostAuthPolicy>) -> Result<Session> {
        self.endpoint.check_hello(&hello)?;
        let conn = tokio::time::timeout(QUIC_CONNECT_TIMEOUT, async { self.incoming.accept()?.await })
            .await
            .map_err(|_| NetError::Timeout("QUIC handshake"))??;
        // Fail fast on certificates that are not Ed25519 identities.
        peer_key(&conn)?;
        host_handshake(conn, hello, policy, self.endpoint.inner.limiter.clone()).await
    }
}
