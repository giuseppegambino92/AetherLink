//! Rendezvous / relay wire protocol (UDP), shared by `aether-net` and `aether-relay`.
//!
//! Every rendezvous packet is `RENDEZVOUS_MAGIC ++ bincode(RendezvousMessage)`.
//! The magic's first byte has the two high bits cleared, so these packets can
//! share a UDP socket with QUIC (whose packets always have the "fixed bit"
//! `0x40` set when QUIC-bit greasing is disabled) and STUN (which carries the
//! `0x2112A442` cookie at bytes 4..8). The client demultiplexes on that basis,
//! which lets one UDP socket be used for STUN, registration, hole punching,
//! relay latching and the QUIC connection itself — so the NAT mapping the
//! rendezvous server observes is exactly the one QUIC uses.
//!
//! ## Flows
//!
//! **Registration** (host): `RegisterInit` → `Challenge { nonce, observed }` →
//! `Register { device_id, public_key, timestamp_ms, nonce, signature }` →
//! `Registered { observed, ttl_secs, next_nonce }`. The Ed25519 signature
//! covers [`register_signing_payload`]; the nonce is bound by the server to the
//! observed source address (stateless keyed hash), preventing replay from
//! another address. An ID derived from the public key
//! ([`crate::DeviceId::from_public_key`]) can always be claimed by its owner;
//! other (ephemeral) IDs are first-come-first-served and stay bound to the
//! registering key until they expire, so they cannot be hijacked.
//! Refreshing uses `next_nonce` directly.
//!
//! **Lookup** (client): `Lookup { target }` → `LookupResult { public_key, reflexive, local_addrs }`;
//! at the same moment the server sends the host a `PunchRequest` with the
//! client's reflexive + local addresses so both sides punch simultaneously.
//!
//! **Relay** (fallback): `RelayRequest { target }` → client gets
//! `RelayAllocated { port, token }`, host gets `RelayOffer { port, token }`.
//! Each side sends `RelayLatch { token }` from its QUIC socket to
//! `server_ip:port` until it receives `RelayLatched { peer_ready: true }`.
//! From then on the relay blindly forwards every non-magic datagram between
//! the two latched addresses (it only ever sees encrypted QUIC packets).

use crate::DeviceId;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// 4-byte prefix of every rendezvous packet (`0x2E` has the two high bits clear).
pub const RENDEZVOUS_MAGIC: [u8; 4] = [0x2E, 0xAE, 0x4C, 0x4B];

/// Maximum encoded size of a rendezvous packet (fits in a single unfragmented datagram).
pub const RENDEZVOUS_MAX_PACKET: usize = 1200;

/// Maximum number of local (LAN) candidate addresses carried in a message.
pub const MAX_LOCAL_ADDRS: usize = 8;

/// Registration lifetime granted by the server unless refreshed.
pub const REGISTRATION_TTL_SECS: u32 = 90;

/// Domain separator for registration signatures.
pub const REGISTER_SIGNING_CONTEXT: &[u8] = b"aetherlink-rendezvous-register-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RendezvousMessage {
    // ---- host ⇄ server: registration ----
    /// Host asks for a registration nonce.
    RegisterInit { request_id: u64 },
    /// Server → host: nonce bound to the observed source address.
    Challenge { request_id: u64, nonce: [u8; 16], observed: SocketAddr, server_time_ms: u64 },
    /// Host → server: signed claim of `device_id`.
    Register {
        request_id: u64,
        device_id: DeviceId,
        public_key: [u8; 32],
        local_addrs: Vec<SocketAddr>,
        timestamp_ms: u64,
        nonce: [u8; 16],
        signature: Vec<u8>,
    },
    /// Server → host: registration accepted.
    Registered { request_id: u64, observed: SocketAddr, ttl_secs: u32, next_nonce: [u8; 16], server_time_ms: u64 },
    /// Host → server: remove the registration (signed like `Register`).
    Unregister { device_id: DeviceId, timestamp_ms: u64, nonce: [u8; 16], signature: Vec<u8> },

    // ---- client ⇄ server: lookup ----
    Lookup { request_id: u64, target: DeviceId, requester_public_key: [u8; 32], local_addrs: Vec<SocketAddr> },
    LookupResult {
        request_id: u64,
        device_id: DeviceId,
        public_key: [u8; 32],
        reflexive: SocketAddr,
        local_addrs: Vec<SocketAddr>,
        /// The requester's address as seen by the server.
        observed: SocketAddr,
    },
    /// Server → host: someone looked you up; punch towards these addresses now.
    PunchRequest { requester_public_key: [u8; 32], reflexive: SocketAddr, local_addrs: Vec<SocketAddr> },

    // ---- peer ⇄ peer ----
    /// Hole-punching probe, sent directly between peers (ignored on receipt).
    Punch { sender_public_key: [u8; 32] },

    // ---- relay ----
    RelayRequest { request_id: u64, target: DeviceId, requester_public_key: [u8; 32] },
    /// Server → client: your relay leg is `server_ip:port`.
    RelayAllocated { request_id: u64, port: u16, token: [u8; 16] },
    /// Server → host: a client wants to reach you through the relay leg `server_ip:port`.
    RelayOffer { port: u16, token: [u8; 16], requester_public_key: [u8; 32] },
    /// Peer → relay leg: bind my current source address to this leg.
    RelayLatch { token: [u8; 16] },
    /// Relay leg → peer: latched; `peer_ready` once the other leg is latched too.
    RelayLatched { token: [u8; 16], peer_ready: bool },

    // ---- generic ----
    Error { request_id: u64, code: RendezvousErrorCode },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RendezvousErrorCode {
    NotFound,
    BadSignature,
    BadNonce,
    ClockSkew,
    IdTaken,
    RateLimited,
    RelayUnavailable,
    Malformed,
}

impl std::fmt::Display for RendezvousErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotFound => "device not registered",
            Self::BadSignature => "invalid signature",
            Self::BadNonce => "stale or invalid nonce",
            Self::ClockSkew => "clock skew too large",
            Self::IdTaken => "device id registered to another key",
            Self::RateLimited => "rate limited",
            Self::RelayUnavailable => "relay capacity exhausted",
            Self::Malformed => "malformed request",
        })
    }
}

/// Bytes signed (Ed25519) by a host to claim `device_id` (also used for `Unregister`
/// with `action = b"unregister"`).
pub fn register_signing_payload(
    action: &[u8],
    device_id: DeviceId,
    public_key: &[u8; 32],
    timestamp_ms: u64,
    nonce: &[u8; 16],
) -> Vec<u8> {
    let mut v = Vec::with_capacity(REGISTER_SIGNING_CONTEXT.len() + action.len() + 2 + 4 + 32 + 8 + 16);
    v.extend_from_slice(REGISTER_SIGNING_CONTEXT);
    v.push(0);
    v.extend_from_slice(action);
    v.push(0);
    v.extend_from_slice(&device_id.get().to_le_bytes());
    v.extend_from_slice(public_key);
    v.extend_from_slice(&timestamp_ms.to_le_bytes());
    v.extend_from_slice(nonce);
    v
}

/// True if `packet` starts with [`RENDEZVOUS_MAGIC`].
pub fn is_rendezvous_packet(packet: &[u8]) -> bool {
    packet.len() > RENDEZVOUS_MAGIC.len() && packet[..4] == RENDEZVOUS_MAGIC
}

/// Encode `magic ++ bincode(msg)`.
pub fn encode_rendezvous(msg: &RendezvousMessage) -> Result<Vec<u8>, crate::FramingError> {
    let body = crate::encode_message(msg)?;
    if body.len() + 4 > RENDEZVOUS_MAX_PACKET {
        return Err(crate::FramingError::TooLarge(body.len() + 4));
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&RENDEZVOUS_MAGIC);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode a packet produced by [`encode_rendezvous`]; `None` if it is not a valid rendezvous packet.
pub fn decode_rendezvous(packet: &[u8]) -> Option<RendezvousMessage> {
    if !is_rendezvous_packet(packet) || packet.len() > RENDEZVOUS_MAX_PACKET {
        return None;
    }
    crate::decode_message(&packet[4..]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_demux() {
        let msg = RendezvousMessage::LookupResult {
            request_id: 9,
            device_id: DeviceId::new(123_456_789).unwrap(),
            public_key: [3; 32],
            reflexive: "203.0.113.5:4000".parse().unwrap(),
            local_addrs: vec!["192.168.1.2:21118".parse().unwrap(), "[fe80::1]:21118".parse().unwrap()],
            observed: "198.51.100.1:5000".parse().unwrap(),
        };
        let bytes = encode_rendezvous(&msg).unwrap();
        assert!(is_rendezvous_packet(&bytes));
        // QUIC fixed bit must be clear so the packet never looks like QUIC.
        assert_eq!(bytes[0] & 0x40, 0);
        assert_eq!(decode_rendezvous(&bytes), Some(msg));
        assert_eq!(decode_rendezvous(&[0x40, 1, 2, 3, 4, 5]), None);
        assert_eq!(decode_rendezvous(&RENDEZVOUS_MAGIC), None);
    }

    #[test]
    fn signing_payload_binds_all_fields() {
        let id = DeviceId::new(111_222_333).unwrap();
        let a = register_signing_payload(b"register", id, &[1; 32], 5, &[2; 16]);
        assert_ne!(a, register_signing_payload(b"unregister", id, &[1; 32], 5, &[2; 16]));
        assert_ne!(a, register_signing_payload(b"register", id, &[1; 32], 6, &[2; 16]));
        assert_ne!(a, register_signing_payload(b"register", id, &[1; 32], 5, &[3; 16]));
        assert_ne!(a, register_signing_payload(b"register", DeviceId::new(111_222_334).unwrap(), &[1; 32], 5, &[2; 16]));
    }
}
