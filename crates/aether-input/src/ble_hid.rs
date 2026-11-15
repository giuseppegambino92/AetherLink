//! BLE HID-over-GATT (HOGP) peripheral: lets a phone/laptop running
//! AetherLink act as a Bluetooth keyboard + mouse for a device that has no
//! AetherLink host installed (BIOS-less TVs, iPads, locked machines).
//!
//! This module provides the platform-independent GATT model (services,
//! characteristics, descriptors and their values, derived from
//! [`crate::hid_report`]) and the [`BleHidPeripheral`] factory.
//!
//! **For controlling an iPhone/iPad use [`crate::bt_hid`]**: Bluetooth
//! Classic HID device emulation (IOBluetooth on macOS, BlueZ on Linux) or the
//! ESP32-S3 BLE/USB HID dongle, which work where HOGP below is blocked.
//!
//! ## Platform reality
//!
//! * **macOS / iOS — not possible.** `CBPeripheralManager` refuses to publish
//!   the HID service: `addService:` with UUID `0x1812` fails with
//!   `CBErrorUUIDNotAllowed` ("The specified UUID is not allowed for this
//!   operation"); Apple reserves HID and the other system-owned services
//!   (GAP 0x1800, GATT 0x1801, Device Information 0x180A) for the OS, and no
//!   entitlement lifts this for third-party apps. [`create_peripheral`]
//!   therefore returns [`InputError::Unsupported`] on Apple platforms; a
//!   custom (non-0x1812) GATT service would work but no host OS would treat
//!   it as a keyboard/mouse.
//! * **Linux — designed, not built here.** BlueZ allows it through
//!   `org.bluez.GattManager1.RegisterApplication` +
//!   `org.bluez.LEAdvertisingManager1.RegisterAdvertisement` (the `bluer`
//!   crate wraps both). The `bluer` → `libdbus-sys` build script needs a
//!   target pkg-config/libdbus, which cannot be compile-verified from the macOS
//!   dev machine, so the backend is not shipped in this build. Design for the
//!   backend (everything it needs is in [`gatt_profile`]):
//!   1. Register a GATT application with the services of [`gatt_profile`]:
//!      HID (0x1812) as primary service; Report characteristics with
//!      `read|notify` (input), `read|write|write-without-response` (output)
//!      and `secure-read`/`encrypt-authenticated` flags (HOGP requires an
//!      encrypted, bonded link), a Report Reference descriptor (0x2908) each,
//!      Report Map (0x2A4B), HID Information (0x2A4A), HID Control Point
//!      (0x2A4C, write-without-response) and Protocol Mode (0x2A4E). Add
//!      Battery (0x180F) and Device Information (0x180A, PnP ID) services —
//!      Windows and Android refuse HOGP devices without them.
//!   2. Advertise as connectable with service UUID 0x1812 and appearance
//!      0x03C0 (generic HID) / 0x03C1 (keyboard); `bluetoothd` must run with
//!      the `input` plugin disabled (`-P input`) so it does not claim the
//!      HID UUID for itself.
//!   3. Feed [`crate::hid_report::HidReportBuilder`] output to the notifier of
//!      the Report characteristic whose Report Reference matches the ID.
//! * **Windows** — `GattServiceProvider` rejects the HID UUID as well
//!   (it is reserved by the in-box HID-over-GATT stack): Unsupported.
//! * **Android** — `BluetoothHidDevice` (Classic HID, API 28+) is the
//!   supported route and lives in the Android app, not in this crate.

use crate::hid_report::{
    ABS_POINTER_REPORT_LEN, CONSUMER_REPORT_LEN, KEYBOARD_OUTPUT_LEN, KEYBOARD_REPORT_LEN, MOUSE_REPORT_LEN,
    REPORT_DESCRIPTOR, REPORT_ID_ABS_POINTER, REPORT_ID_CONSUMER, REPORT_ID_KEYBOARD, REPORT_ID_MOUSE,
};
use crate::{InputError, Result};
use aether_protocol::InputEvent;

pub const HID_SERVICE_UUID: u16 = 0x1812;
pub const BATTERY_SERVICE_UUID: u16 = 0x180F;
pub const DEVICE_INFORMATION_SERVICE_UUID: u16 = 0x180A;
pub const REPORT_MAP_UUID: u16 = 0x2A4B;
pub const HID_INFORMATION_UUID: u16 = 0x2A4A;
pub const HID_CONTROL_POINT_UUID: u16 = 0x2A4C;
pub const PROTOCOL_MODE_UUID: u16 = 0x2A4E;
pub const REPORT_UUID: u16 = 0x2A4D;
pub const REPORT_REFERENCE_DESCRIPTOR_UUID: u16 = 0x2908;
pub const BATTERY_LEVEL_UUID: u16 = 0x2A19;
pub const PNP_ID_UUID: u16 = 0x2A50;
/// GAP appearance: Generic HID.
pub const APPEARANCE_GENERIC_HID: u16 = 0x03C0;

/// Bluetooth base UUID expansion of a 16-bit UUID.
pub fn uuid128(short: u16) -> String {
    format!("0000{short:04x}-0000-1000-8000-00805f9b34fb")
}

/// HID Information value: bcdHID 1.11, country 0, flags = RemoteWake | NormallyConnectable.
pub const HID_INFORMATION: [u8; 4] = [0x11, 0x01, 0x00, 0x03];
/// Protocol Mode: 1 = Report Protocol.
pub const PROTOCOL_MODE_REPORT: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportType {
    Input = 1,
    Output = 2,
    Feature = 3,
}

/// A Report characteristic plus the value of its Report Reference descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportCharacteristic {
    pub report_id: u8,
    pub report_type: ReportType,
    /// Payload length without report ID.
    pub len: usize,
}

impl ReportCharacteristic {
    /// Report Reference descriptor (0x2908) value.
    pub fn reference(&self) -> [u8; 2] {
        [self.report_id, self.report_type as u8]
    }
}

/// Static description of the GATT database a HOGP backend must publish.
#[derive(Debug, Clone, PartialEq)]
pub struct GattProfile {
    pub hid_information: [u8; 4],
    pub report_map: &'static [u8],
    pub reports: Vec<ReportCharacteristic>,
    /// PnP ID: vendor-id source (2 = USB-IF), VID, PID, version — little endian.
    pub pnp_id: [u8; 7],
    pub appearance: u16,
}

pub fn gatt_profile() -> GattProfile {
    let r = |report_id, report_type, len| ReportCharacteristic { report_id, report_type, len };
    GattProfile {
        hid_information: HID_INFORMATION,
        report_map: REPORT_DESCRIPTOR,
        reports: vec![
            r(REPORT_ID_KEYBOARD, ReportType::Input, KEYBOARD_REPORT_LEN),
            r(REPORT_ID_KEYBOARD, ReportType::Output, KEYBOARD_OUTPUT_LEN),
            r(REPORT_ID_MOUSE, ReportType::Input, MOUSE_REPORT_LEN),
            r(REPORT_ID_ABS_POINTER, ReportType::Input, ABS_POINTER_REPORT_LEN),
            r(REPORT_ID_CONSUMER, ReportType::Input, CONSUMER_REPORT_LEN),
        ],
        // USB-IF source, VID 0x1209 (pid.codes open-source VID), PID 0xAE71, v1.0.
        pnp_id: [0x02, 0x09, 0x12, 0x71, 0xAE, 0x00, 0x01],
        appearance: APPEARANCE_GENERIC_HID,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BleHidConfig {
    /// Advertised local name.
    pub device_name: String,
}

/// A running BLE HID peripheral.
pub trait BleHidPeripheral: Send {
    /// Start advertising and serving the GATT database.
    fn start(&mut self) -> Result<()>;
    /// Convert and notify an input event to the connected central.
    fn send(&mut self, ev: &InputEvent) -> Result<()>;
    fn is_connected(&self) -> bool;
    fn stop(&mut self);
}

/// Create the BLE HID peripheral for this OS. See the module docs for why
/// this currently returns [`InputError::Unsupported`] everywhere.
pub fn create_peripheral(cfg: BleHidConfig) -> Result<Box<dyn BleHidPeripheral>> {
    let _ = cfg;
    if cfg!(any(target_os = "macos", target_os = "ios")) {
        Err(InputError::Unsupported(
            "CoreBluetooth refuses to publish the HID service (0x1812) from CBPeripheralManager (CBErrorUUIDNotAllowed)",
        ))
    } else if cfg!(target_os = "windows") {
        Err(InputError::Unsupported("Windows GattServiceProvider reserves the HID-over-GATT service UUID"))
    } else if cfg!(target_os = "android") {
        Err(InputError::Unsupported("on Android use BluetoothHidDevice from the mobile app"))
    } else {
        Err(InputError::Unsupported(
            "BlueZ HOGP backend not built: requires libdbus (bluer); see aether_input::ble_hid docs for the design",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_is_consistent() {
        let p = gatt_profile();
        assert_eq!(p.report_map, REPORT_DESCRIPTOR);
        assert_eq!(p.reports.iter().filter(|r| r.report_type == ReportType::Input).count(), 4);
        assert_eq!(p.reports[1].reference(), [1, 2]);
        assert_eq!(p.reports[0].reference(), [1, 1]);
        // bcdHID 1.11 little endian
        assert_eq!(&p.hid_information[..2], &[0x11, 0x01]);
        assert!(p.report_map.len() <= 512, "GATT attribute values are limited to 512 bytes");
    }

    #[test]
    fn uuids() {
        assert_eq!(uuid128(HID_SERVICE_UUID), "00001812-0000-1000-8000-00805f9b34fb");
    }

    #[test]
    fn apple_is_unsupported() {
        let r = create_peripheral(BleHidConfig { device_name: "AetherLink".into() });
        assert!(matches!(r, Err(InputError::Unsupported(_))));
    }
}
