//! `connect_to_device`: LAN/direct candidates → simultaneous hole punch → relay fallback.

use crate::rendezvous::RendezvousClient;
use crate::tls::peer_key;
use crate::{AetherEndpoint, ClientAuth, NetError, Result, Session};
use aether_protocol::{DeviceId, Hello};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::net::SocketAddr;
use std::time::Duration;

/// Tuning for [`connect_to_device_with`].
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// Try LAN / reflexive candidates with simultaneous hole punching.
    pub try_direct: bool,
    /// Fall back to the relay when no direct path works.
    pub try_relay: bool,
    /// Budget for the direct phase (QUIC handshakes racing over all candidates).
    pub direct_timeout: Duration,
    /// Budget for relay latching and the QUIC handshake through the relay.
    pub relay_timeout: Duration,
    /// Pinned host key (paired device). Must match the key the rendezvous server reports.
    pub expected_host_key: Option<[u8; 32]>,
    /// Extra candidates, e.g. addresses from mDNS discovery, tried first.
    pub extra_candidates: Vec<SocketAddr>,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            try_direct: true,
            try_relay: true,
            direct_timeout: Duration::from_secs(5),
            relay_timeout: Duration::from_secs(10),
            expected_host_key: None,
            extra_candidates: Vec::new(),
        }
    }
}

/// Which path the session ended up using.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectPath {
    Direct(SocketAddr),
    Relay(SocketAddr),
}

/// Connect to `target` by device ID: direct → hole punch → relay, with default options.
pub async fn connect_to_device(
    endpoint: &AetherEndpoint,
    rendezvous: &RendezvousClient,
    target: DeviceId,
    hello: Hello,
    auth: ClientAuth,
) -> Result<Session> {
    connect_to_device_with(endpoint, rendezvous, target, hello, auth, &ConnectOptions::default())
        .await
        .map(|(s, _)| s)
}

/// Like [`connect_to_device`], with explicit options; also reports the path used.
pub async fn connect_to_device_with(
    endpoint: &AetherEndpoint,
    rendezvous: &RendezvousClient,
    target: DeviceId,
    hello: Hello,
    auth: ClientAuth,
    opts: &ConnectOptions,
) -> Result<(Session, ConnectPath)> {
    let info = match rendezvous.lookup(target).await {
        Ok(info) => Some(info),
        Err(e) if !opts.extra_candidates.is_empty() => {
            tracing::info!("rendezvous lookup of {target} failed ({e}); trying extra candidates only");
            None
        }
        Err(e) => return Err(e),
    };
    let host_key = match (opts.expected_host_key, &info) {
        (Some(pinned), Some(i)) if pinned != i.public_key => {
            return Err(NetError::Auth("rendezvous server reports a different key than the pinned one".into()));
        }
        (Some(pinned), _) => Some(pinned),
        (None, Some(i)) => Some(i.public_key),
        (None, None) => None,
    };

    let mut last_err: Option<NetError> = None;

    if opts.try_direct {
        let mut candidates: Vec<SocketAddr> = opts.extra_candidates.clone();
        if let Some(i) = &info {
            candidates.extend(i.local_addrs.iter().copied());
            candidates.push(i.reflexive);
        }
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| !c.ip().is_unspecified() && c.port() != 0 && rendezvous.can_reach(c) && seen.insert(*c));
        if !candidates.is_empty() {
            // Open our NAT towards every candidate while the host (notified by the lookup)
            // punches towards us.
            rendezvous.punch(candidates.clone());
            match race_direct(endpoint, &candidates, host_key, opts.direct_timeout).await {
                Ok((conn, addr)) => {
                    let session = endpoint.handshake_client(conn, hello, auth, host_key).await?;
                    return Ok((session, ConnectPath::Direct(addr)));
                }
                Err(e) => {
                    tracing::info!("direct connection to {target} failed: {e}");
                    last_err = Some(e);
                }
            }
        }
    }

    if opts.try_relay && info.is_some() {
        let alloc = rendezvous.request_relay(target).await?;
        rendezvous.latch_relay(&alloc, opts.relay_timeout).await?;
        let conn = endpoint.connect_quic(alloc.relay_addr, opts.relay_timeout).await?;
        check_key(&conn, host_key)?;
        let session = endpoint.handshake_client(conn, hello, auth, host_key).await?;
        return Ok((session, ConnectPath::Relay(alloc.relay_addr)));
    }

    Err(last_err.unwrap_or_else(|| NetError::Unreachable(format!("no usable path to {target}"))))
}

fn check_key(conn: &quinn::Connection, expected: Option<[u8; 32]>) -> Result<()> {
    if let Some(expected) = expected
        && peer_key(conn)? != expected
    {
        conn.close(1u32.into(), b"unexpected identity");
        return Err(NetError::Auth("candidate answered with a different identity key".into()));
    }
    Ok(())
}

/// Race QUIC handshakes to all candidates (staggered by 50 ms, in priority order).
async fn race_direct(
    endpoint: &AetherEndpoint,
    candidates: &[SocketAddr],
    host_key: Option<[u8; 32]>,
    timeout: Duration,
) -> Result<(quinn::Connection, SocketAddr)> {
    let mut attempts: FuturesUnordered<_> = candidates
        .iter()
        .enumerate()
        .map(|(i, &addr)| async move {
            tokio::time::sleep(Duration::from_millis(50 * i as u64)).await;
            let conn = endpoint.connect_quic(addr, timeout).await?;
            check_key(&conn, host_key)?;
            Ok::<_, NetError>((conn, addr))
        })
        .collect();
    let race = async {
        let mut last = None;
        while let Some(r) = attempts.next().await {
            match r {
                Ok(win) => return Ok(win),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| NetError::Unreachable("no candidates".into())))
    };
    tokio::time::timeout(timeout, race).await.map_err(|_| NetError::Timeout("direct connection"))?
}
