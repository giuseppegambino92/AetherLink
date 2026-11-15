//! # Control an iPhone / iPad (or any HID host) as a Bluetooth/USB HID device
//!
//! iOS cannot be remote-controlled by software, but it natively accepts
//! keyboards and pointing devices (pointer via Settings → Accessibility →
//! Touch → AssistiveTouch). This module makes the AetherLink controller *be*
//! such a device. The same [`crate::hid_report::REPORT_DESCRIPTOR`] and
//! [`crate::hid_report::HidReportBuilder`] report bytes are used by every
//! backend (and by the Android app's `BluetoothHidDevice`, see
//! [`kotlin_report_descriptor`]).
//!
//! | backend | mechanism | notes |
//! |---|---|---|
//! | [`HidPeripheralBackend::MacIoBluetooth`] | Bluetooth Classic HID device emulation via IOBluetooth: transient HID SDP record + HIDP over L2CAP PSM 0x11/0x13 | the Mac initiates the connection to an already-paired iPhone ([`HidPeripheral::connect`]); see [`macos`] |
//! | [`HidPeripheralBackend::LinuxBlueZ`] | BlueZ `ProfileManager1` SDP record + raw L2CAP sockets + pairing agent | needs `CAP_NET_BIND_SERVICE` and bluetoothd's `input` plugin disabled; see [`linux`] |
//! | [`HidPeripheralBackend::SerialDongle`] | ESP32-S3 dongle (BLE HID via NimBLE and/or USB HID) driven over USB-serial with the [`frame`] protocol | works with every OS; see `firmware/esp32-s3-hid-dongle` |
//!
//! ```no_run
//! use aether_input::bt_hid::{available_backends, HidPeripheral, HidPeripheralConfig};
//! use aether_protocol::InputEvent;
//!
//! let backend = available_backends().into_iter().find(|b| b.available).unwrap().backend;
//! // iOS ignores absolute pointers: drive it with relative motion in points.
//! let mut p = HidPeripheral::start(backend, HidPeripheralConfig::for_ios(390, 844)).unwrap();
//! p.connect("aa:bb:cc:dd:ee:ff").ok(); // Classic backends: reconnect to a paired iPhone
//! p.send(&InputEvent::Text { text: "hello".into() }).unwrap();
//! ```

pub mod frame;
pub mod hidp;
pub mod sdp;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub mod serial;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

use crate::hid_report::{HidConfig, HidReport, HidReportBuilder, KeyboardLeds, PointerMode, REPORT_DESCRIPTOR};
use crate::{InputError, Result};
use aether_protocol::InputEvent;
use std::time::Duration;

pub use frame::{DongleInfo, DongleStatus, Target as DongleTarget};
pub use hidp::ProtocolMode;

/// Which mechanism presents AetherLink as a HID device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HidPeripheralBackend {
    /// macOS IOBluetooth Classic HID device emulation.
    MacIoBluetooth,
    /// Linux BlueZ Classic HID device emulation.
    LinuxBlueZ,
    /// ESP32-S3 dongle on the given serial port (`/dev/cu.usbmodem…`,
    /// `/dev/ttyACM0`, `COM5`).
    SerialDongle(String),
}

/// One entry of [`available_backends`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendInfo {
    pub backend: HidPeripheralBackend,
    /// Whether [`HidPeripheral::start`] is expected to succeed.
    pub available: bool,
    /// Human-readable reason / description (missing capability, dongle product name, …).
    pub detail: String,
}

/// Backends usable on this machine, cheapest checks only (no Bluetooth
/// state is changed, nothing is published, no serial port is opened).
pub fn available_backends() -> Vec<BackendInfo> {
    #[allow(unused_mut)]
    let mut v = Vec::new();
    #[cfg(target_os = "macos")]
    v.push(macos::backend_info());
    #[cfg(target_os = "linux")]
    v.push(linux::backend_info());
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    for d in serial::detect_dongles() {
        v.push(BackendInfo {
            available: true,
            detail: format!(
                "USB {:04X}:{:04X} {}",
                d.vid,
                d.pid,
                d.product.clone().unwrap_or_else(|| "serial dongle".into())
            ),
            backend: HidPeripheralBackend::SerialDongle(d.port),
        });
    }
    v
}

#[derive(Debug, Clone, PartialEq)]
pub struct HidPeripheralConfig {
    /// Name shown to the host (SDP ServiceName; BlueZ adapter alias is not changed).
    pub device_name: String,
    /// Report conversion (pointer mode etc.).
    pub hid: HidConfig,
    /// Dongle only: which link(s) to drive.
    pub dongle_target: DongleTarget,
    /// Classic backends: accept host-initiated connections (in addition to
    /// [`HidPeripheral::connect`]).
    pub listen: bool,
    /// Classic backends: switch the local adapter's Class of Device to
    /// "Peripheral / keyboard + pointing" while running (macOS: temporary,
    /// reverts automatically; Linux: via the mgmt socket, needs `CAP_NET_ADMIN`).
    pub set_class_of_device: bool,
}

impl Default for HidPeripheralConfig {
    fn default() -> Self {
        Self {
            device_name: "AetherLink Keyboard".into(),
            hid: HidConfig::default(),
            dongle_target: DongleTarget::Ble,
            listen: true,
            set_class_of_device: false,
        }
    }
}

impl HidPeripheralConfig {
    /// iOS / iPadOS: no absolute pointer support, so absolute positions are
    /// converted to relative motion in a `width × height` point space (set
    /// AssistiveTouch tracking speed to the middle and the converter homes
    /// the cursor to the top-left corner first).
    pub fn for_ios(width_points: u32, height_points: u32) -> Self {
        Self {
            hid: HidConfig {
                pointer: PointerMode::Relative { width: width_points, height: height_points },
                ..HidConfig::default()
            },
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LinkState {
    #[default]
    Stopped,
    /// Ready; waiting for a host to connect (or for [`HidPeripheral::connect`]).
    Listening,
    /// Outgoing connection in progress to the given address.
    Connecting(String),
    /// A host is connected (address, or `"BLE"` / `"USB"` for the dongle).
    Connected(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PeripheralStatus {
    pub state: LinkState,
    pub protocol: ProtocolMode,
    pub leds: KeyboardLeds,
    pub reports_sent: u64,
    pub reports_dropped: u64,
    /// Incremented on every new host connection (the relative-pointer
    /// estimate is re-homed when it changes).
    pub connection_epoch: u64,
    pub last_error: Option<String>,
    /// Passkey / PIN to show the user while pairing (Linux agent).
    pub pairing_code: Option<String>,
    pub dongle: Option<DongleInfo>,
    pub dongle_status: Option<DongleStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    /// `aa:bb:cc:dd:ee:ff`.
    pub address: String,
    pub name: Option<String>,
    pub connected: bool,
}

/// What every backend implements.
pub(crate) trait Transport: Send {
    fn send_report(&mut self, r: &HidReport) -> Result<()>;
    fn status(&self) -> PeripheralStatus;
    fn paired_devices(&self) -> Result<Vec<PairedDevice>>;
    fn connect(&mut self, address: &str) -> Result<()>;
    fn disconnect(&mut self) -> Result<()>;
    fn set_pairing(&mut self, enabled: bool, timeout: Duration) -> Result<()>;
    fn stop(&mut self);
}

/// A running HID peripheral. Dropping it stops the backend and removes every
/// SDP record / L2CAP registration it created.
pub struct HidPeripheral {
    backend: HidPeripheralBackend,
    transport: Box<dyn Transport>,
    builder: HidReportBuilder,
    epoch: u64,
}

impl std::fmt::Debug for HidPeripheral {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HidPeripheral").field("backend", &self.backend).finish_non_exhaustive()
    }
}

impl HidPeripheral {
    pub fn start(backend: HidPeripheralBackend, cfg: HidPeripheralConfig) -> Result<Self> {
        let transport = open_transport(&backend, &cfg)?;
        Ok(Self { backend, transport, builder: HidReportBuilder::new(cfg.hid), epoch: 0 })
    }

    pub fn backend(&self) -> &HidPeripheralBackend {
        &self.backend
    }

    /// Convert an event with the shared [`HidReportBuilder`] and send every
    /// resulting report. Returns an error if no host is connected (the
    /// builder state is still updated, so key/button tracking stays right).
    pub fn send(&mut self, ev: &InputEvent) -> Result<()> {
        let epoch = self.transport.status().connection_epoch;
        if epoch != self.epoch {
            self.epoch = epoch;
            self.builder.rehome();
        }
        let reports = self.builder.convert(ev)?;
        for r in &reports {
            self.transport.send_report(r)?;
        }
        Ok(())
    }

    /// Send a raw report (ID + payload as in [`crate::hid_report`]).
    pub fn send_report(&mut self, r: &HidReport) -> Result<()> {
        self.transport.send_report(r)
    }

    pub fn status(&self) -> PeripheralStatus {
        self.transport.status()
    }

    /// Bonded devices the backend could connect to.
    pub fn paired_devices(&self) -> Result<Vec<PairedDevice>> {
        self.transport.paired_devices()
    }

    /// Connect to an already-paired host (Classic backends: open HID control
    /// + interrupt L2CAP channels to it, as a reconnecting keyboard does).
    pub fn connect(&mut self, address: &str) -> Result<()> {
        parse_bdaddr(address)?;
        self.transport.connect(address)
    }

    pub fn disconnect(&mut self) -> Result<()> {
        self.transport.disconnect()
    }

    /// Allow (or stop allowing) a new host to pair for `timeout`.
    /// Linux: discoverable + pairable with the AetherLink agent; dongle: BLE
    /// advertising; macOS: unsupported (pair from System Settings / the
    /// iPhone, then use [`connect`](Self::connect)).
    pub fn set_pairing(&mut self, enabled: bool, timeout: Duration) -> Result<()> {
        self.transport.set_pairing(enabled, timeout)
    }

    /// Release all keys/buttons on the host, then stop.
    pub fn stop(mut self) {
        let _ = self.send(&InputEvent::ReleaseAll);
        self.transport.stop();
    }
}

impl Drop for HidPeripheral {
    fn drop(&mut self) {
        self.transport.stop();
    }
}

fn open_transport(backend: &HidPeripheralBackend, cfg: &HidPeripheralConfig) -> Result<Box<dyn Transport>> {
    match backend {
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        HidPeripheralBackend::SerialDongle(port) => Ok(Box::new(serial::SerialDongle::open(port, cfg.dongle_target)?)),
        #[cfg(target_os = "macos")]
        HidPeripheralBackend::MacIoBluetooth => Ok(Box::new(macos::MacHidDevice::start(cfg)?)),
        #[cfg(target_os = "linux")]
        HidPeripheralBackend::LinuxBlueZ => Ok(Box::new(linux::BluezHidDevice::start(cfg)?)),
        #[allow(unreachable_patterns)]
        _ => {
            let _ = cfg;
            Err(InputError::Unsupported("this HID peripheral backend is not available on this OS"))
        }
    }
}

/// Bonded devices without starting a peripheral (read-only).
pub fn paired_devices(backend: &HidPeripheralBackend) -> Result<Vec<PairedDevice>> {
    match backend {
        #[cfg(target_os = "macos")]
        HidPeripheralBackend::MacIoBluetooth => macos::paired_devices(),
        #[cfg(target_os = "linux")]
        HidPeripheralBackend::LinuxBlueZ => linux::paired_devices(),
        HidPeripheralBackend::SerialDongle(_) => {
            Err(InputError::Unsupported("the dongle only reports a bond count (PeripheralStatus::dongle_status)"))
        }
        #[allow(unreachable_patterns)]
        _ => Err(InputError::Unsupported("this HID peripheral backend is not available on this OS")),
    }
}

/// Parse `aa:bb:cc:dd:ee:ff` / `aa-bb-cc-dd-ee-ff` into bytes in display
/// order (most significant first).
pub fn parse_bdaddr(s: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    let bad = || InputError::InvalidArgument(format!("invalid Bluetooth address {s:?}"));
    if parts.len() != 6 {
        return Err(bad());
    }
    let mut a = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        if p.len() != 2 {
            return Err(bad());
        }
        a[i] = u8::from_str_radix(p, 16).map_err(|_| bad())?;
    }
    Ok(a)
}

pub fn format_bdaddr(a: &[u8; 6]) -> String {
    a.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// The report descriptor as a Kotlin `byteArrayOf(...)` literal, for the
/// Android app's `BluetoothHidDeviceAppSdpSettings` (subclass
/// [`sdp::SUBCLASS_COMBO_KEYBOARD_POINTING`]). Report payloads for
/// `BluetoothHidDevice.sendReport(device, id, data)` are
/// [`HidReport::data`] with [`HidReport::id`].
pub fn kotlin_report_descriptor() -> String {
    let items: Vec<String> = REPORT_DESCRIPTOR.iter().map(|b| format!("0x{b:02X}.toByte()")).collect();
    format!("byteArrayOf({})", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_protocol::{HidUsage, Modifiers};
    use std::sync::{Arc, Mutex};

    #[test]
    fn bdaddr_round_trip() {
        let a = parse_bdaddr("AA:bb:0c:DD:ee:01").unwrap();
        assert_eq!(a, [0xAA, 0xBB, 0x0C, 0xDD, 0xEE, 0x01]);
        assert_eq!(format_bdaddr(&a), "aa:bb:0c:dd:ee:01");
        assert_eq!(parse_bdaddr("aa-bb-0c-dd-ee-01").unwrap(), a);
        for bad in ["", "aa:bb", "aa:bb:cc:dd:ee:fg", "aaa:bb:cc:dd:ee:f", "aa:bb:cc:dd:ee:ff:00"] {
            assert!(parse_bdaddr(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn kotlin_literal() {
        let k = kotlin_report_descriptor();
        assert!(k.starts_with("byteArrayOf(0x05.toByte(), 0x01.toByte(), 0x09.toByte(), 0x06.toByte()"));
        assert_eq!(k.matches(".toByte()").count(), REPORT_DESCRIPTOR.len());
    }

    /// In-memory transport: checks the event → report → transport plumbing.
    struct Recorder {
        sent: Arc<Mutex<Vec<HidReport>>>,
        epoch: Arc<Mutex<u64>>,
    }

    impl Transport for Recorder {
        fn send_report(&mut self, r: &HidReport) -> Result<()> {
            self.sent.lock().unwrap().push(r.clone());
            Ok(())
        }
        fn status(&self) -> PeripheralStatus {
            PeripheralStatus { connection_epoch: *self.epoch.lock().unwrap(), ..Default::default() }
        }
        fn paired_devices(&self) -> Result<Vec<PairedDevice>> {
            Ok(vec![])
        }
        fn connect(&mut self, _: &str) -> Result<()> {
            Ok(())
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
        fn set_pairing(&mut self, _: bool, _: Duration) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) {}
    }

    #[test]
    fn events_flow_through_builder_and_rehome_on_reconnect() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let epoch = Arc::new(Mutex::new(1));
        let cfg = HidPeripheralConfig::for_ios(100, 200);
        let mut p = HidPeripheral {
            backend: HidPeripheralBackend::SerialDongle("test".into()),
            transport: Box::new(Recorder { sent: sent.clone(), epoch: epoch.clone() }),
            builder: HidReportBuilder::new(cfg.hid),
            epoch: 0,
        };
        p.send(&InputEvent::Key { code: HidUsage::A, pressed: true, modifiers: Modifiers::NONE }).unwrap();
        assert_eq!(sent.lock().unwrap().last().unwrap().to_bytes_with_id(), vec![1, 0, 0, 4, 0, 0, 0, 0, 0]);
        // Relative pointer: first move homes (large negative motion).
        p.send(&InputEvent::MouseMove { display_id: 0, x: 0.5, y: 0.5 }).unwrap();
        let n = sent.lock().unwrap().len();
        p.send(&InputEvent::MouseMove { display_id: 0, x: 0.5, y: 0.5 }).unwrap();
        assert_eq!(sent.lock().unwrap().len(), n, "no motion, no report");
        // New connection → home again.
        *epoch.lock().unwrap() = 2;
        p.send(&InputEvent::MouseMove { display_id: 0, x: 0.5, y: 0.5 }).unwrap();
        let s = sent.lock().unwrap();
        let dx = i16::from_le_bytes([s[n].data[1], s[n].data[2]]);
        assert!(dx < -100, "re-homed: {dx}");
        drop(s);
        assert!(p.connect("bogus").is_err());
        p.stop();
    }

    /// The firmware sketch must use byte-identical descriptor and protocol constants.
    #[test]
    fn firmware_matches_rust_constants() {
        use frame::{AckStatus, MsgType, MAX_PAYLOAD, PROTOCOL_VERSION};
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../firmware/esp32-s3-hid-dongle/esp32-s3-hid-dongle.ino");
        let src = std::fs::read_to_string(path).expect("firmware sketch present");
        let body = &src[src.find("REPORT_DESCRIPTOR[] = {").expect("descriptor array")..];
        let (open, close) = (body.find('{').unwrap(), body.find("};").unwrap());
        let bytes: Vec<u8> = body[open + 1..close]
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| u8::from_str_radix(s.trim_start_matches("0x"), 16).expect("hex byte"))
            .collect();
        assert_eq!(bytes, REPORT_DESCRIPTOR, "firmware REPORT_DESCRIPTOR differs from hid_report::REPORT_DESCRIPTOR");
        assert!(src.contains(&format!("#define PROTO_VERSION {PROTOCOL_VERSION}")));
        assert!(src.contains(&format!("#define MAX_PAYLOAD {MAX_PAYLOAD}")));
        for (name, t) in [
            ("T_HELLO", MsgType::Hello),
            ("T_INFO", MsgType::Info),
            ("T_KEYBOARD", MsgType::Keyboard),
            ("T_MOUSE", MsgType::Mouse),
            ("T_ABS", MsgType::AbsPointer),
            ("T_CONSUMER", MsgType::Consumer),
            ("T_SET_TARGET", MsgType::SetTarget),
            ("T_PAIRING", MsgType::Pairing),
            ("T_GET_STATUS", MsgType::GetStatus),
            ("T_STATUS", MsgType::Status),
            ("T_LEDS", MsgType::Leds),
            ("T_ACK", MsgType::Ack),
        ] {
            assert!(src.contains(&format!("{name} = 0x{:02X}", t as u8)), "{name}");
        }
        for (name, s) in [
            ("ACK_OK", AckStatus::Ok),
            ("ACK_UNKNOWN_TYPE", AckStatus::UnknownType),
            ("ACK_BAD_LENGTH", AckStatus::BadLength),
            ("ACK_NOT_CONNECTED", AckStatus::NotConnected),
            ("ACK_BAD_ARGUMENT", AckStatus::BadArgument),
            ("ACK_VERSION_MISMATCH", AckStatus::VersionMismatch),
        ] {
            assert!(src.contains(&format!("{name} = {}", s as u8)), "{name}");
        }
        assert!(src.contains("TARGET_USB = 1, TARGET_BLE = 2, TARGET_BOTH = 3"));
    }

    #[test]
    fn config_for_ios_is_relative() {
        let c = HidPeripheralConfig::for_ios(390, 844);
        assert_eq!(c.hid.pointer, PointerMode::Relative { width: 390, height: 844 });
        assert!(c.listen);
    }

    #[test]
    fn backends_listing_does_not_panic() {
        let b = available_backends();
        if cfg!(target_os = "macos") {
            assert!(b.iter().any(|b| b.backend == HidPeripheralBackend::MacIoBluetooth));
        }
        if cfg!(target_os = "linux") {
            assert!(b.iter().any(|b| b.backend == HidPeripheralBackend::LinuxBlueZ));
        }
    }
}
