//! An authenticated QUIC connection to a peer.

use crate::{FramedRecv, FramedSend, NetError, Result};
use aether_protocol::{Datagram, StreamHeader, StreamKind, decode_message, encode_message};
use bytes::Bytes;
use std::net::SocketAddr;
use std::time::Duration;

/// Time allowed for a peer to send the [`StreamHeader`] of a stream it opened.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Snapshot of QUIC path statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PathStats {
    pub rtt: Duration,
    pub cwnd_bytes: u64,
    pub sent_packets: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
    pub udp_tx_bytes: u64,
    pub udp_rx_bytes: u64,
    pub mtu: u16,
}

/// A stream opened by the peer, with its [`StreamKind`] already read from the header.
#[derive(Debug)]
pub enum IncomingStream {
    Bi { kind: StreamKind, send: FramedSend, recv: FramedRecv },
    Uni { kind: StreamKind, recv: FramedRecv },
}

impl IncomingStream {
    pub fn kind(&self) -> &StreamKind {
        match self {
            IncomingStream::Bi { kind, .. } | IncomingStream::Uni { kind, .. } => kind,
        }
    }
}

/// Cheaply cloneable handle to an authenticated QUIC connection.
#[derive(Clone, Debug)]
pub struct PeerConnection {
    conn: quinn::Connection,
    peer_key: [u8; 32],
}

impl PeerConnection {
    pub(crate) fn new(conn: quinn::Connection, peer_key: [u8; 32]) -> Self {
        Self { conn, peer_key }
    }

    /// Open a bidirectional stream and announce its kind.
    pub async fn open_bi(&self, kind: StreamKind) -> Result<(FramedSend, FramedRecv)> {
        let (send, recv) = self.conn.open_bi().await?;
        let mut send = FramedSend::new(send);
        send.send(&StreamHeader { kind }).await?;
        Ok((send, FramedRecv::new(recv)))
    }

    /// Open a unidirectional stream and announce its kind.
    pub async fn open_uni(&self, kind: StreamKind) -> Result<FramedSend> {
        let send = self.conn.open_uni().await?;
        let mut send = FramedSend::new(send);
        send.send(&StreamHeader { kind }).await?;
        Ok(send)
    }

    /// Accept the next stream opened by the peer and read its header.
    pub async fn accept_stream(&self) -> Result<IncomingStream> {
        tokio::select! {
            r = self.conn.accept_bi() => {
                let (send, recv) = r?;
                let mut recv = FramedRecv::new(recv);
                let kind = read_header(&mut recv).await?;
                Ok(IncomingStream::Bi { kind, send: FramedSend::new(send), recv })
            }
            r = self.conn.accept_uni() => {
                let mut recv = FramedRecv::new(r?);
                let kind = read_header(&mut recv).await?;
                Ok(IncomingStream::Uni { kind, recv })
            }
        }
    }

    /// Send an unreliable datagram. Fails with [`NetError::Datagram`] if it exceeds
    /// [`PeerConnection::max_datagram_size`].
    pub fn send_datagram(&self, d: &Datagram) -> Result<()> {
        let bytes = encode_message(d)?;
        self.conn.send_datagram(Bytes::from(bytes))?;
        Ok(())
    }

    /// Receive the next datagram (undecodable datagrams are an error, not skipped).
    pub async fn recv_datagram(&self) -> Result<Datagram> {
        let bytes = self.conn.read_datagram().await?;
        Ok(decode_message(&bytes)?)
    }

    /// Largest datagram payload currently sendable (`None` if the peer disabled datagrams).
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }

    pub fn rtt(&self) -> Duration {
        self.conn.rtt()
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// The peer's Ed25519 identity key (from its TLS certificate).
    pub fn peer_public_key(&self) -> [u8; 32] {
        self.peer_key
    }

    /// Path statistics for telemetry (HUD / adaptive bitrate).
    pub fn stats(&self) -> PathStats {
        let s = self.conn.stats();
        PathStats {
            rtt: s.path.rtt,
            cwnd_bytes: s.path.cwnd,
            sent_packets: s.path.sent_packets,
            lost_packets: s.path.lost_packets,
            congestion_events: s.path.congestion_events,
            udp_tx_bytes: s.udp_tx.bytes,
            udp_rx_bytes: s.udp_rx.bytes,
            mtu: s.path.current_mtu,
        }
    }

    pub fn close(&self, reason: &str) {
        self.conn.close(0u32.into(), reason.as_bytes());
    }

    /// Resolves when the connection is closed (by either side or by idle timeout); returns why.
    pub async fn closed(&self) -> NetError {
        NetError::Connection(self.conn.closed().await)
    }

    /// `Some(reason)` if the connection is already closed.
    pub fn close_reason(&self) -> Option<NetError> {
        self.conn.close_reason().map(NetError::Connection)
    }

    /// 32 bytes of TLS keying material bound to this connection (RFC 5705/8446 exporter),
    /// e.g. to derive per-session keys for application-level encryption.
    pub fn export_keying_material(&self, label: &[u8], context: &[u8]) -> Result<[u8; 32]> {
        let mut out = [0u8; 32];
        self.conn
            .export_keying_material(&mut out, label, context)
            .map_err(|_| NetError::Protocol("TLS exporter unavailable".into()))?;
        Ok(out)
    }
}

async fn read_header(recv: &mut FramedRecv) -> Result<StreamKind> {
    let header: StreamHeader = tokio::time::timeout(HEADER_TIMEOUT, recv.recv_required())
        .await
        .map_err(|_| NetError::Timeout("stream header"))??;
    Ok(header.kind)
}
