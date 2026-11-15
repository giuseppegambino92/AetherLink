//! USB standard descriptor parsing (USB 2.0 spec, chapter 9.6).

use aether_protocol::{UsbDirection, UsbTransferType};

use crate::error::{Result, UsbError};

pub const DT_DEVICE: u8 = 1;
pub const DT_CONFIG: u8 = 2;
pub const DT_STRING: u8 = 3;
pub const DT_INTERFACE: u8 = 4;
pub const DT_ENDPOINT: u8 = 5;
pub const DT_INTERFACE_ASSOCIATION: u8 = 11;

fn le16(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn bad(msg: impl Into<String>) -> UsbError {
    UsbError::Descriptor(msg.into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub bcd_usb: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub max_packet_size0: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: u16,
    pub manufacturer_index: u8,
    pub product_index: u8,
    pub serial_index: u8,
    pub num_configurations: u8,
}

impl DeviceDescriptor {
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < 18 || b[0] < 18 || b[1] != DT_DEVICE {
            return Err(bad("device descriptor must be 18 bytes of type 1"));
        }
        Ok(Self {
            bcd_usb: le16(b, 2),
            class: b[4],
            subclass: b[5],
            protocol: b[6],
            max_packet_size0: b[7],
            vendor_id: le16(b, 8),
            product_id: le16(b, 10),
            bcd_device: le16(b, 12),
            manufacturer_index: b[14],
            product_index: b[15],
            serial_index: b[16],
            num_configurations: b[17],
        })
    }

    pub fn to_bytes(&self) -> [u8; 18] {
        let mut b = [0u8; 18];
        b[0] = 18;
        b[1] = DT_DEVICE;
        b[2..4].copy_from_slice(&self.bcd_usb.to_le_bytes());
        b[4] = self.class;
        b[5] = self.subclass;
        b[6] = self.protocol;
        b[7] = self.max_packet_size0;
        b[8..10].copy_from_slice(&self.vendor_id.to_le_bytes());
        b[10..12].copy_from_slice(&self.product_id.to_le_bytes());
        b[12..14].copy_from_slice(&self.bcd_device.to_le_bytes());
        b[14] = self.manufacturer_index;
        b[15] = self.product_index;
        b[16] = self.serial_index;
        b[17] = self.num_configurations;
        b
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointDescriptor {
    /// Address including the direction bit (0x80 = IN).
    pub address: u8,
    pub attributes: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}

impl EndpointDescriptor {
    pub fn number(&self) -> u8 {
        self.address & 0x0F
    }
    pub fn direction(&self) -> UsbDirection {
        if self.address & 0x80 != 0 {
            UsbDirection::In
        } else {
            UsbDirection::Out
        }
    }
    pub fn transfer_type(&self) -> UsbTransferType {
        match self.attributes & 0x03 {
            0 => UsbTransferType::Control,
            1 => UsbTransferType::Isochronous,
            2 => UsbTransferType::Bulk,
            _ => UsbTransferType::Interrupt,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceDescriptor {
    pub number: u8,
    pub alternate_setting: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub interface_index: u8,
    pub endpoints: Vec<EndpointDescriptor>,
    /// Class-specific descriptors following the interface descriptor (raw).
    pub extra: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDescriptor {
    pub total_length: u16,
    pub num_interfaces: u8,
    pub configuration_value: u8,
    pub configuration_index: u8,
    pub attributes: u8,
    /// bMaxPower in 2 mA units (8 mA for SuperSpeed).
    pub max_power: u8,
    /// Every interface *alternate setting*, in descriptor order.
    pub interfaces: Vec<InterfaceDescriptor>,
}

impl ConfigDescriptor {
    /// Parse a full configuration descriptor blob (`wTotalLength` bytes).
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < 9 || b[0] < 9 || b[1] != DT_CONFIG {
            return Err(bad(
                "configuration descriptor must start with a 9-byte type-2 header",
            ));
        }
        let total = le16(b, 2) as usize;
        if total < 9 || total > b.len() {
            return Err(bad(format!(
                "wTotalLength {total} exceeds the {} bytes available",
                b.len()
            )));
        }
        let mut cfg = ConfigDescriptor {
            total_length: total as u16,
            num_interfaces: b[4],
            configuration_value: b[5],
            configuration_index: b[6],
            attributes: b[7],
            max_power: b[8],
            interfaces: Vec::new(),
        };
        let mut i = b[0] as usize;
        while i < total {
            let len = b[i] as usize;
            if len < 2 || i + len > total {
                return Err(bad(format!(
                    "descriptor at offset {i} has invalid length {len}"
                )));
            }
            let d = &b[i..i + len];
            match d[1] {
                DT_INTERFACE => {
                    if len < 9 {
                        return Err(bad("interface descriptor shorter than 9 bytes"));
                    }
                    cfg.interfaces.push(InterfaceDescriptor {
                        number: d[2],
                        alternate_setting: d[3],
                        class: d[5],
                        subclass: d[6],
                        protocol: d[7],
                        interface_index: d[8],
                        endpoints: Vec::with_capacity(d[4] as usize),
                        extra: Vec::new(),
                    });
                }
                DT_ENDPOINT => {
                    if len < 7 {
                        return Err(bad("endpoint descriptor shorter than 7 bytes"));
                    }
                    let iface = cfg
                        .interfaces
                        .last_mut()
                        .ok_or_else(|| bad("endpoint before any interface"))?;
                    iface.endpoints.push(EndpointDescriptor {
                        address: d[2],
                        attributes: d[3],
                        max_packet_size: le16(d, 4),
                        interval: d[6],
                    });
                }
                _ => {
                    if let Some(iface) = cfg.interfaces.last_mut() {
                        iface.extra.extend_from_slice(d);
                    }
                }
            }
            i += len;
        }
        Ok(cfg)
    }

    /// Interface numbers (deduplicated across alternate settings).
    pub fn interface_numbers(&self) -> Vec<u8> {
        let mut v: Vec<u8> = self.interfaces.iter().map(|i| i.number).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Transfer type of an endpoint address (first match over all alternate settings).
    pub fn endpoint_type(&self, address: u8) -> Option<UsbTransferType> {
        self.interfaces
            .iter()
            .flat_map(|i| &i.endpoints)
            .find(|e| e.address == address)
            .map(|e| e.transfer_type())
    }
}

/// Decode a UTF-16LE string descriptor.
pub fn parse_string_descriptor(b: &[u8]) -> Result<String> {
    if b.len() < 2 || b[1] != DT_STRING || (b[0] as usize) > b.len() || b[0] < 2 {
        return Err(bad("invalid string descriptor"));
    }
    let units: Vec<u16> = b[2..b[0] as usize]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A CDC-ACM-like device: config with IAD, a comm interface (interrupt IN + class
    /// descriptors) and a data interface (bulk IN/OUT).
    pub(crate) fn sample_config() -> Vec<u8> {
        let mut c = vec![
            9, 2, 0, 0, 2, 1, 0, 0x80, 50, // config header (total length patched below)
            8, 11, 0, 2, 2, 2, 1, 0, // IAD
            9, 4, 0, 0, 1, 2, 2, 1, 0, // interface 0: CDC comm
            5, 0x24, 0x00, 0x10, 0x01, // CDC header functional descriptor
            7, 5, 0x83, 3, 8, 0, 16, // EP 3 IN interrupt, 8 bytes
            9, 4, 1, 0, 2, 0x0A, 0, 0, 0, // interface 1: CDC data
            7, 5, 0x81, 2, 0x00, 0x02, 0, // EP 1 IN bulk 512
            7, 5, 0x02, 2, 0x00, 0x02, 0, // EP 2 OUT bulk 512
        ];
        let n = c.len() as u16;
        c[2..4].copy_from_slice(&n.to_le_bytes());
        c
    }

    pub(crate) fn sample_device() -> [u8; 18] {
        [
            18, 1, 0x00, 0x02, 0xEF, 0x02, 0x01, 64, 0x34, 0x12, 0x78, 0x56, 0x00, 0x01, 1, 2, 3, 1,
        ]
    }

    #[test]
    fn parses_device_descriptor() {
        let d = DeviceDescriptor::parse(&sample_device()).unwrap();
        assert_eq!(
            (d.vendor_id, d.product_id, d.bcd_usb, d.bcd_device),
            (0x1234, 0x5678, 0x0200, 0x0100)
        );
        assert_eq!(
            (d.class, d.subclass, d.protocol, d.max_packet_size0),
            (0xEF, 2, 1, 64)
        );
        assert_eq!(
            (
                d.manufacturer_index,
                d.product_index,
                d.serial_index,
                d.num_configurations
            ),
            (1, 2, 3, 1)
        );
        assert_eq!(d.to_bytes(), sample_device());
        assert!(DeviceDescriptor::parse(&sample_device()[..17]).is_err());
    }

    #[test]
    fn parses_config_descriptor() {
        let c = ConfigDescriptor::parse(&sample_config()).unwrap();
        assert_eq!(
            (
                c.num_interfaces,
                c.configuration_value,
                c.attributes,
                c.max_power
            ),
            (2, 1, 0x80, 50)
        );
        assert_eq!(c.interfaces.len(), 2);
        assert_eq!(c.interfaces[0].class, 2);
        assert_eq!(c.interfaces[0].extra, vec![5, 0x24, 0x00, 0x10, 0x01]);
        assert_eq!(
            c.interfaces[0].endpoints[0].transfer_type(),
            UsbTransferType::Interrupt
        );
        assert_eq!(c.interfaces[0].endpoints[0].direction(), UsbDirection::In);
        assert_eq!(c.interfaces[1].endpoints[1].max_packet_size, 512);
        assert_eq!(c.endpoint_type(0x81), Some(UsbTransferType::Bulk));
        assert_eq!(c.endpoint_type(0x02), Some(UsbTransferType::Bulk));
        assert_eq!(c.endpoint_type(0x05), None);
        assert_eq!(c.interface_numbers(), vec![0, 1]);

        let mut broken = sample_config();
        broken[9] = 0; // zero-length descriptor
        assert!(ConfigDescriptor::parse(&broken).is_err());
        let mut short = sample_config();
        short.truncate(20);
        assert!(ConfigDescriptor::parse(&short).is_err());
    }

    #[test]
    fn parses_string_descriptor() {
        let b = [10, 3, b'A', 0, b'e', 0, 0xAC, 0x20, b'!', 0];
        assert_eq!(parse_string_descriptor(&b).unwrap(), "Ae€!");
        assert!(parse_string_descriptor(&[2, 1]).is_err());
    }
}
