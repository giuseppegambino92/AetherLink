//! Minimal STUN (RFC 5389) Binding client: discovers the server-reflexive address.

use crate::{NetError, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;

pub const MAGIC_COOKIE: u32 = 0x2112_A442;
pub const BINDING_REQUEST: u16 = 0x0001;
pub const BINDING_SUCCESS: u16 = 0x0101;
pub const BINDING_ERROR: u16 = 0x0111;

const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const ATTR_SOFTWARE: u16 = 0x8022;
const ATTR_FINGERPRINT: u16 = 0x8028;
const ATTR_ERROR_CODE: u16 = 0x0009;
const FINGERPRINT_XOR: u32 = 0x5354_554e;

/// Retransmission schedule (RFC 5389 §7.2.1 style, shortened for interactive use).
pub(crate) const RETRANSMIT_SCHEDULE_MS: [u64; 6] = [300, 600, 1200, 1600, 1600, 1600];

/// Quick classifier used by the socket demultiplexer.
pub fn is_stun_packet(p: &[u8]) -> bool {
    p.len() >= 20 && p[0] & 0xC0 == 0 && p[4..8] == MAGIC_COOKIE.to_be_bytes()
}

/// Encode a Binding request (with FINGERPRINT) for transaction `txid`.
pub fn binding_request(txid: &[u8; 12]) -> Vec<u8> {
    let mut m = Vec::with_capacity(28);
    m.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    m.extend_from_slice(&8u16.to_be_bytes());
    m.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    m.extend_from_slice(txid);
    let crc = crc32(&m) ^ FINGERPRINT_XOR;
    m.extend_from_slice(&ATTR_FINGERPRINT.to_be_bytes());
    m.extend_from_slice(&4u16.to_be_bytes());
    m.extend_from_slice(&crc.to_be_bytes());
    m
}

/// A parsed STUN message (only the attributes we need).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StunMessage {
    pub msg_type: u16,
    pub txid: [u8; 12],
    /// XOR-MAPPED-ADDRESS, falling back to MAPPED-ADDRESS.
    pub mapped: Option<SocketAddr>,
    pub software: Option<String>,
    pub error_code: Option<u16>,
    /// Whether a FINGERPRINT attribute was present (it is always verified when present).
    pub has_fingerprint: bool,
}

fn err(s: &str) -> NetError {
    NetError::Stun(s.to_string())
}

/// Parse and validate a STUN message (header, attribute TLVs, FINGERPRINT).
pub fn parse_message(buf: &[u8]) -> Result<StunMessage> {
    if buf.len() < 20 {
        return Err(err("message shorter than header"));
    }
    if buf[0] & 0xC0 != 0 {
        return Err(err("not a STUN message"));
    }
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if u32::from_be_bytes(buf[4..8].try_into().unwrap()) != MAGIC_COOKIE {
        return Err(err("bad magic cookie"));
    }
    if len & 3 != 0 || buf.len() != 20 + len {
        return Err(err("bad message length"));
    }
    let txid: [u8; 12] = buf[8..20].try_into().unwrap();
    let mut msg = StunMessage {
        msg_type,
        txid,
        mapped: None,
        software: None,
        error_code: None,
        has_fingerprint: false,
    };
    let mut xor_mapped = None;
    let mut mapped = None;
    let mut off = 20;
    while off < buf.len() {
        if msg.has_fingerprint {
            return Err(err("FINGERPRINT must be the last attribute"));
        }
        if off + 4 > buf.len() {
            return Err(err("truncated attribute header"));
        }
        let at = u16::from_be_bytes([buf[off], buf[off + 1]]);
        let alen = u16::from_be_bytes([buf[off + 2], buf[off + 3]]) as usize;
        let vstart = off + 4;
        let vend = vstart + alen;
        if vend > buf.len() {
            return Err(err("truncated attribute"));
        }
        let value = &buf[vstart..vend];
        match at {
            ATTR_XOR_MAPPED_ADDRESS => xor_mapped = Some(parse_address(value, Some(&txid))?),
            ATTR_MAPPED_ADDRESS => mapped = Some(parse_address(value, None)?),
            ATTR_SOFTWARE => msg.software = Some(String::from_utf8_lossy(value).into_owned()),
            ATTR_ERROR_CODE if alen >= 4 => {
                msg.error_code = Some((value[2] & 0x07) as u16 * 100 + value[3] as u16);
            }
            ATTR_FINGERPRINT => {
                if alen != 4 {
                    return Err(err("bad FINGERPRINT length"));
                }
                // CRC over the message up to (excluding) this attribute, with the header length
                // field covering the FINGERPRINT attribute (which it already does, since it's last).
                let expected = crc32(&buf[..off]) ^ FINGERPRINT_XOR;
                if u32::from_be_bytes(value.try_into().unwrap()) != expected {
                    return Err(err("FINGERPRINT mismatch"));
                }
                msg.has_fingerprint = true;
            }
            _ => {}
        }
        off = vstart + alen.div_ceil(4) * 4;
    }
    if off != buf.len() {
        return Err(err("attribute padding overruns message"));
    }
    msg.mapped = xor_mapped.or(mapped);
    Ok(msg)
}

fn parse_address(v: &[u8], xor_txid: Option<&[u8; 12]>) -> Result<SocketAddr> {
    if v.len() < 4 {
        return Err(err("short address attribute"));
    }
    let family = v[1];
    let mut port = u16::from_be_bytes([v[2], v[3]]);
    let cookie = MAGIC_COOKIE.to_be_bytes();
    if xor_txid.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    let ip = match family {
        0x01 => {
            if v.len() != 8 {
                return Err(err("bad IPv4 address length"));
            }
            let mut o: [u8; 4] = v[4..8].try_into().unwrap();
            if xor_txid.is_some() {
                for i in 0..4 {
                    o[i] ^= cookie[i];
                }
            }
            IpAddr::V4(Ipv4Addr::from(o))
        }
        0x02 => {
            if v.len() != 20 {
                return Err(err("bad IPv6 address length"));
            }
            let mut o: [u8; 16] = v[4..20].try_into().unwrap();
            if let Some(txid) = xor_txid {
                let mut key = [0u8; 16];
                key[..4].copy_from_slice(&cookie);
                key[4..].copy_from_slice(txid);
                for i in 0..16 {
                    o[i] ^= key[i];
                }
            }
            IpAddr::V6(Ipv6Addr::from(o))
        }
        _ => return Err(err("unknown address family")),
    };
    Ok(SocketAddr::new(ip, port))
}

/// Validate a Binding success response for `txid` and return the reflexive address.
pub fn parse_binding_response(buf: &[u8], txid: &[u8; 12]) -> Result<SocketAddr> {
    let m = parse_message(buf)?;
    if &m.txid != txid {
        return Err(err("transaction id mismatch"));
    }
    match m.msg_type {
        BINDING_SUCCESS => m.mapped.ok_or_else(|| err("response carries no mapped address")),
        BINDING_ERROR => Err(NetError::Stun(format!("server error {}", m.error_code.unwrap_or(0)))),
        t => Err(NetError::Stun(format!("unexpected message type {t:#06x}"))),
    }
}

pub(crate) fn new_txid() -> [u8; 12] {
    let mut t = [0u8; 12];
    rand::Rng::fill(&mut rand::thread_rng(), &mut t);
    t
}

/// Send a Binding request to `server` (with retransmissions) and return our reflexive address.
/// Other datagrams arriving on `sock` meanwhile are discarded.
pub async fn binding(sock: &UdpSocket, server: SocketAddr) -> Result<SocketAddr> {
    let txid = new_txid();
    let req = binding_request(&txid);
    let mut buf = [0u8; 1500];
    for wait in RETRANSMIT_SCHEDULE_MS {
        sock.send_to(&req, server).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait);
        loop {
            match tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
                Err(_) => break,
                Ok(Err(e)) => return Err(e.into()),
                Ok(Ok((n, from))) => {
                    if from.ip().to_canonical() != server.ip().to_canonical() || from.port() != server.port() {
                        continue;
                    }
                    match parse_binding_response(&buf[..n], &txid) {
                        Ok(addr) => return Ok(addr),
                        Err(NetError::Stun(e)) if e.starts_with("server error") => return Err(NetError::Stun(e)),
                        Err(_) => continue,
                    }
                }
            }
        }
    }
    Err(NetError::Timeout("STUN binding"))
}

/// CRC-32 (IEEE 802.3), as required by the FINGERPRINT attribute.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    const TXID: [u8; 12] = [0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae];

    /// RFC 5769 §2.2 — sample IPv4 response.
    const RFC5769_IPV4_RESPONSE: [u8; 80] = [
        0x01, 0x01, 0x00, 0x3c, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87,
        0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63, 0x74, 0x6f, 0x72, 0x20,
        0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43, 0x00, 0x08, 0x00, 0x14, 0x2b, 0x91,
        0xf5, 0x99, 0xfd, 0x9e, 0x90, 0xc3, 0x8c, 0x74, 0x89, 0xf9, 0x2a, 0xf9, 0xba, 0x53, 0xf0, 0x6b, 0xe7, 0xd7,
        0x80, 0x28, 0x00, 0x04, 0xc0, 0x7d, 0x4c, 0x96,
    ];

    /// RFC 5769 §2.3 — sample IPv6 response.
    const RFC5769_IPV6_RESPONSE: [u8; 92] = [
        0x01, 0x01, 0x00, 0x48, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87,
        0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63, 0x74, 0x6f, 0x72, 0x20,
        0x00, 0x20, 0x00, 0x14, 0x00, 0x02, 0xa1, 0x47, 0x01, 0x13, 0xa9, 0xfa, 0xa5, 0xd3, 0xf1, 0x79, 0xbc, 0x25,
        0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9, 0x00, 0x08, 0x00, 0x14, 0xa3, 0x82, 0x95, 0x4e, 0x4b, 0xe6, 0x7b, 0xf1,
        0x17, 0x84, 0xc9, 0x7c, 0x82, 0x92, 0xc2, 0x75, 0xbf, 0xe3, 0xed, 0x41, 0x80, 0x28, 0x00, 0x04, 0xc8, 0xfb,
        0x0b, 0x4c,
    ];

    /// RFC 5769 §2.1 — sample request (USERNAME, PRIORITY, ICE-CONTROLLED, MESSAGE-INTEGRITY, FINGERPRINT).
    const RFC5769_REQUEST: [u8; 108] = [
        0x00, 0x01, 0x00, 0x58, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87,
        0xdf, 0xae, 0x80, 0x22, 0x00, 0x10, 0x53, 0x54, 0x55, 0x4e, 0x20, 0x74, 0x65, 0x73, 0x74, 0x20, 0x63, 0x6c,
        0x69, 0x65, 0x6e, 0x74, 0x00, 0x24, 0x00, 0x04, 0x6e, 0x00, 0x01, 0xff, 0x80, 0x29, 0x00, 0x08, 0x93, 0x2f,
        0xf9, 0xb1, 0x51, 0x26, 0x3b, 0x36, 0x00, 0x06, 0x00, 0x09, 0x65, 0x76, 0x74, 0x6a, 0x3a, 0x68, 0x36, 0x76,
        0x59, 0x20, 0x20, 0x20, 0x00, 0x08, 0x00, 0x14, 0x9a, 0xea, 0xa7, 0x0c, 0xbf, 0xd8, 0xcb, 0x56, 0x78, 0x1e,
        0xf2, 0xb5, 0xb2, 0xd3, 0xf2, 0x49, 0xc1, 0xb5, 0x71, 0xa2, 0x80, 0x28, 0x00, 0x04, 0xe5, 0x7a, 0x3b, 0xcf,
    ];

    #[test]
    fn rfc5769_ipv4_response() {
        let m = parse_message(&RFC5769_IPV4_RESPONSE).unwrap();
        assert_eq!(m.msg_type, BINDING_SUCCESS);
        assert!(m.has_fingerprint);
        assert_eq!(m.software.as_deref(), Some("test vector"));
        assert_eq!(parse_binding_response(&RFC5769_IPV4_RESPONSE, &TXID).unwrap(), "192.0.2.1:32853".parse().unwrap());
    }

    #[test]
    fn rfc5769_ipv6_response() {
        let addr = parse_binding_response(&RFC5769_IPV6_RESPONSE, &TXID).unwrap();
        assert_eq!(addr, "[2001:db8:1234:5678:11:2233:4455:6677]:32853".parse().unwrap());
    }

    #[test]
    fn rfc5769_request_fingerprint() {
        let m = parse_message(&RFC5769_REQUEST).unwrap();
        assert_eq!(m.msg_type, BINDING_REQUEST);
        assert!(m.has_fingerprint);
        assert_eq!(m.software.as_deref(), Some("STUN test client"));
        assert_eq!(m.mapped, None);
    }

    #[test]
    fn rejects_corruption() {
        let mut bad = RFC5769_IPV4_RESPONSE;
        bad[45] ^= 1; // flip a bit in the mapped address → FINGERPRINT fails
        assert!(parse_message(&bad).is_err());
        assert!(parse_binding_response(&RFC5769_IPV4_RESPONSE, &[0; 12]).is_err());
        assert!(parse_message(&RFC5769_IPV4_RESPONSE[..79]).is_err());
        let mut wrong_cookie = RFC5769_IPV4_RESPONSE;
        wrong_cookie[4] = 0;
        assert!(parse_message(&wrong_cookie).is_err());
    }

    #[test]
    fn request_roundtrip() {
        let txid = new_txid();
        let req = binding_request(&txid);
        assert!(is_stun_packet(&req));
        let m = parse_message(&req).unwrap();
        assert_eq!(m.msg_type, BINDING_REQUEST);
        assert_eq!(m.txid, txid);
        assert!(m.has_fingerprint);
    }

    /// Tiny in-process STUN responder to exercise `binding` (localhost only).
    #[tokio::test]
    async fn binding_against_local_responder() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            // Drop the first request to exercise retransmission.
            let _ = server.recv_from(&mut buf).await.unwrap();
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            let req = parse_message(&buf[..n]).unwrap();
            let SocketAddr::V4(v4) = from else { panic!() };
            let mut resp = Vec::new();
            resp.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
            resp.extend_from_slice(&12u16.to_be_bytes());
            resp.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
            resp.extend_from_slice(&req.txid);
            resp.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
            resp.extend_from_slice(&8u16.to_be_bytes());
            resp.extend_from_slice(&[0, 1]);
            resp.extend_from_slice(&(v4.port() ^ 0x2112).to_be_bytes());
            let ip = u32::from(*v4.ip()) ^ MAGIC_COOKIE;
            resp.extend_from_slice(&ip.to_be_bytes());
            server.send_to(&resp, from).await.unwrap();
        });
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mapped = binding(&client, server_addr).await.unwrap();
        assert_eq!(mapped, client.local_addr().unwrap());
    }
}
