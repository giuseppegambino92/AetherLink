//! # aether-net
//!
//! AetherLink networking:
//!
//! * [`Identity`] — Ed25519 device identity (OS keychain with a 0600 file fallback) and [`TrustStore`].
//! * [`AetherEndpoint`] — QUIC (quinn + rustls/ring, TLS 1.3, ALPN [`aether_protocol::ALPN`]) where
//!   each side's self-signed certificate carries its Ed25519 identity. Verifiers accept any
//!   certificate but expose the peer key; trust is enforced by the post-TLS handshake on the
//!   Control stream (pinned key, SPAKE2 password bound to the TLS exporter, or interactive consent).
//! * [`PeerConnection`] / [`FramedSend`] / [`FramedRecv`] — typed streams & datagrams.
//! * [`tunnel`] — developer port forwarding over QUIC streams.
//! * [`discovery`] — mDNS / DNS-SD LAN discovery.
//! * [`stun`], [`rendezvous`], [`connect_to_device`] — NAT traversal: direct → hole punch → relay.
//!
//! The endpoint's UDP socket is shared (via a demultiplexer) between QUIC, STUN and the
//! rendezvous protocol, so the NAT mapping observed by the rendezvous server is the one QUIC uses.

mod auth;
mod connect;
mod connection;
mod endpoint;
mod error;
mod framed;
mod identity;
mod socket;
mod tls;

pub mod discovery;
pub mod rendezvous;
pub mod stun;
pub mod tunnel;

pub use auth::{AuthRateLimiter, ClientAuth, HostAuthPolicy};
pub use connect::{ConnectOptions, ConnectPath, connect_to_device, connect_to_device_with};
pub use connection::{IncomingStream, PathStats, PeerConnection};
pub use endpoint::{AetherEndpoint, IncomingConnection, Session};
pub use error::{NetError, Result};
pub use framed::{FramedRecv, FramedSend};
pub use identity::{Identity, TrustStore, TrustedPeer, write_private_file};

/// Re-exported so callers can build `HostAuthPolicy::consent` futures without depending on `futures`.
pub use futures::future::BoxFuture;
