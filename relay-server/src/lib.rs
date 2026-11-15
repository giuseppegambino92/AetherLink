//! # aether-relay
//!
//! AetherLink rendezvous + relay server (UDP, protocol in [`aether_protocol::rendezvous`]).
//!
//! * **Registration**: hosts prove ownership of their Ed25519 key by signing
//!   `(device_id, public_key, timestamp, nonce)`; the nonce is a stateless keyed hash of the
//!   observed source address and a 30 s time bucket, so a captured registration cannot be
//!   replayed from elsewhere or later. Key-derived IDs always belong to their key; other
//!   (ephemeral) IDs are first-come-first-served until they expire.
//! * **Lookup**: returns the host's reflexive + LAN addresses and simultaneously sends the host
//!   a `PunchRequest` with the client's addresses so both sides punch at the same time.
//! * **Relay**: allocates two UDP ports per session (one per leg); each peer latches its leg
//!   with a secret token, after which datagrams are forwarded blindly between the two latched
//!   addresses. The relay only ever sees end-to-end encrypted QUIC packets.
//! * Also answers plain STUN Binding requests (RFC 5389) on the main port.
//!
//! Abuse controls: per-IP token-bucket rate limiting of control traffic, global and per-IP
//! session caps, registration cap, idle/latch/max-lifetime timeouts.

use aether_protocol::DeviceId;
use aether_protocol::rendezvous::{
    MAX_LOCAL_ADDRS, REGISTRATION_TTL_SECS, RendezvousErrorCode, RendezvousMessage, decode_rendezvous,
    encode_rendezvous, is_rendezvous_packet, register_signing_payload,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use tokio::net::UdpSocket;
use tokio::sync::watch;

/// Relay buffer per leg; QUIC never sends datagrams above ~1452 bytes by default.
const LEG_BUF: usize = 2048;
const NONCE_BUCKET_SECS: u64 = 30;

/// Server configuration.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Main rendezvous socket.
    pub bind: SocketAddr,
    /// Inclusive UDP port range for relay legs; `None` = ephemeral ports.
    pub relay_ports: Option<(u16, u16)>,
    pub max_sessions: usize,
    pub max_sessions_per_ip: usize,
    pub max_registrations: usize,
    pub registration_ttl: Duration,
    /// A relay session with no forwarded traffic for this long is closed.
    pub session_idle_timeout: Duration,
    /// Both legs must latch within this time.
    pub latch_timeout: Duration,
    /// Hard cap on a relay session's lifetime.
    pub max_session_duration: Duration,
    /// Control packets per second per source IP (token bucket refill rate).
    pub rate_per_sec: f64,
    /// Token bucket depth.
    pub rate_burst: f64,
    pub max_clock_skew: Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], aether_protocol::DEFAULT_RELAY_PORT)),
            relay_ports: None,
            max_sessions: 512,
            max_sessions_per_ip: 8,
            max_registrations: 20_000,
            registration_ttl: Duration::from_secs(REGISTRATION_TTL_SECS as u64),
            session_idle_timeout: Duration::from_secs(60),
            latch_timeout: Duration::from_secs(30),
            max_session_duration: Duration::from_secs(24 * 3600),
            rate_per_sec: 20.0,
            rate_burst: 60.0,
            max_clock_skew: Duration::from_secs(300),
        }
    }
}

/// Live counters.
#[derive(Debug, Default)]
pub struct RelayStats {
    pub registrations: AtomicU64,
    pub lookups: AtomicU64,
    pub sessions_created: AtomicU64,
    pub sessions_active: AtomicU64,
    pub bytes_relayed: AtomicU64,
    pub packets_relayed: AtomicU64,
    pub rate_limited: AtomicU64,
}

struct Registration {
    public_key: [u8; 32],
    observed: SocketAddr,
    local_addrs: Vec<SocketAddr>,
    expires: Instant,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Default)]
struct SessionTable {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

struct Shared {
    cfg: RelayConfig,
    secret: [u8; 32],
    regs: Mutex<HashMap<DeviceId, Registration>>,
    rate: Mutex<HashMap<IpAddr, Bucket>>,
    sessions: Mutex<SessionTable>,
    port_cursor: AtomicU32,
    stats: Arc<RelayStats>,
}

/// The rendezvous + relay server.
pub struct RelayServer {
    socket: Arc<UdpSocket>,
    shared: Arc<Shared>,
    shutdown: watch::Sender<bool>,
}

/// Handle to a server running in the background (see [`RelayServer::spawn`]).
pub struct RelayHandle {
    pub local_addr: SocketAddr,
    pub stats: Arc<RelayStats>,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<io::Result<()>>,
}

impl RelayHandle {
    /// Stop the server and all relay sessions.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn canonical(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip().to_canonical(), a.port())
}

impl RelayServer {
    pub async fn bind(cfg: RelayConfig) -> io::Result<Self> {
        if let Some((lo, hi)) = cfg.relay_ports
            && (lo == 0 || lo > hi)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid relay port range"));
        }
        let socket = Arc::new(UdpSocket::bind(cfg.bind).await?);
        let mut secret = [0u8; 32];
        rand::Rng::fill(&mut rand::thread_rng(), &mut secret);
        let (shutdown, _) = watch::channel(false);
        Ok(Self {
            socket,
            shared: Arc::new(Shared {
                cfg,
                secret,
                regs: Mutex::default(),
                rate: Mutex::default(),
                sessions: Mutex::default(),
                port_cursor: AtomicU32::new(rand::random::<u16>() as u32),
                stats: Arc::default(),
            }),
            shutdown,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn stats(&self) -> Arc<RelayStats> {
        self.shared.stats.clone()
    }

    /// Run in a background task.
    pub fn spawn(self) -> io::Result<RelayHandle> {
        let local_addr = self.local_addr()?;
        let stats = self.stats();
        let shutdown = self.shutdown.clone();
        let task = tokio::spawn(self.run());
        Ok(RelayHandle { local_addr, stats, shutdown, task })
    }

    /// Serve until the socket fails or shutdown is requested.
    pub async fn run(self) -> io::Result<()> {
        let local = self.local_addr()?;
        tracing::info!(%local, "aether-relay listening");
        let mut shutdown_rx = self.shutdown.subscribe();
        let janitor = {
            let shared = self.shared.clone();
            let mut rx = self.shutdown.subscribe();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(10));
                loop {
                    tokio::select! {
                        _ = tick.tick() => shared.cleanup(),
                        _ = rx.changed() => return,
                    }
                }
            })
        };
        let mut buf = vec![0u8; 1500];
        let result = loop {
            let (n, from) = tokio::select! {
                r = self.socket.recv_from(&mut buf) => match r {
                    Ok(x) => x,
                    // ICMP port-unreachable surfaces as ECONNRESET on some platforms; ignore.
                    Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused) => continue,
                    Err(e) => break Err(e),
                },
                _ = shutdown_rx.changed() => break Ok(()),
            };
            let from = canonical(from);
            let packet = &buf[..n];
            if !self.shared.allow(from.ip()) {
                self.shared.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if is_rendezvous_packet(packet) {
                let Some(msg) = decode_rendezvous(packet) else { continue };
                self.handle(msg, from).await;
            } else if let Some(resp) = stun_binding_response(packet, from) {
                let _ = self.socket.send_to(&resp, from).await;
            }
        };
        janitor.abort();
        result
    }

    async fn send(&self, msg: &RendezvousMessage, to: SocketAddr) {
        match encode_rendezvous(msg) {
            Ok(pkt) => {
                if let Err(e) = self.socket.send_to(&pkt, to).await {
                    tracing::debug!(%to, "send failed: {e}");
                }
            }
            Err(e) => tracing::error!("encode failed: {e}"),
        }
    }

    async fn handle(&self, msg: RendezvousMessage, from: SocketAddr) {
        let s = &self.shared;
        match msg {
            RendezvousMessage::RegisterInit { request_id } => {
                let reply = RendezvousMessage::Challenge {
                    request_id,
                    nonce: s.nonce(from, unix_ms() / 1000 / NONCE_BUCKET_SECS),
                    observed: from,
                    server_time_ms: unix_ms(),
                };
                self.send(&reply, from).await;
            }
            RendezvousMessage::Register {
                request_id,
                device_id,
                public_key,
                local_addrs,
                timestamp_ms,
                nonce,
                signature,
            } => {
                let reply = match s.register(from, device_id, public_key, local_addrs, timestamp_ms, nonce, &signature) {
                    Ok(()) => {
                        s.stats.registrations.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(%device_id, %from, "registered");
                        RendezvousMessage::Registered {
                            request_id,
                            observed: from,
                            ttl_secs: s.cfg.registration_ttl.as_secs() as u32,
                            next_nonce: s.nonce(from, unix_ms() / 1000 / NONCE_BUCKET_SECS),
                            server_time_ms: unix_ms(),
                        }
                    }
                    Err(code) => {
                        tracing::debug!(%device_id, %from, ?code, "registration rejected");
                        RendezvousMessage::Error { request_id, code }
                    }
                };
                self.send(&reply, from).await;
            }
            RendezvousMessage::Unregister { device_id, timestamp_ms, nonce, signature } => {
                s.unregister(from, device_id, timestamp_ms, nonce, &signature);
            }
            RendezvousMessage::Lookup { request_id, target, requester_public_key, mut local_addrs } => {
                s.stats.lookups.fetch_add(1, Ordering::Relaxed);
                local_addrs.truncate(MAX_LOCAL_ADDRS);
                let found = {
                    let regs = s.regs.lock();
                    regs.get(&target)
                        .filter(|r| r.expires > Instant::now())
                        .map(|r| (r.public_key, r.observed, r.local_addrs.clone()))
                };
                match found {
                    Some((public_key, host_addr, host_locals)) => {
                        let punch = RendezvousMessage::PunchRequest {
                            requester_public_key,
                            reflexive: from,
                            local_addrs,
                        };
                        self.send(&punch, host_addr).await;
                        let reply = RendezvousMessage::LookupResult {
                            request_id,
                            device_id: target,
                            public_key,
                            reflexive: host_addr,
                            local_addrs: host_locals,
                            observed: from,
                        };
                        self.send(&reply, from).await;
                    }
                    None => {
                        self.send(&RendezvousMessage::Error { request_id, code: RendezvousErrorCode::NotFound }, from)
                            .await
                    }
                }
            }
            RendezvousMessage::RelayRequest { request_id, target, requester_public_key } => {
                let reply = match self.allocate(from, target, requester_public_key).await {
                    Ok((port, token)) => RendezvousMessage::RelayAllocated { request_id, port, token },
                    Err(code) => RendezvousMessage::Error { request_id, code },
                };
                self.send(&reply, from).await;
            }
            // Peer-to-peer or server-originated messages are not valid here.
            _ => {}
        }
    }

    async fn allocate(
        &self,
        from: SocketAddr,
        target: DeviceId,
        requester_public_key: [u8; 32],
    ) -> Result<(u16, [u8; 16]), RendezvousErrorCode> {
        let s = &self.shared;
        let host_addr = {
            let regs = s.regs.lock();
            regs.get(&target).filter(|r| r.expires > Instant::now()).map(|r| r.observed)
        }
        .ok_or(RendezvousErrorCode::NotFound)?;

        let guard = SessionGuard::acquire(s.clone(), from.ip()).ok_or(RendezvousErrorCode::RelayUnavailable)?;
        let leg_client = s.bind_leg().await.map_err(|_| RendezvousErrorCode::RelayUnavailable)?;
        let leg_host = s.bind_leg().await.map_err(|_| RendezvousErrorCode::RelayUnavailable)?;
        let port_client = leg_client.local_addr().map_err(|_| RendezvousErrorCode::RelayUnavailable)?.port();
        let port_host = leg_host.local_addr().map_err(|_| RendezvousErrorCode::RelayUnavailable)?.port();
        let token_client: [u8; 16] = rand::random();
        let token_host: [u8; 16] = rand::random();

        s.stats.sessions_created.fetch_add(1, Ordering::Relaxed);
        tracing::info!(%target, client = %from, host = %host_addr, port_client, port_host, "relay session allocated");
        let session = RelaySession {
            legs: [leg_client, leg_host],
            tokens: [token_client, token_host],
            shared: s.clone(),
            shutdown: self.shutdown.subscribe(),
            _guard: guard,
        };
        tokio::spawn(session.run());

        self.send(&RendezvousMessage::RelayOffer { port: port_host, token: token_host, requester_public_key }, host_addr)
            .await;
        Ok((port_client, token_client))
    }
}

impl Shared {
    fn nonce(&self, addr: SocketAddr, bucket: u64) -> [u8; 16] {
        let mut h = blake3::Hasher::new_keyed(&self.secret);
        h.update(b"register-nonce");
        match addr.ip() {
            IpAddr::V4(v4) => h.update(&v4.octets()),
            IpAddr::V6(v6) => h.update(&v6.octets()),
        };
        h.update(&addr.port().to_le_bytes());
        h.update(&bucket.to_le_bytes());
        h.finalize().as_bytes()[..16].try_into().unwrap()
    }

    fn nonce_valid(&self, addr: SocketAddr, nonce: &[u8; 16]) -> bool {
        let bucket = unix_ms() / 1000 / NONCE_BUCKET_SECS;
        let cur: bool = self.nonce(addr, bucket).ct_eq(nonce).into();
        let prev: bool = self.nonce(addr, bucket.saturating_sub(1)).ct_eq(nonce).into();
        cur | prev
    }

    fn check_signed(&self, from: SocketAddr, claim: &SignedClaim<'_>) -> Result<(), RendezvousErrorCode> {
        let skew = unix_ms().abs_diff(claim.timestamp_ms);
        if skew > self.cfg.max_clock_skew.as_millis() as u64 {
            return Err(RendezvousErrorCode::ClockSkew);
        }
        if !self.nonce_valid(from, claim.nonce) {
            return Err(RendezvousErrorCode::BadNonce);
        }
        let payload =
            register_signing_payload(claim.action, claim.device_id, claim.public_key, claim.timestamp_ms, claim.nonce);
        let vk = ed25519_dalek::VerifyingKey::from_bytes(claim.public_key).map_err(|_| RendezvousErrorCode::BadSignature)?;
        let sig = ed25519_dalek::Signature::from_slice(claim.signature).map_err(|_| RendezvousErrorCode::BadSignature)?;
        vk.verify_strict(&payload, &sig).map_err(|_| RendezvousErrorCode::BadSignature)
    }

    #[allow(clippy::too_many_arguments)]
    fn register(
        &self,
        from: SocketAddr,
        device_id: DeviceId,
        public_key: [u8; 32],
        mut local_addrs: Vec<SocketAddr>,
        timestamp_ms: u64,
        nonce: [u8; 16],
        signature: &[u8],
    ) -> Result<(), RendezvousErrorCode> {
        if local_addrs.len() > MAX_LOCAL_ADDRS {
            return Err(RendezvousErrorCode::Malformed);
        }
        let claim = SignedClaim {
            action: b"register",
            device_id,
            public_key: &public_key,
            timestamp_ms,
            nonce: &nonce,
            signature,
        };
        self.check_signed(from, &claim)?;
        local_addrs.retain(|a| a.port() != 0 && !a.ip().is_unspecified() && !a.ip().is_multicast());
        let now = Instant::now();
        let mut regs = self.regs.lock();
        if let Some(existing) = regs.get(&device_id)
            && existing.expires > now
            && existing.public_key != public_key
            && DeviceId::from_public_key(&public_key) != device_id
        {
            // Ephemeral IDs stay bound to their key until expiry; only the key a stable ID is
            // derived from may take it over.
            return Err(RendezvousErrorCode::IdTaken);
        }
        if !regs.contains_key(&device_id) && regs.len() >= self.cfg.max_registrations {
            return Err(RendezvousErrorCode::RelayUnavailable);
        }
        regs.insert(
            device_id,
            Registration { public_key, observed: from, local_addrs, expires: now + self.cfg.registration_ttl },
        );
        Ok(())
    }

    fn unregister(&self, from: SocketAddr, device_id: DeviceId, timestamp_ms: u64, nonce: [u8; 16], signature: &[u8]) {
        let Some(pk) = self.regs.lock().get(&device_id).map(|r| r.public_key) else { return };
        let claim = SignedClaim {
            action: b"unregister",
            device_id,
            public_key: &pk,
            timestamp_ms,
            nonce: &nonce,
            signature,
        };
        if self.check_signed(from, &claim).is_ok() {
            self.regs.lock().remove(&device_id);
            tracing::debug!(%device_id, "unregistered");
        }
    }

    /// Token bucket per source IP.
    fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut rate = self.rate.lock();
        let b = rate.entry(ip).or_insert(Bucket { tokens: self.cfg.rate_burst, last: now });
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.cfg.rate_per_sec).min(self.cfg.rate_burst);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn cleanup(&self) {
        let now = Instant::now();
        self.regs.lock().retain(|_, r| r.expires > now);
        let full_after = Duration::from_secs_f64(self.cfg.rate_burst / self.cfg.rate_per_sec.max(0.001));
        self.rate.lock().retain(|_, b| now.duration_since(b.last) < full_after);
    }

    async fn bind_leg(&self) -> io::Result<UdpSocket> {
        let ip = self.cfg.bind.ip();
        match self.cfg.relay_ports {
            None => UdpSocket::bind((ip, 0)).await,
            Some((lo, hi)) => {
                let n = (hi - lo) as u32 + 1;
                let start = self.port_cursor.fetch_add(1, Ordering::Relaxed);
                for k in 0..n {
                    let port = lo + ((start.wrapping_add(k)) % n) as u16;
                    if let Ok(s) = UdpSocket::bind((ip, port)).await {
                        return Ok(s);
                    }
                }
                Err(io::Error::new(io::ErrorKind::AddrInUse, "relay port range exhausted"))
            }
        }
    }
}

/// The signed fields of a `Register` / `Unregister` message.
struct SignedClaim<'a> {
    action: &'a [u8],
    device_id: DeviceId,
    public_key: &'a [u8; 32],
    timestamp_ms: u64,
    nonce: &'a [u8; 16],
    signature: &'a [u8],
}

/// Holds one session slot (global + per-IP) until dropped.
struct SessionGuard {
    shared: Arc<Shared>,
    ip: IpAddr,
}

impl SessionGuard {
    fn acquire(shared: Arc<Shared>, ip: IpAddr) -> Option<Self> {
        {
            let mut t = shared.sessions.lock();
            let per_ip = t.per_ip.get(&ip).copied().unwrap_or(0);
            if t.total >= shared.cfg.max_sessions || per_ip >= shared.cfg.max_sessions_per_ip {
                return None;
            }
            t.total += 1;
            *t.per_ip.entry(ip).or_insert(0) += 1;
        }
        shared.stats.sessions_active.fetch_add(1, Ordering::Relaxed);
        Some(Self { shared, ip })
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let mut t = self.shared.sessions.lock();
        t.total = t.total.saturating_sub(1);
        if let Some(c) = t.per_ip.get_mut(&self.ip) {
            *c -= 1;
            if *c == 0 {
                t.per_ip.remove(&self.ip);
            }
        }
        self.shared.stats.sessions_active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One relay session: leg 0 = client, leg 1 = host.
struct RelaySession {
    legs: [UdpSocket; 2],
    tokens: [[u8; 16]; 2],
    shared: Arc<Shared>,
    shutdown: watch::Receiver<bool>,
    _guard: SessionGuard,
}

impl RelaySession {
    async fn run(mut self) {
        let cfg = &self.shared.cfg;
        let created = Instant::now();
        let mut latched: [Option<SocketAddr>; 2] = [None, None];
        let mut bufs = [vec![0u8; LEG_BUF], vec![0u8; LEG_BUF]];
        let mut last_activity = Instant::now();
        let mut bytes = [0u64; 2];
        let reason = loop {
            let both = latched[0].is_some() && latched[1].is_some();
            let deadline = if both {
                (last_activity + cfg.session_idle_timeout).min(created + cfg.max_session_duration)
            } else {
                created + cfg.latch_timeout
            };
            let [buf0, buf1] = &mut bufs;
            let (leg, res) = tokio::select! {
                r = self.legs[0].recv_from(buf0) => (0usize, r),
                r = self.legs[1].recv_from(buf1) => (1usize, r),
                _ = tokio::time::sleep_until(deadline.into()) => {
                    break if both { "idle/lifetime timeout" } else { "latch timeout" };
                }
                _ = self.shutdown.changed() => break "server shutdown",
            };
            let (n, from) = match res {
                Ok(x) => x,
                Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused) => continue,
                Err(e) => {
                    tracing::warn!("relay leg error: {e}");
                    break "socket error";
                }
            };
            let from = canonical(from);
            let other = 1 - leg;
            let packet = &bufs[leg][..n];
            if is_rendezvous_packet(packet) {
                // Control: (re-)latch with the leg's secret token.
                if let Some(RendezvousMessage::RelayLatch { token }) = decode_rendezvous(packet)
                    && bool::from(token.ct_eq(&self.tokens[leg]))
                {
                    let newly = latched[leg] != Some(from);
                    latched[leg] = Some(from);
                    let ready = latched[other].is_some();
                    self.reply(leg, from, ready).await;
                    if newly && ready {
                        let peer = latched[other].unwrap();
                        self.reply(other, peer, true).await;
                        last_activity = Instant::now();
                    }
                }
                continue;
            }
            if latched[leg] != Some(from) {
                continue; // not from the latched peer: drop
            }
            if let Some(dst) = latched[other] {
                match self.legs[other].send_to(packet, dst).await {
                    Ok(_) => {
                        bytes[leg] += n as u64;
                        self.shared.stats.bytes_relayed.fetch_add(n as u64, Ordering::Relaxed);
                        self.shared.stats.packets_relayed.fetch_add(1, Ordering::Relaxed);
                        last_activity = Instant::now();
                    }
                    Err(e) => tracing::debug!(%dst, "relay forward failed: {e}"),
                }
            }
        };
        tracing::info!(
            reason,
            up = bytes[0],
            down = bytes[1],
            secs = created.elapsed().as_secs(),
            "relay session closed"
        );
    }

    async fn reply(&self, leg: usize, to: SocketAddr, peer_ready: bool) {
        if let Ok(pkt) = encode_rendezvous(&RendezvousMessage::RelayLatched { token: self.tokens[leg], peer_ready }) {
            let _ = self.legs[leg].send_to(&pkt, to).await;
        }
    }
}

/// Answer an RFC 5389 Binding request with XOR-MAPPED-ADDRESS (`None` if not a valid request).
pub fn stun_binding_response(req: &[u8], from: SocketAddr) -> Option<Vec<u8>> {
    const COOKIE: u32 = 0x2112_A442;
    if req.len() < 20 || req[0..2] != [0x00, 0x01] || req[4..8] != COOKIE.to_be_bytes() {
        return None;
    }
    let len = u16::from_be_bytes([req[2], req[3]]) as usize;
    if req.len() != 20 + len {
        return None;
    }
    let txid = &req[8..20];
    let mut attr = Vec::with_capacity(24);
    let port = from.port() ^ (COOKIE >> 16) as u16;
    match from.ip() {
        IpAddr::V4(v4) => {
            attr.extend_from_slice(&[0x00, 0x01]);
            attr.extend_from_slice(&port.to_be_bytes());
            attr.extend_from_slice(&(u32::from(v4) ^ COOKIE).to_be_bytes());
        }
        IpAddr::V6(v6) => {
            attr.extend_from_slice(&[0x00, 0x02]);
            attr.extend_from_slice(&port.to_be_bytes());
            let mut key = [0u8; 16];
            key[..4].copy_from_slice(&COOKIE.to_be_bytes());
            key[4..].copy_from_slice(txid);
            let o = v6.octets();
            attr.extend((0..16).map(|i| o[i] ^ key[i]));
        }
    }
    let mut out = Vec::with_capacity(20 + 4 + attr.len());
    out.extend_from_slice(&[0x01, 0x01]);
    out.extend_from_slice(&((4 + attr.len()) as u16).to_be_bytes());
    out.extend_from_slice(&COOKIE.to_be_bytes());
    out.extend_from_slice(txid);
    out.extend_from_slice(&[0x00, 0x20]);
    out.extend_from_slice(&(attr.len() as u16).to_be_bytes());
    out.extend_from_slice(&attr);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(cfg: RelayConfig) -> Shared {
        Shared {
            cfg,
            secret: [9; 32],
            regs: Mutex::default(),
            rate: Mutex::default(),
            sessions: Mutex::default(),
            port_cursor: AtomicU32::new(0),
            stats: Arc::default(),
        }
    }

    #[test]
    fn token_bucket() {
        let s = shared(RelayConfig { rate_per_sec: 0.001, rate_burst: 3.0, ..Default::default() });
        let ip: IpAddr = "198.51.100.1".parse().unwrap();
        assert!(s.allow(ip) && s.allow(ip) && s.allow(ip));
        assert!(!s.allow(ip));
        assert!(s.allow("198.51.100.2".parse().unwrap()));
    }

    #[test]
    fn nonce_is_address_bound() {
        let s = shared(RelayConfig::default());
        let a: SocketAddr = "198.51.100.1:4000".parse().unwrap();
        let b: SocketAddr = "198.51.100.1:4001".parse().unwrap();
        let bucket = unix_ms() / 1000 / NONCE_BUCKET_SECS;
        let n = s.nonce(a, bucket);
        assert!(s.nonce_valid(a, &n));
        assert!(!s.nonce_valid(b, &n));
        assert!(!s.nonce_valid(a, &s.nonce(a, bucket - 5)));
    }

    #[test]
    fn session_caps() {
        let s = Arc::new(shared(RelayConfig { max_sessions: 3, max_sessions_per_ip: 2, ..Default::default() }));
        let a: IpAddr = "198.51.100.1".parse().unwrap();
        let b: IpAddr = "198.51.100.2".parse().unwrap();
        let g1 = SessionGuard::acquire(s.clone(), a).unwrap();
        let _g2 = SessionGuard::acquire(s.clone(), a).unwrap();
        assert!(SessionGuard::acquire(s.clone(), a).is_none(), "per-IP cap");
        let _g3 = SessionGuard::acquire(s.clone(), b).unwrap();
        assert!(SessionGuard::acquire(s.clone(), b).is_none(), "global cap");
        drop(g1);
        assert!(SessionGuard::acquire(s.clone(), b).is_some());
    }

    #[test]
    fn stun_response_encodes_xor_mapped() {
        let mut req = vec![0x00, 0x01, 0x00, 0x00, 0x21, 0x12, 0xa4, 0x42];
        req.extend_from_slice(&[7; 12]);
        let resp = stun_binding_response(&req, "192.0.2.1:32853".parse().unwrap()).unwrap();
        // Same encoding as RFC 5769 §2.2's XOR-MAPPED-ADDRESS.
        assert_eq!(&resp[20..32], &[0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43]);
        assert!(stun_binding_response(&req[..19], "192.0.2.1:1".parse().unwrap()).is_none());
    }
}
