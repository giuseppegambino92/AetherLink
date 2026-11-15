//! UDP socket demultiplexer: one socket carries QUIC, STUN and rendezvous packets.
//!
//! quinn owns the socket through [`DemuxSocket`], which diverts STUN responses (cookie
//! `0x2112A442`) and rendezvous packets ([`aether_protocol::rendezvous::RENDEZVOUS_MAGIC`]) to
//! an auxiliary channel before quinn sees them. QUIC-bit greasing is disabled on our endpoints,
//! so every QUIC packet has the `0x40` fixed bit set and can never be confused with either.
//! [`AuxSender`] sends raw datagrams from the same socket (hole punching, relay latching, STUN).

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::sync::mpsc;

/// A datagram diverted from the QUIC socket: (payload, source address).
pub(crate) type AuxPacket = (Vec<u8>, SocketAddr);

const AUX_QUEUE: usize = 512;

/// True for packets that belong to STUN or the rendezvous protocol rather than QUIC.
pub(crate) fn is_aux_packet(p: &[u8]) -> bool {
    aether_protocol::rendezvous::is_rendezvous_packet(p) || crate::stun::is_stun_packet(p)
}

pub(crate) struct DemuxSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    aux_tx: mpsc::Sender<AuxPacket>,
}

impl fmt::Debug for DemuxSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DemuxSocket").field("inner", &self.inner).finish()
    }
}

impl DemuxSocket {
    pub(crate) fn new(inner: Arc<dyn AsyncUdpSocket>) -> (Arc<Self>, mpsc::Receiver<AuxPacket>) {
        let (aux_tx, aux_rx) = mpsc::channel(AUX_QUEUE);
        (Arc::new(Self { inner, aux_tx }), aux_rx)
    }

    pub(crate) fn sender(&self) -> AuxSender {
        AuxSender { inner: self.inner.clone() }
    }

    fn divert(&self, data: &[u8], from: SocketAddr) {
        let from = SocketAddr::new(from.ip().to_canonical(), from.port());
        // Never block quinn's receive path: drop when nobody is draining the queue.
        let _ = self.aux_tx.try_send((data.to_vec(), from));
    }
}

impl AsyncUdpSocket for DemuxSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        loop {
            let n = ready!(self.inner.poll_recv(cx, bufs, meta))?;
            let mut kept = 0;
            for i in 0..n {
                let m = meta[i];
                let stride = if m.stride == 0 { m.len } else { m.stride };
                let data = &bufs[i][..m.len];
                // A GRO-coalesced buffer contains several datagrams of `stride` bytes.
                let mut any_aux = false;
                let mut all_aux = true;
                for seg in data.chunks(stride.max(1)) {
                    if is_aux_packet(seg) {
                        any_aux = true;
                    } else {
                        all_aux = false;
                    }
                }
                if any_aux {
                    for seg in data.chunks(stride.max(1)) {
                        if is_aux_packet(seg) {
                            self.divert(seg, m.addr);
                        }
                    }
                    if !all_aux {
                        // Mixed coalesced buffer (practically never happens): QUIC segments are
                        // dropped and recovered by QUIC loss detection.
                        tracing::trace!("dropping mixed GRO buffer from {}", m.addr);
                    }
                    continue;
                }
                if kept != i {
                    let (head, tail) = bufs.split_at_mut(i);
                    let dst = &mut head[kept];
                    if dst.len() < m.len {
                        continue;
                    }
                    dst[..m.len].copy_from_slice(&tail[0][..m.len]);
                    meta[kept] = m;
                }
                kept += 1;
            }
            if kept > 0 {
                return Poll::Ready(Ok(kept));
            }
            // Everything was diverted: poll again so the waker stays registered.
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// Sends raw (non-QUIC) datagrams from the endpoint's socket.
#[derive(Clone)]
pub(crate) struct AuxSender {
    inner: Arc<dyn AsyncUdpSocket>,
}

impl fmt::Debug for AuxSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuxSender").finish_non_exhaustive()
    }
}

impl AuxSender {
    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Map `dest` to the socket's address family (IPv4-mapped IPv6 on dual-stack sockets).
    fn map_dest(&self, dest: SocketAddr) -> io::Result<SocketAddr> {
        let local = self.inner.local_addr()?;
        Ok(match (local, dest) {
            (SocketAddr::V6(_), SocketAddr::V4(d)) => SocketAddr::new(d.ip().to_ipv6_mapped().into(), d.port()),
            (SocketAddr::V4(_), SocketAddr::V6(d)) => match d.ip().to_ipv4_mapped() {
                Some(v4) => SocketAddr::new(v4.into(), d.port()),
                None => return Err(io::Error::new(io::ErrorKind::Unsupported, "IPv6 destination on IPv4 socket")),
            },
            _ => dest,
        })
    }

    pub(crate) async fn send_to(&self, data: &[u8], dest: SocketAddr) -> io::Result<()> {
        let destination = self.map_dest(dest)?;
        let transmit = Transmit { destination, ecn: None, contents: data, segment_size: None, src_ip: None };
        let mut poller = self.inner.clone().create_io_poller();
        for _ in 0..64 {
            match self.inner.try_send(&transmit) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::future::poll_fn(|cx| poller.as_mut().poll_writable(cx)).await?;
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(io::ErrorKind::WouldBlock, "socket persistently not writable"))
    }

    /// Whether this socket can reach `addr` (address family compatibility).
    pub(crate) fn can_reach(&self, addr: &SocketAddr) -> bool {
        self.map_dest(*addr).is_ok()
    }
}
