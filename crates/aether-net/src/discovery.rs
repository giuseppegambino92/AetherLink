//! LAN discovery via mDNS / DNS-SD (`_aetherlink._udp.local.`).
//!
//! Instance name = the 9 device-ID digits. TXT records: `id` (digits), `name`, `os`,
//! `pk` (hex Ed25519 public key), `ver` (app version). The advertised port is the QUIC port.
//! TXT data is unauthenticated: `pk` is only a hint used to pin the key when connecting.

use crate::{NetError, Result};
use aether_protocol::{DeviceId, MDNS_SERVICE_TYPE, OsKind};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::net::SocketAddr;

/// What this device announces on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanAdvertisement {
    pub device_id: DeviceId,
    pub device_name: String,
    pub os: OsKind,
    pub public_key: [u8; 32],
    /// UDP port of the QUIC endpoint.
    pub port: u16,
    pub version: String,
}

/// A device found on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanPeer {
    pub device_id: DeviceId,
    pub device_name: String,
    pub os: OsKind,
    pub public_key: [u8; 32],
    /// Candidate QUIC addresses (all resolved IPs × advertised port).
    pub addrs: Vec<SocketAddr>,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanEvent {
    Found(LanPeer),
    Lost(DeviceId),
}

/// Max bytes of the device name carried in TXT (each TXT string is ≤ 255 bytes).
const MAX_NAME_BYTES: usize = 200;

pub fn os_to_str(os: OsKind) -> &'static str {
    match os {
        OsKind::MacOs => "macos",
        OsKind::Windows => "windows",
        OsKind::Linux => "linux",
        OsKind::Android => "android",
        OsKind::Ios => "ios",
        OsKind::Unknown => "unknown",
    }
}

pub fn os_from_str(s: &str) -> OsKind {
    match s {
        "macos" => OsKind::MacOs,
        "windows" => OsKind::Windows,
        "linux" => OsKind::Linux,
        "android" => OsKind::Android,
        "ios" => OsKind::Ios,
        _ => OsKind::Unknown,
    }
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// TXT properties for an advertisement.
pub fn encode_txt(adv: &LanAdvertisement) -> Vec<(String, String)> {
    vec![
        ("id".into(), adv.device_id.get().to_string()),
        ("name".into(), truncate_utf8(&adv.device_name, MAX_NAME_BYTES).to_string()),
        ("os".into(), os_to_str(adv.os).into()),
        ("pk".into(), hex::encode(adv.public_key)),
        ("ver".into(), truncate_utf8(&adv.version, 32).to_string()),
    ]
}

/// Parsed TXT fields: (device_id, name, os, public_key, version).
pub fn decode_txt(props: &HashMap<String, String>) -> Option<(DeviceId, String, OsKind, [u8; 32], String)> {
    let id: DeviceId = props.get("id")?.parse().ok()?;
    let mut pk = [0u8; 32];
    hex::decode_to_slice(props.get("pk")?, &mut pk).ok()?;
    let name = props.get("name").cloned().unwrap_or_else(|| id.to_string());
    let os = props.get("os").map(|s| os_from_str(s)).unwrap_or(OsKind::Unknown);
    let ver = props.get("ver").cloned().unwrap_or_default();
    Some((id, name, os, pk, ver))
}

fn instance_name(id: DeviceId) -> String {
    id.get().to_string()
}

fn map_err(e: mdns_sd::Error) -> NetError {
    NetError::Discovery(e.to_string())
}

/// Announces this device until dropped.
pub struct LanAdvertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl std::fmt::Debug for LanAdvertiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanAdvertiser").field("fullname", &self.fullname).finish()
    }
}

impl LanAdvertiser {
    pub fn start(adv: &LanAdvertisement) -> Result<LanAdvertiser> {
        let daemon = ServiceDaemon::new().map_err(map_err)?;
        let instance = instance_name(adv.device_id);
        let host = format!("aetherlink-{instance}.local.");
        let props: Vec<(String, String)> = encode_txt(adv);
        let info = ServiceInfo::new(MDNS_SERVICE_TYPE, &instance, &host, "", adv.port, &props[..])
            .map_err(map_err)?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info).map_err(map_err)?;
        Ok(LanAdvertiser { daemon, fullname })
    }

    /// Withdraw the announcement (also done on drop).
    pub fn stop(&self) {
        if let Ok(rx) = self.daemon.unregister(&self.fullname) {
            let _ = rx.recv_timeout(std::time::Duration::from_millis(500));
        }
        let _ = self.daemon.shutdown();
    }
}

impl Drop for LanAdvertiser {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Browses for AetherLink devices on the LAN.
pub struct LanBrowser {
    daemon: ServiceDaemon,
    rx: mdns_sd::Receiver<ServiceEvent>,
    exclude: Option<DeviceId>,
}

impl std::fmt::Debug for LanBrowser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanBrowser").finish_non_exhaustive()
    }
}

impl LanBrowser {
    pub fn start() -> Result<LanBrowser> {
        Self::start_excluding(None)
    }

    /// Like [`start`](Self::start) but never reports `own_id` (our own advertisement).
    pub fn start_excluding(own_id: Option<DeviceId>) -> Result<LanBrowser> {
        let daemon = ServiceDaemon::new().map_err(map_err)?;
        let rx = daemon.browse(MDNS_SERVICE_TYPE).map_err(map_err)?;
        Ok(LanBrowser { daemon, rx, exclude: own_id })
    }

    /// Next Found/Lost event; `None` once the browser stopped.
    pub async fn next_event(&mut self) -> Option<LanEvent> {
        loop {
            let ev = self.rx.recv_async().await.ok()?;
            let out = match ev {
                ServiceEvent::ServiceResolved(info) => {
                    let props = info.get_properties().clone().into_property_map_str();
                    let Some((device_id, device_name, os, public_key, version)) = decode_txt(&props) else {
                        tracing::debug!("ignoring mDNS record with invalid TXT: {}", info.get_fullname());
                        continue;
                    };
                    let port = info.get_port();
                    let mut addrs: Vec<SocketAddr> = info
                        .get_addresses()
                        .iter()
                        .map(|ip| SocketAddr::new(ip.to_ip_addr(), port))
                        .collect();
                    // Prefer IPv4 (more often routable on mixed LANs), then IPv6.
                    addrs.sort_by_key(|a| (a.is_ipv6(), *a));
                    LanEvent::Found(LanPeer { device_id, device_name, os, public_key, addrs, version })
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    let Some(id) = fullname.split('.').next().and_then(|s| s.parse::<DeviceId>().ok()) else {
                        continue;
                    };
                    LanEvent::Lost(id)
                }
                _ => continue,
            };
            let id = match &out {
                LanEvent::Found(p) => p.device_id,
                LanEvent::Lost(id) => *id,
            };
            if Some(id) == self.exclude {
                continue;
            }
            return Some(out);
        }
    }

    pub fn stop(&self) {
        let _ = self.daemon.stop_browse(MDNS_SERVICE_TYPE);
        let _ = self.daemon.shutdown();
    }
}

impl Drop for LanBrowser {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adv() -> LanAdvertisement {
        LanAdvertisement {
            device_id: DeviceId::new(987_654_321).unwrap(),
            device_name: "Giu's MacBook Pro — studio".into(),
            os: OsKind::MacOs,
            public_key: [0xA5; 32],
            port: 21118,
            version: "0.1.0".into(),
        }
    }

    #[test]
    fn txt_roundtrip() {
        let a = adv();
        let props: HashMap<String, String> = encode_txt(&a).into_iter().collect();
        for (k, v) in &props {
            assert!(k.len() + 1 + v.len() <= 255, "TXT entry {k} too long");
        }
        let (id, name, os, pk, ver) = decode_txt(&props).unwrap();
        assert_eq!((id, name.as_str(), os, pk, ver.as_str()), (a.device_id, a.device_name.as_str(), a.os, a.public_key, "0.1.0"));
    }

    #[test]
    fn txt_rejects_bad_records_and_truncates_names() {
        let mut props: HashMap<String, String> = encode_txt(&adv()).into_iter().collect();
        props.insert("pk".into(), "zz".into());
        assert!(decode_txt(&props).is_none());
        props.remove("pk");
        assert!(decode_txt(&props).is_none());

        let mut long = adv();
        long.device_name = "é".repeat(300);
        let props: HashMap<String, String> = encode_txt(&long).into_iter().collect();
        assert!(props["name"].len() <= MAX_NAME_BYTES);
        assert!(decode_txt(&props).is_some());
    }

    #[test]
    fn os_strings_roundtrip() {
        for os in [OsKind::MacOs, OsKind::Windows, OsKind::Linux, OsKind::Android, OsKind::Ios, OsKind::Unknown] {
            assert_eq!(os_from_str(os_to_str(os)), os);
        }
    }

    #[tokio::test]
    #[ignore = "uses real multicast on the local network"]
    async fn advertise_and_browse() {
        let a = adv();
        let _adv = LanAdvertiser::start(&a).unwrap();
        let mut browser = LanBrowser::start().unwrap();
        let found = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(LanEvent::Found(p)) = browser.next_event().await
                    && p.device_id == a.device_id
                {
                    return p;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(found.public_key, a.public_key);
        assert!(found.addrs.iter().all(|s| s.port() == 21118));
    }
}
