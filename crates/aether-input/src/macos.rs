//! macOS injector built on Quartz Event Services (`CGEvent`).
//!
//! * Events are posted to `kCGHIDEventTap`, i.e. they enter the system exactly
//!   where hardware events do (requires the Accessibility permission).
//! * Every event carries [`INJECTED_EVENT_TAG`] in `kCGEventSourceUserData`
//!   (field 42) so our own event taps can filter it.
//! * CGEvent coordinates are global **points** (top-left of the main display is
//!   the origin); [`DisplayInfo`] bounds are physical pixels, so displays are
//!   resolved through `CGDisplayBounds` whenever the id is a live
//!   `CGDirectDisplayID`, falling back to `bounds / scale`.
//!
//! Not possible with public API (returns [`InputError::Unsupported`]):
//! magnify / rotate / smart-zoom gestures (they need the private
//! `kCGSEventGesture` event type and undocumented fields), touch injection,
//! the Windows Secure Attention Sequence and Android navigation actions.

use crate::state::{
    self, button_bit, clamp_to_displays, normalized_to_global, plan_key, Accumulator, ClickTracker, FRect,
    ModifierState,
};
use crate::{keymap, InputError, InputInjector, Result, INJECTED_EVENT_TAG};
use aether_protocol::{
    DisplayInfo, Gesture, GesturePhase, HidUsage, InputEvent, Modifiers, MouseButton, ScrollUnit, SystemAction,
};
use core_foundation::base::TCFType;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    fn CGEventSourceSetUserData(source: CGEventSourceRef, data: i64);
    fn CGEventSourceSetLocalEventsSuppressionInterval(source: CGEventSourceRef, seconds: f64);
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    fn CGEventCreateMouseEvent(source: CGEventSourceRef, ty: u32, pos: CGPoint, button: u32) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, key: u16, down: bool) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventGetLocation(ev: CGEventRef) -> CGPoint;
    fn CGEventSetType(ev: CGEventRef, ty: u32);
    fn CGEventSetFlags(ev: CGEventRef, flags: u64);
    fn CGEventSetIntegerValueField(ev: CGEventRef, field: u32, value: i64);
    fn CGEventKeyboardSetUnicodeString(ev: CGEventRef, len: std::ffi::c_ulong, s: *const u16);
    fn CGEventPost(tap: u32, ev: CGEventRef);
    pub(crate) fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
}

// CGEventType
const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOUSE_MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;

// CGEventField
const F_CLICK_STATE: u32 = 1;
const F_BUTTON_NUMBER: u32 = 3;
const F_DELTA_X: u32 = 4;
const F_DELTA_Y: u32 = 5;
const F_SOURCE_USER_DATA: u32 = 42;
const F_SCROLL_IS_CONTINUOUS: u32 = 88;
const F_SCROLL_PHASE: u32 = 99;
const F_SCROLL_MOMENTUM_PHASE: u32 = 123;

// CGEventFlags
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALTERNATE: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
const FLAG_NUMERIC_PAD: u64 = 0x0020_0000;
const FLAG_SECONDARY_FN: u64 = 0x0080_0000;
const FLAG_NON_COALESCED: u64 = 0x0000_0100;
// NX_DEVICE* (IOLLEvent.h) — left/right specific bits.
const DEV_LCTL: u64 = 0x0001;
const DEV_LSHIFT: u64 = 0x0002;
const DEV_RSHIFT: u64 = 0x0004;
const DEV_LCMD: u64 = 0x0008;
const DEV_RCMD: u64 = 0x0010;
const DEV_LALT: u64 = 0x0020;
const DEV_RALT: u64 = 0x0040;
const DEV_RCTL: u64 = 0x2000;

const HID_EVENT_TAP: u32 = 0;
const SOURCE_STATE_HID: i32 = 1;
const SCROLL_UNIT_PIXEL: u32 = 0;
const SCROLL_UNIT_LINE: u32 = 1;

// CGScrollPhase / CGMomentumScrollPhase
const SCROLL_PHASE_BEGAN: i64 = 1;
const SCROLL_PHASE_CHANGED: i64 = 2;
const SCROLL_PHASE_ENDED: i64 = 4;
const SCROLL_PHASE_CANCELLED: i64 = 8;
const MOMENTUM_BEGIN: i64 = 1;
const MOMENTUM_CONTINUE: i64 = 2;
const MOMENTUM_END: i64 = 3;

/// Unicode chunk size accepted by `CGEventKeyboardSetUnicodeString`.
const UNICODE_CHUNK: usize = 20;

/// Quartz event flags for a modifier state (device-independent masks plus the
/// left/right device bits real keyboards set).
pub fn cg_flags(m: ModifierState) -> u64 {
    let mut f = FLAG_NON_COALESCED;
    let pairs = [
        (HidUsage::CONTROL_LEFT, FLAG_CONTROL, DEV_LCTL),
        (HidUsage::CONTROL_RIGHT, FLAG_CONTROL, DEV_RCTL),
        (HidUsage::SHIFT_LEFT, FLAG_SHIFT, DEV_LSHIFT),
        (HidUsage::SHIFT_RIGHT, FLAG_SHIFT, DEV_RSHIFT),
        (HidUsage::ALT_LEFT, FLAG_ALTERNATE, DEV_LALT),
        (HidUsage::ALT_RIGHT, FLAG_ALTERNATE, DEV_RALT),
        (HidUsage::META_LEFT, FLAG_COMMAND, DEV_LCMD),
        (HidUsage::META_RIGHT, FLAG_COMMAND, DEV_RCMD),
    ];
    for (u, mask, dev) in pairs {
        if m.is_down(u) {
            f |= mask | dev;
        }
    }
    f
}

/// Extra flags real Apple keyboards attach to specific keys.
pub fn key_extra_flags(u: HidUsage) -> u64 {
    match u.0 {
        0x4F..=0x52 => FLAG_NUMERIC_PAD | FLAG_SECONDARY_FN, // arrows
        0x3A..=0x45 | 0x68..=0x73 | 0x49..=0x4E | 0x75 => FLAG_SECONDARY_FN, // F-keys, nav cluster, help
        0x53..=0x63 | 0x67 | 0x85 => FLAG_NUMERIC_PAD,                         // keypad
        _ => 0,
    }
}

/// Split text into `CGEventKeyboardSetUnicodeString`-sized UTF-16 chunks,
/// never splitting a surrogate pair. `\n`/`\r`/`\t` become separate
/// [`TextPiece::Key`] entries because many apps ignore them as Unicode payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextPiece {
    Unicode(Vec<u16>),
    Key(HidUsage),
}

pub fn chunk_text(text: &str) -> Vec<TextPiece> {
    let mut out = Vec::new();
    let mut cur: Vec<u16> = Vec::new();
    let mut last_was_cr = false;
    for c in text.chars() {
        let key = match c {
            '\r' => Some(HidUsage::ENTER),
            '\n' if last_was_cr => {
                last_was_cr = false;
                continue;
            }
            '\n' => Some(HidUsage::ENTER),
            '\t' => Some(HidUsage::TAB),
            _ => None,
        };
        last_was_cr = c == '\r';
        if let Some(k) = key {
            if !cur.is_empty() {
                out.push(TextPiece::Unicode(std::mem::take(&mut cur)));
            }
            out.push(TextPiece::Key(k));
            continue;
        }
        let mut buf = [0u16; 2];
        let units = c.encode_utf16(&mut buf);
        if cur.len() + units.len() > UNICODE_CHUNK {
            out.push(TextPiece::Unicode(std::mem::take(&mut cur)));
        }
        cur.extend_from_slice(units);
    }
    if !cur.is_empty() {
        out.push(TextPiece::Unicode(cur));
    }
    out
}

/// Owned CGEvent.
struct Ev(CGEventRef);
impl Drop for Ev {
    fn drop(&mut self) {
        unsafe { core_foundation_sys::base::CFRelease(self.0 as _) }
    }
}

fn check(ev: CGEventRef, what: &'static str) -> Result<Ev> {
    if ev.is_null() { Err(InputError::Os(format!("{what} returned NULL"))) } else { Ok(Ev(ev)) }
}

/// The active `CGDirectDisplayID`s.
pub fn active_displays() -> Vec<u32> {
    let mut ids = [0u32; 32];
    let mut n = 0u32;
    let err = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n) };
    if err != 0 {
        return Vec::new();
    }
    ids[..n as usize].to_vec()
}

/// Bounds of a live display in global points, if `id` is an active display.
pub fn display_bounds_points(id: u32) -> Option<FRect> {
    if !active_displays().contains(&id) {
        return None;
    }
    let r = unsafe { CGDisplayBounds(id) };
    (r.size.width > 0.0 && r.size.height > 0.0).then_some(FRect {
        x: r.origin.x,
        y: r.origin.y,
        w: r.size.width,
        h: r.size.height,
    })
}

fn double_click_interval() -> Duration {
    use core_foundation_sys::preferences::{kCFPreferencesAnyApplication, CFPreferencesCopyAppValue};
    let key = CFString::from_static_string("com.apple.mouse.doubleClickThreshold");
    let v = unsafe { CFPreferencesCopyAppValue(key.as_concrete_TypeRef(), kCFPreferencesAnyApplication) };
    if v.is_null() {
        return Duration::from_millis(500);
    }
    let any = unsafe { core_foundation::base::CFType::wrap_under_create_rule(v) };
    any.downcast::<CFNumber>()
        .and_then(|n| n.to_f64())
        .filter(|s| (0.05..=5.0).contains(s))
        .map(Duration::from_secs_f64)
        .unwrap_or(Duration::from_millis(500))
}

pub struct MacInjector {
    source: CGEventSourceRef,
    displays: Vec<DisplayInfo>,
    pos: CGPoint,
    mods: ModifierState,
    keys_down: BTreeSet<u16>,
    buttons: u8,
    clicks: ClickTracker,
    scroll_x: Accumulator,
    scroll_y: Accumulator,
    in_momentum: bool,
    scroll_gesture_active: bool,
}

// The event source is a CF object with thread-safe reference counting; the
// injector is only ever used from one thread at a time (`&mut self`).
unsafe impl Send for MacInjector {}

impl Drop for MacInjector {
    fn drop(&mut self) {
        if !self.source.is_null() {
            unsafe { core_foundation_sys::base::CFRelease(self.source as _) }
        }
    }
}

impl MacInjector {
    pub fn new(displays: Vec<DisplayInfo>) -> Result<Self> {
        let source = unsafe { CGEventSourceCreate(SOURCE_STATE_HID) };
        if source.is_null() {
            return Err(InputError::Os("CGEventSourceCreate failed".into()));
        }
        unsafe {
            CGEventSourceSetUserData(source, INJECTED_EVENT_TAG);
            // Do not swallow physical input after our synthetic moves.
            CGEventSourceSetLocalEventsSuppressionInterval(source, 0.0);
        }
        let pos = unsafe {
            let e = CGEventCreate(std::ptr::null_mut());
            if e.is_null() {
                CGPoint::default()
            } else {
                let p = CGEventGetLocation(e);
                core_foundation_sys::base::CFRelease(e as _);
                p
            }
        };
        Ok(Self {
            source,
            displays,
            pos,
            mods: ModifierState::default(),
            keys_down: BTreeSet::new(),
            buttons: 0,
            clicks: ClickTracker::new(double_click_interval(), 5.0),
            scroll_x: Accumulator::default(),
            scroll_y: Accumulator::default(),
            in_momentum: false,
            scroll_gesture_active: false,
        })
    }

    fn post(&self, ev: Ev) {
        unsafe {
            CGEventSetIntegerValueField(ev.0, F_SOURCE_USER_DATA, INJECTED_EVENT_TAG);
            CGEventPost(HID_EVENT_TAP, ev.0);
        }
    }

    fn display_rect(&self, id: u32) -> Result<FRect> {
        if let Some(r) = display_bounds_points(id) {
            return Ok(r);
        }
        self.displays
            .iter()
            .find(|d| d.id == id)
            .map(state::display_points_rect)
            .ok_or(InputError::UnknownDisplay(id))
    }

    fn all_rects(&self) -> Vec<FRect> {
        let live: Vec<FRect> = active_displays().into_iter().filter_map(display_bounds_points).collect();
        if !live.is_empty() {
            return live;
        }
        self.displays.iter().map(state::display_points_rect).collect()
    }

    fn mouse_event(&self, ty: u32, button: u32, click_state: Option<i64>) -> Result<Ev> {
        let ev = check(unsafe { CGEventCreateMouseEvent(self.source, ty, self.pos, button) }, "CGEventCreateMouseEvent")?;
        unsafe {
            CGEventSetFlags(ev.0, cg_flags(self.mods));
            if let Some(c) = click_state {
                CGEventSetIntegerValueField(ev.0, F_CLICK_STATE, c);
            }
            if button >= 2 {
                CGEventSetIntegerValueField(ev.0, F_BUTTON_NUMBER, button as i64);
            }
        }
        Ok(ev)
    }

    fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        let (dx, dy) = (x - self.pos.x, y - self.pos.y);
        self.pos = CGPoint { x, y };
        let (ty, button) = if self.buttons & button_bit(MouseButton::Left) != 0 {
            (LEFT_DRAGGED, 0)
        } else if self.buttons & button_bit(MouseButton::Right) != 0 {
            (RIGHT_DRAGGED, 1)
        } else if self.buttons & button_bit(MouseButton::Middle) != 0 {
            (OTHER_DRAGGED, 2)
        } else if self.buttons & button_bit(MouseButton::Back) != 0 {
            (OTHER_DRAGGED, 3)
        } else if self.buttons & button_bit(MouseButton::Forward) != 0 {
            (OTHER_DRAGGED, 4)
        } else {
            (MOUSE_MOVED, 0)
        };
        let click = (ty != MOUSE_MOVED).then(|| self.clicks.current());
        let ev = self.mouse_event(ty, button, click)?;
        unsafe {
            CGEventSetIntegerValueField(ev.0, F_DELTA_X, dx.round() as i64);
            CGEventSetIntegerValueField(ev.0, F_DELTA_Y, dy.round() as i64);
        }
        self.post(ev);
        Ok(())
    }

    fn button(&mut self, b: MouseButton, pressed: bool) -> Result<()> {
        let bit = button_bit(b);
        if pressed && self.buttons & bit != 0 || !pressed && self.buttons & bit == 0 {
            return Ok(()); // already in that state; avoid unbalanced events
        }
        let (down, up, num) = match b {
            MouseButton::Left => (LEFT_DOWN, LEFT_UP, 0),
            MouseButton::Right => (RIGHT_DOWN, RIGHT_UP, 1),
            MouseButton::Middle => (OTHER_DOWN, OTHER_UP, 2),
            MouseButton::Back => (OTHER_DOWN, OTHER_UP, 3),
            MouseButton::Forward => (OTHER_DOWN, OTHER_UP, 4),
        };
        let clicks = if pressed { self.clicks.press(b, Instant::now(), self.pos.x, self.pos.y) } else { self.clicks.current() };
        let ev = self.mouse_event(if pressed { down } else { up }, num, Some(clicks))?;
        if pressed {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        self.post(ev);
        Ok(())
    }

    fn scroll(&mut self, dx: f32, dy: f32, unit: ScrollUnit, phase: Option<GesturePhase>) -> Result<()> {
        // Protocol follows DOM WheelEvent: +dy = scroll down (content moves up).
        // Quartz: +wheel1 = scroll up, +wheel2 = scroll left.
        let w1 = self.scroll_y.take(-dy as f64) as i32;
        let w2 = self.scroll_x.take(-dx as f64) as i32;
        let units = if unit == ScrollUnit::Pixel { SCROLL_UNIT_PIXEL } else { SCROLL_UNIT_LINE };
        let (scroll_phase, momentum) = match phase {
            None => (0, 0),
            Some(GesturePhase::Begin) => {
                self.in_momentum = false;
                self.scroll_gesture_active = true;
                (SCROLL_PHASE_BEGAN, 0)
            }
            Some(GesturePhase::Change) => {
                self.scroll_gesture_active = true;
                (SCROLL_PHASE_CHANGED, 0)
            }
            Some(GesturePhase::End) if self.in_momentum => {
                self.in_momentum = false;
                (0, MOMENTUM_END)
            }
            Some(GesturePhase::End) => {
                self.scroll_gesture_active = false;
                (SCROLL_PHASE_ENDED, 0)
            }
            Some(GesturePhase::Cancel) => {
                self.scroll_gesture_active = false;
                let m = if self.in_momentum { MOMENTUM_END } else { 0 };
                self.in_momentum = false;
                (if m == 0 { SCROLL_PHASE_CANCELLED } else { 0 }, m)
            }
            Some(GesturePhase::Momentum) => {
                self.scroll_gesture_active = false;
                let m = if self.in_momentum { MOMENTUM_CONTINUE } else { MOMENTUM_BEGIN };
                self.in_momentum = true;
                (0, m)
            }
        };
        if w1 == 0 && w2 == 0 && phase.is_none() {
            return Ok(()); // nothing accumulated yet
        }
        let ev = check(
            unsafe { CGEventCreateScrollWheelEvent2(self.source, units, 2, w1, w2, 0) },
            "CGEventCreateScrollWheelEvent2",
        )?;
        unsafe {
            CGEventSetFlags(ev.0, cg_flags(self.mods));
            if unit == ScrollUnit::Pixel {
                CGEventSetIntegerValueField(ev.0, F_SCROLL_IS_CONTINUOUS, 1);
            }
            if scroll_phase != 0 {
                CGEventSetIntegerValueField(ev.0, F_SCROLL_PHASE, scroll_phase);
            }
            if momentum != 0 {
                CGEventSetIntegerValueField(ev.0, F_SCROLL_MOMENTUM_PHASE, momentum);
            }
        }
        self.post(ev);
        Ok(())
    }

    fn key_event(&self, code: HidUsage, vk: u16, pressed: bool) -> Result<()> {
        let ev = check(unsafe { CGEventCreateKeyboardEvent(self.source, vk, pressed) }, "CGEventCreateKeyboardEvent")?;
        unsafe {
            if code.is_modifier() {
                CGEventSetType(ev.0, FLAGS_CHANGED);
            } else {
                CGEventSetType(ev.0, if pressed { KEY_DOWN } else { KEY_UP });
            }
            CGEventSetFlags(ev.0, cg_flags(self.mods) | key_extra_flags(code));
        }
        self.post(ev);
        Ok(())
    }

    fn key(&mut self, code: HidUsage, pressed: bool, modifiers: Modifiers) -> Result<()> {
        let Some(_) = keymap::hid_to_macos_vk(code) else {
            return Err(InputError::InvalidArgument(format!("HID usage {:#04x} has no macOS key code", code.0)));
        };
        for step in plan_key(&mut self.mods.clone(), code, pressed, modifiers) {
            let vk = keymap::hid_to_macos_vk(step.code).expect("modifier usages are mapped");
            if step.code.is_modifier() {
                self.mods.set(step.code, step.pressed);
            } else if step.pressed {
                self.keys_down.insert(step.code.0);
            } else {
                self.keys_down.remove(&step.code.0);
            }
            self.key_event(step.code, vk, step.pressed)?;
        }
        Ok(())
    }

    /// Press `mods` + `key` and release everything again, restoring the
    /// previously tracked modifier state.
    fn combo(&mut self, mods: &[HidUsage], key: HidUsage) -> Result<()> {
        let saved = self.mods;
        let mut added = Vec::new();
        for &m in mods {
            if !self.mods.is_down(m) {
                self.mods.set(m, true);
                self.key_event(m, keymap::hid_to_macos_vk(m).unwrap_or(0), true)?;
                added.push(m);
            }
        }
        let vk = keymap::hid_to_macos_vk(key).ok_or(InputError::InvalidArgument("unmapped combo key".into()))?;
        self.key_event(key, vk, true)?;
        self.key_event(key, vk, false)?;
        for m in added.into_iter().rev() {
            self.mods.set(m, false);
            self.key_event(m, keymap::hid_to_macos_vk(m).unwrap_or(0), false)?;
        }
        debug_assert_eq!(self.mods, saved);
        Ok(())
    }

    fn text(&mut self, text: &str) -> Result<()> {
        for piece in chunk_text(text) {
            match piece {
                TextPiece::Key(k) => self.combo(&[], k)?,
                TextPiece::Unicode(units) => {
                    for down in [true, false] {
                        let ev = check(
                            unsafe { CGEventCreateKeyboardEvent(self.source, 0, down) },
                            "CGEventCreateKeyboardEvent",
                        )?;
                        unsafe {
                            // No modifiers: a held Cmd must not turn text into shortcuts.
                            CGEventSetFlags(ev.0, FLAG_NON_COALESCED);
                            CGEventKeyboardSetUnicodeString(ev.0, units.len() as _, units.as_ptr());
                        }
                        self.post(ev);
                    }
                }
            }
        }
        Ok(())
    }

    fn system(&mut self, a: SystemAction) -> Result<()> {
        const MC_APP: &str = "/System/Applications/Mission Control.app";
        match a {
            SystemAction::LockScreen => self.combo(&[HidUsage::CONTROL_LEFT, HidUsage::META_LEFT], HidUsage::Q),
            SystemAction::Screenshot => {
                self.combo(&[HidUsage::SHIFT_LEFT, HidUsage::META_LEFT], HidUsage::DIGIT3)
            }
            SystemAction::MissionControl => {
                if std::path::Path::new(MC_APP).exists() {
                    std::process::Command::new("/usr/bin/open").args(["-a", MC_APP]).spawn()?;
                    Ok(())
                } else {
                    self.combo(&[HidUsage::CONTROL_LEFT], HidUsage::ARROW_UP)
                }
            }
            SystemAction::ShowDesktop => {
                let bin = format!("{MC_APP}/Contents/MacOS/Mission Control");
                if std::path::Path::new(&bin).exists() {
                    // Argument "1" = Show Desktop (2 = App Exposé).
                    std::process::Command::new(bin).arg("1").spawn()?;
                    Ok(())
                } else {
                    self.combo(&[], HidUsage::F11)
                }
            }
            SystemAction::CtrlAltDel => {
                Err(InputError::Unsupported("Ctrl+Alt+Del (Secure Attention Sequence) exists only on Windows"))
            }
            SystemAction::AndroidBack
            | SystemAction::AndroidHome
            | SystemAction::AndroidRecents
            | SystemAction::AndroidNotifications => {
                Err(InputError::Unsupported("Android navigation actions are not available on macOS"))
            }
        }
    }

    fn gesture(&mut self, g: Gesture, phase: GesturePhase) -> Result<()> {
        match g {
            Gesture::SwipeNavigate { dx, dy } => {
                if phase != GesturePhase::End {
                    return Ok(());
                }
                if dx.abs() <= dy.abs() || dx == 0.0 {
                    return Err(InputError::Unsupported("vertical swipe navigation has no macOS equivalent"));
                }
                // Two-finger swipe right = back (Safari/Finder/Chrome): ⌘[ / ⌘].
                let key = if dx > 0.0 { HidUsage::BRACKET_LEFT } else { HidUsage::BRACKET_RIGHT };
                self.combo(&[HidUsage::META_LEFT], key)
            }
            Gesture::Magnify { .. } | Gesture::Rotate { .. } | Gesture::SmartZoom => Err(InputError::Unsupported(
                "magnify/rotate/smart-zoom need the private kCGSEventGesture event type; not synthesized",
            )),
        }
    }

    fn release_all(&mut self) -> Result<()> {
        for b in [MouseButton::Left, MouseButton::Right, MouseButton::Middle, MouseButton::Back, MouseButton::Forward] {
            if self.buttons & button_bit(b) != 0 {
                self.button(b, false)?;
            }
        }
        for code in std::mem::take(&mut self.keys_down) {
            let u = HidUsage(code);
            if let Some(vk) = keymap::hid_to_macos_vk(u) {
                self.key_event(u, vk, false)?;
            }
        }
        for m in self.mods.pressed() {
            self.mods.set(m, false);
            self.key_event(m, keymap::hid_to_macos_vk(m).unwrap_or(0), false)?;
        }
        if self.scroll_gesture_active || self.in_momentum {
            self.scroll(0.0, 0.0, ScrollUnit::Pixel, Some(GesturePhase::Cancel))?;
        }
        self.scroll_x.reset();
        self.scroll_y.reset();
        self.clicks.reset();
        Ok(())
    }
}

impl InputInjector for MacInjector {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        match ev {
            InputEvent::MouseMove { display_id, x, y } => {
                let r = self.display_rect(*display_id)?;
                let (gx, gy) = normalized_to_global(r, *x, *y);
                self.move_to(gx, gy)
            }
            InputEvent::MouseMoveRelative { dx, dy } => {
                let rects = self.all_rects();
                let (gx, gy) = clamp_to_displays(&rects, self.pos.x + *dx as f64, self.pos.y + *dy as f64);
                self.move_to(gx, gy)
            }
            InputEvent::MouseButton { button, pressed } => self.button(*button, *pressed),
            InputEvent::Scroll { dx, dy, unit, phase } => self.scroll(*dx, *dy, *unit, *phase),
            InputEvent::Key { code, pressed, modifiers } => self.key(*code, *pressed, *modifiers),
            InputEvent::Text { text } => self.text(text),
            InputEvent::Gesture { gesture, phase } => self.gesture(*gesture, *phase),
            InputEvent::Touch { .. } => {
                Err(InputError::Unsupported("macOS has no touch injection API; send pointer events instead"))
            }
            InputEvent::System(a) => self.system(*a),
            InputEvent::ReleaseAll => self.release_all(),
        }
    }

    fn update_displays(&mut self, d: Vec<DisplayInfo>) {
        self.displays = d;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_include_device_bits() {
        let mut m = ModifierState::default();
        assert_eq!(cg_flags(m), FLAG_NON_COALESCED);
        m.set(HidUsage::META_RIGHT, true);
        m.set(HidUsage::SHIFT_LEFT, true);
        assert_eq!(cg_flags(m), FLAG_NON_COALESCED | FLAG_COMMAND | DEV_RCMD | FLAG_SHIFT | DEV_LSHIFT);
        m.set(HidUsage::CONTROL_RIGHT, true);
        assert_eq!(cg_flags(m) & (FLAG_CONTROL | DEV_RCTL), FLAG_CONTROL | DEV_RCTL);
    }

    #[test]
    fn extra_key_flags() {
        assert_eq!(key_extra_flags(HidUsage::ARROW_UP), FLAG_NUMERIC_PAD | FLAG_SECONDARY_FN);
        assert_eq!(key_extra_flags(HidUsage::F5), FLAG_SECONDARY_FN);
        assert_eq!(key_extra_flags(HidUsage(0x59)), FLAG_NUMERIC_PAD);
        assert_eq!(key_extra_flags(HidUsage::A), 0);
    }

    #[test]
    fn text_chunking() {
        let pieces = chunk_text("Ciao è\nmondo\r\n\tfine");
        assert_eq!(
            pieces,
            vec![
                TextPiece::Unicode("Ciao è".encode_utf16().collect()),
                TextPiece::Key(HidUsage::ENTER),
                TextPiece::Unicode("mondo".encode_utf16().collect()),
                TextPiece::Key(HidUsage::ENTER),
                TextPiece::Key(HidUsage::TAB),
                TextPiece::Unicode("fine".encode_utf16().collect()),
            ]
        );
        // 15 emoji = 30 UTF-16 units → chunks of ≤ 20 without split pairs.
        let s = "😀".repeat(15);
        let pieces = chunk_text(&s);
        let mut total = 0;
        for p in &pieces {
            let TextPiece::Unicode(u) = p else { panic!() };
            assert!(u.len() <= UNICODE_CHUNK);
            assert!(!(0xD800..0xDC00).contains(u.last().unwrap()), "split surrogate");
            total += u.len();
        }
        assert_eq!(total, 30);
        assert_eq!(String::from_utf16(&pieces.iter().flat_map(|p| match p {
            TextPiece::Unicode(u) => u.clone(),
            TextPiece::Key(_) => vec![],
        }).collect::<Vec<_>>()).unwrap(), s);
    }

    #[test]
    fn display_queries_do_not_need_permissions() {
        // CGGetActiveDisplayList / CGDisplayBounds are permission-free reads.
        for id in active_displays() {
            let r = display_bounds_points(id).expect("active display has bounds");
            assert!(r.w > 0.0 && r.h > 0.0);
        }
        assert_eq!(display_bounds_points(0xDEAD_BEEF), None);
    }

    /// Moves the real cursor, types text and clicks. Run manually only.
    #[test]
    #[ignore = "injects real input into the live desktop; needs Accessibility permission"]
    fn live_injection_smoke() {
        let mut inj = MacInjector::new(vec![]).unwrap();
        let main = active_displays()[0];
        inj.inject(&InputEvent::MouseMove { display_id: main, x: 0.5, y: 0.5 }).unwrap();
        inj.inject(&InputEvent::Text { text: "aetherlink".into() }).unwrap();
        inj.inject(&InputEvent::ReleaseAll).unwrap();
    }
}
