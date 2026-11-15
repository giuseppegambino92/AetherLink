//! Platform-neutral input events.
//!
//! Keys use **USB HID Usage IDs (page 0x07)**, the same codes a BLE HID
//! keyboard sends, so the same events drive OS injection and BLE HID emulation.

use serde::{Deserialize, Serialize};

/// USB HID keyboard usage (page 0x07).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HidUsage(pub u16);

#[allow(missing_docs)]
impl HidUsage {
    pub const A: Self = Self(0x04);
    pub const B: Self = Self(0x05);
    pub const C: Self = Self(0x06);
    pub const D: Self = Self(0x07);
    pub const E: Self = Self(0x08);
    pub const F: Self = Self(0x09);
    pub const G: Self = Self(0x0A);
    pub const H: Self = Self(0x0B);
    pub const I: Self = Self(0x0C);
    pub const J: Self = Self(0x0D);
    pub const K: Self = Self(0x0E);
    pub const L: Self = Self(0x0F);
    pub const M: Self = Self(0x10);
    pub const N: Self = Self(0x11);
    pub const O: Self = Self(0x12);
    pub const P: Self = Self(0x13);
    pub const Q: Self = Self(0x14);
    pub const R: Self = Self(0x15);
    pub const S: Self = Self(0x16);
    pub const T: Self = Self(0x17);
    pub const U: Self = Self(0x18);
    pub const V: Self = Self(0x19);
    pub const W: Self = Self(0x1A);
    pub const X: Self = Self(0x1B);
    pub const Y: Self = Self(0x1C);
    pub const Z: Self = Self(0x1D);
    pub const DIGIT1: Self = Self(0x1E);
    pub const DIGIT2: Self = Self(0x1F);
    pub const DIGIT3: Self = Self(0x20);
    pub const DIGIT4: Self = Self(0x21);
    pub const DIGIT5: Self = Self(0x22);
    pub const DIGIT6: Self = Self(0x23);
    pub const DIGIT7: Self = Self(0x24);
    pub const DIGIT8: Self = Self(0x25);
    pub const DIGIT9: Self = Self(0x26);
    pub const DIGIT0: Self = Self(0x27);
    pub const ENTER: Self = Self(0x28);
    pub const ESCAPE: Self = Self(0x29);
    pub const BACKSPACE: Self = Self(0x2A);
    pub const TAB: Self = Self(0x2B);
    pub const SPACE: Self = Self(0x2C);
    pub const MINUS: Self = Self(0x2D);
    pub const EQUAL: Self = Self(0x2E);
    pub const BRACKET_LEFT: Self = Self(0x2F);
    pub const BRACKET_RIGHT: Self = Self(0x30);
    pub const BACKSLASH: Self = Self(0x31);
    pub const SEMICOLON: Self = Self(0x33);
    pub const QUOTE: Self = Self(0x34);
    pub const BACKQUOTE: Self = Self(0x35);
    pub const COMMA: Self = Self(0x36);
    pub const PERIOD: Self = Self(0x37);
    pub const SLASH: Self = Self(0x38);
    pub const CAPS_LOCK: Self = Self(0x39);
    pub const F1: Self = Self(0x3A);
    pub const F2: Self = Self(0x3B);
    pub const F3: Self = Self(0x3C);
    pub const F4: Self = Self(0x3D);
    pub const F5: Self = Self(0x3E);
    pub const F6: Self = Self(0x3F);
    pub const F7: Self = Self(0x40);
    pub const F8: Self = Self(0x41);
    pub const F9: Self = Self(0x42);
    pub const F10: Self = Self(0x43);
    pub const F11: Self = Self(0x44);
    pub const F12: Self = Self(0x45);
    pub const PRINT_SCREEN: Self = Self(0x46);
    pub const SCROLL_LOCK: Self = Self(0x47);
    pub const PAUSE: Self = Self(0x48);
    pub const INSERT: Self = Self(0x49);
    pub const HOME: Self = Self(0x4A);
    pub const PAGE_UP: Self = Self(0x4B);
    pub const DELETE: Self = Self(0x4C);
    pub const END: Self = Self(0x4D);
    pub const PAGE_DOWN: Self = Self(0x4E);
    pub const ARROW_RIGHT: Self = Self(0x4F);
    pub const ARROW_LEFT: Self = Self(0x50);
    pub const ARROW_DOWN: Self = Self(0x51);
    pub const ARROW_UP: Self = Self(0x52);
    pub const NUM_LOCK: Self = Self(0x53);
    pub const INTL_BACKSLASH: Self = Self(0x64);
    pub const CONTEXT_MENU: Self = Self(0x65);
    pub const F13: Self = Self(0x68);
    pub const F14: Self = Self(0x69);
    pub const F15: Self = Self(0x6A);
    pub const MUTE: Self = Self(0x7F);
    pub const VOLUME_UP: Self = Self(0x80);
    pub const VOLUME_DOWN: Self = Self(0x81);
    pub const CONTROL_LEFT: Self = Self(0xE0);
    pub const SHIFT_LEFT: Self = Self(0xE1);
    pub const ALT_LEFT: Self = Self(0xE2);
    pub const META_LEFT: Self = Self(0xE3);
    pub const CONTROL_RIGHT: Self = Self(0xE4);
    pub const SHIFT_RIGHT: Self = Self(0xE5);
    pub const ALT_RIGHT: Self = Self(0xE6);
    pub const META_RIGHT: Self = Self(0xE7);

    /// Map a W3C `KeyboardEvent.code` string (what the desktop/mobile UI sees) to a HID usage.
    pub fn from_dom_code(code: &str) -> Option<Self> {
        let u = match code {
            "Enter" | "NumpadEnter" => 0x28,
            "Escape" => 0x29,
            "Backspace" => 0x2A,
            "Tab" => 0x2B,
            "Space" => 0x2C,
            "Minus" => 0x2D,
            "Equal" => 0x2E,
            "BracketLeft" => 0x2F,
            "BracketRight" => 0x30,
            "Backslash" => 0x31,
            "Semicolon" => 0x33,
            "Quote" => 0x34,
            "Backquote" => 0x35,
            "Comma" => 0x36,
            "Period" => 0x37,
            "Slash" => 0x38,
            "CapsLock" => 0x39,
            "PrintScreen" => 0x46,
            "ScrollLock" => 0x47,
            "Pause" => 0x48,
            "Insert" => 0x49,
            "Home" => 0x4A,
            "PageUp" => 0x4B,
            "Delete" => 0x4C,
            "End" => 0x4D,
            "PageDown" => 0x4E,
            "ArrowRight" => 0x4F,
            "ArrowLeft" => 0x50,
            "ArrowDown" => 0x51,
            "ArrowUp" => 0x52,
            "NumLock" => 0x53,
            "IntlBackslash" => 0x64,
            "ContextMenu" => 0x65,
            "AudioVolumeMute" => 0x7F,
            "AudioVolumeUp" => 0x80,
            "AudioVolumeDown" => 0x81,
            "ControlLeft" => 0xE0,
            "ShiftLeft" => 0xE1,
            "AltLeft" => 0xE2,
            "MetaLeft" | "OSLeft" => 0xE3,
            "ControlRight" => 0xE4,
            "ShiftRight" => 0xE5,
            "AltRight" => 0xE6,
            "MetaRight" | "OSRight" => 0xE7,
            _ => {
                if let Some(c) = code.strip_prefix("Key") {
                    let b = c.as_bytes();
                    if b.len() == 1 && b[0].is_ascii_uppercase() {
                        return Some(Self(0x04 + (b[0] - b'A') as u16));
                    }
                    return None;
                }
                if let Some(d) = code.strip_prefix("Digit") {
                    let b = d.as_bytes();
                    if b.len() == 1 && b[0].is_ascii_digit() {
                        let n = b[0] - b'0';
                        return Some(Self(if n == 0 { 0x27 } else { 0x1E + (n - 1) as u16 }));
                    }
                    return None;
                }
                if let Some(Ok(n)) = code.strip_prefix('F').map(str::parse::<u16>) {
                    return match n {
                        1..=12 => Some(Self(0x3A + n - 1)),
                        13..=24 => Some(Self(0x68 + n - 13)),
                        _ => None,
                    };
                }
                if let Some(n) = code.strip_prefix("Numpad") {
                    return match n {
                        "Divide" => Some(Self(0x54)),
                        "Multiply" => Some(Self(0x55)),
                        "Subtract" => Some(Self(0x56)),
                        "Add" => Some(Self(0x57)),
                        "Decimal" => Some(Self(0x63)),
                        "0" => Some(Self(0x62)),
                        d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => {
                            Some(Self(0x59 + (d.as_bytes()[0] - b'1') as u16))
                        }
                        _ => None,
                    };
                }
                return None;
            }
        };
        Some(Self(u))
    }

    pub fn is_modifier(self) -> bool {
        (0xE0..=0xE7).contains(&self.0)
    }
}

/// Modifier bitmask (matches the HID boot-protocol modifier byte layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const CTRL: Self = Self(0x01);
    pub const SHIFT: Self = Self(0x02);
    pub const ALT: Self = Self(0x04);
    /// Cmd on Apple, Win on Windows, Super on Linux.
    pub const META: Self = Self(0x08);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollUnit {
    /// Precise trackpad deltas in logical points.
    Pixel,
    /// Classic wheel notches.
    Line,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GesturePhase {
    Begin,
    Change,
    End,
    Cancel,
    /// Inertial momentum after the fingers lifted.
    Momentum,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Gesture {
    Magnify { delta: f32 },
    Rotate { degrees: f32 },
    SmartZoom,
    SwipeNavigate { dx: f32, dy: f32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

/// OS-level actions without a universal key combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemAction {
    AndroidBack,
    AndroidHome,
    AndroidRecents,
    AndroidNotifications,
    LockScreen,
    /// Secure Attention Sequence (Windows service only).
    CtrlAltDel,
    MissionControl,
    ShowDesktop,
    Screenshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Absolute pointer position, normalized 0..1 within `display_id`.
    MouseMove { display_id: u32, x: f32, y: f32 },
    /// Relative motion in logical points (games / pointer-lock).
    MouseMoveRelative { dx: f32, dy: f32 },
    MouseButton { button: MouseButton, pressed: bool },
    Scroll { dx: f32, dy: f32, unit: ScrollUnit, phase: Option<GesturePhase> },
    Key { code: HidUsage, pressed: bool, modifiers: Modifiers },
    /// Unicode text typed at the cursor (IME output, mobile keyboards, palette text entry).
    Text { text: String },
    Gesture { gesture: Gesture, phase: GesturePhase },
    /// Direct touch on touch-capable hosts (Android / Windows touch).
    Touch { id: u32, display_id: u32, x: f32, y: f32, phase: TouchPhase, pressure: f32 },
    System(SystemAction),
    /// Release every pressed key/button (sent on focus loss to avoid stuck keys).
    ReleaseAll,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dom_code_mapping() {
        assert_eq!(HidUsage::from_dom_code("KeyA"), Some(HidUsage::A));
        assert_eq!(HidUsage::from_dom_code("KeyZ"), Some(HidUsage::Z));
        assert_eq!(HidUsage::from_dom_code("Digit1"), Some(HidUsage::DIGIT1));
        assert_eq!(HidUsage::from_dom_code("Digit0"), Some(HidUsage::DIGIT0));
        assert_eq!(HidUsage::from_dom_code("F1"), Some(HidUsage::F1));
        assert_eq!(HidUsage::from_dom_code("F12"), Some(HidUsage::F12));
        assert_eq!(HidUsage::from_dom_code("F13"), Some(HidUsage::F13));
        assert_eq!(HidUsage::from_dom_code("MetaLeft"), Some(HidUsage::META_LEFT));
        assert_eq!(HidUsage::from_dom_code("Numpad1"), Some(HidUsage(0x59)));
        assert_eq!(HidUsage::from_dom_code("Numpad0"), Some(HidUsage(0x62)));
        assert_eq!(HidUsage::from_dom_code("Keyboard"), None);
        assert_eq!(HidUsage::from_dom_code("Nope"), None);
        assert!(HidUsage::SHIFT_RIGHT.is_modifier());
        assert!(!HidUsage::A.is_modifier());
    }

    #[test]
    fn modifiers() {
        let m = Modifiers::META | Modifiers::SHIFT;
        assert!(m.contains(Modifiers::META));
        assert!(!m.contains(Modifiers::CTRL));
    }
}
