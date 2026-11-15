//! Rendezvous / relay client. Wire types live in [`aether_protocol::rendezvous`].
//!
//! The client runs on the [`AetherEndpoint`]'s own UDP socket (through the demultiplexer), so
//! the reflexive address the server records is exactly the NAT mapping QUIC uses, punch
//! packets open that same mapping, and relay latching binds that same socket.
//!
//! A host calls [`RendezvousClient::start_hosting`]: it registers (and refreshes) its device
//! ID and automatically answers `PunchRequest`s (punch burst towards the client) and
//! `RelayOffer`s (latch onto the relay leg). A client calls [`RendezvousClient::lookup`] /
//! [`RendezvousClient::request_relay`], usually through [`crate::connect_to_device`].

use crate::socket::{AuxPacket, AuxSender};
use crate::{AetherEndpoint, Identity, NetError, Result, stun};
use aether_protocol::DeviceId;
use aether_protocol::rendezvous::{
    MAX_LOCAL_ADDRS, RendezvousMessage, decode_rendezvous, encode_rendezvous, register_signing_payload,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::AbortHandle;

/// Request retransmission schedule towards the rendezvous server.
const REQUEST_SCHEDULE_MS: [u64; 4] = [400, 800, 1600, 2400];
/// Registration refresh period (also keeps the NAT mapping towards the server alive).
const REFRESH_EVERY: Duration = Duration::from_secs(20);
/// Hole-punch probe schedule (ms offsets from the start of the burst).
const PUNCH_SCHEDULE_MS: [u64; 9] = [0, 20, 50, 100, 200, 400, 700, 1100, 1600];
/// How long a host keeps trying to latch onto a relay leg it was offered.
const HOST_LATCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Host-side cap on punch bursts per second (all requesters together).
const MAX_PUNCH_BURSTS_PER_SEC: usize = 10;
/// Minimum interval between two punch bursts towards the same address.
const PUNCH_DEDUP_WINDOW: Duration = Duration::from_secs(2);

/// Result of a lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub device_id: DeviceId,
    /// Host identity key as registered (signed) with the server.
    pub public_key: [u8; 32],
    /// Host's server-reflexive address (its NAT mapping towards the server).
    pub reflexive: SocketAddr,
    /// Host's LAN addresses.
    pub local_addrs: Vec<SocketAddr>,
    /// Our own reflexive address as seen by the server.
    pub observed_self: SocketAddr,
}

/// A relay leg allocated for us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAllocation {
    pub relay_addr: SocketAddr,
    pub token: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub device_id: DeviceId,
    /// Our reflexive address as seen by the server.
    pub observed: SocketAddr,
    pub ttl: Duration,
}

/// Notifications for the host UI / logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RendezvousEvent {
    Registered(Registration),
    RegistrationFailed(String),
    /// A client looked us up; we punched towards `addrs`.
    PunchRequested { requester_public_key: [u8; 32], addrs: Vec<SocketAddr> },
    /// A client requested a relay; we are latching onto `relay_addr`.
    RelayOffered { relay_addr: SocketAddr, requester_public_key: [u8; 32] },
}

struct Inner {
    server: SocketAddr,
    identity: Identity,
    aux: AuxSender,
    pending: Mutex<HashMap<u64, oneshot::Sender<RendezvousMessage>>>,
    stun_pending: Mutex<HashMap<[u8; 12], oneshot::Sender<stun::StunMessage>>>,
    latch_waiters: Mutex<HashMap<[u8; 16], watch::Sender<Option<bool>>>>,
    events: broadcast::Sender<RendezvousEvent>,
    hosting: AtomicBool,
    clock_offset_ms: AtomicI64,
    next_nonce: Mutex<Option<([u8; 16], Instant)>>,
    /// Recent host-side punch bursts (anti-reflection rate limit).
    recent_punches: Mutex<std::collections::VecDeque<(Instant, SocketAddr)>>,
}

/// Rendezvous / relay client bound to an endpoint's socket. Dropping it stops background tasks.
pub struct RendezvousClient {
    inner: Arc<Inner>,
    dispatcher: AbortHandle,
    hosting_task: Mutex<Option<AbortHandle>>,
}

impl std::fmt::Debug for RendezvousClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RendezvousClient").field("server", &self.inner.server).finish()
    }
}

impl Drop for RendezvousClient {
    fn drop(&mut self) {
        self.dispatcher.abort();
        if let Some(h) = self.hosting_task.lock().take() {
            h.abort();
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn canonical(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip().to_canonical(), a.port())
}

fn response_id(msg: &RendezvousMessage) -> Option<u64> {
    use RendezvousMessage::*;
    match msg {
        Challenge { request_id, .. }
        | Registered { request_id, .. }
        | LookupResult { request_id, .. }
        | RelayAllocated { request_id, .. }
        | Error { request_id, .. } => Some(*request_id),
        _ => None,
    }
}

impl RendezvousClient {
    /// Attach to `endpoint`'s socket. Only one client per endpoint (the socket's auxiliary
    /// channel can only be taken once).
    pub fn new(endpoint: &AetherEndpoint, server: SocketAddr) -> Result<Self> {
        let rx = endpoint
            .take_aux_receiver()
            .ok_or_else(|| NetError::Config("a RendezvousClient is already attached to this endpoint".into()))?;
        let (events, _) = broadcast::channel(64);
        let inner = Arc::new(Inner {
            server: canonical(server),
            identity: endpoint.identity().clone(),
            aux: endpoint.aux_sender(),
            pending: Mutex::default(),
            stun_pending: Mutex::default(),
            latch_waiters: Mutex::default(),
            events,
            hosting: AtomicBool::new(false),
            clock_offset_ms: AtomicI64::new(0),
            next_nonce: Mutex::new(None),
            recent_punches: Mutex::default(),
        });
        let dispatcher = tokio::spawn(dispatch(Arc::downgrade(&inner), rx)).abort_handle();
        Ok(Self { inner, dispatcher, hosting_task: Mutex::new(None) })
    }

    pub fn server_addr(&self) -> SocketAddr {
        self.inner.server
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RendezvousEvent> {
        self.inner.events.subscribe()
    }

    /// Register `device_id` once (signed with the endpoint identity). Also enables automatic
    /// answering of punch requests and relay offers.
    pub async fn register(&self, device_id: DeviceId) -> Result<Registration> {
        self.inner.hosting.store(true, Ordering::Relaxed);
        self.inner.register(device_id).await
    }

    /// Register `device_id` and keep the registration fresh in the background until
    /// [`stop_hosting`](Self::stop_hosting) or drop. Progress is reported via [`subscribe`](Self::subscribe).
    pub fn start_hosting(&self, device_id: DeviceId) {
        self.inner.hosting.store(true, Ordering::Relaxed);
        let weak = Arc::downgrade(&self.inner);
        let task = tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                let Some(inner) = weak.upgrade() else { return };
                let wait = match inner.register(device_id).await {
                    Ok(reg) => {
                        backoff = Duration::from_secs(1);
                        let wait = REFRESH_EVERY.min(reg.ttl / 3);
                        let _ = inner.events.send(RendezvousEvent::Registered(reg));
                        wait
                    }
                    Err(e) => {
                        tracing::warn!("rendezvous registration failed: {e}");
                        let _ = inner.events.send(RendezvousEvent::RegistrationFailed(e.to_string()));
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                        backoff
                    }
                };
                drop(inner);
                tokio::time::sleep(wait).await;
            }
        });
        if let Some(old) = self.hosting_task.lock().replace(task.abort_handle()) {
            old.abort();
        }
    }

    /// Stop refreshing and answering punch/relay requests (the registration then expires).
    pub fn stop_hosting(&self) {
        self.inner.hosting.store(false, Ordering::Relaxed);
        if let Some(h) = self.hosting_task.lock().take() {
            h.abort();
        }
    }

    /// Remove our registration immediately (best effort, signed).
    pub async fn unregister(&self, device_id: DeviceId) -> Result<()> {
        self.stop_hosting();
        let nonce = self.inner.fresh_nonce().await?;
        let ts = self.inner.server_now_ms();
        let pk = self.inner.identity.public_key();
        let sig = self.inner.identity.sign(&register_signing_payload(b"unregister", device_id, &pk, ts, &nonce));
        let msg = RendezvousMessage::Unregister { device_id, timestamp_ms: ts, nonce, signature: sig.to_vec() };
        self.inner.send_server(&msg).await
    }

    /// Look up `target`. The server simultaneously asks the host to punch towards us.
    pub async fn lookup(&self, target: DeviceId) -> Result<PeerInfo> {
        let request_id = rand::random();
        let msg = RendezvousMessage::Lookup {
            request_id,
            target,
            requester_public_key: self.inner.identity.public_key(),
            local_addrs: self.inner.local_candidates(),
        };
        match self.inner.request(msg, request_id).await? {
            RendezvousMessage::LookupResult { device_id, public_key, reflexive, local_addrs, observed, .. } => {
                Ok(PeerInfo { device_id, public_key, reflexive, local_addrs, observed_self: observed })
            }
            other => Err(NetError::Protocol(format!("unexpected rendezvous reply {other:?}"))),
        }
    }

    /// Ask the server for a relay session to `target` (the host is offered the other leg).
    pub async fn request_relay(&self, target: DeviceId) -> Result<RelayAllocation> {
        let request_id = rand::random();
        let msg = RendezvousMessage::RelayRequest {
            request_id,
            target,
            requester_public_key: self.inner.identity.public_key(),
        };
        match self.inner.request(msg, request_id).await? {
            RendezvousMessage::RelayAllocated { port, token, .. } => {
                Ok(RelayAllocation { relay_addr: SocketAddr::new(self.inner.server.ip(), port), token })
            }
            other => Err(NetError::Protocol(format!("unexpected rendezvous reply {other:?}"))),
        }
    }

    /// Latch our socket onto a relay leg; resolves once the peer's leg is latched too.
    pub async fn latch_relay(&self, alloc: &RelayAllocation, timeout: Duration) -> Result<()> {
        self.inner.latch_relay(alloc, timeout).await
    }

    /// Send a burst of hole-punching probes to `targets` from the endpoint socket (returns
    /// immediately; the burst runs in the background for ~1.6 s).
    pub fn punch(&self, targets: Vec<SocketAddr>) {
        self.inner.clone().spawn_punch(targets);
    }

    /// STUN Binding through the endpoint socket (reflexive address of the QUIC socket itself).
    pub async fn stun_binding(&self, stun_server: SocketAddr) -> Result<SocketAddr> {
        let txid = stun::new_txid();
        let (tx, mut rx) = oneshot::channel();
        self.inner.stun_pending.lock().insert(txid, tx);
        let req = stun::binding_request(&txid);
        let result = async {
            for wait in stun::RETRANSMIT_SCHEDULE_MS {
                self.inner.aux.send_to(&req, stun_server).await?;
                match tokio::time::timeout(Duration::from_millis(wait), &mut rx).await {
                    Ok(Ok(m)) => {
                        return match m.msg_type {
                            stun::BINDING_SUCCESS => m.mapped.ok_or_else(|| NetError::Stun("no mapped address".into())),
                            t => Err(NetError::Stun(format!("binding failed (type {t:#06x}, code {:?})", m.error_code))),
                        };
                    }
                    Ok(Err(_)) => break,
                    Err(_) => continue,
                }
            }
            Err(NetError::Timeout("STUN binding"))
        }
        .await;
        self.inner.stun_pending.lock().remove(&txid);
        result
    }

    /// Our LAN candidate addresses (interface IPs × endpoint port).
    pub fn local_candidates(&self) -> Vec<SocketAddr> {
        self.inner.local_candidates()
    }

    pub(crate) fn can_reach(&self, addr: &SocketAddr) -> bool {
        self.inner.aux.can_reach(addr)
    }
}

impl Inner {
    fn server_now_ms(&self) -> u64 {
        (now_ms() + self.clock_offset_ms.load(Ordering::Relaxed)).max(0) as u64
    }

    fn note_server_time(&self, server_time_ms: u64) {
        self.clock_offset_ms.store(server_time_ms as i64 - now_ms(), Ordering::Relaxed);
    }

    async fn send_server(&self, msg: &RendezvousMessage) -> Result<()> {
        let pkt = encode_rendezvous(msg)?;
        self.aux.send_to(&pkt, self.server).await?;
        Ok(())
    }

    /// Send `msg` with retransmissions until a reply with `request_id` arrives.
    async fn request(&self, msg: RendezvousMessage, request_id: u64) -> Result<RendezvousMessage> {
        let pkt = encode_rendezvous(&msg)?;
        let (tx, mut rx) = oneshot::channel();
        self.pending.lock().insert(request_id, tx);
        let result = async {
            for wait in REQUEST_SCHEDULE_MS {
                self.aux.send_to(&pkt, self.server).await?;
                match tokio::time::timeout(Duration::from_millis(wait), &mut rx).await {
                    Ok(Ok(RendezvousMessage::Error { code, .. })) => return Err(NetError::Rendezvous(code)),
                    Ok(Ok(reply)) => return Ok(reply),
                    Ok(Err(_)) => break,
                    Err(_) => continue,
                }
            }
            Err(NetError::Timeout("rendezvous server"))
        }
        .await;
        self.pending.lock().remove(&request_id);
        result
    }

    async fn fresh_nonce(&self) -> Result<[u8; 16]> {
        let request_id = rand::random();
        match self.request(RendezvousMessage::RegisterInit { request_id }, request_id).await? {
            RendezvousMessage::Challenge { nonce, server_time_ms, .. } => {
                self.note_server_time(server_time_ms);
                Ok(nonce)
            }
            other => Err(NetError::Protocol(format!("unexpected rendezvous reply {other:?}"))),
        }
    }

    async fn register(&self, device_id: DeviceId) -> Result<Registration> {
        let cached = self.next_nonce.lock().take().filter(|(_, at)| at.elapsed() < Duration::from_secs(25));
        let mut nonce = match cached {
            Some((n, _)) => n,
            None => self.fresh_nonce().await?,
        };
        for attempt in 0..2 {
            let request_id = rand::random();
            let ts = self.server_now_ms();
            let pk = self.identity.public_key();
            let sig = self.identity.sign(&register_signing_payload(b"register", device_id, &pk, ts, &nonce));
            let msg = RendezvousMessage::Register {
                request_id,
                device_id,
                public_key: pk,
                local_addrs: self.local_candidates(),
                timestamp_ms: ts,
                nonce,
                signature: sig.to_vec(),
            };
            match self.request(msg, request_id).await {
                Ok(RendezvousMessage::Registered { observed, ttl_secs, next_nonce, server_time_ms, .. }) => {
                    self.note_server_time(server_time_ms);
                    *self.next_nonce.lock() = Some((next_nonce, Instant::now()));
                    return Ok(Registration { device_id, observed, ttl: Duration::from_secs(ttl_secs as u64) });
                }
                Ok(other) => return Err(NetError::Protocol(format!("unexpected rendezvous reply {other:?}"))),
                Err(NetError::Rendezvous(
                    aether_protocol::rendezvous::RendezvousErrorCode::BadNonce
                    | aether_protocol::rendezvous::RendezvousErrorCode::ClockSkew,
                )) if attempt == 0 => {
                    // Our cached nonce expired or our address changed: get a fresh challenge.
                    nonce = self.fresh_nonce().await?;
                }
                Err(e) => return Err(e),
            }
        }
        Err(NetError::Rendezvous(aether_protocol::rendezvous::RendezvousErrorCode::BadNonce))
    }

    fn local_candidates(&self) -> Vec<SocketAddr> {
        let Ok(local) = self.aux.local_addr() else { return Vec::new() };
        let local = canonical(local);
        if !local.ip().is_unspecified() {
            return vec![local];
        }
        let v6_socket = local.is_ipv6();
        let mut out: Vec<SocketAddr> = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| !i.is_loopback())
            .map(|i| i.ip())
            .filter(|ip| match ip {
                IpAddr::V4(_) => true,
                // Link-local IPv6 needs a scope id the peer cannot use; skip.
                IpAddr::V6(v6) => v6_socket && (v6.segments()[0] & 0xffc0) != 0xfe80,
            })
            .map(|ip| SocketAddr::new(ip, local.port()))
            .collect();
        out.sort_by_key(|a| (a.is_ipv6(), *a));
        out.dedup();
        out.truncate(MAX_LOCAL_ADDRS);
        out
    }

    fn allow_punch(&self, target: SocketAddr) -> bool {
        let now = Instant::now();
        let mut recent = self.recent_punches.lock();
        while recent.front().is_some_and(|(t, _)| now.duration_since(*t) > PUNCH_DEDUP_WINDOW) {
            recent.pop_front();
        }
        let last_second = recent.iter().filter(|(t, _)| now.duration_since(*t) < Duration::from_secs(1)).count();
        if last_second >= MAX_PUNCH_BURSTS_PER_SEC || recent.iter().any(|(_, a)| *a == target) {
            return false;
        }
        recent.push_back((now, target));
        true
    }

    fn spawn_punch(self: Arc<Self>, targets: Vec<SocketAddr>) {
        let targets: Vec<SocketAddr> = targets.into_iter().filter(|t| self.aux.can_reach(t)).collect();
        if targets.is_empty() {
            return;
        }
        tokio::spawn(async move {
            let Ok(pkt) = encode_rendezvous(&RendezvousMessage::Punch { sender_public_key: self.identity.public_key() })
            else {
                return;
            };
            let start = tokio::time::Instant::now();
            for off in PUNCH_SCHEDULE_MS {
                tokio::time::sleep_until(start + Duration::from_millis(off)).await;
                for t in &targets {
                    let _ = self.aux.send_to(&pkt, *t).await;
                }
            }
        });
    }

    async fn latch_relay(&self, alloc: &RelayAllocation, timeout: Duration) -> Result<()> {
        let (tx, mut rx) = watch::channel(None);
        self.latch_waiters.lock().insert(alloc.token, tx);
        let pkt = encode_rendezvous(&RendezvousMessage::RelayLatch { token: alloc.token })?;
        let deadline = tokio::time::Instant::now() + timeout;
        let result = async {
            loop {
                if *rx.borrow() == Some(true) {
                    return Ok(());
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(NetError::Timeout("relay latch"));
                }
                self.aux.send_to(&pkt, alloc.relay_addr).await?;
                let step = deadline.min(tokio::time::Instant::now() + Duration::from_millis(250));
                let _ = tokio::time::timeout_at(step, rx.changed()).await;
            }
        }
        .await;
        self.latch_waiters.lock().remove(&alloc.token);
        result
    }

    fn handle(self: &Arc<Self>, msg: RendezvousMessage, from: SocketAddr) {
        // Everything except peer punches must come from the rendezvous server host
        // (the main port or one of its relay leg ports).
        if !matches!(msg, RendezvousMessage::Punch { .. }) && from.ip() != self.server.ip() {
            tracing::debug!(%from, "ignoring rendezvous message from unexpected source");
            return;
        }
        if let Some(id) = response_id(&msg) {
            if let Some(tx) = self.pending.lock().remove(&id) {
                let _ = tx.send(msg);
            }
            return;
        }
        match msg {
            RendezvousMessage::Punch { .. } => tracing::trace!(%from, "punch probe received"),
            RendezvousMessage::RelayLatched { token, peer_ready } => {
                if let Some(w) = self.latch_waiters.lock().get(&token) {
                    let _ = w.send(Some(peer_ready));
                }
            }
            RendezvousMessage::PunchRequest { requester_public_key, reflexive, .. } => {
                if !self.hosting.load(Ordering::Relaxed) {
                    return;
                }
                // Only the server-observed reflexive address is punched: `local_addrs` are
                // client-controlled and LAN peers need no NAT opening (the client's own QUIC
                // Initials reach us directly). Rate-limited so we can't be used as a reflector.
                if !self.allow_punch(reflexive) {
                    tracing::debug!(%reflexive, "punch request rate-limited");
                    return;
                }
                tracing::debug!(%reflexive, "punching towards client");
                self.clone().spawn_punch(vec![reflexive]);
                let _ = self.events.send(RendezvousEvent::PunchRequested { requester_public_key, addrs: vec![reflexive] });
            }
            RendezvousMessage::RelayOffer { port, token, requester_public_key } => {
                if !self.hosting.load(Ordering::Relaxed) {
                    return;
                }
                let alloc = RelayAllocation { relay_addr: SocketAddr::new(self.server.ip(), port), token };
                let _ = self
                    .events
                    .send(RendezvousEvent::RelayOffered { relay_addr: alloc.relay_addr, requester_public_key });
                let this = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = this.latch_relay(&alloc, HOST_LATCH_TIMEOUT).await {
                        tracing::warn!(relay = %alloc.relay_addr, "relay latch failed: {e}");
                    }
                });
            }
            other => tracing::debug!("unexpected rendezvous message {other:?}"),
        }
    }
}

async fn dispatch(weak: Weak<Inner>, mut rx: mpsc::Receiver<AuxPacket>) {
    while let Some((data, from)) = rx.recv().await {
        let Some(inner) = weak.upgrade() else { return };
        if stun::is_stun_packet(&data) {
            if let Ok(m) = stun::parse_message(&data)
                && let Some(tx) = inner.stun_pending.lock().remove(&m.txid)
            {
                let _ = tx.send(m);
            }
            continue;
        }
        if let Some(msg) = decode_rendezvous(&data) {
            inner.handle(msg, from);
        }
    }
}
