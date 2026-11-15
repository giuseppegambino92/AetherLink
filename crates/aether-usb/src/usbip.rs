//! USB/IP wire format (Linux `drivers/usb/usbip`, protocol version 1.1.1). All integers
//! are big-endian.
//!
//! Connection setup uses "operation" messages ([`OpMessage`]); after a successful
//! `OP_REP_IMPORT` the same TCP connection carries URB PDUs ([`UrbPdu`]) — 48-byte headers
//! followed by OUT data (in `CMD_SUBMIT`) or IN data (in `RET_SUBMIT`).

use aether_protocol::{
    UrbComplete, UrbStatus, UrbSubmit, UsbDeviceInfo, UsbDirection, UsbSpeed, UsbTransferType,
};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::descriptors::{ConfigDescriptor, DeviceDescriptor};
use crate::error::{Result, UsbError};

pub const USBIP_VERSION: u16 = 0x0111;
pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;

pub const USBIP_CMD_SUBMIT: u32 = 1;
pub const USBIP_CMD_UNLINK: u32 = 2;
pub const USBIP_RET_SUBMIT: u32 = 3;
pub const USBIP_RET_UNLINK: u32 = 4;

pub const USBIP_DIR_OUT: u32 = 0;
pub const USBIP_DIR_IN: u32 = 1;

/// `OP_REP_*` status codes.
pub const ST_OK: u32 = 0;
pub const ST_NA: u32 = 1;
pub const ST_DEV_BUSY: u32 = 2;
pub const ST_DEV_ERR: u32 = 3;
pub const ST_NODEV: u32 = 4;
pub const ST_ERROR: u32 = 5;

/// Linux errno values used in URB status fields (negated on the wire).
pub mod errno {
    pub const EPIPE: i32 = 32;
    pub const ETIMEDOUT: i32 = 110;
    pub const ENODEV: i32 = 19;
    pub const ECONNRESET: i32 = 104;
    pub const EOVERFLOW: i32 = 75;
    pub const EPROTO: i32 = 71;
    pub const ENOENT: i32 = 2;
    pub const ESHUTDOWN: i32 = 108;
}

/// Size of `struct usbip_usb_device` on the wire.
pub const USB_DEVICE_SIZE: usize = 312;
/// Size of every URB PDU header.
pub const URB_HEADER_SIZE: usize = 48;
/// Size of `struct usbip_iso_packet_descriptor`.
pub const ISO_PACKET_SIZE: usize = 16;
/// Refuse URB buffers larger than this (hostile peer protection).
pub const MAX_URB_BUFFER: usize = 16 * 1024 * 1024;

/// Linux `enum usb_device_speed`.
pub fn speed_to_linux(s: UsbSpeed) -> u32 {
    match s {
        UsbSpeed::Unknown => 0,
        UsbSpeed::Low => 1,
        UsbSpeed::Full => 2,
        UsbSpeed::High => 3,
        UsbSpeed::Super => 5,
        UsbSpeed::SuperPlus => 6,
    }
}

pub fn speed_from_linux(v: u32) -> UsbSpeed {
    match v {
        1 => UsbSpeed::Low,
        2 => UsbSpeed::Full,
        3 | 4 => UsbSpeed::High,
        5 => UsbSpeed::Super,
        6 => UsbSpeed::SuperPlus,
        _ => UsbSpeed::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UsbipInterface {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

/// `struct usbip_usb_device` (+ interfaces, which only `OP_REP_DEVLIST` carries).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UsbipDevice {
    pub path: String,
    pub busid: String,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: u32,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub configuration_value: u8,
    pub num_configurations: u8,
    pub num_interfaces: u8,
    pub interfaces: Vec<UsbipInterface>,
}

fn put_str(out: &mut Vec<u8>, s: &str, size: usize) {
    let b = s.as_bytes();
    let n = b.len().min(size - 1); // always NUL-terminated
    out.extend_from_slice(&b[..n]);
    out.resize(out.len() + size - n, 0);
}

fn get_str(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i..i + 4].try_into().unwrap())
}

fn bei32(b: &[u8], i: usize) -> i32 {
    be32(b, i) as i32
}

impl UsbipDevice {
    /// Encode the 312-byte struct (without interfaces).
    pub fn encode(&self, out: &mut Vec<u8>) {
        put_str(out, &self.path, 256);
        put_str(out, &self.busid, 32);
        out.extend_from_slice(&self.busnum.to_be_bytes());
        out.extend_from_slice(&self.devnum.to_be_bytes());
        out.extend_from_slice(&self.speed.to_be_bytes());
        out.extend_from_slice(&self.id_vendor.to_be_bytes());
        out.extend_from_slice(&self.id_product.to_be_bytes());
        out.extend_from_slice(&self.bcd_device.to_be_bytes());
        out.extend_from_slice(&[
            self.device_class,
            self.device_subclass,
            self.device_protocol,
            self.configuration_value,
            self.num_configurations,
            self.num_interfaces,
        ]);
    }

    /// Decode the 312-byte struct (interfaces left empty).
    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < USB_DEVICE_SIZE {
            return Err(UsbError::Protocol("short usbip_usb_device".into()));
        }
        Ok(Self {
            path: get_str(&b[..256]),
            busid: get_str(&b[256..288]),
            busnum: be32(b, 288),
            devnum: be32(b, 292),
            speed: be32(b, 296),
            id_vendor: be16(b, 300),
            id_product: be16(b, 302),
            bcd_device: be16(b, 304),
            device_class: b[306],
            device_subclass: b[307],
            device_protocol: b[308],
            configuration_value: b[309],
            num_configurations: b[310],
            num_interfaces: b[311],
            interfaces: Vec::new(),
        })
    }

    /// Build the USB/IP view of a redirected device from its AetherLink description.
    pub fn from_info(info: &UsbDeviceInfo, busid: &str) -> Result<Self> {
        let dd = DeviceDescriptor::parse(&info.device_descriptor)?;
        let cfg = info
            .config_descriptors
            .first()
            .map(|c| ConfigDescriptor::parse(c))
            .transpose()?;
        let mut interfaces = Vec::new();
        if let Some(cfg) = &cfg {
            for n in cfg.interface_numbers() {
                // Alternate setting 0 describes the interface.
                if let Some(i) = cfg.interfaces.iter().find(|i| i.number == n) {
                    interfaces.push(UsbipInterface {
                        class: i.class,
                        subclass: i.subclass,
                        protocol: i.protocol,
                    });
                }
            }
        }
        Ok(Self {
            path: format!("/sys/devices/aetherlink/{busid}"),
            busid: busid.to_string(),
            busnum: info.bus as u32,
            devnum: info.address as u32,
            speed: speed_to_linux(info.speed),
            id_vendor: dd.vendor_id,
            id_product: dd.product_id,
            bcd_device: dd.bcd_device,
            device_class: dd.class,
            device_subclass: dd.subclass,
            device_protocol: dd.protocol,
            configuration_value: cfg.as_ref().map(|c| c.configuration_value).unwrap_or(1),
            num_configurations: dd.num_configurations,
            num_interfaces: interfaces.len() as u8,
            interfaces,
        })
    }
}

/// Connection-setup messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpMessage {
    ReqDevlist,
    RepDevlist {
        status: u32,
        devices: Vec<UsbipDevice>,
    },
    ReqImport {
        busid: String,
    },
    /// `device` is present iff `status == ST_OK`.
    RepImport {
        status: u32,
        device: Option<UsbipDevice>,
    },
}

impl OpMessage {
    pub fn code(&self) -> u16 {
        match self {
            OpMessage::ReqDevlist => OP_REQ_DEVLIST,
            OpMessage::RepDevlist { .. } => OP_REP_DEVLIST,
            OpMessage::ReqImport { .. } => OP_REQ_IMPORT,
            OpMessage::RepImport { .. } => OP_REP_IMPORT,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let status = match self {
            OpMessage::RepDevlist { status, .. } | OpMessage::RepImport { status, .. } => *status,
            _ => 0,
        };
        let mut out = Vec::with_capacity(8 + USB_DEVICE_SIZE);
        out.extend_from_slice(&USBIP_VERSION.to_be_bytes());
        out.extend_from_slice(&self.code().to_be_bytes());
        out.extend_from_slice(&status.to_be_bytes());
        match self {
            OpMessage::ReqDevlist => {}
            OpMessage::RepDevlist { devices, .. } => {
                out.extend_from_slice(&(devices.len() as u32).to_be_bytes());
                for d in devices {
                    let mut d = d.clone();
                    d.num_interfaces = d.interfaces.len() as u8;
                    d.encode(&mut out);
                    for i in &d.interfaces {
                        out.extend_from_slice(&[i.class, i.subclass, i.protocol, 0]);
                    }
                }
            }
            OpMessage::ReqImport { busid } => put_str(&mut out, busid, 32),
            OpMessage::RepImport { device, .. } => {
                if let Some(d) = device {
                    d.encode(&mut out);
                }
            }
        }
        out
    }

    /// Read one operation message from an async stream.
    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self> {
        let mut hdr = [0u8; 8];
        r.read_exact(&mut hdr).await?;
        let version = be16(&hdr, 0);
        let code = be16(&hdr, 2);
        let status = be32(&hdr, 4);
        if version != USBIP_VERSION {
            return Err(UsbError::Protocol(format!(
                "unsupported USB/IP version {version:#06x}"
            )));
        }
        match code {
            OP_REQ_DEVLIST => Ok(OpMessage::ReqDevlist),
            OP_REQ_IMPORT => {
                let mut b = [0u8; 32];
                r.read_exact(&mut b).await?;
                Ok(OpMessage::ReqImport { busid: get_str(&b) })
            }
            OP_REP_DEVLIST => {
                let mut n = [0u8; 4];
                r.read_exact(&mut n).await?;
                let n = u32::from_be_bytes(n);
                if n > 1024 {
                    return Err(UsbError::Protocol(format!("implausible device count {n}")));
                }
                let mut devices = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let mut b = vec![0u8; USB_DEVICE_SIZE];
                    r.read_exact(&mut b).await?;
                    let mut d = UsbipDevice::decode(&b)?;
                    let mut ib = vec![0u8; 4 * d.num_interfaces as usize];
                    r.read_exact(&mut ib).await?;
                    d.interfaces = ib
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| UsbipInterface {
                            class: c[0],
                            subclass: c[1],
                            protocol: c[2],
                        })
                        .collect();
                    devices.push(d);
                }
                Ok(OpMessage::RepDevlist { status, devices })
            }
            OP_REP_IMPORT => {
                let device = if status == ST_OK {
                    let mut b = vec![0u8; USB_DEVICE_SIZE];
                    r.read_exact(&mut b).await?;
                    Some(UsbipDevice::decode(&b)?)
                } else {
                    None
                };
                Ok(OpMessage::RepImport { status, device })
            }
            c => Err(UsbError::Protocol(format!(
                "unknown USB/IP operation {c:#06x}"
            ))),
        }
    }
}

/// `struct usbip_iso_packet_descriptor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsoPacket {
    pub offset: u32,
    pub length: u32,
    pub actual_length: u32,
    pub status: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CmdSubmit {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
    pub transfer_flags: u32,
    pub transfer_buffer_length: u32,
    pub start_frame: i32,
    /// 0 or `-1` (0xffffffff) for non-isochronous URBs.
    pub number_of_packets: i32,
    pub interval: i32,
    pub setup: [u8; 8],
    /// OUT payload (`transfer_buffer_length` bytes when `direction == OUT`).
    pub data: Vec<u8>,
    pub iso: Vec<IsoPacket>,
}

impl CmdSubmit {
    pub fn is_iso(&self) -> bool {
        self.number_of_packets > 0
    }
}

/// `RET_SUBMIT`. The header's devid/direction/ep are 0 on the wire, as in Linux.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetSubmit {
    pub seqnum: u32,
    pub status: i32,
    pub actual_length: u32,
    pub start_frame: i32,
    pub number_of_packets: i32,
    pub error_count: i32,
    /// IN payload (`actual_length` bytes) — empty for OUT transfers.
    pub data: Vec<u8>,
    pub iso: Vec<IsoPacket>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CmdUnlink {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
    pub unlink_seqnum: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetUnlink {
    pub seqnum: u32,
    pub status: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrbPdu {
    CmdSubmit(CmdSubmit),
    RetSubmit(RetSubmit),
    CmdUnlink(CmdUnlink),
    RetUnlink(RetUnlink),
}

fn header(out: &mut Vec<u8>, command: u32, seqnum: u32, devid: u32, direction: u32, ep: u32) {
    for v in [command, seqnum, devid, direction, ep] {
        out.extend_from_slice(&v.to_be_bytes());
    }
}

fn put_iso(out: &mut Vec<u8>, iso: &[IsoPacket]) {
    for p in iso {
        for v in [p.offset, p.length, p.actual_length, p.status] {
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}

impl UrbPdu {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(URB_HEADER_SIZE);
        match self {
            UrbPdu::CmdSubmit(c) => {
                header(
                    &mut out,
                    USBIP_CMD_SUBMIT,
                    c.seqnum,
                    c.devid,
                    c.direction,
                    c.ep,
                );
                out.extend_from_slice(&c.transfer_flags.to_be_bytes());
                out.extend_from_slice(&c.transfer_buffer_length.to_be_bytes());
                out.extend_from_slice(&c.start_frame.to_be_bytes());
                out.extend_from_slice(&c.number_of_packets.to_be_bytes());
                out.extend_from_slice(&c.interval.to_be_bytes());
                out.extend_from_slice(&c.setup);
                if c.direction == USBIP_DIR_OUT {
                    out.extend_from_slice(&c.data);
                }
                put_iso(&mut out, &c.iso);
            }
            UrbPdu::RetSubmit(r) => {
                header(&mut out, USBIP_RET_SUBMIT, r.seqnum, 0, 0, 0);
                out.extend_from_slice(&r.status.to_be_bytes());
                out.extend_from_slice(&r.actual_length.to_be_bytes());
                out.extend_from_slice(&r.start_frame.to_be_bytes());
                out.extend_from_slice(&r.number_of_packets.to_be_bytes());
                out.extend_from_slice(&r.error_count.to_be_bytes());
                out.extend_from_slice(&[0u8; 8]);
                out.extend_from_slice(&r.data);
                put_iso(&mut out, &r.iso);
            }
            UrbPdu::CmdUnlink(u) => {
                header(
                    &mut out,
                    USBIP_CMD_UNLINK,
                    u.seqnum,
                    u.devid,
                    u.direction,
                    u.ep,
                );
                out.extend_from_slice(&u.unlink_seqnum.to_be_bytes());
                out.extend_from_slice(&[0u8; 24]);
            }
            UrbPdu::RetUnlink(u) => {
                header(&mut out, USBIP_RET_UNLINK, u.seqnum, 0, 0, 0);
                out.extend_from_slice(&u.status.to_be_bytes());
                out.extend_from_slice(&[0u8; 24]);
            }
        }
        out
    }

    /// Read one URB PDU. `ret_dir_in(seqnum)` tells, for `RET_SUBMIT`, whether the original
    /// submission was IN (the wire header does not say); servers never receive `RET_*`
    /// and may pass `|_| false`.
    pub async fn read<R: AsyncRead + Unpin>(
        r: &mut R,
        ret_dir_in: impl Fn(u32) -> bool,
    ) -> Result<Self> {
        let mut h = [0u8; URB_HEADER_SIZE];
        r.read_exact(&mut h).await?;
        let command = be32(&h, 0);
        let seqnum = be32(&h, 4);
        let devid = be32(&h, 8);
        let direction = be32(&h, 12);
        let ep = be32(&h, 16);
        let read_iso = |n: i32| -> Result<usize> {
            if n <= 0 {
                return Ok(0);
            }
            if n as usize > 1024 {
                return Err(UsbError::Protocol(format!(
                    "implausible iso packet count {n}"
                )));
            }
            Ok(n as usize)
        };
        match command {
            USBIP_CMD_SUBMIT => {
                let len = be32(&h, 24);
                if len as usize > MAX_URB_BUFFER {
                    return Err(UsbError::Protocol(format!(
                        "URB buffer of {len} bytes refused"
                    )));
                }
                let mut c = CmdSubmit {
                    seqnum,
                    devid,
                    direction,
                    ep,
                    transfer_flags: be32(&h, 20),
                    transfer_buffer_length: len,
                    start_frame: bei32(&h, 28),
                    number_of_packets: bei32(&h, 32),
                    interval: bei32(&h, 36),
                    setup: h[40..48].try_into().unwrap(),
                    data: Vec::new(),
                    iso: Vec::new(),
                };
                if direction == USBIP_DIR_OUT && len > 0 {
                    c.data = vec![0u8; len as usize];
                    r.read_exact(&mut c.data).await?;
                }
                c.iso = read_iso_packets(r, read_iso(c.number_of_packets)?).await?;
                Ok(UrbPdu::CmdSubmit(c))
            }
            USBIP_RET_SUBMIT => {
                let actual = be32(&h, 24);
                if actual as usize > MAX_URB_BUFFER {
                    return Err(UsbError::Protocol(format!(
                        "URB buffer of {actual} bytes refused"
                    )));
                }
                let mut ret = RetSubmit {
                    seqnum,
                    status: bei32(&h, 20),
                    actual_length: actual,
                    start_frame: bei32(&h, 28),
                    number_of_packets: bei32(&h, 32),
                    error_count: bei32(&h, 36),
                    data: Vec::new(),
                    iso: Vec::new(),
                };
                if ret_dir_in(seqnum) && actual > 0 {
                    ret.data = vec![0u8; actual as usize];
                    r.read_exact(&mut ret.data).await?;
                }
                ret.iso = read_iso_packets(r, read_iso(ret.number_of_packets)?).await?;
                Ok(UrbPdu::RetSubmit(ret))
            }
            USBIP_CMD_UNLINK => Ok(UrbPdu::CmdUnlink(CmdUnlink {
                seqnum,
                devid,
                direction,
                ep,
                unlink_seqnum: be32(&h, 20),
            })),
            USBIP_RET_UNLINK => Ok(UrbPdu::RetUnlink(RetUnlink {
                seqnum,
                status: bei32(&h, 20),
            })),
            c => Err(UsbError::Protocol(format!("unknown USB/IP command {c:#x}"))),
        }
    }
}

async fn read_iso_packets<R: AsyncRead + Unpin>(r: &mut R, n: usize) -> Result<Vec<IsoPacket>> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut b = [0u8; ISO_PACKET_SIZE];
        r.read_exact(&mut b).await?;
        out.push(IsoPacket {
            offset: be32(&b, 0),
            length: be32(&b, 4),
            actual_length: be32(&b, 8),
            status: be32(&b, 12),
        });
    }
    Ok(out)
}

/// Linux URB status (negative errno) for an AetherLink [`UrbStatus`].
pub fn status_to_linux(s: UrbStatus) -> i32 {
    match s {
        UrbStatus::Ok => 0,
        UrbStatus::Stall => -errno::EPIPE,
        UrbStatus::Timeout => -errno::ETIMEDOUT,
        UrbStatus::NoDevice => -errno::ENODEV,
        UrbStatus::Cancelled => -errno::ECONNRESET,
        UrbStatus::Overflow => -errno::EOVERFLOW,
        UrbStatus::Error => -errno::EPROTO,
    }
}

pub fn status_from_linux(v: i32) -> UrbStatus {
    match -v {
        0 => UrbStatus::Ok,
        errno::EPIPE => UrbStatus::Stall,
        errno::ETIMEDOUT => UrbStatus::Timeout,
        errno::ENODEV | errno::ESHUTDOWN => UrbStatus::NoDevice,
        errno::ECONNRESET | errno::ENOENT => UrbStatus::Cancelled,
        errno::EOVERFLOW => UrbStatus::Overflow,
        _ => UrbStatus::Error,
    }
}

/// Convert a `CMD_SUBMIT` into an AetherLink [`UrbSubmit`] (`urb_id` = seqnum).
///
/// `endpoint_type` resolves the transfer type of a non-zero endpoint address (from the
/// device's configuration descriptors); unknown endpoints default to bulk. Endpoint 0 is
/// control; URBs with ISO packets are isochronous.
pub fn cmd_submit_to_urb(
    c: &CmdSubmit,
    endpoint_type: impl Fn(u8) -> Option<UsbTransferType>,
) -> UrbSubmit {
    let dir_in = c.direction == USBIP_DIR_IN;
    let address = (c.ep as u8 & 0x0F) | if dir_in { 0x80 } else { 0 };
    let transfer_type = if c.ep & 0x0F == 0 {
        UsbTransferType::Control
    } else if c.is_iso() {
        UsbTransferType::Isochronous
    } else {
        endpoint_type(address).unwrap_or(UsbTransferType::Bulk)
    };
    UrbSubmit {
        urb_id: c.seqnum as u64,
        endpoint: address,
        transfer_type,
        direction: if dir_in {
            UsbDirection::In
        } else {
            UsbDirection::Out
        },
        setup: (transfer_type == UsbTransferType::Control).then_some(c.setup),
        data: if dir_in { Vec::new() } else { c.data.clone() },
        length: c.transfer_buffer_length,
        // USB/IP URBs have no timeout: they wait until completed or unlinked.
        timeout_ms: 0,
    }
}

/// Inverse of [`cmd_submit_to_urb`] (used by tests and by USB/IP clients).
pub fn urb_to_cmd_submit(u: &UrbSubmit, devid: u32) -> CmdSubmit {
    let dir_in = u.direction == UsbDirection::In;
    CmdSubmit {
        seqnum: u.urb_id as u32,
        devid,
        direction: if dir_in { USBIP_DIR_IN } else { USBIP_DIR_OUT },
        ep: (u.endpoint & 0x0F) as u32,
        transfer_flags: 0,
        transfer_buffer_length: if dir_in {
            u.length
        } else {
            u.data.len() as u32
        },
        start_frame: 0,
        number_of_packets: 0,
        interval: 0,
        setup: u.setup.unwrap_or_default(),
        data: if dir_in { Vec::new() } else { u.data.clone() },
        iso: Vec::new(),
    }
}

/// Convert a completion into a `RET_SUBMIT` for `seqnum`.
pub fn urb_complete_to_ret_submit(seqnum: u32, c: &UrbComplete, dir_in: bool) -> RetSubmit {
    let data = if dir_in {
        c.data[..c.data.len().min(c.actual_length as usize)].to_vec()
    } else {
        Vec::new()
    };
    RetSubmit {
        seqnum,
        status: status_to_linux(c.status),
        actual_length: if dir_in {
            data.len() as u32
        } else {
            c.actual_length
        },
        start_frame: 0,
        number_of_packets: 0,
        error_count: 0,
        data,
        iso: Vec::new(),
    }
}

pub fn ret_submit_to_urb_complete(r: &RetSubmit) -> UrbComplete {
    UrbComplete {
        urb_id: r.seqnum as u64,
        status: status_from_linux(r.status),
        data: r.data.clone(),
        actual_length: r.actual_length,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn roundtrip(p: &UrbPdu, dir_in: bool) -> UrbPdu {
        let bytes = p.encode();
        let mut cur = std::io::Cursor::new(bytes);
        let got = UrbPdu::read(&mut cur, |_| dir_in).await.unwrap();
        assert_eq!(
            cur.position() as usize,
            cur.get_ref().len(),
            "consumed exactly"
        );
        got
    }

    #[tokio::test]
    async fn cmd_submit_vector() {
        // GET_DESCRIPTOR(DEVICE) on ep0, IN, 18 bytes, seqnum 7, devid 0x00010002.
        let c = CmdSubmit {
            seqnum: 7,
            devid: 0x0001_0002,
            direction: USBIP_DIR_IN,
            ep: 0,
            transfer_flags: 0x200,
            transfer_buffer_length: 18,
            start_frame: 0,
            number_of_packets: -1,
            interval: 0,
            setup: [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00],
            data: vec![],
            iso: vec![],
        };
        let expected: Vec<u8> = [
            &[0, 0, 0, 1][..],         // command
            &[0, 0, 0, 7],             // seqnum
            &[0, 1, 0, 2],             // devid
            &[0, 0, 0, 1],             // direction IN
            &[0, 0, 0, 0],             // ep
            &[0, 0, 2, 0],             // transfer_flags
            &[0, 0, 0, 18],            // transfer_buffer_length
            &[0, 0, 0, 0],             // start_frame
            &[0xff, 0xff, 0xff, 0xff], // number_of_packets = -1
            &[0, 0, 0, 0],             // interval
            &[0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00],
        ]
        .concat();
        let p = UrbPdu::CmdSubmit(c.clone());
        assert_eq!(p.encode(), expected);
        assert_eq!(roundtrip(&p, false).await, p);
        assert!(!c.is_iso());

        // OUT with payload appended.
        let out = CmdSubmit {
            seqnum: 8,
            direction: USBIP_DIR_OUT,
            ep: 2,
            transfer_buffer_length: 3,
            data: vec![1, 2, 3],
            ..c
        };
        let bytes = UrbPdu::CmdSubmit(out.clone()).encode();
        assert_eq!(bytes.len(), 48 + 3);
        assert_eq!(&bytes[48..], &[1, 2, 3]);
        assert_eq!(
            roundtrip(&UrbPdu::CmdSubmit(out.clone()), false).await,
            UrbPdu::CmdSubmit(out)
        );
    }

    #[tokio::test]
    async fn ret_submit_and_unlink_vectors() {
        let r = RetSubmit {
            seqnum: 7,
            status: -32,
            actual_length: 2,
            data: vec![0xAB, 0xCD],
            ..Default::default()
        };
        let bytes = UrbPdu::RetSubmit(r.clone()).encode();
        let mut expected = vec![0, 0, 0, 3, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        expected.extend_from_slice(&[0xff, 0xff, 0xff, 0xe0]); // -32 (EPIPE)
        expected.extend_from_slice(&[0, 0, 0, 2]);
        expected.extend_from_slice(&[0; 12]);
        expected.extend_from_slice(&[0; 8]);
        expected.extend_from_slice(&[0xAB, 0xCD]);
        assert_eq!(bytes, expected);
        assert_eq!(
            roundtrip(&UrbPdu::RetSubmit(r.clone()), true).await,
            UrbPdu::RetSubmit(r)
        );

        let u = CmdUnlink {
            seqnum: 9,
            devid: 0x10002,
            direction: 0,
            ep: 0,
            unlink_seqnum: 7,
        };
        let bytes = UrbPdu::CmdUnlink(u.clone()).encode();
        assert_eq!(bytes.len(), 48);
        assert_eq!(&bytes[..4], &[0, 0, 0, 2]);
        assert_eq!(&bytes[20..24], &[0, 0, 0, 7]);
        assert_eq!(
            roundtrip(&UrbPdu::CmdUnlink(u.clone()), false).await,
            UrbPdu::CmdUnlink(u)
        );

        let ru = RetUnlink {
            seqnum: 9,
            status: -104,
        };
        let bytes = UrbPdu::RetUnlink(ru.clone()).encode();
        assert_eq!(&bytes[..8], &[0, 0, 0, 4, 0, 0, 0, 9]);
        assert_eq!(&bytes[20..24], &(-104i32).to_be_bytes());
        assert_eq!(
            roundtrip(&UrbPdu::RetUnlink(ru.clone()), false).await,
            UrbPdu::RetUnlink(ru)
        );
    }

    #[tokio::test]
    async fn op_messages_vectors() {
        assert_eq!(
            OpMessage::ReqDevlist.encode(),
            vec![0x01, 0x11, 0x80, 0x05, 0, 0, 0, 0]
        );
        let imp = OpMessage::ReqImport {
            busid: "1-2".into(),
        };
        let b = imp.encode();
        assert_eq!(b.len(), 8 + 32);
        assert_eq!(&b[..8], &[0x01, 0x11, 0x80, 0x03, 0, 0, 0, 0]);
        assert_eq!(&b[8..12], b"1-2\0");
        assert_eq!(
            OpMessage::read(&mut std::io::Cursor::new(b)).await.unwrap(),
            imp
        );

        let dev = UsbipDevice {
            path: "/sys/devices/aetherlink/1-2".into(),
            busid: "1-2".into(),
            busnum: 1,
            devnum: 2,
            speed: 3,
            id_vendor: 0x1234,
            id_product: 0x5678,
            bcd_device: 0x0100,
            device_class: 0xEF,
            device_subclass: 2,
            device_protocol: 1,
            configuration_value: 1,
            num_configurations: 1,
            num_interfaces: 2,
            interfaces: vec![
                UsbipInterface {
                    class: 2,
                    subclass: 2,
                    protocol: 1,
                },
                UsbipInterface {
                    class: 0x0A,
                    subclass: 0,
                    protocol: 0,
                },
            ],
        };
        let rep = OpMessage::RepDevlist {
            status: 0,
            devices: vec![dev.clone()],
        };
        let b = rep.encode();
        assert_eq!(b.len(), 8 + 4 + USB_DEVICE_SIZE + 8);
        assert_eq!(&b[..12], &[0x01, 0x11, 0x00, 0x05, 0, 0, 0, 0, 0, 0, 0, 1]);
        let d = &b[12..];
        assert_eq!(&d[256..260], b"1-2\0");
        assert_eq!(&d[288..300], &[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]);
        assert_eq!(
            &d[300..312],
            &[0x12, 0x34, 0x56, 0x78, 0x01, 0x00, 0xEF, 2, 1, 1, 1, 2]
        );
        assert_eq!(&d[312..], &[2, 2, 1, 0, 0x0A, 0, 0, 0]);
        assert_eq!(
            OpMessage::read(&mut std::io::Cursor::new(b)).await.unwrap(),
            rep
        );

        let ok = OpMessage::RepImport {
            status: 0,
            device: Some(UsbipDevice {
                interfaces: vec![],
                ..dev
            }),
        };
        let b = ok.encode();
        assert_eq!(b.len(), 8 + USB_DEVICE_SIZE);
        assert_eq!(
            OpMessage::read(&mut std::io::Cursor::new(b)).await.unwrap(),
            ok
        );
        let fail = OpMessage::RepImport {
            status: ST_NODEV,
            device: None,
        };
        assert_eq!(fail.encode().len(), 8);
        assert_eq!(
            OpMessage::read(&mut std::io::Cursor::new(fail.encode()))
                .await
                .unwrap(),
            fail
        );

        let mut wrong = OpMessage::ReqDevlist.encode();
        wrong[1] = 0x06;
        assert!(
            OpMessage::read(&mut std::io::Cursor::new(wrong))
                .await
                .is_err()
        );
    }

    #[test]
    fn urb_conversion() {
        let c = CmdSubmit {
            seqnum: 5,
            direction: USBIP_DIR_IN,
            ep: 1,
            transfer_buffer_length: 512,
            number_of_packets: 0,
            ..Default::default()
        };
        let u = cmd_submit_to_urb(&c, |a| (a == 0x81).then_some(UsbTransferType::Interrupt));
        assert_eq!(u.urb_id, 5);
        assert_eq!(u.endpoint, 0x81);
        assert_eq!(u.transfer_type, UsbTransferType::Interrupt);
        assert_eq!(
            (u.direction, u.length, u.setup),
            (UsbDirection::In, 512, None)
        );
        assert_eq!(urb_to_cmd_submit(&u, 0).ep, 1);

        let ctrl = CmdSubmit {
            ep: 0,
            setup: [0x80, 6, 0, 1, 0, 0, 18, 0],
            ..c.clone()
        };
        let u = cmd_submit_to_urb(&ctrl, |_| None);
        assert_eq!(u.transfer_type, UsbTransferType::Control);
        assert_eq!(u.setup, Some([0x80, 6, 0, 1, 0, 0, 18, 0]));

        let iso = CmdSubmit {
            number_of_packets: 4,
            ..c.clone()
        };
        assert_eq!(
            cmd_submit_to_urb(&iso, |_| None).transfer_type,
            UsbTransferType::Isochronous
        );

        let done = UrbComplete {
            urb_id: 5,
            status: UrbStatus::Stall,
            data: vec![1, 2, 3],
            actual_length: 3,
        };
        let r = urb_complete_to_ret_submit(5, &done, true);
        assert_eq!(
            (r.status, r.actual_length, r.data.clone()),
            (-32, 3, vec![1, 2, 3])
        );
        assert_eq!(ret_submit_to_urb_complete(&r), done);
        let out_done = UrbComplete {
            urb_id: 6,
            status: UrbStatus::Ok,
            data: vec![],
            actual_length: 64,
        };
        let r = urb_complete_to_ret_submit(6, &out_done, false);
        assert_eq!((r.actual_length, r.data.len()), (64, 0));
        for s in [
            UrbStatus::Ok,
            UrbStatus::Stall,
            UrbStatus::Timeout,
            UrbStatus::NoDevice,
            UrbStatus::Cancelled,
            UrbStatus::Overflow,
            UrbStatus::Error,
        ] {
            assert_eq!(status_from_linux(status_to_linux(s)), s);
        }
    }
}
