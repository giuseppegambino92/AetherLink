//! X11 XTest injector (`XTestFakeInput` via x11rb): the no-root fallback on X11 sessions when
//! `/dev/uinput` is not writable.
//!
//! * Pointer: absolute moves in root-window pixels (the space RandR / `aether-capture`'s X11
//!   backend report displays in), relative moves, buttons 1/2/3/8/9, wheel as buttons 4–7
//!   (one click per detent; pixel deltas are accumulated, 40 px ≈ one detent).
//! * Keys: X keycode = evdev code + 8 (the `evdev` XKB keycodes used by Xorg, Xwayland and
//!   Xvfb).
//! * Text: any Unicode character. Characters present in the current keymap are typed with
//!   their keycode (+ Shift for level 2); others are bound temporarily to a spare keycode via
//!   `ChangeKeyboardMapping` (the classic xdotool technique) and the spare is cleared on drop.
//! * On a Wayland session XTest only reaches Xwayland clients, so the portal backend is
//!   preferred there (see [`crate::linux_backend`]).

use std::collections::BTreeSet;
use std::time::Duration;

use aether_protocol::{DisplayInfo, Gesture, GesturePhase, HidUsage, InputEvent, Modifiers, MouseButton, ScrollUnit, SystemAction};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{self, ConnectionExt as _, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::state::{Accumulator, ModifierState, button_bit, display_pixel_rect, normalized_to_global, plan_key};
use crate::{InputError, InputInjector, Result, keymap};

/// evdev → X keycode offset.
const EVDEV_OFFSET: u16 = 8;
/// Pixel scroll → detents.
const PIXELS_PER_DETENT: f64 = 40.0;

fn x_err<E: std::fmt::Display>(e: E) -> InputError {
    InputError::Os(format!("X11: {e}"))
}

fn x_button(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
    }
}

/// X keysym for a character (`None` for control characters without a keysym).
pub fn char_to_keysym(c: char) -> Option<u32> {
    let cp = c as u32;
    match c {
        '\n' | '\r' => Some(0xFF0D), // Return
        '\t' => Some(0xFF09),        // Tab
        '\u{8}' => Some(0xFF08),     // BackSpace
        '\u{1b}' => Some(0xFF1B),    // Escape
        _ if c.is_control() => None,
        _ if (0x20..=0x7E).contains(&cp) || (0xA0..=0xFF).contains(&cp) => Some(cp),
        _ => Some(0x0100_0000 | cp),
    }
}

/// Snapshot of the server's keyboard mapping.
struct KeyMap {
    min: u8,
    per: usize,
    syms: Vec<u32>,
}

impl KeyMap {
    fn load(conn: &RustConnection) -> Result<Self> {
        let s = conn.setup();
        let (min, max) = (s.min_keycode, s.max_keycode);
        let r = conn.get_keyboard_mapping(min, max - min + 1).map_err(x_err)?.reply().map_err(x_err)?;
        Ok(Self { min, per: r.keysyms_per_keycode as usize, syms: r.keysyms })
    }

    fn count(&self) -> usize {
        self.syms.len().checked_div(self.per).unwrap_or(0)
    }

    /// (keycode, needs shift) producing `sym` at level 1 or 2.
    fn find(&self, sym: u32) -> Option<(u8, bool)> {
        (0..self.count()).find_map(|i| {
            let row = &self.syms[i * self.per..(i + 1) * self.per];
            let kc = self.min.checked_add(i as u8)?;
            if row.first() == Some(&sym) {
                Some((kc, false))
            } else if row.get(1) == Some(&sym) {
                Some((kc, true))
            } else {
                None
            }
        })
    }

    /// Highest keycode with no symbols at all (free for temporary bindings).
    fn spare(&self) -> Option<u8> {
        (0..self.count()).rev().find_map(|i| {
            let row = &self.syms[i * self.per..(i + 1) * self.per];
            row.iter().all(|s| *s == 0).then(|| self.min.checked_add(i as u8)).flatten()
        })
    }
}

/// XTest-based injector for X11 sessions.
pub struct XTestInjector {
    conn: RustConnection,
    root: Window,
    displays: Vec<DisplayInfo>,
    mods: ModifierState,
    keys_down: BTreeSet<u16>,
    buttons: u8,
    wheel_v: Accumulator,
    wheel_h: Accumulator,
    keymap: KeyMap,
    /// Spare keycode currently bound to a temporary keysym.
    scratch: Option<(u8, u32)>,
}

impl XTestInjector {
    /// Connects to `$DISPLAY` and checks the XTEST extension.
    pub fn new(displays: Vec<DisplayInfo>) -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| InputError::Unsupported(leak(format!("no X display: {e}"))))?;
        if conn.extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME).map_err(x_err)?.is_none() {
            return Err(InputError::Unsupported("the X server has no XTEST extension"));
        }
        conn.xtest_get_version(2, 2).map_err(x_err)?.reply().map_err(x_err)?;
        let root = conn.setup().roots[screen].root;
        let keymap = KeyMap::load(&conn)?;
        Ok(Self {
            conn,
            root,
            displays,
            mods: ModifierState::default(),
            keys_down: BTreeSet::new(),
            buttons: 0,
            wheel_v: Accumulator::default(),
            wheel_h: Accumulator::default(),
            keymap,
            scratch: None,
        })
    }

    fn fake(&self, ty: u8, detail: u8, x: i16, y: i16, root: Window) -> Result<()> {
        self.conn.xtest_fake_input(ty, detail, x11rb::CURRENT_TIME, root, x, y, 0).map_err(x_err)?;
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        self.conn.flush().map_err(x_err)
    }

    fn key_raw(&self, keycode: u8, pressed: bool) -> Result<()> {
        let ty = if pressed { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        self.fake(ty, keycode, 0, 0, x11rb::NONE)
    }

    fn evdev_keycode(u: HidUsage) -> Option<u8> {
        keymap::hid_to_linux_evdev(u).and_then(|k| u8::try_from(k + EVDEV_OFFSET).ok())
    }

    fn button(&mut self, b: MouseButton, pressed: bool) -> Result<()> {
        let bit = button_bit(b);
        if pressed {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        let ty = if pressed { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.fake(ty, x_button(b), 0, 0, x11rb::NONE)?;
        self.flush()
    }

    fn click(&self, button: u8, times: i64) -> Result<()> {
        for _ in 0..times {
            self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0, x11rb::NONE)?;
            self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0, x11rb::NONE)?;
        }
        Ok(())
    }

    fn scroll(&mut self, dx: f32, dy: f32, unit: ScrollUnit, phase: Option<GesturePhase>) -> Result<()> {
        if phase == Some(GesturePhase::Cancel) {
            self.wheel_v.reset();
            self.wheel_h.reset();
            return Ok(());
        }
        let k = match unit {
            ScrollUnit::Line => 1.0,
            ScrollUnit::Pixel => 1.0 / PIXELS_PER_DETENT,
        };
        // DOM sign: +dy = content moves up (scroll down) → button 5; +dx = right → button 7.
        let v = self.wheel_v.take(dy as f64 * k);
        let h = self.wheel_h.take(dx as f64 * k);
        self.click(if v > 0 { 5 } else { 4 }, v.abs())?;
        self.click(if h > 0 { 7 } else { 6 }, h.abs())?;
        self.flush()
    }

    fn key(&mut self, code: HidUsage, pressed: bool, modifiers: Modifiers) -> Result<()> {
        Self::evdev_keycode(code)
            .ok_or_else(|| InputError::InvalidArgument(format!("HID usage {:#04x} has no X keycode", code.0)))?;
        for s in plan_key(&mut self.mods, code, pressed, modifiers) {
            if !s.code.is_modifier() {
                if s.pressed {
                    self.keys_down.insert(s.code.0);
                } else {
                    self.keys_down.remove(&s.code.0);
                }
            }
            if let Some(kc) = Self::evdev_keycode(s.code) {
                self.key_raw(kc, s.pressed)?;
            }
        }
        self.flush()
    }

    fn combo(&mut self, mods: &[HidUsage], key: HidUsage) -> Result<()> {
        let added: Vec<u8> =
            mods.iter().filter(|m| !self.mods.is_down(**m)).filter_map(|m| Self::evdev_keycode(*m)).collect();
        let kc = Self::evdev_keycode(key).ok_or(InputError::InvalidArgument("unmapped combo key".into()))?;
        for m in &added {
            self.key_raw(*m, true)?;
        }
        self.key_raw(kc, true)?;
        self.key_raw(kc, false)?;
        for m in added.iter().rev() {
            self.key_raw(*m, false)?;
        }
        self.flush()
    }

    /// Binds `sym` to the spare keycode (both levels) and returns it.
    fn bind_scratch(&mut self, sym: u32) -> Result<u8> {
        if let Some((kc, s)) = self.scratch
            && s == sym
        {
            return Ok(kc);
        }
        let kc = match self.scratch {
            Some((kc, _)) => kc,
            None => self.keymap.spare().ok_or(InputError::Unsupported("no free X keycode to type this character"))?,
        };
        let per = self.keymap.per.max(2);
        let mut syms = vec![0u32; per];
        syms[0] = sym;
        syms[1] = sym;
        self.conn.change_keyboard_mapping(1, kc, per as u8, &syms).map_err(x_err)?.check().map_err(x_err)?;
        self.scratch = Some((kc, sym));
        // Give clients a moment to process the MappingNotify before the key arrives.
        std::thread::sleep(Duration::from_millis(15));
        Ok(kc)
    }

    fn clear_scratch(&mut self) {
        if let Some((kc, _)) = self.scratch.take() {
            let per = self.keymap.per.max(1);
            let _ = self.conn.change_keyboard_mapping(1, kc, per as u8, &vec![0u32; per]).map(|c| c.check());
        }
    }

    fn text(&mut self, text: &str) -> Result<()> {
        let shift = Self::evdev_keycode(HidUsage::SHIFT_LEFT).unwrap_or(50);
        let shift_held = self.mods.is_down(HidUsage::SHIFT_LEFT) || self.mods.is_down(HidUsage::SHIFT_RIGHT);
        for c in text.chars() {
            let Some(sym) = char_to_keysym(c) else { continue };
            let (kc, needs_shift) = match self.keymap.find(sym) {
                Some(found) => found,
                None => (self.bind_scratch(sym)?, false),
            };
            let toggle = needs_shift != shift_held;
            if toggle {
                self.key_raw(shift, needs_shift)?;
            }
            self.key_raw(kc, true)?;
            self.key_raw(kc, false)?;
            if toggle {
                self.key_raw(shift, shift_held)?;
            }
            self.flush()?;
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        for code in std::mem::take(&mut self.keys_down) {
            if let Some(kc) = Self::evdev_keycode(HidUsage(code)) {
                self.key_raw(kc, false)?;
            }
        }
        for m in self.mods.pressed() {
            if let Some(kc) = Self::evdev_keycode(m) {
                self.key_raw(kc, false)?;
            }
        }
        self.mods = ModifierState::default();
        for b in [MouseButton::Left, MouseButton::Right, MouseButton::Middle, MouseButton::Back, MouseButton::Forward] {
            if self.buttons & button_bit(b) != 0 {
                self.fake(xproto::BUTTON_RELEASE_EVENT, x_button(b), 0, 0, x11rb::NONE)?;
            }
        }
        self.buttons = 0;
        self.flush()
    }
}

impl Drop for XTestInjector {
    fn drop(&mut self) {
        let _ = self.release_all();
        self.clear_scratch();
        let _ = self.conn.flush();
    }
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

impl InputInjector for XTestInjector {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        match ev {
            InputEvent::MouseMove { display_id, x, y } => {
                let d = self.displays.iter().find(|d| d.id == *display_id).ok_or(InputError::UnknownDisplay(*display_id))?;
                let (gx, gy) = normalized_to_global(display_pixel_rect(d), *x, *y);
                let clamp = |v: f64| v.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                self.fake(xproto::MOTION_NOTIFY_EVENT, 0, clamp(gx), clamp(gy), self.root)?;
                self.flush()
            }
            InputEvent::MouseMoveRelative { dx, dy } => {
                let (ix, iy) = (dx.round() as i16, dy.round() as i16);
                if ix == 0 && iy == 0 {
                    return Ok(());
                }
                self.fake(xproto::MOTION_NOTIFY_EVENT, 1, ix, iy, x11rb::NONE)?;
                self.flush()
            }
            InputEvent::MouseButton { button, pressed } => self.button(*button, *pressed),
            InputEvent::Scroll { dx, dy, unit, phase } => self.scroll(*dx, *dy, *unit, *phase),
            InputEvent::Key { code, pressed, modifiers } => self.key(*code, *pressed, *modifiers),
            InputEvent::Text { text } => self.text(text),
            InputEvent::Gesture { gesture: Gesture::SwipeNavigate { dx, dy }, phase } => {
                if *phase != GesturePhase::End || dx.abs() <= dy.abs() {
                    return Ok(());
                }
                let key = if *dx > 0.0 { HidUsage::ARROW_LEFT } else { HidUsage::ARROW_RIGHT };
                self.combo(&[HidUsage::ALT_LEFT], key)
            }
            InputEvent::Gesture { .. } => Err(InputError::Unsupported("pinch/rotate gestures cannot be synthesized with XTest")),
            InputEvent::Touch { .. } => Err(InputError::Unsupported("touch injection is not available through XTest")),
            InputEvent::System(a) => match a {
                SystemAction::Screenshot => self.combo(&[], HidUsage::PRINT_SCREEN),
                SystemAction::LockScreen => self.combo(&[HidUsage::META_LEFT], HidUsage::L),
                SystemAction::ShowDesktop => self.combo(&[HidUsage::META_LEFT], HidUsage::D),
                SystemAction::MissionControl => {
                    let kc = Self::evdev_keycode(HidUsage::META_LEFT).unwrap_or(133);
                    self.key_raw(kc, true)?;
                    self.key_raw(kc, false)?;
                    self.flush()
                }
                SystemAction::CtrlAltDel => self.combo(&[HidUsage::CONTROL_LEFT, HidUsage::ALT_LEFT], HidUsage::DELETE),
                _ => Err(InputError::Unsupported("Android navigation actions are not available on Linux")),
            },
            InputEvent::ReleaseAll => self.release_all(),
        }
    }

    fn update_displays(&mut self, d: Vec<DisplayInfo>) {
        self.displays = d;
    }
}

#[cfg(test)]
mod tests {
    //! Live tests need an X server with XTEST (`Xvfb :99`, see ci/linux-test.sh).
    use super::*;
    use aether_protocol::Rect;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{CreateWindowAux, EventMask, InputFocus, WindowClass};
    use x11rb::wrapper::ConnectionExt as _;

    #[test]
    fn keysyms_for_text() {
        assert_eq!(char_to_keysym('a'), Some(0x61));
        assert_eq!(char_to_keysym('é'), Some(0xE9));
        assert_eq!(char_to_keysym('€'), Some(0x0100_20AC));
        assert_eq!(char_to_keysym('\n'), Some(0xFF0D));
        assert_eq!(char_to_keysym('\u{7}'), None);
    }

    fn screen_display(conn: &RustConnection) -> DisplayInfo {
        let s = &conn.setup().roots[0];
        DisplayInfo {
            id: 1,
            name: "screen".into(),
            bounds: Rect::new(0, 0, s.width_in_pixels as u32, s.height_in_pixels as u32),
            scale: 1.0,
            refresh_hz: 60.0,
            primary: true,
            is_virtual: false,
        }
    }

    #[test]
    #[ignore = "needs an X server with XTEST ($DISPLAY, e.g. Xvfb via ci/linux-test.sh)"]
    fn pointer_moves_and_keys_reach_a_window() {
        let (probe, _) = x11rb::connect(None).unwrap();
        let root = probe.setup().roots[0].root;
        let disp = screen_display(&probe);
        let mut inj = XTestInjector::new(vec![disp.clone()]).unwrap();

        // Absolute move: normalized (0.25, 0.5) of the screen.
        inj.inject(&InputEvent::MouseMove { display_id: 1, x: 0.25, y: 0.5 }).unwrap();
        probe.sync().unwrap();
        let p = probe.query_pointer(root).unwrap().reply().unwrap();
        assert_eq!((p.root_x as u32, p.root_y as u32), (disp.bounds.width / 4, disp.bounds.height / 2));
        // Relative move.
        inj.inject(&InputEvent::MouseMoveRelative { dx: 7.0, dy: -3.0 }).unwrap();
        probe.sync().unwrap();
        let q = probe.query_pointer(root).unwrap().reply().unwrap();
        assert_eq!((q.root_x - p.root_x, q.root_y - p.root_y), (7, -3));
        assert!(matches!(inj.inject(&InputEvent::MouseMove { display_id: 9, x: 0.0, y: 0.0 }), Err(InputError::UnknownDisplay(9))));

        // A focused window receives the synthesized keys, buttons and wheel.
        let win = probe.generate_id().unwrap();
        probe
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                win,
                root,
                0,
                0,
                200,
                200,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE),
            )
            .unwrap();
        probe.map_window(win).unwrap();
        probe.sync().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        probe.set_input_focus(InputFocus::POINTER_ROOT, win, x11rb::CURRENT_TIME).unwrap();
        probe.sync().unwrap();
        inj.inject(&InputEvent::MouseMove { display_id: 1, x: 50.0 / disp.bounds.width as f32, y: 50.0 / disp.bounds.height as f32 }).unwrap();
        inj.inject(&InputEvent::Key { code: HidUsage::A, pressed: true, modifiers: Modifiers::NONE }).unwrap();
        inj.inject(&InputEvent::Key { code: HidUsage::A, pressed: false, modifiers: Modifiers::NONE }).unwrap();
        inj.inject(&InputEvent::MouseButton { button: MouseButton::Right, pressed: true }).unwrap();
        inj.inject(&InputEvent::MouseButton { button: MouseButton::Right, pressed: false }).unwrap();
        inj.inject(&InputEvent::Scroll { dx: 0.0, dy: 2.0, unit: ScrollUnit::Line, phase: None }).unwrap();
        inj.inject(&InputEvent::Text { text: "Aé€".into() }).unwrap();
        probe.sync().unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let mut keys = Vec::new();
        let mut buttons = Vec::new();
        while let Some(ev) = probe.poll_for_event().unwrap() {
            match ev {
                Event::KeyPress(k) => keys.push((k.detail, k.state)),
                Event::ButtonPress(b) => buttons.push(b.detail),
                _ => {}
            }
        }
        println!("keys {keys:?} buttons {buttons:?}");
        // evdev KEY_A (30) + 8.
        assert_eq!(keys.first().map(|k| k.0), Some(38));
        assert_eq!(buttons, vec![3, 5, 5]);
        // "A" = Shift + a; then é and € through keymap lookup or the scratch keycode.
        let shift = keys.iter().position(|k| k.0 == 50).expect("shift pressed for 'A'");
        assert_eq!(keys[shift + 1].0, 38);
        assert!(keys.len() >= 5, "{keys:?}");
        // The keysym bound for '€' is visible in the server keymap while the injector lives.
        let map = KeyMap::load(&probe).unwrap();
        assert!(map.find(0x0100_20AC).is_some());
        drop(inj);
        probe.sync().unwrap();
        let map = KeyMap::load(&probe).unwrap();
        assert!(map.find(0x0100_20AC).is_none(), "scratch keycode restored on drop");
        probe.destroy_window(win).unwrap();
        probe.sync().unwrap();
    }
}
