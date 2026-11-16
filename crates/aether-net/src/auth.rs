//! Post-TLS application handshake on the Control stream.
//!
//! 1. The client opens the Control stream and sends `StreamHeader{Control}` + its [`Hello`];
//!    the host answers with its own `Hello`. Each `Hello.public_key` must equal the key in the
//!    peer's TLS certificate (whose possession TLS 1.3 CertificateVerify already proved).
//! 2. The host picks an [`AuthMethod`] and sends `AuthMessage::Required`:
//!    * `TrustedKey` — the client's pinned key is in the host's trust store;
//!    * `Password`   — SPAKE2 (Ed25519 group, identities = client/host public keys), followed by
//!      key confirmation: `MAC = BLAKE3-keyed(K_c, role || transcript)` with
//!      `K_c = derive_key(spake_key || TLS exporter)`, so a relay/MITM terminating TLS on either
//!      side cannot make both confirmations verify. Client confirms first; the host checks it in
//!      constant time, records failures per source address, then confirms back;
//!    * `InteractiveConsent` — the host awaits [`HostAuthPolicy::consent`].
//! 3. The host sends `AuthMessage::Result`.

use crate::connection::PeerConnection;
use crate::endpoint::Session;
use crate::tls::{EXPORTER_LABEL, peer_key};
use crate::{FramedRecv, FramedSend, NetError, Result};
use aether_protocol::{
    AuthMessage, AuthMethod, AuthRejectReason, Hello, PROTOCOL_VERSION, StreamHeader, StreamKind, encode_message,
};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password as SpakePassword, Spake2};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// Timeout for each non-interactive handshake step.
const STEP_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum time the host waits for the local user to accept/deny.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(120);

/// How the client authenticates.
#[derive(Clone)]
pub enum ClientAuth {
    /// Rely on the host having pinned our key (paired device / trusted fleet) or on host consent.
    Trusted,
    /// One-time password / pairing PIN (SPAKE2).
    Password(String),
}

impl fmt::Debug for ClientAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientAuth::Trusted => f.write_str("Trusted"),
            ClientAuth::Password(_) => f.write_str("Password(<redacted>)"),
        }
    }
}

/// Host-side authorization decisions.
pub trait HostAuthPolicy: Send + Sync + 'static {
    /// True if `peer.public_key` is pinned (the key is verified against TLS before this call).
    fn is_trusted(&self, peer: &Hello) -> bool;
    /// Current rotating one-time password / pairing PIN; `None` disables password auth.
    fn password(&self) -> Option<String>;
    /// Interactive consent; may await a UI prompt.
    fn consent(&self, peer: &Hello) -> BoxFuture<'static, bool>;
    /// Extra local check before a *trusted* peer is admitted (e.g. Touch ID / Windows Hello of
    /// the person at the host). Must fail closed: `false` on error, timeout or unavailability.
    /// The default admits trusted peers without a local check.
    fn verify_trusted(&self, _peer: &Hello) -> BoxFuture<'static, bool> {
        Box::pin(async { true })
    }
}

// ---------------------------------------------------------------------------------------------
// Rate limiting of failed password attempts
// ---------------------------------------------------------------------------------------------

/// Limits failed password attempts per source (IPv4 address / IPv6 /64) and globally.
#[derive(Debug)]
pub struct AuthRateLimiter {
    state: Mutex<LimiterState>,
    max_failures: u32,
    window: Duration,
    base_lockout: Duration,
    global_per_minute: usize,
}

#[derive(Debug, Default)]
struct LimiterState {
    per_source: HashMap<IpAddr, SourceEntry>,
    global: VecDeque<Instant>,
}

#[derive(Debug)]
struct SourceEntry {
    failures: u32,
    window_start: Instant,
    locked_until: Option<Instant>,
    lockouts: u32,
}

impl Default for AuthRateLimiter {
    fn default() -> Self {
        Self::new(5, Duration::from_secs(600), Duration::from_secs(60), 30)
    }
}

impl AuthRateLimiter {
    /// `max_failures` per source within `window` → lockout of `base_lockout` (doubling per
    /// repeated lockout, capped at 1 h); at most `global_per_minute` failures across all sources.
    pub fn new(max_failures: u32, window: Duration, base_lockout: Duration, global_per_minute: usize) -> Self {
        Self { state: Mutex::default(), max_failures, window, base_lockout, global_per_minute }
    }

    fn key(ip: IpAddr) -> IpAddr {
        match ip.to_canonical() {
            IpAddr::V6(v6) => {
                let mut o = v6.octets();
                o[8..].fill(0);
                IpAddr::V6(o.into())
            }
            v4 => v4,
        }
    }

    /// `Err(remaining)` while `ip` (or everyone) is locked out.
    pub fn check(&self, ip: IpAddr) -> std::result::Result<(), Duration> {
        let now = Instant::now();
        let mut st = self.state.lock();
        while st.global.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
            st.global.pop_front();
        }
        if st.global.len() >= self.global_per_minute {
            let oldest = *st.global.front().unwrap();
            return Err(Duration::from_secs(60).saturating_sub(now.duration_since(oldest)));
        }
        if let Some(e) = st.per_source.get(&Self::key(ip))
            && let Some(until) = e.locked_until
            && until > now
        {
            return Err(until - now);
        }
        Ok(())
    }

    pub fn record_failure(&self, ip: IpAddr) {
        let now = Instant::now();
        let mut st = self.state.lock();
        st.global.push_back(now);
        if st.per_source.len() > 8192 {
            let window = self.window;
            st.per_source.retain(|_, e| {
                e.locked_until.is_some_and(|u| u > now) || now.duration_since(e.window_start) < window
            });
        }
        let e = st.per_source.entry(Self::key(ip)).or_insert(SourceEntry {
            failures: 0,
            window_start: now,
            locked_until: None,
            lockouts: 0,
        });
        if now.duration_since(e.window_start) > self.window {
            e.failures = 0;
            e.window_start = now;
        }
        e.failures += 1;
        if e.failures >= self.max_failures {
            let factor = 1u32 << e.lockouts.min(6);
            let lock = (self.base_lockout * factor).min(Duration::from_secs(3600));
            e.locked_until = Some(now + lock);
            e.lockouts += 1;
            e.failures = 0;
            e.window_start = now;
            tracing::warn!(%ip, ?lock, "too many failed password attempts; locking out source");
        }
    }

    pub fn record_success(&self, ip: IpAddr) {
        self.state.lock().per_source.remove(&Self::key(ip));
    }
}

// ---------------------------------------------------------------------------------------------
// Key confirmation
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Client,
    Host,
}

/// Hash of everything both sides must agree on.
pub(crate) fn transcript_hash(client_hello: &[u8], host_hello: &[u8], spake_a: &[u8], spake_b: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("aetherlink auth transcript v1");
    for part in [client_hello, host_hello, spake_a, spake_b] {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    *h.finalize().as_bytes()
}

/// Confirmation key bound to both the SPAKE2 output and the TLS session.
pub(crate) fn confirm_key(spake_key: &[u8], tls_exporter: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let mut h = blake3::Hasher::new_derive_key("aetherlink spake2 confirm v1");
    h.update(&(spake_key.len() as u64).to_le_bytes());
    h.update(spake_key);
    h.update(tls_exporter);
    Zeroizing::new(*h.finalize().as_bytes())
}

pub(crate) fn confirm_mac(key: &[u8; 32], role: Role, transcript: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_keyed(key);
    h.update(match role {
        Role::Client => b"client-confirm",
        Role::Host => b"host-confirm..",
    });
    h.update(transcript);
    *h.finalize().as_bytes()
}

pub(crate) fn macs_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.ct_eq(b).into()
}

fn tls_exporter(conn: &quinn::Connection) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, EXPORTER_LABEL, b"")
        .map_err(|_| NetError::Protocol("TLS exporter unavailable".into()))?;
    Ok(out)
}

async fn step<T>(fut: impl std::future::Future<Output = Result<T>>, what: &'static str) -> Result<T> {
    tokio::time::timeout(STEP_TIMEOUT, fut).await.map_err(|_| NetError::Timeout(what))?
}

// ---------------------------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------------------------

pub(crate) async fn client_handshake(
    conn: quinn::Connection,
    hello: Hello,
    auth: ClientAuth,
    expected_host_key: Option<[u8; 32]>,
) -> Result<Session> {
    let result = client_handshake_inner(&conn, hello, auth, expected_host_key).await;
    if let Err(e) = &result {
        conn.close(1u32.into(), format!("handshake failed: {e}").as_bytes());
    }
    result
}

async fn client_handshake_inner(
    conn: &quinn::Connection,
    hello: Hello,
    auth: ClientAuth,
    expected_host_key: Option<[u8; 32]>,
) -> Result<Session> {
    let host_key = peer_key(conn)?;
    if let Some(expected) = expected_host_key
        && !macs_equal(&expected, &host_key)
    {
        return Err(NetError::Auth("host identity key does not match the expected key".into()));
    }

    let (send, recv) = step(async { Ok(conn.open_bi().await?) }, "open control stream").await?;
    let mut tx = FramedSend::new(send);
    let mut rx = FramedRecv::new(recv);
    tx.send(&StreamHeader { kind: StreamKind::Control }).await?;
    tx.send(&hello).await?;

    let remote: Hello = step(rx.recv_required(), "host hello").await?;
    if remote.public_key != host_key {
        return Err(NetError::Protocol("host Hello key differs from its TLS certificate key".into()));
    }
    let client_hello_bytes = encode_message(&hello)?;
    let host_hello_bytes = encode_message(&remote)?;

    let mut host_confirmed = false;
    let mut password_used = false;
    let mut consent_wait = false;
    loop {
        let timeout = if consent_wait { CONSENT_TIMEOUT + STEP_TIMEOUT } else { STEP_TIMEOUT };
        let msg: AuthMessage = tokio::time::timeout(timeout, rx.recv_required())
            .await
            .map_err(|_| NetError::Timeout("authentication"))??;
        match msg {
            // The host may verify its local owner (Touch ID / Windows Hello) before admitting us.
            AuthMessage::Required { method: AuthMethod::TrustedKey } => consent_wait = true,
            AuthMessage::Required { method: AuthMethod::InteractiveConsent } => consent_wait = true,
            AuthMessage::Required { method: AuthMethod::Password } => {
                let ClientAuth::Password(pw) = &auth else {
                    return Err(NetError::Auth("the host requires a password".into()));
                };
                if password_used {
                    return Err(NetError::Protocol("duplicate password request".into()));
                }
                password_used = true;
                let pw = Zeroizing::new(pw.clone().into_bytes());
                let (state, msg_a) = Spake2::<Ed25519Group>::start_a(
                    &SpakePassword::new(&pw),
                    &SpakeIdentity::new(&hello.public_key),
                    &SpakeIdentity::new(&host_key),
                );
                tx.send(&AuthMessage::Spake2 { message: msg_a.clone() }).await?;
                let msg_b = match step(rx.recv_required(), "SPAKE2 reply").await? {
                    AuthMessage::Spake2 { message } => message,
                    AuthMessage::Result { accepted: false, reason, .. } => {
                        return Err(NetError::AuthRejected(reason.unwrap_or(AuthRejectReason::WrongPassword)));
                    }
                    other => return Err(NetError::Protocol(format!("expected SPAKE2 message, got {other:?}"))),
                };
                let key = Zeroizing::new(
                    state.finish(&msg_b).map_err(|e| NetError::Protocol(format!("SPAKE2: {e:?}")))?,
                );
                let transcript = transcript_hash(&client_hello_bytes, &host_hello_bytes, &msg_a, &msg_b);
                let ck = confirm_key(&key, &tls_exporter(conn)?);
                tx.send(&AuthMessage::Confirm { mac: confirm_mac(&ck, Role::Client, &transcript) }).await?;
                match step(rx.recv_required(), "host key confirmation").await? {
                    AuthMessage::Confirm { mac } => {
                        if !macs_equal(&mac, &confirm_mac(&ck, Role::Host, &transcript)) {
                            return Err(NetError::Auth(
                                "host failed key confirmation (possible man-in-the-middle)".into(),
                            ));
                        }
                        host_confirmed = true;
                    }
                    AuthMessage::Result { accepted: false, reason, .. } => {
                        return Err(NetError::AuthRejected(reason.unwrap_or(AuthRejectReason::WrongPassword)));
                    }
                    other => return Err(NetError::Protocol(format!("expected confirmation, got {other:?}"))),
                }
            }
            AuthMessage::Result { accepted: true, .. } => {
                if password_used && !host_confirmed {
                    return Err(NetError::Auth("host accepted without key confirmation".into()));
                }
                break;
            }
            AuthMessage::Result { accepted: false, reason, .. } => {
                return Err(NetError::AuthRejected(reason.unwrap_or(AuthRejectReason::DeniedByUser)));
            }
            other => return Err(NetError::Protocol(format!("unexpected auth message {other:?}"))),
        }
    }

    Ok(Session {
        conn: PeerConnection::new(conn.clone(), host_key),
        control_tx: tx,
        control_rx: rx,
        remote_hello: remote,
    })
}

// ---------------------------------------------------------------------------------------------
// Host side
// ---------------------------------------------------------------------------------------------

pub(crate) async fn host_handshake(
    conn: quinn::Connection,
    hello: Hello,
    policy: Arc<dyn HostAuthPolicy>,
    limiter: Arc<AuthRateLimiter>,
) -> Result<Session> {
    let result = host_handshake_inner(&conn, hello, policy, limiter).await;
    if let Err(e) = &result {
        conn.close(1u32.into(), format!("handshake failed: {e}").as_bytes());
    }
    result
}

async fn reject(conn: &quinn::Connection, tx: &mut FramedSend, reason: AuthRejectReason) -> NetError {
    let _ = tx.send(&AuthMessage::Result { accepted: false, reason: Some(reason), persist_trust: false }).await;
    let _ = tx.finish();
    // Give the client a moment to read the verdict and close first.
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    NetError::AuthRejected(reason)
}

async fn host_handshake_inner(
    conn: &quinn::Connection,
    hello: Hello,
    policy: Arc<dyn HostAuthPolicy>,
    limiter: Arc<AuthRateLimiter>,
) -> Result<Session> {
    let client_key = peer_key(conn)?;
    let source = conn.remote_address().ip();

    let (send, recv) = step(async { Ok(conn.accept_bi().await?) }, "control stream").await?;
    let mut tx = FramedSend::new(send);
    let mut rx = FramedRecv::new(recv);
    let header: StreamHeader = step(rx.recv_required(), "control header").await?;
    if header.kind != StreamKind::Control {
        return Err(NetError::Protocol("first stream is not the Control stream".into()));
    }
    let remote: Hello = step(rx.recv_required(), "client hello").await?;
    tx.send(&hello).await?;
    if remote.public_key != client_key {
        return Err(NetError::Protocol("client Hello key differs from its TLS certificate key".into()));
    }
    if remote.protocol_version != PROTOCOL_VERSION {
        return Err(reject(conn, &mut tx, AuthRejectReason::VersionMismatch).await);
    }
    let client_hello_bytes = encode_message(&remote)?;
    let host_hello_bytes = encode_message(&hello)?;

    let method = if policy.is_trusted(&remote) {
        AuthMethod::TrustedKey
    } else if policy.password().is_some() {
        AuthMethod::Password
    } else {
        AuthMethod::InteractiveConsent
    };
    if method == AuthMethod::Password && limiter.check(source).is_err() {
        return Err(reject(conn, &mut tx, AuthRejectReason::TooManyAttempts).await);
    }
    tx.send(&AuthMessage::Required { method }).await?;

    match method {
        AuthMethod::TrustedKey => {
            let verified = tokio::time::timeout(CONSENT_TIMEOUT, policy.verify_trusted(&remote)).await.unwrap_or(false);
            if !verified {
                return Err(reject(conn, &mut tx, AuthRejectReason::OwnerVerificationFailed).await);
            }
        }
        AuthMethod::InteractiveConsent => {
            let accepted = tokio::time::timeout(CONSENT_TIMEOUT, policy.consent(&remote)).await.unwrap_or(false);
            if !accepted {
                return Err(reject(conn, &mut tx, AuthRejectReason::DeniedByUser).await);
            }
        }
        AuthMethod::Password => {
            let msg_a = match step(rx.recv_required(), "SPAKE2 message").await? {
                AuthMessage::Spake2 { message } => message,
                other => return Err(NetError::Protocol(format!("expected SPAKE2 message, got {other:?}"))),
            };
            // Re-read the password now: it may have rotated while we waited.
            let Some(pw) = policy.password() else {
                return Err(reject(conn, &mut tx, AuthRejectReason::WrongPassword).await);
            };
            let pw = Zeroizing::new(pw.into_bytes());
            let (state, msg_b) = Spake2::<Ed25519Group>::start_b(
                &SpakePassword::new(&pw),
                &SpakeIdentity::new(&client_key),
                &SpakeIdentity::new(&hello.public_key),
            );
            tx.send(&AuthMessage::Spake2 { message: msg_b.clone() }).await?;
            let key = match state.finish(&msg_a) {
                Ok(k) => Zeroizing::new(k),
                Err(e) => {
                    limiter.record_failure(source);
                    tracing::debug!("SPAKE2 failure from {source}: {e:?}");
                    return Err(reject(conn, &mut tx, AuthRejectReason::WrongPassword).await);
                }
            };
            let transcript = transcript_hash(&client_hello_bytes, &host_hello_bytes, &msg_a, &msg_b);
            let ck = confirm_key(&key, &tls_exporter(conn)?);
            let client_mac = match step(rx.recv_required(), "client key confirmation").await? {
                AuthMessage::Confirm { mac } => mac,
                other => return Err(NetError::Protocol(format!("expected confirmation, got {other:?}"))),
            };
            if !macs_equal(&client_mac, &confirm_mac(&ck, Role::Client, &transcript)) {
                limiter.record_failure(source);
                return Err(reject(conn, &mut tx, AuthRejectReason::WrongPassword).await);
            }
            limiter.record_success(source);
            tx.send(&AuthMessage::Confirm { mac: confirm_mac(&ck, Role::Host, &transcript) }).await?;
        }
    }
    tx.send(&AuthMessage::Result { accepted: true, reason: None, persist_trust: false }).await?;

    Ok(Session {
        conn: PeerConnection::new(conn.clone(), client_key),
        control_tx: tx,
        control_rx: rx,
        remote_hello: remote,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_macs_depend_on_transcript_exporter_and_role() {
        let t1 = transcript_hash(b"hello-c", b"hello-h", b"spake-a", b"spake-b");
        let t2 = transcript_hash(b"hello-c", b"hello-h", b"spake-a", b"spake-B");
        // Length-prefixing prevents boundary shifting.
        let t3 = transcript_hash(b"hello-", b"chello-h", b"spake-a", b"spake-b");
        assert_ne!(t1, t2);
        assert_ne!(t1, t3);

        let k = confirm_key(&[7; 32], &[1; 32]);
        let k_other_tls = confirm_key(&[7; 32], &[2; 32]);
        let k_other_pw = confirm_key(&[8; 32], &[1; 32]);
        let mac = confirm_mac(&k, Role::Client, &t1);
        assert!(macs_equal(&mac, &confirm_mac(&k, Role::Client, &t1)));
        assert!(!macs_equal(&mac, &confirm_mac(&k, Role::Client, &t2)), "different transcripts");
        assert!(!macs_equal(&mac, &confirm_mac(&k_other_tls, Role::Client, &t1)), "different TLS sessions (MITM)");
        assert!(!macs_equal(&mac, &confirm_mac(&k_other_pw, Role::Client, &t1)), "different SPAKE2 keys");
        assert!(!macs_equal(&mac, &confirm_mac(&k, Role::Host, &t1)), "reflection");
    }

    #[test]
    fn spake2_keys_agree_only_with_same_password() {
        let (ka, kb) = ([1u8; 32], [2u8; 32]);
        let run = |pa: &[u8], pb: &[u8]| {
            let (a, ma) = Spake2::<Ed25519Group>::start_a(
                &SpakePassword::new(pa),
                &SpakeIdentity::new(&ka),
                &SpakeIdentity::new(&kb),
            );
            let (b, mb) = Spake2::<Ed25519Group>::start_b(
                &SpakePassword::new(pb),
                &SpakeIdentity::new(&ka),
                &SpakeIdentity::new(&kb),
            );
            (a.finish(&mb).unwrap(), b.finish(&ma).unwrap())
        };
        let (a, b) = run(b"123456", b"123456");
        assert_eq!(a, b);
        let (a, b) = run(b"123456", b"654321");
        assert_ne!(a, b);
    }

    #[test]
    fn rate_limiter_locks_out_and_resets() {
        let rl = AuthRateLimiter::new(3, Duration::from_secs(60), Duration::from_secs(30), 100);
        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        let other: IpAddr = "192.0.2.8".parse().unwrap();
        for _ in 0..2 {
            rl.record_failure(ip);
            assert!(rl.check(ip).is_ok());
        }
        rl.record_failure(ip);
        assert!(rl.check(ip).is_err());
        assert!(rl.check(other).is_ok());
        // IPv6 sources are grouped per /64.
        let a6: IpAddr = "2001:db8::1".parse().unwrap();
        let b6: IpAddr = "2001:db8::ffff:1".parse().unwrap();
        for _ in 0..3 {
            rl.record_failure(a6);
        }
        assert!(rl.check(b6).is_err());
        rl.record_success(b6);
        assert!(rl.check(a6).is_ok());
    }

    #[test]
    fn global_limit() {
        let rl = AuthRateLimiter::new(100, Duration::from_secs(60), Duration::from_secs(30), 4);
        for i in 0..4u8 {
            rl.record_failure(IpAddr::from([10, 0, 0, i]));
        }
        assert!(rl.check(IpAddr::from([10, 0, 0, 99])).is_err());
    }
}
