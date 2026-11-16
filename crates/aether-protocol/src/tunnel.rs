//! Developer port forwarding.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TunnelProtocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListeningPort {
    pub port: u16,
    pub protocol: TunnelProtocol,
    /// Bound address, e.g. `127.0.0.1`, `0.0.0.0`, `::`.
    pub address: String,
    pub pid: Option<u32>,
    pub process_name: Option<String>,
    /// Friendly guess: "Vite", "PostgreSQL", "ADB"…
    pub service_hint: Option<String>,
}

/// Sent by the host as the first frame on a `PortTunnel` stream, after which the
/// stream carries raw bytes in both directions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortTunnelOpen {
    Connected,
    Refused { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortTunnelMessage {
    ScanRequest,
    ScanResult { ports: Vec<ListeningPort> },
    /// Host-side policy: exactly which ports may be tunneled (empty = none).
    Policy { allowed_ports: Vec<u16> },
}

/// Best-effort service names for well-known dev ports.
pub fn service_hint_for_port(port: u16) -> Option<&'static str> {
    Some(match port {
        22 => "SSH",
        80 | 8080 | 8000 => "HTTP",
        443 | 8443 => "HTTPS",
        1433 => "SQL Server",
        3000 => "Node / Next.js",
        3306 => "MySQL",
        4200 => "Angular",
        5000 | 5001 => "Flask / ASP.NET",
        5173 | 5174 => "Vite",
        5432 => "PostgreSQL",
        5555 => "ADB",
        5900 => "VNC",
        6379 => "Redis",
        8888 => "Jupyter",
        9000 => "PHP-FPM / MinIO",
        9200 => "Elasticsearch",
        11434 => "Ollama",
        27017 => "MongoDB",
        _ => return None,
    })
}
