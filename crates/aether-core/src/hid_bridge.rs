//! "Control iPhone / iPad" mode: route the local UI's input events to a Bluetooth HID
//! peripheral (macOS IOBluetooth, Linux BlueZ) or an ESP32-S3 HID dongle instead of a
//! network session. iOS accepts these as a standard keyboard + pointing device.
//!
//! The peripheral lives on a dedicated worker thread (IOBluetooth objects are not `Send`);
//! this module exposes a small, serializable API for the app shell.

use crate::{CoreError, Result};
use aether_input::bt_hid::{self, HidPeripheral, HidPeripheralBackend, HidPeripheralConfig, LinkState};
use aether_protocol::InputEvent;
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HidBackendInfo {
    /// Stable id for `start`: `macIoBluetooth`, `linuxBlueZ` or `serial:<port>`.
    pub id: String,
    pub kind: &'static str,
    pub available: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HidStatus {
    pub backend: String,
    /// `stopped | listening | connecting | connected`.
    pub state: &'static str,
    pub peer: Option<String>,
    pub reports_sent: u64,
    pub reports_dropped: u64,
    pub pairing_code: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HidPairedDevice {
    pub address: String,
    pub name: Option<String>,
    pub connected: bool,
}

fn backend_id(b: &HidPeripheralBackend) -> (String, &'static str) {
    match b {
        HidPeripheralBackend::MacIoBluetooth => ("macIoBluetooth".into(), "bluetooth"),
        HidPeripheralBackend::LinuxBlueZ => ("linuxBlueZ".into(), "bluetooth"),
        HidPeripheralBackend::SerialDongle(p) => (format!("serial:{p}"), "dongle"),
    }
}

fn parse_backend(id: &str) -> Result<HidPeripheralBackend> {
    Ok(match id {
        "macIoBluetooth" => HidPeripheralBackend::MacIoBluetooth,
        "linuxBlueZ" => HidPeripheralBackend::LinuxBlueZ,
        s if s.starts_with("serial:") => HidPeripheralBackend::SerialDongle(s["serial:".len()..].to_owned()),
        _ => return Err(CoreError::Invalid("errors.invalid".into())),
    })
}

fn hid_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::Other(format!("HID: {e}"))
}

/// Backends usable on this machine (cheap checks; nothing is published or opened).
pub fn backends() -> Vec<HidBackendInfo> {
    bt_hid::available_backends()
        .into_iter()
        .map(|b| {
            let (id, kind) = backend_id(&b.backend);
            HidBackendInfo { id, kind, available: b.available, detail: b.detail }
        })
        .collect()
}

pub fn paired_devices(backend: &str) -> Result<Vec<HidPairedDevice>> {
    let b = parse_backend(backend)?;
    Ok(bt_hid::paired_devices(&b)
        .map_err(hid_err)?
        .into_iter()
        .map(|d| HidPairedDevice { address: d.address, name: d.name, connected: d.connected })
        .collect())
}

enum Cmd {
    Send(InputEvent),
    Connect(String, mpsc::Sender<Result<()>>),
    Disconnect,
    Status(mpsc::Sender<HidStatus>),
    Stop,
}

struct Worker {
    tx: mpsc::Sender<Cmd>,
    thread: std::thread::JoinHandle<()>,
}

static WORKER: Mutex<Option<Worker>> = Mutex::new(None);

fn to_status(id: &str, p: &HidPeripheral) -> HidStatus {
    let s = p.status();
    let (state, peer) = match &s.state {
        LinkState::Stopped => ("stopped", None),
        LinkState::Listening => ("listening", None),
        LinkState::Connecting(a) => ("connecting", Some(a.clone())),
        LinkState::Connected(a) => ("connected", Some(a.clone())),
    };
    HidStatus {
        backend: id.to_owned(),
        state,
        peer,
        reports_sent: s.reports_sent,
        reports_dropped: s.reports_dropped,
        pairing_code: s.pairing_code,
        last_error: s.last_error,
    }
}

/// Start presenting this machine (or the dongle) as a HID keyboard + pointer.
/// `width`/`height` are the target's screen size in points (used to map absolute pointer
/// positions onto relative HID motion for iOS).
pub fn start(backend: &str, width: u32, height: u32) -> Result<HidStatus> {
    stop();
    let b = parse_backend(backend)?;
    let id = backend.to_owned();
    let (tx, rx) = mpsc::channel::<Cmd>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<HidStatus>>();
    let thread = std::thread::Builder::new().name("aether-hid".into()).spawn(move || {
        let mut p = match HidPeripheral::start(b, HidPeripheralConfig::for_ios(width.max(1), height.max(1))) {
            Ok(p) => p,
            Err(e) => {
                let _ = ready_tx.send(Err(hid_err(e)));
                return;
            }
        };
        let _ = ready_tx.send(Ok(to_status(&id, &p)));
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Send(ev) => {
                    if let Err(e) = p.send(&ev) {
                        tracing::debug!("hid send: {e}");
                    }
                }
                Cmd::Connect(addr, reply) => {
                    let _ = reply.send(p.connect(&addr).map_err(hid_err));
                }
                Cmd::Disconnect => {
                    let _ = p.disconnect();
                }
                Cmd::Status(reply) => {
                    let _ = reply.send(to_status(&id, &p));
                }
                Cmd::Stop => break,
            }
        }
        p.stop();
    })?;
    let status = ready_rx.recv_timeout(Duration::from_secs(15)).map_err(|_| hid_err("peripheral did not start"))??;
    *WORKER.lock() = Some(Worker { tx, thread });
    Ok(status)
}

fn worker_tx() -> Result<mpsc::Sender<Cmd>> {
    WORKER.lock().as_ref().map(|w| w.tx.clone()).ok_or_else(|| CoreError::Invalid("errors.hidNotStarted".into()))
}

/// Forward one UI input event (fire-and-forget; order is preserved).
pub fn send(ev: InputEvent) -> Result<()> {
    worker_tx()?.send(Cmd::Send(ev)).map_err(|_| hid_err("worker stopped"))
}

/// Connect to an already-paired target (e.g. the iPhone paired in Bluetooth settings).
pub fn connect(address: &str) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    worker_tx()?.send(Cmd::Connect(address.to_owned(), tx)).map_err(|_| hid_err("worker stopped"))?;
    rx.recv_timeout(Duration::from_secs(20)).map_err(|_| CoreError::Net("errors.timeout".into()))?
}

pub fn disconnect() -> Result<()> {
    worker_tx()?.send(Cmd::Disconnect).map_err(|_| hid_err("worker stopped"))
}

pub fn status() -> Option<HidStatus> {
    let tx = worker_tx().ok()?;
    let (rtx, rrx) = mpsc::channel();
    tx.send(Cmd::Status(rtx)).ok()?;
    rrx.recv_timeout(Duration::from_secs(2)).ok()
}

/// Stop and remove every SDP record / registration / dongle link (idempotent).
pub fn stop() {
    if let Some(w) = WORKER.lock().take() {
        let _ = w.tx.send(Cmd::Stop);
        let _ = w.thread.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_ids_roundtrip_and_listing_is_side_effect_free() {
        for id in ["macIoBluetooth", "linuxBlueZ", "serial:/dev/cu.usbmodem1101"] {
            let b = parse_backend(id).unwrap();
            assert_eq!(backend_id(&b).0, id);
        }
        assert!(parse_backend("bogus").is_err());
        // Listing must not publish anything or require hardware.
        let _ = backends();
        assert!(status().is_none());
        assert!(send(InputEvent::ReleaseAll).is_err(), "no peripheral running");
    }
}
