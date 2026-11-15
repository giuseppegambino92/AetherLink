//! Pure translation tables from USB HID keyboard usages (page 0x07) to the
//! native key codes of each host OS.
//!
//! * macOS: virtual key codes (`kVK_*` from `HIToolbox/Events.h`).
//! * Windows: virtual-key codes (`VK_*`) plus PC/AT **scan code set 1** make
//!   codes; extended keys (the ones prefixed with `0xE0` on the wire) carry
//!   `extended = true`.
//! * Linux: evdev `KEY_*` codes (`linux/input-event-codes.h`), identical to the
//!   kernel's own `hid_keyboard[]` table in `drivers/hid/hid-input.c`.
//!
//! The tables are plain data and compiled/tested on every OS.

use aether_protocol::HidUsage;

const NONE16: u16 = 0xFFFF;

/// One row of the master table.
#[derive(Clone, Copy)]
struct Row {
    hid: u16,
    mac: u16,
    win_vk: u16,
    /// Set-1 make code; `0xE0xx` marks an extended key. 0 = none.
    win_sc: u16,
    linux: u16,
}

const fn r(hid: u16, mac: u16, win_vk: u16, win_sc: u16, linux: u16) -> Row {
    Row { hid, mac, win_vk, win_sc, linux }
}

/// Sorted by HID usage (binary-searched).
#[rustfmt::skip]
const TABLE: &[Row] = &[
    //   HID    macOS   VK     SC1     evdev
    r(0x04, 0x00, 0x41, 0x1E, 30),   // A
    r(0x05, 0x0B, 0x42, 0x30, 48),   // B
    r(0x06, 0x08, 0x43, 0x2E, 46),   // C
    r(0x07, 0x02, 0x44, 0x20, 32),   // D
    r(0x08, 0x0E, 0x45, 0x12, 18),   // E
    r(0x09, 0x03, 0x46, 0x21, 33),   // F
    r(0x0A, 0x05, 0x47, 0x22, 34),   // G
    r(0x0B, 0x04, 0x48, 0x23, 35),   // H
    r(0x0C, 0x22, 0x49, 0x17, 23),   // I
    r(0x0D, 0x26, 0x4A, 0x24, 36),   // J
    r(0x0E, 0x28, 0x4B, 0x25, 37),   // K
    r(0x0F, 0x25, 0x4C, 0x26, 38),   // L
    r(0x10, 0x2E, 0x4D, 0x32, 50),   // M
    r(0x11, 0x2D, 0x4E, 0x31, 49),   // N
    r(0x12, 0x1F, 0x4F, 0x18, 24),   // O
    r(0x13, 0x23, 0x50, 0x19, 25),   // P
    r(0x14, 0x0C, 0x51, 0x10, 16),   // Q
    r(0x15, 0x0F, 0x52, 0x13, 19),   // R
    r(0x16, 0x01, 0x53, 0x1F, 31),   // S
    r(0x17, 0x11, 0x54, 0x14, 20),   // T
    r(0x18, 0x20, 0x55, 0x16, 22),   // U
    r(0x19, 0x09, 0x56, 0x2F, 47),   // V
    r(0x1A, 0x0D, 0x57, 0x11, 17),   // W
    r(0x1B, 0x07, 0x58, 0x2D, 45),   // X
    r(0x1C, 0x10, 0x59, 0x15, 21),   // Y
    r(0x1D, 0x06, 0x5A, 0x2C, 44),   // Z
    r(0x1E, 0x12, 0x31, 0x02, 2),    // 1
    r(0x1F, 0x13, 0x32, 0x03, 3),    // 2
    r(0x20, 0x14, 0x33, 0x04, 4),    // 3
    r(0x21, 0x15, 0x34, 0x05, 5),    // 4
    r(0x22, 0x17, 0x35, 0x06, 6),    // 5
    r(0x23, 0x16, 0x36, 0x07, 7),    // 6
    r(0x24, 0x1A, 0x37, 0x08, 8),    // 7
    r(0x25, 0x1C, 0x38, 0x09, 9),    // 8
    r(0x26, 0x19, 0x39, 0x0A, 10),   // 9
    r(0x27, 0x1D, 0x30, 0x0B, 11),   // 0
    r(0x28, 0x24, 0x0D, 0x1C, 28),   // Enter
    r(0x29, 0x35, 0x1B, 0x01, 1),    // Escape
    r(0x2A, 0x33, 0x08, 0x0E, 14),   // Backspace
    r(0x2B, 0x30, 0x09, 0x0F, 15),   // Tab
    r(0x2C, 0x31, 0x20, 0x39, 57),   // Space
    r(0x2D, 0x1B, 0xBD, 0x0C, 12),   // - _
    r(0x2E, 0x18, 0xBB, 0x0D, 13),   // = +
    r(0x2F, 0x21, 0xDB, 0x1A, 26),   // [ {
    r(0x30, 0x1E, 0xDD, 0x1B, 27),   // ] }
    r(0x31, 0x2A, 0xDC, 0x2B, 43),   // \ |
    r(0x32, 0x2A, 0xDC, 0x2B, 43),   // Non-US # ~ (same physical code as \ on ISO boards)
    r(0x33, 0x29, 0xBA, 0x27, 39),   // ; :
    r(0x34, 0x27, 0xDE, 0x28, 40),   // ' "
    r(0x35, 0x32, 0xC0, 0x29, 41),   // ` ~
    r(0x36, 0x2B, 0xBC, 0x33, 51),   // , <
    r(0x37, 0x2F, 0xBE, 0x34, 52),   // . >
    r(0x38, 0x2C, 0xBF, 0x35, 53),   // / ?
    r(0x39, 0x39, 0x14, 0x3A, 58),   // Caps Lock
    r(0x3A, 0x7A, 0x70, 0x3B, 59),   // F1
    r(0x3B, 0x78, 0x71, 0x3C, 60),   // F2
    r(0x3C, 0x63, 0x72, 0x3D, 61),   // F3
    r(0x3D, 0x76, 0x73, 0x3E, 62),   // F4
    r(0x3E, 0x60, 0x74, 0x3F, 63),   // F5
    r(0x3F, 0x61, 0x75, 0x40, 64),   // F6
    r(0x40, 0x62, 0x76, 0x41, 65),   // F7
    r(0x41, 0x64, 0x77, 0x42, 66),   // F8
    r(0x42, 0x65, 0x78, 0x43, 67),   // F9
    r(0x43, 0x6D, 0x79, 0x44, 68),   // F10
    r(0x44, 0x67, 0x7A, 0x57, 87),   // F11
    r(0x45, 0x6F, 0x7B, 0x58, 88),   // F12
    r(0x46, 0x69, 0x2C, 0xE037, 99), // PrintScreen (macOS treats it as F13)
    r(0x47, 0x6B, 0x91, 0x46, 70),   // Scroll Lock (macOS: F14)
    r(0x48, 0x71, 0x13, 0x45, 119),  // Pause (macOS: F15; Windows: inject by VK, see `windows_inject_by_vk`)
    r(0x49, 0x72, 0x2D, 0xE052, 110),// Insert (macOS: Help)
    r(0x4A, 0x73, 0x24, 0xE047, 102),// Home
    r(0x4B, 0x74, 0x21, 0xE049, 104),// Page Up
    r(0x4C, 0x75, 0x2E, 0xE053, 111),// Delete (forward)
    r(0x4D, 0x77, 0x23, 0xE04F, 107),// End
    r(0x4E, 0x79, 0x22, 0xE051, 109),// Page Down
    r(0x4F, 0x7C, 0x27, 0xE04D, 106),// Right
    r(0x50, 0x7B, 0x25, 0xE04B, 105),// Left
    r(0x51, 0x7D, 0x28, 0xE050, 108),// Down
    r(0x52, 0x7E, 0x26, 0xE048, 103),// Up
    r(0x53, 0x47, 0x90, 0xE045, 69), // Num Lock (macOS: keypad Clear)
    r(0x54, 0x4B, 0x6F, 0xE035, 98), // KP /
    r(0x55, 0x43, 0x6A, 0x37, 55),   // KP *
    r(0x56, 0x4E, 0x6D, 0x4A, 74),   // KP -
    r(0x57, 0x45, 0x6B, 0x4E, 78),   // KP +
    r(0x58, 0x4C, 0x0D, 0xE01C, 96), // KP Enter
    r(0x59, 0x53, 0x61, 0x4F, 79),   // KP 1
    r(0x5A, 0x54, 0x62, 0x50, 80),   // KP 2
    r(0x5B, 0x55, 0x63, 0x51, 81),   // KP 3
    r(0x5C, 0x56, 0x64, 0x4B, 75),   // KP 4
    r(0x5D, 0x57, 0x65, 0x4C, 76),   // KP 5
    r(0x5E, 0x58, 0x66, 0x4D, 77),   // KP 6
    r(0x5F, 0x59, 0x67, 0x47, 71),   // KP 7
    r(0x60, 0x5B, 0x68, 0x48, 72),   // KP 8
    r(0x61, 0x5C, 0x69, 0x49, 73),   // KP 9
    r(0x62, 0x52, 0x60, 0x52, 82),   // KP 0
    r(0x63, 0x41, 0x6E, 0x53, 83),   // KP .
    r(0x64, 0x0A, 0xE2, 0x56, 86),   // Non-US \ | (ISO section key)
    r(0x65, 0x6E, 0x5D, 0xE05D, 127),// Application / ContextMenu
    r(0x66, NONE16, 0, 0xE05E, 116), // Power
    r(0x67, 0x51, 0x92, 0x59, 117),  // KP =
    r(0x68, 0x69, 0x7C, 0x64, 183),  // F13
    r(0x69, 0x6B, 0x7D, 0x65, 184),  // F14
    r(0x6A, 0x71, 0x7E, 0x66, 185),  // F15
    r(0x6B, 0x6A, 0x7F, 0x67, 186),  // F16
    r(0x6C, 0x40, 0x80, 0x68, 187),  // F17
    r(0x6D, 0x4F, 0x81, 0x69, 188),  // F18
    r(0x6E, 0x50, 0x82, 0x6A, 189),  // F19
    r(0x6F, 0x5A, 0x83, 0x6B, 190),  // F20
    r(0x70, NONE16, 0x84, 0x6C, 191),// F21
    r(0x71, NONE16, 0x85, 0x6D, 192),// F22
    r(0x72, NONE16, 0x86, 0x6E, 193),// F23
    r(0x73, NONE16, 0x87, 0x76, 194),// F24
    r(0x74, NONE16, 0x2B, 0, 134),   // Execute (VK_EXECUTE / KEY_OPEN)
    r(0x75, 0x72, 0x2F, 0xE03B, 138),// Help
    r(0x76, NONE16, 0, 0, 130),      // Menu (KEY_PROPS)
    r(0x77, NONE16, 0x29, 0, 132),   // Select (VK_SELECT / KEY_FRONT)
    r(0x78, NONE16, 0, 0, 128),      // Stop
    r(0x79, NONE16, 0, 0, 129),      // Again
    r(0x7A, NONE16, 0, 0, 131),      // Undo
    r(0x7B, NONE16, 0, 0, 137),      // Cut
    r(0x7C, NONE16, 0, 0, 133),      // Copy
    r(0x7D, NONE16, 0, 0, 135),      // Paste
    r(0x7E, NONE16, 0, 0, 136),      // Find
    r(0x7F, 0x4A, 0xAD, 0xE020, 113),// Mute
    r(0x80, 0x48, 0xAF, 0xE030, 115),// Volume Up
    r(0x81, 0x49, 0xAE, 0xE02E, 114),// Volume Down
    r(0x85, 0x5F, 0x6C, 0x7E, 121),  // KP , (Brazilian / JIS keypad comma)
    r(0x87, 0x5E, 0xC1, 0x73, 89),   // International1 (JIS Ro / _)
    r(0x88, NONE16, 0x15, 0x70, 93), // International2 (Katakana/Hiragana)
    r(0x89, 0x5D, 0xDC, 0x7D, 124),  // International3 (JIS Yen)
    r(0x8A, NONE16, 0x1C, 0x79, 92), // International4 (Henkan / Convert)
    r(0x8B, NONE16, 0x1D, 0x7B, 94), // International5 (Muhenkan / NonConvert)
    r(0x8C, NONE16, 0, 0, 95),       // International6 (KP JP comma)
    r(0x90, 0x68, 0x15, 0x72, 122),  // LANG1 (Hangul / Kana)
    r(0x91, 0x66, 0x19, 0x71, 123),  // LANG2 (Hanja / Eisu)
    r(0x92, NONE16, 0, 0x78, 90),    // LANG3 (Katakana)
    r(0x93, NONE16, 0, 0x77, 91),    // LANG4 (Hiragana)
    r(0x94, NONE16, 0, 0x76, 85),    // LANG5 (Zenkaku/Hankaku)
    r(0xE0, 0x3B, 0xA2, 0x1D, 29),   // Left Control
    r(0xE1, 0x38, 0xA0, 0x2A, 42),   // Left Shift
    r(0xE2, 0x3A, 0xA4, 0x38, 56),   // Left Alt / Option
    r(0xE3, 0x37, 0x5B, 0xE05B, 125),// Left Meta / Command / Win
    r(0xE4, 0x3E, 0xA3, 0xE01D, 97), // Right Control
    r(0xE5, 0x3C, 0xA1, 0x36, 54),   // Right Shift
    r(0xE6, 0x3D, 0xA5, 0xE038, 100),// Right Alt / Option / AltGr
    r(0xE7, 0x36, 0x5C, 0xE05C, 126),// Right Meta / Command / Win
];

fn row(u: HidUsage) -> Option<&'static Row> {
    TABLE.binary_search_by_key(&u.0, |r| r.hid).ok().map(|i| &TABLE[i])
}

/// macOS virtual key code (`kVK_*`) for a HID usage.
pub fn hid_to_macos_vk(u: HidUsage) -> Option<u16> {
    row(u).map(|r| r.mac).filter(|&c| c != NONE16)
}

/// A Windows key: virtual-key code plus scan code set 1 make code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsKey {
    /// `VK_*` virtual-key code, 0 when the key has no VK.
    pub vk: u16,
    /// Set-1 make code without the `0xE0` prefix, 0 when the key has no scan code.
    pub scancode: u16,
    /// Whether the key is an extended (`0xE0`-prefixed) key (`KEYEVENTF_EXTENDEDKEY`).
    pub extended: bool,
}

/// Full Windows mapping (VK + scancode + extended flag).
pub fn hid_to_windows(u: HidUsage) -> Option<WindowsKey> {
    let r = row(u)?;
    if r.win_vk == 0 && r.win_sc == 0 {
        return None;
    }
    Some(WindowsKey { vk: r.win_vk, scancode: r.win_sc & 0xFF, extended: r.win_sc & 0xFF00 == 0xE000 })
}

/// Windows virtual-key code (`VK_*`) for a HID usage.
pub fn hid_to_windows_vk(u: HidUsage) -> Option<u16> {
    hid_to_windows(u).map(|k| k.vk).filter(|&v| v != 0)
}

/// Windows scan code set 1 make code and the extended flag for a HID usage.
pub fn hid_to_windows_scancode(u: HidUsage) -> Option<(u16, bool)> {
    hid_to_windows(u).filter(|k| k.scancode != 0).map(|k| (k.scancode, k.extended))
}

/// Keys whose set-1 scan code is ambiguous for `SendInput(KEYEVENTF_SCANCODE)`
/// and must be injected by virtual-key instead (Pause shares `0x45` with
/// NumLock and is really the `E1 1D 45` sequence on the wire).
pub fn windows_inject_by_vk(u: HidUsage) -> bool {
    u == HidUsage::PAUSE || hid_to_windows_scancode(u).is_none()
}

/// Linux evdev `KEY_*` code for a HID usage.
pub fn hid_to_linux_evdev(u: HidUsage) -> Option<u16> {
    row(u).map(|r| r.linux).filter(|&c| c != 0)
}

/// Every HID usage that has at least one mapping (useful for building uinput key bitmaps).
pub fn all_mapped_usages() -> impl Iterator<Item = HidUsage> {
    TABLE.iter().map(|r| HidUsage(r.hid))
}

/// US-ANSI layout: HID usage + shift for a printable ASCII character (or `\n`, `\t`).
///
/// Used where a host can only receive key codes (BLE/USB HID, Linux uinput),
/// which is inherently layout-dependent: the result is correct only when the
/// receiving host uses a US keyboard layout.
pub fn ascii_to_hid_us(c: char) -> Option<(HidUsage, bool)> {
    let (u, shift) = match c {
        'a'..='z' => (0x04 + (c as u16 - 'a' as u16), false),
        'A'..='Z' => (0x04 + (c as u16 - 'A' as u16), true),
        '1'..='9' => (0x1E + (c as u16 - '1' as u16), false),
        '0' => (0x27, false),
        '\n' | '\r' => (0x28, false),
        '\t' => (0x2B, false),
        ' ' => (0x2C, false),
        '!' => (0x1E, true),
        '@' => (0x1F, true),
        '#' => (0x20, true),
        '$' => (0x21, true),
        '%' => (0x22, true),
        '^' => (0x23, true),
        '&' => (0x24, true),
        '*' => (0x25, true),
        '(' => (0x26, true),
        ')' => (0x27, true),
        '-' => (0x2D, false),
        '_' => (0x2D, true),
        '=' => (0x2E, false),
        '+' => (0x2E, true),
        '[' => (0x2F, false),
        '{' => (0x2F, true),
        ']' => (0x30, false),
        '}' => (0x30, true),
        '\\' => (0x31, false),
        '|' => (0x31, true),
        ';' => (0x33, false),
        ':' => (0x33, true),
        '\'' => (0x34, false),
        '"' => (0x34, true),
        '`' => (0x35, false),
        '~' => (0x35, true),
        ',' => (0x36, false),
        '<' => (0x36, true),
        '.' => (0x37, false),
        '>' => (0x37, true),
        '/' => (0x38, false),
        '?' => (0x38, true),
        _ => return None,
    };
    Some((HidUsage(u), shift))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn core_usages() -> impl Iterator<Item = HidUsage> {
        (0x04u16..=0x65).chain(0xE0..=0xE7).map(HidUsage)
    }

    #[test]
    fn table_sorted_and_unique() {
        for w in TABLE.windows(2) {
            assert!(w[0].hid < w[1].hid, "table not sorted at {:#x}", w[1].hid);
        }
    }

    #[test]
    fn every_core_usage_maps_on_every_os() {
        // Power (0x66) has no virtual-key code on macOS/Windows.
        for u in core_usages().filter(|u| u.0 != 0x66) {
            assert!(hid_to_linux_evdev(u).is_some(), "linux {:#x}", u.0);
            assert!(hid_to_windows_vk(u).is_some(), "win vk {:#x}", u.0);
            assert!(hid_to_windows_scancode(u).is_some(), "win sc {:#x}", u.0);
            assert!(hid_to_macos_vk(u).is_some(), "mac {:#x}", u.0);
        }
    }

    #[test]
    fn mappings_are_injective_except_iso_alias() {
        // 0x32 (Non-US #) is the same physical key as 0x31 on every OS;
        // macOS also folds PrintScreen/ScrollLock/Pause into F13/F14/F15.
        let alias_ok = |a: u16, b: u16| {
            let pair = (a.min(b), a.max(b));
            matches!(pair, (0x31, 0x32) | (0x46, 0x68) | (0x47, 0x69) | (0x48, 0x6A) | (0x58, 0x28))
        };
        let mut mac: HashMap<u16, u16> = HashMap::new();
        let mut lin: HashMap<u16, u16> = HashMap::new();
        let mut sc: HashMap<(u16, bool), u16> = HashMap::new();
        for u in core_usages().chain((0x68..=0x6F).map(HidUsage)) {
            if let Some(m) = hid_to_macos_vk(u)
                && let Some(prev) = mac.insert(m, u.0)
            {
                assert!(alias_ok(prev, u.0), "mac dup {m:#x}: {prev:#x} / {:#x}", u.0);
            }
            if let Some(l) = hid_to_linux_evdev(u)
                && let Some(prev) = lin.insert(l, u.0)
            {
                assert!(alias_ok(prev, u.0), "linux dup {l}: {prev:#x} / {:#x}", u.0);
            }
            // NumLock (E0 45 in Windows' view) vs Pause (45) are distinct.
            if let Some(s) = hid_to_windows_scancode(u)
                && let Some(prev) = sc.insert(s, u.0)
            {
                assert!(alias_ok(prev, u.0), "win sc dup {s:?}: {prev:#x} / {:#x}", u.0);
            }
        }
    }

    #[test]
    fn known_values() {
        // macOS
        assert_eq!(hid_to_macos_vk(HidUsage::A), Some(0x00));
        assert_eq!(hid_to_macos_vk(HidUsage::Z), Some(0x06));
        assert_eq!(hid_to_macos_vk(HidUsage::ENTER), Some(0x24));
        assert_eq!(hid_to_macos_vk(HidUsage::META_LEFT), Some(0x37));
        assert_eq!(hid_to_macos_vk(HidUsage::META_RIGHT), Some(0x36));
        assert_eq!(hid_to_macos_vk(HidUsage::F1), Some(0x7A));
        assert_eq!(hid_to_macos_vk(HidUsage::ARROW_UP), Some(0x7E));
        assert_eq!(hid_to_macos_vk(HidUsage::INTL_BACKSLASH), Some(0x0A));
        assert_eq!(hid_to_macos_vk(HidUsage::DELETE), Some(0x75));
        assert_eq!(hid_to_macos_vk(HidUsage(0x66)), None);
        // Windows
        assert_eq!(hid_to_windows(HidUsage::A), Some(WindowsKey { vk: 0x41, scancode: 0x1E, extended: false }));
        assert_eq!(
            hid_to_windows(HidUsage::ARROW_LEFT),
            Some(WindowsKey { vk: 0x25, scancode: 0x4B, extended: true })
        );
        assert_eq!(hid_to_windows_scancode(HidUsage::CONTROL_RIGHT), Some((0x1D, true)));
        assert_eq!(hid_to_windows_scancode(HidUsage::CONTROL_LEFT), Some((0x1D, false)));
        assert_eq!(hid_to_windows_scancode(HidUsage(0x58)), Some((0x1C, true))); // KP Enter
        assert_eq!(hid_to_windows_vk(HidUsage::META_LEFT), Some(0x5B));
        assert_eq!(hid_to_windows_vk(HidUsage(0x73)), Some(0x87));
        assert!(windows_inject_by_vk(HidUsage::PAUSE));
        assert!(!windows_inject_by_vk(HidUsage::A));
        // Linux
        assert_eq!(hid_to_linux_evdev(HidUsage::A), Some(30));
        assert_eq!(hid_to_linux_evdev(HidUsage::ESCAPE), Some(1));
        assert_eq!(hid_to_linux_evdev(HidUsage::META_LEFT), Some(125));
        assert_eq!(hid_to_linux_evdev(HidUsage::ALT_RIGHT), Some(100));
        assert_eq!(hid_to_linux_evdev(HidUsage::F13), Some(183));
        assert_eq!(hid_to_linux_evdev(HidUsage::MUTE), Some(113));
        assert_eq!(hid_to_linux_evdev(HidUsage(0x00)), None);
        assert_eq!(hid_to_linux_evdev(HidUsage(0x1234)), None);
    }

    #[test]
    fn letters_and_digits_match_set1_on_linux() {
        // evdev KEY_* codes equal set-1 scan codes for the main block.
        for u in (0x04u16..=0x39).map(HidUsage) {
            let (sc, ext) = hid_to_windows_scancode(u).unwrap();
            assert!(!ext);
            assert_eq!(hid_to_linux_evdev(u).unwrap(), sc, "usage {:#x}", u.0);
        }
    }

    #[test]
    fn ascii_us_layout() {
        assert_eq!(ascii_to_hid_us('a'), Some((HidUsage::A, false)));
        assert_eq!(ascii_to_hid_us('Q'), Some((HidUsage::Q, true)));
        assert_eq!(ascii_to_hid_us('0'), Some((HidUsage::DIGIT0, false)));
        assert_eq!(ascii_to_hid_us(')'), Some((HidUsage::DIGIT0, true)));
        assert_eq!(ascii_to_hid_us('?'), Some((HidUsage::SLASH, true)));
        assert_eq!(ascii_to_hid_us('\n'), Some((HidUsage::ENTER, false)));
        assert_eq!(ascii_to_hid_us('è'), None);
        for c in (0x20u8..0x7F).map(char::from) {
            assert!(ascii_to_hid_us(c).is_some(), "{c:?}");
        }
    }
}
