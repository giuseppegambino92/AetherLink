//! Linux injector via `/dev/uinput` (works on X11 and Wayland alike, since the
//! events enter the kernel input layer like a real device).
//!
//! Two virtual devices are created:
//! * a keyboard + relative mouse (all mapped keys, 5 buttons, REL_X/Y,
//!   wheel and hi-res wheel),
//! * an absolute pointer whose ABS_X/ABS_Y range spans the union of the
//!   displays in physical pixels (compositors map absolute pointers onto the
//!   whole desktop layout).
//!
//! Needs write access to `/dev/uinput` (root or the `input`/`uinput` group).
//! Text is limited to US-layout ASCII because uinput only carries key codes.
//!
//! Compile-verified with `cargo check --target x86_64-unknown-linux-gnu`; not
//! run-tested from the macOS dev machine.

use crate::state::{button_bit, display_pixel_rect, normalized_to_global, plan_key, Accumulator, ModifierState};
use crate::{keymap, InputError, InputInjector, Result};
use aether_protocol::{
    DisplayInfo, Gesture, GesturePhase, HidUsage, InputEvent, Modifiers, MouseButton, ScrollUnit, SystemAction,
};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0B;
const REL_HWHEEL_HI_RES: u16 = 0x0C;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const BUS_VIRTUAL: u16 = 0x06;

// ioctl numbers from <linux/uinput.h> ('U' = 0x55).
const fn iow(nr: u32, size: usize) -> u32 {
    (1 << 30) | ((size as u32) << 16) | (0x55 << 8) | nr
}
const UI_DEV_CREATE: u32 = 0x5501;
const UI_DEV_DESTROY: u32 = 0x5502;
const UI_DEV_SETUP: u32 = iow(3, std::mem::size_of::<libc::uinput_setup>());
const UI_ABS_SETUP: u32 = iow(4, std::mem::size_of::<libc::uinput_abs_setup>());
const UI_SET_EVBIT: u32 = iow(100, 4);
const UI_SET_KEYBIT: u32 = iow(101, 4);
const UI_SET_RELBIT: u32 = iow(102, 4);
const UI_SET_ABSBIT: u32 = iow(103, 4);

/// Hi-res wheel units per detent (kernel convention).
const HI_RES_PER_DETENT: f64 = 120.0;
/// `ScrollUnit::Pixel` → hi-res units (one detent ≈ 40 px).
const HI_RES_PER_PIXEL: f64 = 3.0;

fn button_code(b: MouseButton) -> u16 {
    match b {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Back => BTN_SIDE,
        MouseButton::Forward => BTN_EXTRA,
    }
}

struct Device {
    file: File,
}

impl Device {
    fn ioctl_int(&self, req: u32, arg: libc::c_int) -> Result<()> {
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), req as _, arg) };
        if r < 0 { Err(std::io::Error::last_os_error().into()) } else { Ok(()) }
    }

    fn ioctl_ptr<T>(&self, req: u32, arg: &T) -> Result<()> {
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), req as _, arg as *const T) };
        if r < 0 { Err(std::io::Error::last_os_error().into()) } else { Ok(()) }
    }

    fn open(name: &str, product: u16, setup: impl FnOnce(&Device) -> Result<()>) -> Result<Device> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/uinput")
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    InputError::PermissionDenied("no write access to /dev/uinput (add the user to the input group)")
                } else {
                    InputError::Io(e)
                }
            })?;
        let dev = Device { file };
        setup(&dev)?;
        let mut s: libc::uinput_setup = unsafe { std::mem::zeroed() };
        s.id = libc::input_id { bustype: BUS_VIRTUAL, vendor: 0x1209, product, version: 1 };
        for (d, c) in s.name.iter_mut().zip(name.bytes().take(libc::UINPUT_MAX_NAME_SIZE - 1)) {
            *d = c as libc::c_char;
        }
        dev.ioctl_ptr(UI_DEV_SETUP, &s)?;
        dev.ioctl_int(UI_DEV_CREATE, 0)?;
        Ok(dev)
    }

    fn emit(&mut self, events: &[(u16, u16, i32)]) -> Result<()> {
        let mut buf = Vec::with_capacity((events.len() + 1) * std::mem::size_of::<libc::input_event>());
        for &(ty, code, value) in events.iter().chain(std::iter::once(&(EV_SYN, SYN_REPORT, 0))) {
            let mut ev: libc::input_event = unsafe { std::mem::zeroed() };
            ev.type_ = ty;
            ev.code = code;
            ev.value = value;
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (&ev as *const libc::input_event).cast::<u8>(),
                    std::mem::size_of::<libc::input_event>(),
                )
            };
            buf.extend_from_slice(bytes);
        }
        self.file.write_all(&buf)?;
        Ok(())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.ioctl_int(UI_DEV_DESTROY, 0);
    }
}

fn union_rect(displays: &[DisplayInfo]) -> Option<(i32, i32, i32, i32)> {
    let mut it = displays.iter();
    let first = it.next()?;
    let mut u = first.bounds;
    for d in it {
        u = u.union(&d.bounds);
    }
    Some((u.x, u.y, u.width.max(1) as i32, u.height.max(1) as i32))
}

pub struct UinputInjector {
    kbd: Device,
    abs: Option<(Device, (i32, i32, i32, i32))>,
    displays: Vec<DisplayInfo>,
    mods: ModifierState,
    keys_down: BTreeSet<u16>,
    buttons: u8,
    last_abs: bool,
    rel_x: Accumulator,
    rel_y: Accumulator,
    wheel_v: Accumulator,
    wheel_h: Accumulator,
    detent_v: Accumulator,
    detent_h: Accumulator,
}

impl UinputInjector {
    pub fn new(displays: Vec<DisplayInfo>) -> Result<Self> {
        let kbd = Device::open("AetherLink Virtual Keyboard/Mouse", 0xAE70, |d| {
            for ev in [EV_KEY, EV_REL, EV_SYN] {
                d.ioctl_int(UI_SET_EVBIT, ev as _)?;
            }
            for u in keymap::all_mapped_usages() {
                if let Some(k) = keymap::hid_to_linux_evdev(u) {
                    d.ioctl_int(UI_SET_KEYBIT, k as _)?;
                }
            }
            for b in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                d.ioctl_int(UI_SET_KEYBIT, b as _)?;
            }
            for r in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
                d.ioctl_int(UI_SET_RELBIT, r as _)?;
            }
            Ok(())
        })?;
        let mut s = Self {
            kbd,
            abs: None,
            displays: Vec::new(),
            mods: ModifierState::default(),
            keys_down: BTreeSet::new(),
            buttons: 0,
            last_abs: false,
            rel_x: Accumulator::default(),
            rel_y: Accumulator::default(),
            wheel_v: Accumulator::default(),
            wheel_h: Accumulator::default(),
            detent_v: Accumulator::default(),
            detent_h: Accumulator::default(),
        };
        s.set_displays(displays)?;
        Ok(s)
    }

    fn set_displays(&mut self, displays: Vec<DisplayInfo>) -> Result<()> {
        let rect = union_rect(&displays);
        self.displays = displays;
        if self.abs.as_ref().map(|(_, r)| *r) == rect {
            return Ok(());
        }
        self.abs = None;
        let Some(rect) = rect else { return Ok(()) };
        let dev = Device::open("AetherLink Virtual Pointer", 0xAE71, |d| {
            for ev in [EV_KEY, EV_ABS, EV_SYN] {
                d.ioctl_int(UI_SET_EVBIT, ev as _)?;
            }
            for b in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                d.ioctl_int(UI_SET_KEYBIT, b as _)?;
            }
            for (code, len) in [(ABS_X, rect.2), (ABS_Y, rect.3)] {
                d.ioctl_int(UI_SET_ABSBIT, code as _)?;
                let mut a: libc::uinput_abs_setup = unsafe { std::mem::zeroed() };
                a.code = code;
                a.absinfo.minimum = 0;
                a.absinfo.maximum = (len - 1).max(1);
                d.ioctl_ptr(UI_ABS_SETUP, &a)?;
            }
            Ok(())
        })?;
        self.abs = Some((dev, rect));
        Ok(())
    }

    fn key_events(&mut self, code: HidUsage, pressed: bool, modifiers: Modifiers) -> Result<Vec<(u16, u16, i32)>> {
        keymap::hid_to_linux_evdev(code)
            .ok_or_else(|| InputError::InvalidArgument(format!("HID usage {:#04x} has no evdev code", code.0)))?;
        let mut evs = Vec::new();
        for s in plan_key(&mut self.mods, code, pressed, modifiers) {
            if !s.code.is_modifier() {
                if s.pressed {
                    self.keys_down.insert(s.code.0);
                } else {
                    self.keys_down.remove(&s.code.0);
                }
            }
            if let Some(k) = keymap::hid_to_linux_evdev(s.code) {
                evs.push((EV_KEY, k, s.pressed as i32));
            }
        }
        Ok(evs)
    }

    fn combo(&mut self, mods: &[HidUsage], key: HidUsage) -> Result<()> {
        let code = |u: HidUsage| keymap::hid_to_linux_evdev(u).unwrap_or(0);
        let added: Vec<HidUsage> = mods.iter().copied().filter(|m| !self.mods.is_down(*m)).collect();
        let mut evs: Vec<(u16, u16, i32)> = added.iter().map(|&m| (EV_KEY, code(m), 1)).collect();
        self.kbd.emit(&evs)?;
        self.kbd.emit(&[(EV_KEY, code(key), 1)])?;
        self.kbd.emit(&[(EV_KEY, code(key), 0)])?;
        evs = added.iter().rev().map(|&m| (EV_KEY, code(m), 0)).collect();
        self.kbd.emit(&evs)
    }

    fn scroll(&mut self, dx: f32, dy: f32, unit: ScrollUnit, phase: Option<GesturePhase>) -> Result<()> {
        if phase == Some(GesturePhase::Cancel) {
            for a in [&mut self.wheel_v, &mut self.wheel_h, &mut self.detent_v, &mut self.detent_h] {
                a.reset();
            }
            return Ok(());
        }
        let k = match unit {
            ScrollUnit::Line => HI_RES_PER_DETENT,
            ScrollUnit::Pixel => HI_RES_PER_PIXEL,
        };
        // DOM sign (+dy = down) → evdev (+wheel = up); +dx = right for both.
        let v = self.wheel_v.take(-(dy as f64) * k);
        let h = self.wheel_h.take(dx as f64 * k);
        let dv = self.detent_v.take(v as f64 / HI_RES_PER_DETENT);
        let dh = self.detent_h.take(h as f64 / HI_RES_PER_DETENT);
        let mut evs = Vec::new();
        if v != 0 {
            evs.push((EV_REL, REL_WHEEL_HI_RES, v as i32));
        }
        if dv != 0 {
            evs.push((EV_REL, REL_WHEEL, dv as i32));
        }
        if h != 0 {
            evs.push((EV_REL, REL_HWHEEL_HI_RES, h as i32));
        }
        if dh != 0 {
            evs.push((EV_REL, REL_HWHEEL, dh as i32));
        }
        if evs.is_empty() { Ok(()) } else { self.kbd.emit(&evs) }
    }

    fn text(&mut self, text: &str) -> Result<()> {
        let mapped: Vec<(HidUsage, bool)> = text.chars().map(keymap::ascii_to_hid_us).collect::<Option<_>>().ok_or(
            InputError::Unsupported("uinput can only type US-layout ASCII; non-ASCII text needs an IME/portal"),
        )?;
        for (u, shift) in mapped {
            let mods: &[HidUsage] = if shift { &[HidUsage::SHIFT_LEFT] } else { &[] };
            self.combo(mods, u)?;
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let mut evs = Vec::new();
        for code in std::mem::take(&mut self.keys_down) {
            if let Some(k) = keymap::hid_to_linux_evdev(HidUsage(code)) {
                evs.push((EV_KEY, k, 0));
            }
        }
        for m in self.mods.pressed() {
            evs.push((EV_KEY, keymap::hid_to_linux_evdev(m).unwrap_or(0), 0));
        }
        self.mods = ModifierState::default();
        let mut btn = Vec::new();
        for b in [MouseButton::Left, MouseButton::Right, MouseButton::Middle, MouseButton::Back, MouseButton::Forward] {
            if self.buttons & button_bit(b) != 0 {
                btn.push((EV_KEY, button_code(b), 0));
            }
        }
        self.buttons = 0;
        self.kbd.emit(&evs)?;
        self.kbd.emit(&btn)?;
        if let Some((d, _)) = self.abs.as_mut() {
            d.emit(&btn)?;
        }
        Ok(())
    }
}

impl InputInjector for UinputInjector {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        match ev {
            InputEvent::MouseMove { display_id, x, y } => {
                let d = self
                    .displays
                    .iter()
                    .find(|d| d.id == *display_id)
                    .ok_or(InputError::UnknownDisplay(*display_id))?;
                let (gx, gy) = normalized_to_global(display_pixel_rect(d), *x, *y);
                let (dev, (ux, uy, _, _)) = self.abs.as_mut().ok_or(InputError::UnknownDisplay(*display_id))?;
                self.last_abs = true;
                dev.emit(&[(EV_ABS, ABS_X, (gx as i32) - *ux), (EV_ABS, ABS_Y, (gy as i32) - *uy)])
            }
            InputEvent::MouseMoveRelative { dx, dy } => {
                let ix = self.rel_x.take(*dx as f64) as i32;
                let iy = self.rel_y.take(*dy as f64) as i32;
                self.last_abs = false;
                let mut evs = Vec::new();
                if ix != 0 {
                    evs.push((EV_REL, REL_X, ix));
                }
                if iy != 0 {
                    evs.push((EV_REL, REL_Y, iy));
                }
                if evs.is_empty() { Ok(()) } else { self.kbd.emit(&evs) }
            }
            InputEvent::MouseButton { button, pressed } => {
                let bit = button_bit(*button);
                if *pressed {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                let e = [(EV_KEY, button_code(*button), *pressed as i32)];
                match (self.last_abs, self.abs.as_mut()) {
                    (true, Some((d, _))) => d.emit(&e),
                    _ => self.kbd.emit(&e),
                }
            }
            InputEvent::Scroll { dx, dy, unit, phase } => self.scroll(*dx, *dy, *unit, *phase),
            InputEvent::Key { code, pressed, modifiers } => {
                let evs = self.key_events(*code, *pressed, *modifiers)?;
                self.kbd.emit(&evs)
            }
            InputEvent::Text { text } => self.text(text),
            InputEvent::Gesture { gesture: Gesture::SwipeNavigate { dx, dy }, phase } => {
                if *phase != GesturePhase::End || dx.abs() <= dy.abs() {
                    return Ok(());
                }
                let key = if *dx > 0.0 { HidUsage::ARROW_LEFT } else { HidUsage::ARROW_RIGHT };
                self.combo(&[HidUsage::ALT_LEFT], key)
            }
            InputEvent::Gesture { .. } => {
                Err(InputError::Unsupported("pinch/rotate need a multitouch touchpad device; not emulated"))
            }
            InputEvent::Touch { .. } => {
                Err(InputError::Unsupported("touch injection needs a multitouch uinput device; not implemented"))
            }
            InputEvent::System(a) => match a {
                SystemAction::Screenshot => self.combo(&[], HidUsage::PRINT_SCREEN),
                // GNOME / KDE defaults.
                SystemAction::LockScreen => self.combo(&[HidUsage::META_LEFT], HidUsage::L),
                SystemAction::ShowDesktop => self.combo(&[HidUsage::META_LEFT], HidUsage::D),
                SystemAction::MissionControl => {
                    let k = keymap::hid_to_linux_evdev(HidUsage::META_LEFT).unwrap_or(125);
                    self.kbd.emit(&[(EV_KEY, k, 1)])?;
                    self.kbd.emit(&[(EV_KEY, k, 0)])
                }
                SystemAction::CtrlAltDel => {
                    self.combo(&[HidUsage::CONTROL_LEFT, HidUsage::ALT_LEFT], HidUsage::DELETE)
                }
                _ => Err(InputError::Unsupported("Android navigation actions are not available on Linux")),
            },
            InputEvent::ReleaseAll => self.release_all(),
        }
    }

    fn update_displays(&mut self, d: Vec<DisplayInfo>) {
        // On failure the absolute device is dropped (set_displays clears it
        // first); the keyboard keeps working and absolute moves report
        // UnknownDisplay.
        let _ = self.set_displays(d);
    }
}

#[cfg(test)]
mod tests {
    //! Live test: needs `/dev/uinput` and permission to open the created evdev node
    //! (Docker: `--device /dev/uinput --device-cgroup-rule='c 13:* rmw'`, see ci/linux-test.sh).
    //! The node is located through sysfs and created with mknod when /dev/input is not
    //! populated (no udev in containers).
    use super::*;
    use aether_protocol::Rect;
    use std::io::Read;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn event_node(name: &str) -> Option<PathBuf> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            for e in std::fs::read_dir("/sys/class/input").ok()?.flatten() {
                let fname = e.file_name().to_string_lossy().into_owned();
                if !fname.starts_with("event") {
                    continue;
                }
                let dev_name = std::fs::read_to_string(e.path().join("device/name")).unwrap_or_default();
                if dev_name.trim() != name {
                    continue;
                }
                let dev = PathBuf::from("/dev/input").join(&fname);
                if dev.exists() {
                    return Some(dev);
                }
                let mm = std::fs::read_to_string(e.path().join("dev")).ok()?;
                let (maj, min) = mm.trim().split_once(':')?;
                let node = std::env::temp_dir().join(format!("aether-{fname}-{}", std::process::id()));
                let c = std::ffi::CString::new(node.to_str()?).ok()?;
                let devt = libc::makedev(maj.parse().ok()?, min.parse().ok()?);
                // SAFETY: mknod with a valid path; failure is reported below.
                if unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR | 0o600, devt) } == 0 {
                    return Some(node);
                }
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    fn read_events(f: &mut File, want: usize) -> Vec<(u16, u16, i32)> {
        let mut out = Vec::new();
        let sz = std::mem::size_of::<libc::input_event>();
        let mut buf = vec![0u8; sz * 64];
        let deadline = Instant::now() + Duration::from_secs(3);
        while out.len() < want && Instant::now() < deadline {
            match f.read(&mut buf) {
                Ok(n) => {
                    for chunk in buf[..n].chunks_exact(sz) {
                        // SAFETY: the kernel writes whole input_event structs.
                        let ev: libc::input_event = unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                        if ev.type_ != EV_SYN {
                            out.push((ev.type_, ev.code, ev.value));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => panic!("read evdev: {e}"),
            }
        }
        out
    }

    #[test]
    #[ignore = "needs /dev/uinput (+ access to the created evdev node), see ci/linux-test.sh"]
    fn uinput_devices_emit_real_evdev_events() {
        let disp = DisplayInfo {
            id: 1,
            name: "test".into(),
            bounds: Rect::new(0, 0, 1000, 500),
            scale: 1.0,
            refresh_hz: 60.0,
            primary: true,
            is_virtual: false,
        };
        let mut inj = UinputInjector::new(vec![disp]).expect("open /dev/uinput");
        let kbd_node = event_node("AetherLink Virtual Keyboard/Mouse").expect("keyboard evdev node");
        let abs_node = event_node("AetherLink Virtual Pointer").expect("pointer evdev node");
        let open = |p: &PathBuf| OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(p).expect("open evdev node");
        let (mut kbd, mut abs) = (open(&kbd_node), open(&abs_node));
        std::thread::sleep(Duration::from_millis(100));

        inj.inject(&InputEvent::Key { code: HidUsage::A, pressed: true, modifiers: Modifiers::SHIFT }).unwrap();
        inj.inject(&InputEvent::Key { code: HidUsage::A, pressed: false, modifiers: Modifiers::SHIFT }).unwrap();
        inj.inject(&InputEvent::Scroll { dx: 0.0, dy: 1.0, unit: ScrollUnit::Line, phase: None }).unwrap();
        let k = read_events(&mut kbd, 5);
        println!("keyboard events: {k:?}");
        // Shift is synthesized from the modifier state, then KEY_A (30) down/up.
        assert_eq!(k[0], (EV_KEY, 42, 1));
        assert!(k.contains(&(EV_KEY, 30, 1)) && k.contains(&(EV_KEY, 30, 0)));
        assert!(k.contains(&(EV_REL, REL_WHEEL, -1)), "{k:?}");
        assert!(k.contains(&(EV_REL, REL_WHEEL_HI_RES, -120)), "{k:?}");

        inj.inject(&InputEvent::MouseMove { display_id: 1, x: 0.5, y: 0.25 }).unwrap();
        inj.inject(&InputEvent::MouseButton { button: MouseButton::Left, pressed: true }).unwrap();
        inj.inject(&InputEvent::ReleaseAll).unwrap();
        let a = read_events(&mut abs, 4);
        println!("pointer events: {a:?}");
        assert_eq!(&a[..2], &[(EV_ABS, ABS_X, 500), (EV_ABS, ABS_Y, 125)]);
        assert!(a.contains(&(EV_KEY, BTN_LEFT, 1)) && a.contains(&(EV_KEY, BTN_LEFT, 0)), "{a:?}");
        for n in [kbd_node, abs_node] {
            if n.starts_with(std::env::temp_dir()) {
                let _ = std::fs::remove_file(n);
            }
        }
    }
}
