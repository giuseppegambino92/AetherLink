//! Bluetooth HID Profile (HIDP 1.1) transaction layer for the *device* role,
//! shared by the IOBluetooth (macOS) and BlueZ (Linux) Classic backends.
//!
//! Two L2CAP channels carry HIDP: **control** (PSM 0x11) for host requests
//! (GET_REPORT, SET_REPORT, GET/SET_PROTOCOL, GET/SET_IDLE, HID_CONTROL) and
//! **interrupt** (PSM 0x13) for `DATA` input reports (`0xA1 id payload…`)
//! and host output reports (`0xA2 id payload…`, keyboard LEDs).
//!
//! [`HidpDevice`] is a pure state machine: feed it host messages, it returns
//! the bytes to write back and tracks protocol mode / LEDs / last reports.

use crate::hid_report::{
    HidReport, KeyboardLeds, ABS_POINTER_REPORT_LEN, CONSUMER_REPORT_LEN, KEYBOARD_OUTPUT_LEN, KEYBOARD_REPORT_LEN,
    MOUSE_REPORT_LEN, REPORT_ID_ABS_POINTER, REPORT_ID_CONSUMER, REPORT_ID_KEYBOARD, REPORT_ID_MOUSE,
};

pub const PSM_HID_CONTROL: u16 = 0x0011;
pub const PSM_HID_INTERRUPT: u16 = 0x0013;

/// HIDP transaction types (high nibble of the header byte).
pub mod trans {
    pub const HANDSHAKE: u8 = 0x0;
    pub const HID_CONTROL: u8 = 0x1;
    pub const GET_REPORT: u8 = 0x4;
    pub const SET_REPORT: u8 = 0x5;
    pub const GET_PROTOCOL: u8 = 0x6;
    pub const SET_PROTOCOL: u8 = 0x7;
    pub const GET_IDLE: u8 = 0x8;
    pub const SET_IDLE: u8 = 0x9;
    pub const DATA: u8 = 0xA;
    pub const DATC: u8 = 0xB;
}

/// HANDSHAKE result codes (low nibble).
pub mod handshake {
    pub const SUCCESSFUL: u8 = 0x0;
    pub const NOT_READY: u8 = 0x1;
    pub const ERR_INVALID_REPORT_ID: u8 = 0x2;
    pub const ERR_UNSUPPORTED_REQUEST: u8 = 0x3;
    pub const ERR_INVALID_PARAMETER: u8 = 0x4;
    pub const ERR_UNKNOWN: u8 = 0xE;
    pub const ERR_FATAL: u8 = 0xF;
}

/// HID_CONTROL operations (low nibble).
pub mod control_op {
    pub const NOP: u8 = 0x0;
    pub const HARD_RESET: u8 = 0x1;
    pub const SOFT_RESET: u8 = 0x2;
    pub const SUSPEND: u8 = 0x3;
    pub const EXIT_SUSPEND: u8 = 0x4;
    pub const VIRTUAL_CABLE_UNPLUG: u8 = 0x5;
}

/// Report type (low two bits of GET/SET_REPORT and DATA parameters).
pub mod rtype {
    pub const OTHER: u8 = 0;
    pub const INPUT: u8 = 1;
    pub const OUTPUT: u8 = 2;
    pub const FEATURE: u8 = 3;
}

/// `DATA | INPUT` header for interrupt-channel input reports.
pub const DATA_INPUT: u8 = (trans::DATA << 4) | rtype::INPUT;
/// `DATA | OUTPUT` header (host → device).
pub const DATA_OUTPUT: u8 = (trans::DATA << 4) | rtype::OUTPUT;

pub const fn header(t: u8, param: u8) -> u8 {
    (t << 4) | (param & 0x0F)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtocolMode {
    /// Boot protocol (BIOS-style fixed reports; keyboard ID 1, mouse ID 2).
    Boot,
    /// Report protocol with our report descriptor (default after connect).
    #[default]
    Report,
}

/// Something the host did that the owner of the link must act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEvent {
    /// The host updated the keyboard LEDs.
    Leds(KeyboardLeds),
    /// VIRTUAL_CABLE_UNPLUG: the host wants the bond removed and the link closed.
    VirtualCableUnplug,
    Suspend,
    ExitSuspend,
    ProtocolChanged(ProtocolMode),
}

/// Result of handling one host message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    /// Bytes to write back on the *same* channel (control), if any.
    pub response: Option<Vec<u8>>,
    pub event: Option<HostEvent>,
}

/// Device-side HIDP state.
#[derive(Debug, Clone)]
pub struct HidpDevice {
    pub protocol: ProtocolMode,
    pub leds: KeyboardLeds,
    pub suspended: bool,
    idle_rate: u8,
    last_keyboard: [u8; KEYBOARD_REPORT_LEN],
    last_mouse_buttons: u8,
    last_abs: [u8; ABS_POINTER_REPORT_LEN],
    last_consumer: [u8; CONSUMER_REPORT_LEN],
}

impl Default for HidpDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl HidpDevice {
    pub fn new() -> Self {
        Self {
            protocol: ProtocolMode::Report,
            leds: KeyboardLeds::default(),
            suspended: false,
            idle_rate: 0,
            last_keyboard: [0; KEYBOARD_REPORT_LEN],
            last_mouse_buttons: 0,
            last_abs: [0; ABS_POINTER_REPORT_LEN],
            last_consumer: [0; CONSUMER_REPORT_LEN],
        }
    }

    /// Reset per-connection state (protocol back to Report mode).
    pub fn on_connect(&mut self) {
        *self = Self::new();
    }

    fn handshake(code: u8) -> Reply {
        Reply { response: Some(vec![header(trans::HANDSHAKE, code)]), event: None }
    }

    fn current_input(&self, id: u8) -> Option<Vec<u8>> {
        Some(match (self.protocol, id) {
            (_, REPORT_ID_KEYBOARD) => self.last_keyboard.to_vec(),
            (ProtocolMode::Boot, REPORT_ID_MOUSE) => vec![self.last_mouse_buttons, 0, 0],
            (ProtocolMode::Report, REPORT_ID_MOUSE) => {
                let mut v = vec![0u8; MOUSE_REPORT_LEN];
                v[0] = self.last_mouse_buttons;
                v
            }
            (ProtocolMode::Report, REPORT_ID_ABS_POINTER) => self.last_abs.to_vec(),
            (ProtocolMode::Report, REPORT_ID_CONSUMER) => self.last_consumer.to_vec(),
            _ => return None,
        })
    }

    /// Handle a message received on the **control** channel.
    pub fn handle_control(&mut self, msg: &[u8]) -> Reply {
        let Some(&h) = msg.first() else {
            return Reply::default();
        };
        let (t, p) = (h >> 4, h & 0x0F);
        match t {
            trans::HID_CONTROL => match p {
                control_op::VIRTUAL_CABLE_UNPLUG => Reply { response: None, event: Some(HostEvent::VirtualCableUnplug) },
                control_op::SUSPEND => {
                    self.suspended = true;
                    Reply { response: None, event: Some(HostEvent::Suspend) }
                }
                control_op::EXIT_SUSPEND => {
                    self.suspended = false;
                    Reply { response: None, event: Some(HostEvent::ExitSuspend) }
                }
                control_op::NOP | control_op::HARD_RESET | control_op::SOFT_RESET => {
                    if p != control_op::NOP {
                        let leds = self.leds;
                        self.on_connect();
                        self.leds = leds;
                    }
                    Reply::default()
                }
                _ => Self::handshake(handshake::ERR_INVALID_PARAMETER),
            },
            trans::GET_REPORT => {
                let report_type = p & 0x03;
                let has_size = p & 0x08 != 0;
                let Some(&id) = msg.get(1) else {
                    return Self::handshake(handshake::ERR_INVALID_PARAMETER);
                };
                let max = if has_size {
                    match msg.get(2..4) {
                        Some(b) => Some(u16::from_le_bytes([b[0], b[1]]) as usize),
                        None => return Self::handshake(handshake::ERR_INVALID_PARAMETER),
                    }
                } else {
                    None
                };
                let payload = match report_type {
                    rtype::INPUT => self.current_input(id),
                    rtype::OUTPUT if id == REPORT_ID_KEYBOARD => Some(vec![leds_to_byte(self.leds)]),
                    rtype::FEATURE => return Self::handshake(handshake::ERR_UNSUPPORTED_REQUEST),
                    _ => None,
                };
                let Some(payload) = payload else {
                    return Self::handshake(handshake::ERR_INVALID_REPORT_ID);
                };
                let mut v = vec![header(trans::DATA, report_type), id];
                v.extend_from_slice(&payload);
                // BufferSize counts the payload including the report ID.
                if let Some(max) = max {
                    v.truncate(1 + max.max(1));
                }
                Reply { response: Some(v), event: None }
            }
            trans::SET_REPORT => {
                let report_type = p & 0x03;
                match (report_type, msg.get(1), msg.get(2)) {
                    (rtype::OUTPUT, Some(&REPORT_ID_KEYBOARD), Some(&b)) if msg.len() - 2 >= KEYBOARD_OUTPUT_LEN => {
                        self.leds = KeyboardLeds::from_byte(b);
                        Reply {
                            response: Some(vec![header(trans::HANDSHAKE, handshake::SUCCESSFUL)]),
                            event: Some(HostEvent::Leds(self.leds)),
                        }
                    }
                    (rtype::OUTPUT | rtype::INPUT, Some(_), _) => Self::handshake(handshake::ERR_INVALID_REPORT_ID),
                    (rtype::FEATURE, _, _) => Self::handshake(handshake::ERR_UNSUPPORTED_REQUEST),
                    _ => Self::handshake(handshake::ERR_INVALID_PARAMETER),
                }
            }
            trans::GET_PROTOCOL => Reply {
                response: Some(vec![
                    header(trans::DATA, rtype::OTHER),
                    match self.protocol {
                        ProtocolMode::Boot => 0,
                        ProtocolMode::Report => 1,
                    },
                ]),
                event: None,
            },
            trans::SET_PROTOCOL => {
                let mode = if p & 0x01 == 0 { ProtocolMode::Boot } else { ProtocolMode::Report };
                let changed = mode != self.protocol;
                self.protocol = mode;
                Reply {
                    response: Some(vec![header(trans::HANDSHAKE, handshake::SUCCESSFUL)]),
                    event: changed.then_some(HostEvent::ProtocolChanged(mode)),
                }
            }
            trans::GET_IDLE => Reply { response: Some(vec![header(trans::DATA, rtype::OTHER), self.idle_rate]), event: None },
            trans::SET_IDLE => {
                self.idle_rate = msg.get(1).copied().unwrap_or(0);
                Self::handshake(handshake::SUCCESSFUL)
            }
            // DATA on control (HIDP 1.0 hosts) — treat an output report like SET_REPORT.
            trans::DATA => self.handle_interrupt(msg).map(|e| Reply { response: None, event: Some(e) }).unwrap_or_default(),
            trans::HANDSHAKE => Reply::default(),
            _ => Self::handshake(handshake::ERR_UNSUPPORTED_REQUEST),
        }
    }

    /// Handle a message received on the **interrupt** channel (host output reports).
    pub fn handle_interrupt(&mut self, msg: &[u8]) -> Option<HostEvent> {
        match msg {
            [DATA_OUTPUT, REPORT_ID_KEYBOARD, b, ..] => {
                self.leds = KeyboardLeds::from_byte(*b);
                Some(HostEvent::Leds(self.leds))
            }
            _ => None,
        }
    }

    /// Encode a report for the interrupt channel in the current protocol
    /// mode. Returns zero or more HIDP packets (boot mode splits large
    /// relative motion into ±127 steps and drops reports that have no boot
    /// equivalent).
    pub fn encode_input(&mut self, r: &HidReport) -> Vec<Vec<u8>> {
        match r.id {
            REPORT_ID_KEYBOARD if r.data.len() == KEYBOARD_REPORT_LEN => {
                self.last_keyboard.copy_from_slice(&r.data);
            }
            REPORT_ID_MOUSE if r.data.len() == MOUSE_REPORT_LEN => self.last_mouse_buttons = r.data[0],
            REPORT_ID_ABS_POINTER if r.data.len() == ABS_POINTER_REPORT_LEN => {
                self.last_abs.copy_from_slice(&r.data);
                self.last_mouse_buttons = r.data[0];
            }
            REPORT_ID_CONSUMER if r.data.len() == CONSUMER_REPORT_LEN => self.last_consumer.copy_from_slice(&r.data),
            _ => return Vec::new(),
        }
        let pkt = |id: u8, data: &[u8]| {
            let mut v = Vec::with_capacity(data.len() + 2);
            v.push(DATA_INPUT);
            v.push(id);
            v.extend_from_slice(data);
            v
        };
        match self.protocol {
            ProtocolMode::Report => vec![pkt(r.id, &r.data)],
            ProtocolMode::Boot => match r.id {
                REPORT_ID_KEYBOARD => vec![pkt(REPORT_ID_KEYBOARD, &r.data)],
                REPORT_ID_MOUSE => {
                    // Boot mouse: buttons, dx:i8, dy:i8, wheel:i8.
                    let buttons = r.data[0] & 0x07;
                    let mut dx = i16::from_le_bytes([r.data[1], r.data[2]]) as i32;
                    let mut dy = i16::from_le_bytes([r.data[3], r.data[4]]) as i32;
                    let mut wheel = r.data[5] as i8;
                    let mut out = Vec::new();
                    loop {
                        let sx = dx.clamp(-127, 127);
                        let sy = dy.clamp(-127, 127);
                        out.push(pkt(REPORT_ID_MOUSE, &[buttons, sx as i8 as u8, sy as i8 as u8, wheel as u8]));
                        wheel = 0;
                        dx -= sx;
                        dy -= sy;
                        if dx == 0 && dy == 0 {
                            break;
                        }
                    }
                    out
                }
                REPORT_ID_ABS_POINTER => {
                    // No absolute axes in boot mode; keep buttons in sync only.
                    vec![pkt(REPORT_ID_MOUSE, &[r.data[0] & 0x07, 0, 0, 0])]
                }
                _ => Vec::new(),
            },
        }
    }
}

pub fn leds_to_byte(l: KeyboardLeds) -> u8 {
    (l.num_lock as u8) | (l.caps_lock as u8) << 1 | (l.scroll_lock as u8) << 2 | (l.compose as u8) << 3 | (l.kana as u8) << 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_constants() {
        assert_eq!(DATA_INPUT, 0xA1);
        assert_eq!(DATA_OUTPUT, 0xA2);
        assert_eq!(header(trans::HANDSHAKE, handshake::ERR_UNSUPPORTED_REQUEST), 0x03);
        assert_eq!(header(trans::SET_PROTOCOL, 1), 0x71);
    }

    #[test]
    fn report_mode_input_packets() {
        let mut d = HidpDevice::new();
        let r = HidReport { id: 1, data: vec![0x02, 0, 0x04, 0, 0, 0, 0, 0] };
        assert_eq!(d.encode_input(&r), vec![vec![0xA1, 1, 0x02, 0, 0x04, 0, 0, 0, 0, 0]]);
        let c = HidReport { id: 4, data: vec![0xE9, 0x00] };
        assert_eq!(d.encode_input(&c), vec![vec![0xA1, 4, 0xE9, 0]]);
        // Malformed reports are ignored.
        assert!(d.encode_input(&HidReport { id: 1, data: vec![0; 3] }).is_empty());
        assert!(d.encode_input(&HidReport { id: 9, data: vec![] }).is_empty());
    }

    #[test]
    fn protocol_switch_and_boot_encoding() {
        let mut d = HidpDevice::new();
        let rep = d.handle_control(&[0x60]);
        assert_eq!(rep.response, Some(vec![0xA0, 1]));
        let rep = d.handle_control(&[0x70]); // SET_PROTOCOL boot
        assert_eq!(rep.response, Some(vec![0x00]));
        assert_eq!(rep.event, Some(HostEvent::ProtocolChanged(ProtocolMode::Boot)));
        assert_eq!(d.handle_control(&[0x60]).response, Some(vec![0xA0, 0]));
        // Boot mouse: 300 px right splits into 127 + 127 + 46.
        let m = HidReport { id: 2, data: [vec![0x01], 300i16.to_le_bytes().to_vec(), (-5i16).to_le_bytes().to_vec(), vec![2, 0]].concat() };
        let pk = d.encode_input(&m);
        assert_eq!(pk.len(), 3);
        assert_eq!(pk[0], vec![0xA1, 2, 0x01, 127, (-5i8) as u8, 2]);
        assert_eq!(pk[1], vec![0xA1, 2, 0x01, 127, 0, 0]);
        assert_eq!(pk[2], vec![0xA1, 2, 0x01, 46, 0, 0]);
        // Consumer has no boot form; abs pointer only keeps buttons.
        assert!(d.encode_input(&HidReport { id: 4, data: vec![1, 0] }).is_empty());
        assert_eq!(d.encode_input(&HidReport { id: 3, data: vec![1, 0, 0, 0, 0] }), vec![vec![0xA1, 2, 1, 0, 0, 0]]);
        // Back to report mode.
        let rep = d.handle_control(&[0x71]);
        assert_eq!(rep.event, Some(HostEvent::ProtocolChanged(ProtocolMode::Report)));
    }

    #[test]
    fn get_report_returns_last_state() {
        let mut d = HidpDevice::new();
        d.encode_input(&HidReport { id: 1, data: vec![0, 0, 0x05, 0, 0, 0, 0, 0] });
        // GET_REPORT input, id 1.
        assert_eq!(d.handle_control(&[0x41, 1]).response, Some(vec![0xA1, 1, 0, 0, 0x05, 0, 0, 0, 0, 0]));
        // With BufferSize = 4 (id + 3 bytes).
        assert_eq!(d.handle_control(&[0x49, 1, 4, 0]).response, Some(vec![0xA1, 1, 0, 0, 0x05]));
        // Unknown id / feature / missing id.
        assert_eq!(d.handle_control(&[0x41, 9]).response, Some(vec![0x02]));
        assert_eq!(d.handle_control(&[0x43, 1]).response, Some(vec![0x03]));
        assert_eq!(d.handle_control(&[0x41]).response, Some(vec![0x04]));
        // Output report (LEDs).
        assert_eq!(d.handle_control(&[0x42, 1]).response, Some(vec![0xA2, 1, 0]));
    }

    #[test]
    fn leds_via_set_report_and_interrupt() {
        let mut d = HidpDevice::new();
        let rep = d.handle_control(&[0x52, 1, 0x02]);
        assert_eq!(rep.response, Some(vec![0x00]));
        assert!(matches!(rep.event, Some(HostEvent::Leds(l)) if l.caps_lock));
        assert_eq!(d.handle_control(&[0x52, 7, 0x02]).response, Some(vec![0x02]));
        assert_eq!(d.handle_control(&[0x53, 1, 0]).response, Some(vec![0x03]));
        let ev = d.handle_interrupt(&[0xA2, 1, 0x01]);
        assert!(matches!(ev, Some(HostEvent::Leds(l)) if l.num_lock && !l.caps_lock));
        assert_eq!(leds_to_byte(d.leds), 0x01);
        assert_eq!(d.handle_interrupt(&[0xA1, 1, 0]), None);
    }

    #[test]
    fn control_ops_idle_and_unsupported() {
        let mut d = HidpDevice::new();
        assert_eq!(d.handle_control(&[0x15]).event, Some(HostEvent::VirtualCableUnplug));
        assert_eq!(d.handle_control(&[0x13]).event, Some(HostEvent::Suspend));
        assert!(d.suspended);
        assert_eq!(d.handle_control(&[0x14]).event, Some(HostEvent::ExitSuspend));
        assert_eq!(d.handle_control(&[0x90, 0x10]).response, Some(vec![0x00]));
        assert_eq!(d.handle_control(&[0x80]).response, Some(vec![0xA0, 0x10]));
        assert_eq!(d.handle_control(&[0xB0]).response, Some(vec![0x03]));
        assert_eq!(d.handle_control(&[0x1F]).response, Some(vec![0x04]));
        assert_eq!(d.handle_control(&[0x00]), Reply::default());
        assert_eq!(d.handle_control(&[]), Reply::default());
    }
}
