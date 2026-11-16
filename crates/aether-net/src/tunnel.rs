//! Developer port forwarding: a local TCP listener on the client, one QUIC bidirectional
//! stream (`StreamKind::PortTunnel`) per accepted TCP connection, and a host side that only
//! ever connects to its own loopback interface.

use crate::{FramedRecv, FramedSend, NetError, PeerConnection, Result};
use aether_protocol::{ListeningPort, PortTunnelOpen, StreamKind, TunnelProtocol, service_hint_for_port};
use parking_lot::Mutex;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;

const COPY_BUF: usize = 64 * 1024;
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// Host-side tunnel authorization.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TunnelPolicy {
    /// Master switch; `false` (the default) refuses every tunnel.
    pub enabled: bool,
    /// Ports that may be tunneled; `None` = any loopback port, `Some(empty)` = none.
    pub allowed_ports: Option<Vec<u16>>,
}

impl TunnelPolicy {
    /// Allow any loopback port.
    pub fn allow_all() -> Self {
        Self { enabled: true, allowed_ports: None }
    }

    /// Allow only `ports` (an empty list allows nothing).
    pub fn only(ports: impl Into<Vec<u16>>) -> Self {
        Self { enabled: true, allowed_ports: Some(ports.into()) }
    }

    pub fn allows(&self, port: u16) -> bool {
        self.enabled && port != 0 && self.allowed_ports.as_ref().is_none_or(|p| p.contains(&port))
    }
}

/// Live byte counters of a tunnel.
#[derive(Debug, Default)]
pub struct TunnelStats {
    /// Bytes sent from local TCP clients to the remote port.
    pub bytes_up: AtomicU64,
    /// Bytes received from the remote port.
    pub bytes_down: AtomicU64,
    /// TCP connections accepted so far.
    pub connections: AtomicU64,
    /// Connections currently open.
    pub active: AtomicU64,
}

/// A running client-side port forward. Dropping it stops the tunnel.
#[derive(Debug)]
pub struct TunnelHandle {
    local_addr: SocketAddr,
    remote_port: u16,
    tunnel_id: u64,
    stats: Arc<TunnelStats>,
    accept_task: AbortHandle,
    conns: Arc<Mutex<Vec<AbortHandle>>>,
}

impl TunnelHandle {
    /// Address of the local listener (connect here to reach the remote port).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn remote_port(&self) -> u16 {
        self.remote_port
    }

    pub fn tunnel_id(&self) -> u64 {
        self.tunnel_id
    }

    pub fn bytes_up(&self) -> u64 {
        self.stats.bytes_up.load(Ordering::Relaxed)
    }

    pub fn bytes_down(&self) -> u64 {
        self.stats.bytes_down.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> &Arc<TunnelStats> {
        &self.stats
    }

    /// Close the listener and all active forwarded connections.
    pub fn stop(&self) {
        self.accept_task.abort();
        for h in self.conns.lock().drain(..) {
            h.abort();
        }
    }
}

impl Drop for TunnelHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Listen on `bind` locally and forward every TCP connection to `remote_port` on the host's
/// loopback interface through `conn`.
pub async fn forward_local(conn: PeerConnection, bind: SocketAddr, remote_port: u16) -> Result<TunnelHandle> {
    let listener = TcpListener::bind(bind).await?;
    let local_addr = listener.local_addr()?;
    let tunnel_id: u64 = rand::random();
    let stats = Arc::new(TunnelStats::default());
    let conns: Arc<Mutex<Vec<AbortHandle>>> = Arc::default();

    let task = {
        let stats = stats.clone();
        let conns = conns.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, peer) = match listener.accept().await {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!("tunnel accept error: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let _ = tcp.set_nodelay(true);
                stats.connections.fetch_add(1, Ordering::Relaxed);
                let conn = conn.clone();
                let stats2 = stats.clone();
                let h = tokio::spawn(async move {
                    stats2.active.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = forward_one(&conn, tcp, tunnel_id, remote_port, &stats2).await {
                        tracing::debug!(%peer, remote_port, "tunnel connection ended: {e}");
                    }
                    stats2.active.fetch_sub(1, Ordering::Relaxed);
                });
                let mut list = conns.lock();
                list.retain(|h| !h.is_finished());
                list.push(h.abort_handle());
            }
        })
    };

    Ok(TunnelHandle { local_addr, remote_port, tunnel_id, stats, accept_task: task.abort_handle(), conns })
}

async fn forward_one(
    conn: &PeerConnection,
    tcp: TcpStream,
    tunnel_id: u64,
    remote_port: u16,
    stats: &TunnelStats,
) -> Result<()> {
    let kind = StreamKind::PortTunnel { tunnel_id, remote_port, protocol: TunnelProtocol::Tcp };
    let (send, mut recv) = conn.open_bi(kind).await?;
    let reply: PortTunnelOpen = tokio::time::timeout(OPEN_TIMEOUT, recv.recv_required())
        .await
        .map_err(|_| NetError::Timeout("tunnel open"))??;
    match reply {
        PortTunnelOpen::Connected => pipe(tcp, send, recv, &stats.bytes_up, &stats.bytes_down).await,
        PortTunnelOpen::Refused { reason } => Err(NetError::TunnelRefused(reason)),
    }
}

/// Host side: serve one `PortTunnel` stream accepted with [`PeerConnection::accept_stream`].
/// Connects only to `127.0.0.1:remote_port` (falling back to `[::1]:remote_port`).
pub async fn serve_tunnel(remote_port: u16, mut send: FramedSend, recv: FramedRecv, policy: &TunnelPolicy) -> Result<()> {
    if !policy.allows(remote_port) {
        let reason = format!("port {remote_port} is not allowed by the host's tunnel policy");
        send.send(&PortTunnelOpen::Refused { reason: reason.clone() }).await?;
        let _ = send.finish();
        return Err(NetError::TunnelRefused(reason));
    }
    let tcp = match connect_loopback(remote_port).await {
        Ok(s) => s,
        Err(e) => {
            let reason = format!("nothing is listening on localhost:{remote_port} ({e})");
            send.send(&PortTunnelOpen::Refused { reason: reason.clone() }).await?;
            let _ = send.finish();
            return Err(NetError::TunnelRefused(reason));
        }
    };
    let _ = tcp.set_nodelay(true);
    send.send(&PortTunnelOpen::Connected).await?;
    let (up, down) = (AtomicU64::new(0), AtomicU64::new(0));
    pipe(tcp, send, recv, &down, &up).await
}

async fn connect_loopback(port: u16) -> std::io::Result<TcpStream> {
    let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
    match tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(v4)).await {
        Ok(Ok(s)) => Ok(s),
        _ => tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(v6))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out"))?,
    }
}

/// Bidirectional copy between a TCP socket and a QUIC stream pair with half-close propagation.
async fn pipe(tcp: TcpStream, mut send: FramedSend, mut recv: FramedRecv, to_quic: &AtomicU64, from_quic: &AtomicU64) -> Result<()> {
    let (mut tr, mut tw) = tcp.into_split();
    let up = async {
        copy_counting(&mut tr, &mut send, to_quic).await?;
        send.finish()?;
        Ok::<_, NetError>(())
    };
    let down = async {
        copy_counting(&mut recv, &mut tw, from_quic).await?;
        tw.shutdown().await?;
        Ok::<_, NetError>(())
    };
    // Half-closes propagate naturally; a hard error in either direction tears down both.
    tokio::try_join!(up, down).map(|_| ())
}

async fn copy_counting<R, W>(r: &mut R, w: &mut W, counter: &AtomicU64) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            w.flush().await?;
            return Ok(());
        }
        w.write_all(&buf[..n]).await?;
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------------------------
// Listening port scan
// ---------------------------------------------------------------------------------------------

/// Enumerate TCP sockets in LISTEN state on this machine (with owning process when visible).
/// Returns an empty list when enumeration is not possible on this platform.
pub fn scan_listening_ports() -> Vec<ListeningPort> {
    let mut ports = match listeners::get_all() {
        Ok(all) => all
            .into_iter()
            .filter(|l| l.protocol == listeners::Protocol::TCP && l.state == listeners::SocketState::Listen)
            .map(|l| ListeningPort {
                port: l.socket.port(),
                protocol: TunnelProtocol::Tcp,
                address: l.socket.ip().to_string(),
                pid: (l.process.pid != 0).then_some(l.process.pid),
                process_name: (!l.process.name.is_empty()).then_some(l.process.name),
                service_hint: service_hint_for_port(l.socket.port()).map(str::to_string),
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            tracing::debug!("listeners enumeration failed ({e}); trying lsof");
            scan_with_lsof()
        }
    };
    ports.sort_by(|a, b| a.port.cmp(&b.port).then_with(|| a.address.cmp(&b.address)));
    ports.dedup_by(|a, b| a.port == b.port && a.address == b.address);
    ports
}

#[cfg(unix)]
fn scan_with_lsof() -> Vec<ListeningPort> {
    match std::process::Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN"]).output() {
        Ok(out) => parse_lsof(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => Vec::new(),
    }
}

#[cfg(not(unix))]
fn scan_with_lsof() -> Vec<ListeningPort> {
    Vec::new()
}

/// Parse `lsof -nP -iTCP -sTCP:LISTEN` output.
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_lsof(text: &str) -> Vec<ListeningPort> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 9 || !line.contains("(LISTEN)") {
            continue;
        }
        // NAME is the token before "(LISTEN)", e.g. `127.0.0.1:5432`, `*:3000`, `[::1]:8080`.
        let Some(pos) = cols.iter().position(|c| *c == "(LISTEN)") else { continue };
        if pos == 0 {
            continue;
        }
        let name = cols[pos - 1];
        let Some((host, port)) = name.rsplit_once(':') else { continue };
        let Ok(port) = port.parse::<u16>() else { continue };
        let address = match host.trim_start_matches('[').trim_end_matches(']') {
            "*" => if cols.contains(&"IPv6") { "::" } else { "0.0.0.0" }.to_string(),
            h => h.to_string(),
        };
        out.push(ListeningPort {
            port,
            protocol: TunnelProtocol::Tcp,
            address,
            pid: cols[1].parse().ok(),
            process_name: Some(cols[0].replace("\\x20", " ")),
            service_hint: service_hint_for_port(port).map(str::to_string),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy() {
        assert!(!TunnelPolicy::default().allows(22));
        assert!(TunnelPolicy::allow_all().allows(5432));
        assert!(!TunnelPolicy::allow_all().allows(0));
        assert!(!TunnelPolicy::only(Vec::<u16>::new()).allows(22), "an empty allowlist allows nothing");
        let p = TunnelPolicy::only([3000, 5173]);
        assert!(p.allows(3000) && !p.allows(22));
    }

    #[test]
    fn lsof_parser() {
        let text = "COMMAND     PID USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME\n\
postgres   812 me    7u  IPv6 0x1234      0t0  TCP [::1]:5432 (LISTEN)\n\
postgres   812 me    8u  IPv4 0x1235      0t0  TCP 127.0.0.1:5432 (LISTEN)\n\
node      4242 me   23u  IPv4 0x1236      0t0  TCP *:5173 (LISTEN)\n\
garbage line\n";
        let ports = parse_lsof(text);
        assert_eq!(ports.len(), 3);
        assert_eq!(ports[0].address, "::1");
        assert_eq!(ports[1].address, "127.0.0.1");
        assert_eq!(ports[2].address, "0.0.0.0");
        assert_eq!(ports[2].service_hint.as_deref(), Some("Vite"));
        assert_eq!(ports[2].pid, Some(4242));
        assert_eq!(ports[0].process_name.as_deref(), Some("postgres"));
    }

    #[tokio::test]
    async fn scan_finds_our_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let ports = tokio::task::spawn_blocking(scan_listening_ports).await.unwrap();
        assert!(
            ports.iter().any(|p| p.port == port),
            "port {port} not found in {} scanned listeners",
            ports.len()
        );
    }
}
