//! HID report descriptor and a stateful [`InputEvent`] → HID report
//! converter, shared by BLE HID-over-GATT and USB gadget emulation.
//!
//! Report layout (all little-endian):
//!
//! | ID | collection                         | payload (without ID)                                   |
//! |----|------------------------------------|--------------------------------------------------------|
//! | 1  | Keyboard (boot-compatible, 6KRO)   | `[modifiers, 0, k1..k6]` (8 B); output: LED byte       |
//! | 2  | Mouse, relative                    | `[buttons(5b), x:i16, y:i16, wheel:i8, ac_pan:i8]` (7 B) |
//! | 3  | Absolute pointer (tablet style)    | `[buttons(5b), x:u16, y:u16]` 0..=32767 (5 B)          |
//! | 4  | Consumer control                   | `[usage:u16]` (2 B)                                    |
//!
//! The absolute pointer is a Generic-Desktop Mouse with absolute axes (the
//! "USB tablet" shape that Windows, Linux, macOS and Android all accept
//! without drivers) rather than a Digitizer-page pen, which needs per-OS
//! quirks. Hosts that ignore absolute pointers (iOS/iPadOS) are driven with
//! [`PointerMode::Relative`], which converts absolute positions into
//! accumulated relative motion after homing the cursor to the top-left corner.

use crate::keymap::ascii_to_hid_us;
use crate::state::{button_bit, plan_key, Accumulator, ModifierState};
use crate::{InputError, Result};
use aether_protocol::{GesturePhase, HidUsage, InputEvent, Modifiers, ScrollUnit, SystemAction, TouchPhase};

pub const REPORT_ID_KEYBOARD: u8 = 1;
pub const REPORT_ID_MOUSE: u8 = 2;
pub const REPORT_ID_ABS_POINTER: u8 = 3;
pub const REPORT_ID_CONSUMER: u8 = 4;

pub const KEYBOARD_REPORT_LEN: usize = 8;
pub const MOUSE_REPORT_LEN: usize = 7;
pub const ABS_POINTER_REPORT_LEN: usize = 5;
pub const CONSUMER_REPORT_LEN: usize = 2;
/// Output report (host → device): LED bitmap.
pub const KEYBOARD_OUTPUT_LEN: usize = 1;

/// Logical range of the absolute pointer axes.
pub const ABS_MAX: u16 = 32767;

/// Combined report descriptor (keyboard + mouse + absolute pointer + consumer).
#[rustfmt::skip]
pub const REPORT_DESCRIPTOR: &[u8] = &[
    // ---- Report 1: keyboard ----
    0x05, 0x01,       // Usage Page (Generic Desktop)
    0x09, 0x06,       // Usage (Keyboard)
    0xA1, 0x01,       // Collection (Application)
    0x85, 0x01,       //   Report ID (1)
    0x05, 0x07,       //   Usage Page (Keyboard/Keypad)
    0x19, 0xE0,       //   Usage Minimum (Left Control)
    0x29, 0xE7,       //   Usage Maximum (Right GUI)
    0x15, 0x00,       //   Logical Minimum (0)
    0x25, 0x01,       //   Logical Maximum (1)
    0x75, 0x01,       //   Report Size (1)
    0x95, 0x08,       //   Report Count (8)
    0x81, 0x02,       //   Input (Data, Var, Abs)          ; modifier byte
    0x95, 0x01,       //   Report Count (1)
    0x75, 0x08,       //   Report Size (8)
    0x81, 0x01,       //   Input (Const)                   ; reserved byte
    0x95, 0x05,       //   Report Count (5)
    0x75, 0x01,       //   Report Size (1)
    0x05, 0x08,       //   Usage Page (LEDs)
    0x19, 0x01,       //   Usage Minimum (Num Lock)
    0x29, 0x05,       //   Usage Maximum (Kana)
    0x91, 0x02,       //   Output (Data, Var, Abs)         ; LED report
    0x95, 0x01,       //   Report Count (1)
    0x75, 0x03,       //   Report Size (3)
    0x91, 0x01,       //   Output (Const)                  ; LED padding
    0x95, 0x06,       //   Report Count (6)
    0x75, 0x08,       //   Report Size (8)
    0x15, 0x00,       //   Logical Minimum (0)
    0x26, 0xE7, 0x00, //   Logical Maximum (231)
    0x05, 0x07,       //   Usage Page (Keyboard/Keypad)
    0x19, 0x00,       //   Usage Minimum (0)
    0x29, 0xE7,       //   Usage Maximum (231)
    0x81, 0x00,       //   Input (Data, Array, Abs)        ; 6 key slots
    0xC0,             // End Collection

    // ---- Report 2: relative mouse ----
    0x05, 0x01,       // Usage Page (Generic Desktop)
    0x09, 0x02,       // Usage (Mouse)
    0xA1, 0x01,       // Collection (Application)
    0x85, 0x02,       //   Report ID (2)
    0x09, 0x01,       //   Usage (Pointer)
    0xA1, 0x00,       //   Collection (Physical)
    0x05, 0x09,       //     Usage Page (Button)
    0x19, 0x01,       //     Usage Minimum (1)
    0x29, 0x05,       //     Usage Maximum (5)
    0x15, 0x00,       //     Logical Minimum (0)
    0x25, 0x01,       //     Logical Maximum (1)
    0x95, 0x05,       //     Report Count (5)
    0x75, 0x01,       //     Report Size (1)
    0x81, 0x02,       //     Input (Data, Var, Abs)        ; buttons
    0x95, 0x01,       //     Report Count (1)
    0x75, 0x03,       //     Report Size (3)
    0x81, 0x01,       //     Input (Const)                 ; padding
    0x05, 0x01,       //     Usage Page (Generic Desktop)
    0x09, 0x30,       //     Usage (X)
    0x09, 0x31,       //     Usage (Y)
    0x16, 0x01, 0x80, //     Logical Minimum (-32767)
    0x26, 0xFF, 0x7F, //     Logical Maximum (32767)
    0x75, 0x10,       //     Report Size (16)
    0x95, 0x02,       //     Report Count (2)
    0x81, 0x06,       //     Input (Data, Var, Rel)        ; X, Y
    0x09, 0x38,       //     Usage (Wheel)
    0x15, 0x81,       //     Logical Minimum (-127)
    0x25, 0x7F,       //     Logical Maximum (127)
    0x75, 0x08,       //     Report Size (8)
    0x95, 0x01,       //     Report Count (1)
    0x81, 0x06,       //     Input (Data, Var, Rel)        ; vertical wheel
    0x05, 0x0C,       //     Usage Page (Consumer)
    0x0A, 0x38, 0x02, //     Usage (AC Pan)
    0x15, 0x81,       //     Logical Minimum (-127)
    0x25, 0x7F,       //     Logical Maximum (127)
    0x75, 0x08,       //     Report Size (8)
    0x95, 0x01,       //     Report Count (1)
    0x81, 0x06,       //     Input (Data, Var, Rel)        ; horizontal pan
    0xC0,             //   End Collection
    0xC0,             // End Collection

    // ---- Report 3: absolute pointer ----
    0x05, 0x01,       // Usage Page (Generic Desktop)
    0x09, 0x02,       // Usage (Mouse)
    0xA1, 0x01,       // Collection (Application)
    0x85, 0x03,       //   Report ID (3)
    0x09, 0x01,       //   Usage (Pointer)
    0xA1, 0x00,       //   Collection (Physical)
    0x05, 0x09,       //     Usage Page (Button)
    0x19, 0x01,       //     Usage Minimum (1)
    0x29, 0x05,       //     Usage Maximum (5)
    0x15, 0x00,       //     Logical Minimum (0)
    0x25, 0x01,       //     Logical Maximum (1)
    0x95, 0x05,       //     Report Count (5)
    0x75, 0x01,       //     Report Size (1)
    0x81, 0x02,       //     Input (Data, Var, Abs)
    0x95, 0x01,       //     Report Count (1)
    0x75, 0x03,       //     Report Size (3)
    0x81, 0x01,       //     Input (Const)
    0x05, 0x01,       //     Usage Page (Generic Desktop)
    0x09, 0x30,       //     Usage (X)
    0x09, 0x31,       //     Usage (Y)
    0x15, 0x00,       //     Logical Minimum (0)
    0x26, 0xFF, 0x7F, //     Logical Maximum (32767)
    0x75, 0x10,       //     Report Size (16)
    0x95, 0x02,       //     Report Count (2)
    0x81, 0x02,       //     Input (Data, Var, Abs)        ; X, Y
    0xC0,             //   End Collection
    0xC0,             // End Collection

    // ---- Report 4: consumer control ----
    0x05, 0x0C,       // Usage Page (Consumer)
    0x09, 0x01,       // Usage (Consumer Control)
    0xA1, 0x01,       // Collection (Application)
    0x85, 0x04,       //   Report ID (4)
    0x15, 0x00,       //   Logical Minimum (0)
    0x26, 0xFF, 0x03, //   Logical Maximum (1023)
    0x19, 0x00,       //   Usage Minimum (0)
    0x2A, 0xFF, 0x03, //   Usage Maximum (1023)
    0x75, 0x10,       //   Report Size (16)
    0x95, 0x01,       //   Report Count (1)
    0x81, 0x00,       //   Input (Data, Array, Abs)
    0xC0,             // End Collection
];

/// Consumer-page usages used for system actions.
pub mod consumer {
    pub const AL_TERMINAL_LOCK: u16 = 0x019E;
    pub const AC_HOME: u16 = 0x0223;
    pub const AC_BACK: u16 = 0x0224;
    /// "AC Desktop Show All Windows" — Android maps it to APP_SWITCH, macOS to Mission Control.
    pub const AC_SHOW_ALL_WINDOWS: u16 = 0x029F;
}

/// A single input report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidReport {
    pub id: u8,
    /// Payload without the report ID (what a BLE Report characteristic carries).
    pub data: Vec<u8>,
}

impl HidReport {
    /// USB interrupt-endpoint form: report ID followed by the payload.
    pub fn to_bytes_with_id(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.data.len() + 1);
        v.push(self.id);
        v.extend_from_slice(&self.data);
        v
    }
}

/// How `InputEvent::MouseMove` (normalized absolute) is sent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerMode {
    /// Report 3 with absolute coordinates.
    Absolute,
    /// Report 2 relative motion, assuming the host cursor space is
    /// `width × height` units at 1:1 (disable pointer acceleration on the host
    /// for exact tracking). The first move homes the cursor to (0, 0).
    Relative { width: u32, height: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HidConfig {
    pub pointer: PointerMode,
    /// Pixels of `ScrollUnit::Pixel` scrolling per wheel detent.
    pub pixels_per_detent: f32,
}

impl Default for HidConfig {
    fn default() -> Self {
        Self { pointer: PointerMode::Absolute, pixels_per_detent: 40.0 }
    }
}

/// Stateful converter. Tracks pressed keys, modifiers and buttons so every
/// emitted report describes the complete device state.
#[derive(Debug, Clone)]
pub struct HidReportBuilder {
    cfg: HidConfig,
    mods: ModifierState,
    /// Pressed non-modifier keys in press order.
    keys: Vec<u8>,
    buttons: u8,
    /// Last absolute position (0..=ABS_MAX) for report 3.
    abs: (u16, u16),
    /// Estimated host cursor position (relative mode), `None` until homed.
    est: Option<(f64, f64)>,
    acc_x: Accumulator,
    acc_y: Accumulator,
    wheel_acc: Accumulator,
    pan_acc: Accumulator,
    /// Which pointer report carried the last motion (button reports follow it).
    last_pointer_abs: bool,
}

impl HidReportBuilder {
    pub fn new(cfg: HidConfig) -> Self {
        Self {
            cfg,
            mods: ModifierState::default(),
            keys: Vec::new(),
            buttons: 0,
            abs: (0, 0),
            est: None,
            acc_x: Accumulator::default(),
            acc_y: Accumulator::default(),
            wheel_acc: Accumulator::default(),
            pan_acc: Accumulator::default(),
            last_pointer_abs: matches!(cfg.pointer, PointerMode::Absolute),
        }
    }

    pub fn config(&self) -> HidConfig {
        self.cfg
    }

    /// Forget the relative-mode cursor estimate (e.g. after reconnect) so the
    /// next absolute move homes again.
    pub fn rehome(&mut self) {
        self.est = None;
    }

    pub fn keyboard_report(&self) -> HidReport {
        let mut d = vec![0u8; KEYBOARD_REPORT_LEN];
        d[0] = self.mods.0;
        if self.keys.len() > 6 {
            d[2..8].fill(0x01); // ErrorRollOver
        } else {
            d[2..2 + self.keys.len()].copy_from_slice(&self.keys);
        }
        HidReport { id: REPORT_ID_KEYBOARD, data: d }
    }

    fn mouse_report(&self, dx: i16, dy: i16, wheel: i8, pan: i8) -> HidReport {
        let mut d = Vec::with_capacity(MOUSE_REPORT_LEN);
        d.push(self.buttons);
        d.extend_from_slice(&dx.to_le_bytes());
        d.extend_from_slice(&dy.to_le_bytes());
        d.push(wheel as u8);
        d.push(pan as u8);
        HidReport { id: REPORT_ID_MOUSE, data: d }
    }

    fn abs_report(&self) -> HidReport {
        let mut d = Vec::with_capacity(ABS_POINTER_REPORT_LEN);
        d.push(self.buttons);
        d.extend_from_slice(&self.abs.0.to_le_bytes());
        d.extend_from_slice(&self.abs.1.to_le_bytes());
        HidReport { id: REPORT_ID_ABS_POINTER, data: d }
    }

    fn consumer_report(usage: u16) -> HidReport {
        HidReport { id: REPORT_ID_CONSUMER, data: usage.to_le_bytes().to_vec() }
    }

    fn pointer_state_report(&self) -> HidReport {
        if self.last_pointer_abs { self.abs_report() } else { self.mouse_report(0, 0, 0, 0) }
    }

    /// Emit relative motion, split into i16-sized reports.
    fn relative(&mut self, out: &mut Vec<HidReport>, dx: f64, dy: f64) {
        let mut ix = self.acc_x.take(dx);
        let mut iy = self.acc_y.take(dy);
        while ix != 0 || iy != 0 {
            let sx = ix.clamp(-32767, 32767);
            let sy = iy.clamp(-32767, 32767);
            out.push(self.mouse_report(sx as i16, sy as i16, 0, 0));
            ix -= sx;
            iy -= sy;
        }
        self.last_pointer_abs = false;
    }

    fn move_abs(&mut self, out: &mut Vec<HidReport>, nx: f32, ny: f32) {
        let nx = if nx.is_finite() { nx.clamp(0.0, 1.0) } else { 0.0 };
        let ny = if ny.is_finite() { ny.clamp(0.0, 1.0) } else { 0.0 };
        match self.cfg.pointer {
            PointerMode::Absolute => {
                self.abs = ((nx * ABS_MAX as f32).round() as u16, (ny * ABS_MAX as f32).round() as u16);
                self.last_pointer_abs = true;
                out.push(self.abs_report());
            }
            PointerMode::Relative { width, height } => {
                let (w, h) = (width.max(1) as f64, height.max(1) as f64);
                let (est_x, est_y) = match self.est {
                    Some(p) => p,
                    None => {
                        // Slam into the top-left corner: twice the screen
                        // size guarantees saturation even without acceleration.
                        self.acc_x.reset();
                        self.acc_y.reset();
                        self.relative(out, -2.0 * w, -2.0 * h);
                        (0.0, 0.0)
                    }
                };
                let (tx, ty) = (nx as f64 * (w - 1.0), ny as f64 * (h - 1.0));
                self.relative(out, tx - est_x, ty - est_y);
                self.est = Some((tx, ty));
            }
        }
    }

    fn set_key(&mut self, code: HidUsage, pressed: bool) {
        if code.is_modifier() {
            self.mods.set(code, pressed);
            return;
        }
        let k = code.0 as u8;
        if pressed {
            if !self.keys.contains(&k) {
                self.keys.push(k);
            }
        } else {
            self.keys.retain(|&x| x != k);
        }
    }

    fn key(&mut self, out: &mut Vec<HidReport>, code: HidUsage, pressed: bool, modifiers: Modifiers) -> Result<()> {
        if code.0 == 0 || code.0 > 0xE7 {
            return Err(InputError::InvalidArgument(format!("HID usage {:#x} outside the keyboard page", code.0)));
        }
        let mut st = self.mods;
        let steps = plan_key(&mut st, code, pressed, modifiers);
        let before = out.len();
        for s in steps {
            self.set_key(s.code, s.pressed);
            // Modifier-only transitions can share one report with the key.
            if !s.code.is_modifier() || s.code == code {
                out.push(self.keyboard_report());
            }
        }
        if out.len() == before {
            out.push(self.keyboard_report());
        }
        Ok(())
    }

    fn tap(&mut self, out: &mut Vec<HidReport>, mods: u8, key: HidUsage) {
        let saved_mods = self.mods;
        self.mods = ModifierState(self.mods.0 | mods);
        if mods & !saved_mods.0 != 0 {
            out.push(self.keyboard_report());
        }
        self.set_key(key, true);
        out.push(self.keyboard_report());
        self.set_key(key, false);
        out.push(self.keyboard_report());
        if self.mods != saved_mods {
            self.mods = saved_mods;
            out.push(self.keyboard_report());
        }
    }

    fn consumer_tap(out: &mut Vec<HidReport>, usage: u16) {
        out.push(Self::consumer_report(usage));
        out.push(Self::consumer_report(0));
    }

    /// Convert one event into the reports to send, in order.
    pub fn convert(&mut self, ev: &InputEvent) -> Result<Vec<HidReport>> {
        let mut out = Vec::new();
        match ev {
            InputEvent::MouseMove { x, y, .. } => self.move_abs(&mut out, *x, *y),
            InputEvent::MouseMoveRelative { dx, dy } => {
                if let Some((ex, ey)) = self.est.as_mut() {
                    *ex += *dx as f64;
                    *ey += *dy as f64;
                }
                self.relative(&mut out, *dx as f64, *dy as f64);
            }
            InputEvent::MouseButton { button, pressed } => {
                let bit = button_bit(*button);
                if *pressed {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                out.push(self.pointer_state_report());
            }
            InputEvent::Scroll { dx, dy, unit, phase } => {
                if *phase == Some(GesturePhase::Cancel) {
                    self.wheel_acc.reset();
                    self.pan_acc.reset();
                    return Ok(out);
                }
                let per = match unit {
                    ScrollUnit::Line => 1.0,
                    ScrollUnit::Pixel => self.cfg.pixels_per_detent.max(1.0) as f64,
                };
                // DOM sign: +dy scrolls down; HID wheel: + scrolls up. AC Pan: + = right.
                let mut w = self.wheel_acc.take(-(*dy as f64) / per);
                let mut p = self.pan_acc.take(*dx as f64 / per);
                while w != 0 || p != 0 {
                    let sw = w.clamp(-127, 127);
                    let sp = p.clamp(-127, 127);
                    out.push(self.mouse_report(0, 0, sw as i8, sp as i8));
                    w -= sw;
                    p -= sp;
                }
            }
            InputEvent::Key { code, pressed, modifiers } => self.key(&mut out, *code, *pressed, *modifiers)?,
            InputEvent::Text { text } => {
                let mapped: Vec<(HidUsage, bool)> = text
                    .chars()
                    .map(ascii_to_hid_us)
                    .collect::<Option<_>>()
                    .ok_or(InputError::Unsupported(
                        "HID keyboards can only type US-layout ASCII; non-ASCII text needs a host-side agent",
                    ))?;
                // Type with no other modifiers held (a held Ctrl must not turn
                // text into shortcuts), then restore the tracked state.
                let saved = self.mods;
                if saved.0 != 0 {
                    self.mods = ModifierState::default();
                    out.push(self.keyboard_report());
                }
                for (u, shift) in mapped {
                    self.tap(&mut out, if shift { 0x02 } else { 0 }, u);
                }
                if saved.0 != 0 {
                    self.mods = saved;
                    out.push(self.keyboard_report());
                }
            }
            InputEvent::Gesture { .. } => {
                return Err(InputError::Unsupported("HID mouse/keyboard reports have no gesture usages"));
            }
            InputEvent::Touch { id, x, y, phase, .. } => {
                if *id != 0 {
                    return Ok(out); // single-pointer device: only the primary touch is tracked
                }
                match phase {
                    TouchPhase::Down => {
                        self.move_abs(&mut out, *x, *y);
                        self.buttons |= 0x01;
                        out.push(self.pointer_state_report());
                    }
                    TouchPhase::Move => self.move_abs(&mut out, *x, *y),
                    TouchPhase::Up | TouchPhase::Cancel => {
                        self.buttons &= !0x01;
                        out.push(self.pointer_state_report());
                    }
                }
            }
            InputEvent::System(a) => match a {
                SystemAction::AndroidBack => Self::consumer_tap(&mut out, consumer::AC_BACK),
                SystemAction::AndroidHome => Self::consumer_tap(&mut out, consumer::AC_HOME),
                SystemAction::AndroidRecents | SystemAction::MissionControl => {
                    Self::consumer_tap(&mut out, consumer::AC_SHOW_ALL_WINDOWS)
                }
                SystemAction::LockScreen => Self::consumer_tap(&mut out, consumer::AL_TERMINAL_LOCK),
                SystemAction::Screenshot => self.tap(&mut out, 0, HidUsage::PRINT_SCREEN),
                // A real keyboard's Ctrl+Alt+Del does reach Winlogon.
                SystemAction::CtrlAltDel => self.tap(&mut out, 0x01 | 0x04, HidUsage::DELETE),
                SystemAction::AndroidNotifications | SystemAction::ShowDesktop => {
                    return Err(InputError::Unsupported(
                        "no host-independent HID usage for this action (notification shade / show desktop)",
                    ));
                }
            },
            InputEvent::ReleaseAll => {
                self.mods = ModifierState::default();
                self.keys.clear();
                self.buttons = 0;
                self.wheel_acc.reset();
                self.pan_acc.reset();
                out.push(self.keyboard_report());
                out.push(self.mouse_report(0, 0, 0, 0));
                out.push(self.abs_report());
                out.push(Self::consumer_report(0));
            }
        }
        Ok(out)
    }
}

/// Parse an output report (LED bitmap) sent by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyboardLeds {
    pub num_lock: bool,
    pub caps_lock: bool,
    pub scroll_lock: bool,
    pub compose: bool,
    pub kana: bool,
}

impl KeyboardLeds {
    pub fn from_byte(b: u8) -> Self {
        Self {
            num_lock: b & 0x01 != 0,
            caps_lock: b & 0x02 != 0,
            scroll_lock: b & 0x04 != 0,
            compose: b & 0x08 != 0,
            kana: b & 0x10 != 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_protocol::MouseButton;

    /// Minimal HID descriptor parser: computes input report sizes (bits) per report id.
    fn input_report_bits(desc: &[u8]) -> std::collections::BTreeMap<u8, (u32, u32)> {
        let mut sizes = std::collections::BTreeMap::new();
        let (mut id, mut size, mut count) = (0u8, 0u32, 0u32);
        let mut depth = 0i32;
        let mut i = 0;
        while i < desc.len() {
            let prefix = desc[i];
            let len = match prefix & 0x03 {
                3 => 4,
                n => n as usize,
            };
            let mut val = 0u32;
            for k in 0..len {
                val |= (desc[i + 1 + k] as u32) << (8 * k);
            }
            match prefix & 0xFC {
                0x84 => id = val as u8,
                0x74 => size = val,
                0x94 => count = val,
                0x80 => sizes.entry(id).or_insert((0, 0)).0 += size * count, // Input
                0x90 => sizes.entry(id).or_insert((0, 0)).1 += size * count, // Output
                0xA0 => depth += 1,
                0xC0 => depth -= 1,
                _ => {}
            }
            i += 1 + len;
        }
        assert_eq!(depth, 0, "unbalanced collections");
        sizes
    }

    #[test]
    fn descriptor_matches_report_lengths() {
        let s = input_report_bits(REPORT_DESCRIPTOR);
        assert_eq!(s[&REPORT_ID_KEYBOARD], (KEYBOARD_REPORT_LEN as u32 * 8, KEYBOARD_OUTPUT_LEN as u32 * 8));
        assert_eq!(s[&REPORT_ID_MOUSE].0, MOUSE_REPORT_LEN as u32 * 8);
        assert_eq!(s[&REPORT_ID_ABS_POINTER].0, ABS_POINTER_REPORT_LEN as u32 * 8);
        assert_eq!(s[&REPORT_ID_CONSUMER].0, CONSUMER_REPORT_LEN as u32 * 8);
        assert_eq!(s.len(), 4);
    }

    fn key(code: HidUsage, pressed: bool, m: Modifiers) -> InputEvent {
        InputEvent::Key { code, pressed, modifiers: m }
    }

    #[test]
    fn keyboard_reports_track_state() {
        let mut b = HidReportBuilder::new(HidConfig::default());
        let r = b.convert(&key(HidUsage::SHIFT_LEFT, true, Modifiers::SHIFT)).unwrap();
        assert_eq!(r, vec![HidReport { id: 1, data: vec![0x02, 0, 0, 0, 0, 0, 0, 0] }]);
        let r = b.convert(&key(HidUsage::A, true, Modifiers::SHIFT)).unwrap();
        assert_eq!(r[0].data, vec![0x02, 0, 0x04, 0, 0, 0, 0, 0]);
        let r = b.convert(&key(HidUsage::B, true, Modifiers::SHIFT)).unwrap();
        assert_eq!(r[0].data, vec![0x02, 0, 0x04, 0x05, 0, 0, 0, 0]);
        let r = b.convert(&key(HidUsage::A, false, Modifiers::SHIFT)).unwrap();
        assert_eq!(r[0].data, vec![0x02, 0, 0x05, 0, 0, 0, 0, 0]);
        // Client says shift is gone: synced in the same report as the key-up.
        let r = b.convert(&key(HidUsage::B, false, Modifiers::NONE)).unwrap();
        assert_eq!(r.last().unwrap().data, vec![0, 0, 0, 0, 0, 0, 0, 0]);
        // Wire form.
        assert_eq!(r.last().unwrap().to_bytes_with_id()[0], 1);
    }

    #[test]
    fn missing_modifier_is_pressed() {
        let mut b = HidReportBuilder::new(HidConfig::default());
        let r = b.convert(&key(HidUsage::C, true, Modifiers::META)).unwrap();
        assert_eq!(r.last().unwrap().data, vec![0x08, 0, 0x06, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn rollover_after_six_keys() {
        let mut b = HidReportBuilder::new(HidConfig::default());
        for k in 0x04..0x0B {
            b.convert(&key(HidUsage(k), true, Modifiers::NONE)).unwrap();
        }
        assert_eq!(b.keyboard_report().data[2..], [1u8; 6]);
        b.convert(&key(HidUsage(0x04), false, Modifiers::NONE)).unwrap();
        assert_eq!(b.keyboard_report().data[2..], [5, 6, 7, 8, 9, 10]);
    }

    #[test]
    fn absolute_pointer_and_buttons() {
        let mut b = HidReportBuilder::new(HidConfig::default());
        let r = b.convert(&InputEvent::MouseMove { display_id: 0, x: 0.5, y: 1.0 }).unwrap();
        assert_eq!(r, vec![HidReport { id: 3, data: vec![0, 0x00, 0x40, 0xFF, 0x7F] }]);
        let r = b.convert(&InputEvent::MouseButton { button: MouseButton::Right, pressed: true }).unwrap();
        assert_eq!(r[0].id, 3);
        assert_eq!(r[0].data[0], 0x02);
        let r = b.convert(&InputEvent::MouseButton { button: MouseButton::Forward, pressed: true }).unwrap();
        assert_eq!(r[0].data[0], 0x12);
    }

    fn sum_motion(r: &[HidReport]) -> (i64, i64) {
        r.iter().filter(|r| r.id == REPORT_ID_MOUSE).fold((0, 0), |(x, y), r| {
            (
                x + i16::from_le_bytes([r.data[1], r.data[2]]) as i64,
                y + i16::from_le_bytes([r.data[3], r.data[4]]) as i64,
            )
        })
    }

    #[test]
    fn relative_mode_homes_then_tracks() {
        let mut b = HidReportBuilder::new(HidConfig {
            pointer: PointerMode::Relative { width: 40001, height: 1001 },
            ..HidConfig::default()
        });
        let r = b.convert(&InputEvent::MouseMove { display_id: 0, x: 0.5, y: 0.5 }).unwrap();
        // Homing: -80002 / -2002, split into i16 chunks.
        assert!(r.len() >= 3);
        assert!(r.iter().all(|r| r.id == REPORT_ID_MOUSE));
        let home: Vec<_> = r[..3].to_vec();
        assert_eq!(sum_motion(&home), (-80002, -2002));
        assert_eq!(sum_motion(&r[3..]), (20000, 500));
        // Next move is a pure delta.
        let r = b.convert(&InputEvent::MouseMove { display_id: 0, x: 0.25, y: 0.5 }).unwrap();
        assert_eq!(sum_motion(&r), (-10000, 0));
        // Sub-pixel relative motion accumulates.
        let mut total = (0, 0);
        for _ in 0..8 {
            let r = b.convert(&InputEvent::MouseMoveRelative { dx: 0.25, dy: -0.5 }).unwrap();
            let m = sum_motion(&r);
            total = (total.0 + m.0, total.1 + m.1);
        }
        assert_eq!(total, (2, -4));
        b.rehome();
        let r = b.convert(&InputEvent::MouseMove { display_id: 0, x: 0.0, y: 0.0 }).unwrap();
        assert_eq!(sum_motion(&r), (-80002, -2002));
    }

    #[test]
    fn scroll_detents_and_pan() {
        let mut b = HidReportBuilder::new(HidConfig::default());
        let r = b
            .convert(&InputEvent::Scroll { dx: 0.0, dy: 3.0, unit: ScrollUnit::Line, phase: None })
            .unwrap();
        assert_eq!(r, vec![HidReport { id: 2, data: vec![0, 0, 0, 0, 0, (-3i8) as u8, 0] }]);
        // 30 px with 40 px/detent: nothing yet, then 20 more → one detent.
        let ev = |dy| InputEvent::Scroll { dx: 0.0, dy, unit: ScrollUnit::Pixel, phase: Some(GesturePhase::Change) };
        assert!(b.convert(&ev(30.0)).unwrap().is_empty());
        let r = b.convert(&ev(20.0)).unwrap();
        assert_eq!(r[0].data[5] as i8, -1);
        let r = b
            .convert(&InputEvent::Scroll { dx: 2.0, dy: 0.0, unit: ScrollUnit::Line, phase: None })
            .unwrap();
        assert_eq!(r[0].data[6] as i8, 2);
        // Big line scrolls are split into ±127 chunks.
        let mut b = HidReportBuilder::new(HidConfig::default());
        let r = b
            .convert(&InputEvent::Scroll { dx: 0.0, dy: -300.0, unit: ScrollUnit::Line, phase: None })
            .unwrap();
        assert_eq!(r.iter().map(|r| r.data[5] as i8 as i32).sum::<i32>(), 300);
    }

    #[test]
    fn text_types_with_shift_and_restores() {
        let mut b = HidReportBuilder::new(HidConfig::default());
        let r = b.convert(&InputEvent::Text { text: "aB".into() }).unwrap();
        let datas: Vec<Vec<u8>> = r.iter().map(|r| r.data.clone()).collect();
        assert_eq!(
            datas,
            vec![
                vec![0, 0, 0x04, 0, 0, 0, 0, 0],
                vec![0, 0, 0, 0, 0, 0, 0, 0],
                vec![0x02, 0, 0, 0, 0, 0, 0, 0],
                vec![0x02, 0, 0x05, 0, 0, 0, 0, 0],
                vec![0x02, 0, 0, 0, 0, 0, 0, 0],
                vec![0, 0, 0, 0, 0, 0, 0, 0],
            ]
        );
        // Held Ctrl is lifted while typing and restored afterwards.
        b.convert(&key(HidUsage::CONTROL_LEFT, true, Modifiers::CTRL)).unwrap();
        let r = b.convert(&InputEvent::Text { text: "a".into() }).unwrap();
        assert_eq!(r.first().unwrap().data[0], 0);
        assert_eq!(r[1].data, vec![0, 0, 0x04, 0, 0, 0, 0, 0]);
        assert_eq!(r.last().unwrap().data[0], 0x01);
        assert!(matches!(
            b.convert(&InputEvent::Text { text: "perché".into() }),
            Err(InputError::Unsupported(_))
        ));
    }

    #[test]
    fn system_actions_and_release_all() {
        let mut b = HidReportBuilder::new(HidConfig::default());
        let r = b.convert(&InputEvent::System(SystemAction::AndroidBack)).unwrap();
        assert_eq!(r, vec![HidReport { id: 4, data: vec![0x24, 0x02] }, HidReport { id: 4, data: vec![0, 0] }]);
        let r = b.convert(&InputEvent::System(SystemAction::CtrlAltDel)).unwrap();
        assert!(r.iter().any(|r| r.data == vec![0x05, 0, 0x4C, 0, 0, 0, 0, 0]));
        assert_eq!(r.last().unwrap().data, vec![0; 8]);
        assert!(b.convert(&InputEvent::System(SystemAction::ShowDesktop)).is_err());

        b.convert(&key(HidUsage::A, true, Modifiers::CTRL)).unwrap();
        b.convert(&InputEvent::MouseButton { button: MouseButton::Left, pressed: true }).unwrap();
        let r = b.convert(&InputEvent::ReleaseAll).unwrap();
        assert_eq!(r.len(), 4);
        assert!(r.iter().all(|r| r.data.iter().all(|&x| x == 0) || r.id == REPORT_ID_ABS_POINTER));
        assert_eq!(r[2].data[0], 0); // abs pointer buttons released
    }

    #[test]
    fn touch_maps_to_primary_button() {
        let mut b = HidReportBuilder::new(HidConfig::default());
        let t = |phase| InputEvent::Touch { id: 0, display_id: 0, x: 0.1, y: 0.2, phase, pressure: 1.0 };
        let r = b.convert(&t(TouchPhase::Down)).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[1].data[0], 1);
        let r = b.convert(&t(TouchPhase::Up)).unwrap();
        assert_eq!(r[0].data[0], 0);
        assert!(b.convert(&InputEvent::Touch { id: 1, display_id: 0, x: 0.0, y: 0.0, phase: TouchPhase::Down, pressure: 1.0 }).unwrap().is_empty());
    }

    #[test]
    fn leds() {
        let l = KeyboardLeds::from_byte(0x03);
        assert!(l.num_lock && l.caps_lock && !l.scroll_lock);
    }
}
