//! Bluetooth Classic HID SDP service record (HID Profile 1.1 §5.3), as a
//! data-element tree with two renderers:
//!
//! * [`to_bluez_xml`] — the XML accepted by BlueZ
//!   `org.bluez.ProfileManager1.RegisterProfile` (`ServiceRecord` option);
//! * [`to_apple_dict`] — the NSDictionary shape accepted by IOBluetooth
//!   `+[IOBluetoothSDPServiceRecord publishedServiceRecordWithDictionary:]`
//!   (keys `"XXXX - Name"`, typed values as `DataElementType` /
//!   `DataElementSize` / `DataElementValue` dictionaries).

use super::hidp::{PSM_HID_CONTROL, PSM_HID_INTERRUPT};
use crate::hid_report::REPORT_DESCRIPTOR;

/// SDP attribute IDs.
pub mod attr {
    pub const SERVICE_CLASS_ID_LIST: u16 = 0x0001;
    pub const PROTOCOL_DESCRIPTOR_LIST: u16 = 0x0004;
    pub const BROWSE_GROUP_LIST: u16 = 0x0005;
    pub const LANGUAGE_BASE_ATTRIBUTE_ID_LIST: u16 = 0x0006;
    pub const BLUETOOTH_PROFILE_DESCRIPTOR_LIST: u16 = 0x0009;
    pub const ADDITIONAL_PROTOCOL_DESCRIPTOR_LISTS: u16 = 0x000D;
    pub const SERVICE_NAME: u16 = 0x0100;
    pub const SERVICE_DESCRIPTION: u16 = 0x0101;
    pub const PROVIDER_NAME: u16 = 0x0102;
    pub const HID_DEVICE_RELEASE_NUMBER: u16 = 0x0200;
    pub const HID_PARSER_VERSION: u16 = 0x0201;
    pub const HID_DEVICE_SUBCLASS: u16 = 0x0202;
    pub const HID_COUNTRY_CODE: u16 = 0x0203;
    pub const HID_VIRTUAL_CABLE: u16 = 0x0204;
    pub const HID_RECONNECT_INITIATE: u16 = 0x0205;
    pub const HID_DESCRIPTOR_LIST: u16 = 0x0206;
    pub const HID_LANGID_BASE_LIST: u16 = 0x0207;
    pub const HID_SDP_DISABLE: u16 = 0x0208;
    pub const HID_BATTERY_POWER: u16 = 0x0209;
    pub const HID_REMOTE_WAKE: u16 = 0x020A;
    pub const HID_PROFILE_VERSION: u16 = 0x020B;
    pub const HID_SUPERVISION_TIMEOUT: u16 = 0x020C;
    pub const HID_NORMALLY_CONNECTABLE: u16 = 0x020D;
    pub const HID_BOOT_DEVICE: u16 = 0x020E;
    pub const HID_SSR_HOST_MAX_LATENCY: u16 = 0x020F;
    pub const HID_SSR_HOST_MIN_TIMEOUT: u16 = 0x0210;
}

/// 16-bit UUIDs used by the record.
pub mod uuid {
    pub const L2CAP: u16 = 0x0100;
    pub const HIDP: u16 = 0x0011;
    pub const HUMAN_INTERFACE_DEVICE: u16 = 0x1124;
    pub const PUBLIC_BROWSE_ROOT: u16 = 0x1002;
}

/// HIDDeviceSubclass / minor device class: combo keyboard + pointing device.
pub const SUBCLASS_COMBO_KEYBOARD_POINTING: u8 = 0xC0;
/// HIDDescriptorList class descriptor type: Report descriptor.
pub const DESCRIPTOR_TYPE_REPORT: u8 = 0x22;
/// Class of Device for a combo keyboard/pointing peripheral:
/// major class 0x05 (Peripheral), minor 0x30 (keyboard + pointing; byte 0xC0),
/// service class bit 13 (Limited Discoverable) is managed by the stack.
pub const CLASS_OF_DEVICE: u32 = 0x0005C0;
pub const MAJOR_CLASS_PERIPHERAL: u8 = 0x05;
pub const MINOR_CLASS_KEYBOARD_POINTING: u8 = 0xC0;

/// An SDP data element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataElement {
    U8(u8),
    U16(u16),
    U32(u32),
    Bool(bool),
    Uuid16(u16),
    Text(String),
    /// Text-string element carrying raw bytes (HID report descriptor).
    Bytes(Vec<u8>),
    Seq(Vec<DataElement>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub id: u16,
    pub name: &'static str,
    pub value: DataElement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidSdpConfig {
    pub service_name: String,
    pub service_description: String,
    pub provider_name: String,
    pub report_descriptor: Vec<u8>,
}

impl Default for HidSdpConfig {
    fn default() -> Self {
        Self {
            service_name: "AetherLink Keyboard".into(),
            service_description: "AetherLink keyboard and pointing device".into(),
            provider_name: "AetherLink".into(),
            report_descriptor: REPORT_DESCRIPTOR.to_vec(),
        }
    }
}

/// Build the HID service record, sorted by attribute ID.
pub fn hid_service_record(cfg: &HidSdpConfig) -> Vec<Attribute> {
    use DataElement::*;
    let l2cap = |psm| Seq(vec![Seq(vec![Uuid16(uuid::L2CAP), U16(psm)]), Seq(vec![Uuid16(uuid::HIDP)])]);
    let a = |id, name, value| Attribute { id, name, value };
    vec![
        a(attr::SERVICE_CLASS_ID_LIST, "ServiceClassIDList", Seq(vec![Uuid16(uuid::HUMAN_INTERFACE_DEVICE)])),
        a(attr::PROTOCOL_DESCRIPTOR_LIST, "ProtocolDescriptorList", l2cap(PSM_HID_CONTROL)),
        a(attr::BROWSE_GROUP_LIST, "BrowseGroupList", Seq(vec![Uuid16(uuid::PUBLIC_BROWSE_ROOT)])),
        // "en", UTF-8 (MIBenum 106), base 0x0100.
        a(attr::LANGUAGE_BASE_ATTRIBUTE_ID_LIST, "LanguageBaseAttributeIDList", Seq(vec![U16(0x656E), U16(0x006A), U16(0x0100)])),
        a(
            attr::BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
            "BluetoothProfileDescriptorList",
            Seq(vec![Seq(vec![Uuid16(uuid::HUMAN_INTERFACE_DEVICE), U16(0x0101)])]),
        ),
        a(attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LISTS, "AdditionalProtocolDescriptorLists", Seq(vec![l2cap(PSM_HID_INTERRUPT)])),
        a(attr::SERVICE_NAME, "ServiceName", Text(cfg.service_name.clone())),
        a(attr::SERVICE_DESCRIPTION, "ServiceDescription", Text(cfg.service_description.clone())),
        a(attr::PROVIDER_NAME, "ProviderName", Text(cfg.provider_name.clone())),
        a(attr::HID_DEVICE_RELEASE_NUMBER, "HIDDeviceReleaseNumber", U16(0x0100)),
        a(attr::HID_PARSER_VERSION, "HIDParserVersion", U16(0x0111)),
        a(attr::HID_DEVICE_SUBCLASS, "HIDDeviceSubclass", U8(SUBCLASS_COMBO_KEYBOARD_POINTING)),
        a(attr::HID_COUNTRY_CODE, "HIDCountryCode", U8(0x00)),
        a(attr::HID_VIRTUAL_CABLE, "HIDVirtualCable", Bool(true)),
        a(attr::HID_RECONNECT_INITIATE, "HIDReconnectInitiate", Bool(true)),
        a(
            attr::HID_DESCRIPTOR_LIST,
            "HIDDescriptorList",
            Seq(vec![Seq(vec![U8(DESCRIPTOR_TYPE_REPORT), Bytes(cfg.report_descriptor.clone())])]),
        ),
        // en-US, base 0x0100.
        a(attr::HID_LANGID_BASE_LIST, "HIDLANGIDBaseList", Seq(vec![Seq(vec![U16(0x0409), U16(0x0100)])])),
        a(attr::HID_SDP_DISABLE, "HIDSDPDisable", Bool(false)),
        a(attr::HID_BATTERY_POWER, "HIDBatteryPower", Bool(true)),
        a(attr::HID_REMOTE_WAKE, "HIDRemoteWake", Bool(true)),
        a(attr::HID_PROFILE_VERSION, "HIDProfileVersion", U16(0x0100)),
        // 0x0C80 * 0.625 ms = 2 s.
        a(attr::HID_SUPERVISION_TIMEOUT, "HIDSupervisionTimeout", U16(0x0C80)),
        a(attr::HID_NORMALLY_CONNECTABLE, "HIDNormallyConnectable", Bool(false)),
        a(attr::HID_BOOT_DEVICE, "HIDBootDevice", Bool(true)),
        a(attr::HID_SSR_HOST_MAX_LATENCY, "HIDSSRHostMaxLatency", U16(0x0640)),
        a(attr::HID_SSR_HOST_MIN_TIMEOUT, "HIDSSRHostMinTimeout", U16(0x0320)),
    ]
}

fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

fn xml_element(e: &DataElement, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    match e {
        DataElement::U8(v) => out.push_str(&format!("{pad}<uint8 value=\"0x{v:02x}\" />\n")),
        DataElement::U16(v) => out.push_str(&format!("{pad}<uint16 value=\"0x{v:04x}\" />\n")),
        DataElement::U32(v) => out.push_str(&format!("{pad}<uint32 value=\"0x{v:08x}\" />\n")),
        DataElement::Bool(v) => out.push_str(&format!("{pad}<boolean value=\"{v}\" />\n")),
        DataElement::Uuid16(v) => out.push_str(&format!("{pad}<uuid value=\"0x{v:04x}\" />\n")),
        DataElement::Text(s) => out.push_str(&format!("{pad}<text value=\"{}\" />\n", xml_escape(s))),
        DataElement::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            out.push_str(&format!("{pad}<text encoding=\"hex\" value=\"{hex}\" />\n"));
        }
        DataElement::Seq(items) => {
            out.push_str(&format!("{pad}<sequence>\n"));
            for i in items {
                xml_element(i, depth + 1, out);
            }
            out.push_str(&format!("{pad}</sequence>\n"));
        }
    }
}

/// BlueZ `ServiceRecord` XML.
pub fn to_bluez_xml(record: &[Attribute]) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<record>\n");
    for a in record {
        s.push_str(&format!("  <attribute id=\"0x{:04x}\">\n", a.id));
        xml_element(&a.value, 2, &mut s);
        s.push_str("  </attribute>\n");
    }
    s.push_str("</record>\n");
    s
}

/// Platform-neutral model of the property-list objects handed to IOBluetooth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlistValue {
    Int(i64),
    String(String),
    Data(Vec<u8>),
    Array(Vec<PlistValue>),
    Dict(Vec<(String, PlistValue)>),
}

impl PlistValue {
    /// Look up a key in a `Dict`.
    pub fn get(&self, key: &str) -> Option<&PlistValue> {
        match self {
            PlistValue::Dict(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// `DataElementType` values (SDP type descriptors).
pub mod de_type {
    pub const UNSIGNED: i64 = 1;
    pub const UUID: i64 = 3;
    pub const TEXT: i64 = 4;
    pub const BOOL: i64 = 5;
}

fn typed(t: i64, size: Option<i64>, value: PlistValue) -> PlistValue {
    let mut kv = vec![("DataElementType".to_string(), PlistValue::Int(t))];
    if let Some(sz) = size {
        kv.push(("DataElementSize".to_string(), PlistValue::Int(sz)));
    }
    kv.push(("DataElementValue".to_string(), value));
    PlistValue::Dict(kv)
}

fn apple_element(e: &DataElement) -> PlistValue {
    match e {
        DataElement::U8(v) => typed(de_type::UNSIGNED, Some(1), PlistValue::Int(*v as i64)),
        DataElement::U16(v) => typed(de_type::UNSIGNED, Some(2), PlistValue::Int(*v as i64)),
        DataElement::U32(v) => typed(de_type::UNSIGNED, Some(4), PlistValue::Int(*v as i64)),
        DataElement::Bool(v) => typed(de_type::BOOL, Some(1), PlistValue::Int(*v as i64)),
        // A 2-byte NSData is inferred as a 16-bit UUID (big-endian).
        DataElement::Uuid16(v) => PlistValue::Data(v.to_be_bytes().to_vec()),
        DataElement::Text(s) => PlistValue::String(s.clone()),
        // Explicit type: an NSData alone would be taken as a UUID.
        DataElement::Bytes(b) => typed(de_type::TEXT, None, PlistValue::Data(b.clone())),
        DataElement::Seq(items) => PlistValue::Array(items.iter().map(apple_element).collect()),
    }
}

/// Attribute key in Apple's SDP dictionary format (`"0206 - HIDDescriptorList"`).
pub fn apple_key(a: &Attribute) -> String {
    format!("{:04X} - {}", a.id, a.name)
}

/// IOBluetooth service dictionary. No `LocalAttributes` / `Persistent` key:
/// the record is transient and disappears when the process exits.
pub fn to_apple_dict(record: &[Attribute]) -> PlistValue {
    PlistValue::Dict(record.iter().map(|a| (apple_key(a), apple_element(&a.value))).collect())
}

/// Serialise a data element in SDP binary form (Core spec Vol 3 Part B §3),
/// used to sanity-check the record size and structure.
pub fn encode_element(e: &DataElement, out: &mut Vec<u8>) {
    fn var(out: &mut Vec<u8>, t: u8, body: &[u8]) {
        let n = body.len();
        if n <= 0xFF {
            out.extend_from_slice(&[(t << 3) | 5, n as u8]);
        } else if n <= 0xFFFF {
            out.push((t << 3) | 6);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            out.push((t << 3) | 7);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
        out.extend_from_slice(body);
    }
    match e {
        DataElement::U8(v) => out.extend_from_slice(&[0x08, *v]),
        DataElement::U16(v) => {
            out.push(0x09);
            out.extend_from_slice(&v.to_be_bytes());
        }
        DataElement::U32(v) => {
            out.push(0x0A);
            out.extend_from_slice(&v.to_be_bytes());
        }
        DataElement::Bool(v) => out.extend_from_slice(&[0x28, *v as u8]),
        DataElement::Uuid16(v) => {
            out.push(0x19);
            out.extend_from_slice(&v.to_be_bytes());
        }
        DataElement::Text(s) => var(out, 4, s.as_bytes()),
        DataElement::Bytes(b) => var(out, 4, b),
        DataElement::Seq(items) => {
            let mut body = Vec::new();
            for i in items {
                encode_element(i, &mut body);
            }
            var(out, 6, &body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DataElement::*;

    fn find(r: &[Attribute], id: u16) -> &DataElement {
        &r.iter().find(|a| a.id == id).unwrap_or_else(|| panic!("attribute {id:#06x} missing")).value
    }

    #[test]
    fn record_matches_hid_spec() {
        let r = hid_service_record(&HidSdpConfig::default());
        // Sorted, unique IDs.
        assert!(r.windows(2).all(|w| w[0].id < w[1].id));
        assert_eq!(find(&r, 0x0001), &Seq(vec![Uuid16(0x1124)]));
        assert_eq!(
            find(&r, 0x0004),
            &Seq(vec![Seq(vec![Uuid16(0x0100), U16(0x0011)]), Seq(vec![Uuid16(0x0011)])])
        );
        assert_eq!(
            find(&r, 0x000D),
            &Seq(vec![Seq(vec![Seq(vec![Uuid16(0x0100), U16(0x0013)]), Seq(vec![Uuid16(0x0011)])])])
        );
        assert_eq!(find(&r, 0x0009), &Seq(vec![Seq(vec![Uuid16(0x1124), U16(0x0101)])]));
        assert_eq!(find(&r, 0x0201), &U16(0x0111));
        assert_eq!(find(&r, 0x0202), &U8(0xC0));
        assert_eq!(find(&r, 0x0204), &Bool(true)); // VirtualCable
        assert_eq!(find(&r, 0x0205), &Bool(true)); // ReconnectInitiate
        assert_eq!(find(&r, 0x020E), &Bool(true)); // BootDevice
        assert_eq!(find(&r, 0x0206), &Seq(vec![Seq(vec![U8(0x22), Bytes(REPORT_DESCRIPTOR.to_vec())])]));
        assert_eq!(find(&r, 0x0207), &Seq(vec![Seq(vec![U16(0x0409), U16(0x0100)])]));
        // Mandatory HID attributes (HID 1.1 table 5.3): all present.
        for id in [0x0201, 0x0202, 0x0203, 0x0204, 0x0205, 0x0206, 0x0207, 0x020E] {
            find(&r, id);
        }
        // Binary encoding fits comfortably in one SDP response.
        let mut total = 0;
        for a in &r {
            let mut b = Vec::new();
            encode_element(&a.value, &mut b);
            total += 3 + b.len();
        }
        assert!(total < 1024, "record is {total} bytes");
        assert_eq!(CLASS_OF_DEVICE >> 8 & 0x1F, MAJOR_CLASS_PERIPHERAL as u32);
        assert_eq!(CLASS_OF_DEVICE & 0xFF, MINOR_CLASS_KEYBOARD_POINTING as u32);
    }

    #[test]
    fn sdp_binary_encoding() {
        let mut b = Vec::new();
        encode_element(&Seq(vec![Seq(vec![Uuid16(0x0100), U16(0x0011)]), Seq(vec![Uuid16(0x0011)])]), &mut b);
        assert_eq!(b, vec![0x35, 0x0D, 0x35, 0x06, 0x19, 0x01, 0x00, 0x09, 0x00, 0x11, 0x35, 0x03, 0x19, 0x00, 0x11]);
        let mut b = Vec::new();
        encode_element(&Bytes(vec![0xAA; 300]), &mut b);
        assert_eq!(&b[..3], &[0x26, 0x01, 0x2C]);
    }

    #[test]
    fn bluez_xml_contents() {
        let cfg = HidSdpConfig { service_name: "A&B <kbd>".into(), ..HidSdpConfig::default() };
        let x = to_bluez_xml(&hid_service_record(&cfg));
        assert!(x.starts_with("<?xml"));
        assert!(x.contains("<attribute id=\"0x0001\">\n    <sequence>\n      <uuid value=\"0x1124\" />"));
        assert!(x.contains("<uuid value=\"0x0100\" />\n        <uint16 value=\"0x0011\" />"));
        assert!(x.contains("<uint16 value=\"0x0013\" />"));
        assert!(x.contains("<attribute id=\"0x0202\">\n    <uint8 value=\"0xc0\" />"));
        assert!(x.contains("<attribute id=\"0x020e\">\n    <boolean value=\"true\" />"));
        assert!(x.contains("<uint8 value=\"0x22\" />\n        <text encoding=\"hex\" value=\"05010906a101850105"));
        let hex: String = REPORT_DESCRIPTOR.iter().map(|b| format!("{b:02x}")).collect();
        assert!(x.contains(&hex));
        assert!(x.contains("value=\"A&amp;B &lt;kbd&gt;\""));
        assert_eq!(x.matches("<sequence>").count(), x.matches("</sequence>").count());
        assert_eq!(x.matches("<attribute ").count(), hid_service_record(&cfg).len());
    }

    #[test]
    fn apple_dict_contents() {
        let d = to_apple_dict(&hid_service_record(&HidSdpConfig::default()));
        assert_eq!(d.get("0001 - ServiceClassIDList"), Some(&PlistValue::Array(vec![PlistValue::Data(vec![0x11, 0x24])])));
        let PlistValue::Array(pdl) = d.get("0004 - ProtocolDescriptorList").unwrap() else { panic!() };
        let PlistValue::Array(l2) = &pdl[0] else { panic!() };
        assert_eq!(l2[0], PlistValue::Data(vec![0x01, 0x00]));
        assert_eq!(l2[1].get("DataElementType"), Some(&PlistValue::Int(1)));
        assert_eq!(l2[1].get("DataElementSize"), Some(&PlistValue::Int(2)));
        assert_eq!(l2[1].get("DataElementValue"), Some(&PlistValue::Int(0x11)));
        let PlistValue::Array(add) = d.get("000D - AdditionalProtocolDescriptorLists").unwrap() else { panic!() };
        let PlistValue::Array(inner) = &add[0] else { panic!() };
        let PlistValue::Array(l2) = &inner[0] else { panic!() };
        assert_eq!(l2[1].get("DataElementValue"), Some(&PlistValue::Int(0x13)));
        let boot = d.get("020E - HIDBootDevice").unwrap();
        assert_eq!(boot.get("DataElementType"), Some(&PlistValue::Int(5)));
        assert_eq!(boot.get("DataElementValue"), Some(&PlistValue::Int(1)));
        let PlistValue::Array(dl) = d.get("0206 - HIDDescriptorList").unwrap() else { panic!() };
        let PlistValue::Array(desc) = &dl[0] else { panic!() };
        assert_eq!(desc[0].get("DataElementValue"), Some(&PlistValue::Int(0x22)));
        assert_eq!(desc[1].get("DataElementType"), Some(&PlistValue::Int(4)));
        assert_eq!(desc[1].get("DataElementValue"), Some(&PlistValue::Data(REPORT_DESCRIPTOR.to_vec())));
        assert_eq!(d.get("0100 - ServiceName"), Some(&PlistValue::String("AetherLink Keyboard".into())));
        assert!(d.get("LocalAttributes").is_none(), "record must stay transient");
    }
}
