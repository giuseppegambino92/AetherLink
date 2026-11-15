//! USB-over-QUIC redirection (URB level, USB/IP-compatible semantics).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDeviceInfo {
    /// Host-assigned handle, stable while plugged in.
    pub device_id: u32,
    pub bus: u8,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub speed: UsbSpeed,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
    /// Raw device descriptor (18 bytes).
    pub device_descriptor: Vec<u8>,
    /// Raw configuration descriptors (each including interfaces/endpoints).
    pub config_descriptors: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsbSpeed {
    Low,
    Full,
    High,
    Super,
    SuperPlus,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsbTransferType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsbDirection {
    /// Host → device.
    Out,
    /// Device → host.
    In,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrbSubmit {
    pub urb_id: u64,
    /// Endpoint address including direction bit (0x80 = IN).
    pub endpoint: u8,
    pub transfer_type: UsbTransferType,
    pub direction: UsbDirection,
    /// 8-byte SETUP packet for control transfers.
    pub setup: Option<[u8; 8]>,
    /// OUT payload.
    pub data: Vec<u8>,
    /// Requested IN length.
    pub length: u32,
    pub timeout_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UrbStatus {
    Ok,
    Stall,
    Timeout,
    NoDevice,
    Cancelled,
    Overflow,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrbComplete {
    pub urb_id: u64,
    pub status: UrbStatus,
    pub data: Vec<u8>,
    pub actual_length: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsbMessage {
    ListDevices,
    Devices(Vec<UsbDeviceInfo>),
    Attach { device_id: u32 },
    Attached { device_id: u32, ok: bool, error: Option<String> },
    Detach { device_id: u32 },
    Submit(UrbSubmit),
    Unlink { urb_id: u64 },
    Complete(UrbComplete),
    DeviceGone { device_id: u32 },
}
