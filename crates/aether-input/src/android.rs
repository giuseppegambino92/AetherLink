//! Android injector.
//!
//! An unprivileged app cannot inject input from native code, so this module only
//! *translates* [`InputEvent`]s into [`AndroidInput`] commands in the shared display's
//! pixel space and hands them to a [`AndroidInputSink`] registered by the mobile app.
//! The app implements the sink in Kotlin (via JNI):
//!
//! * **Shizuku** (`InputManager.injectInputEvent` from a Shizuku UserService, shell uid)
//!   when Shizuku is running and permission was granted — real `MotionEvent`/`KeyEvent`s;
//! * otherwise **`AetherAccessibilityService`** — `dispatchGesture` strokes for
//!   tap / drag / swipe / pinch, `performGlobalAction` for Back / Home / Recents /
//!   Notifications, `ACTION_SET_TEXT` for text.
//!
//! Without a registered (and ready) sink [`AndroidInjector::new`] fails with
//! `PermissionDenied`, which the host session reports as `errors.inputPermission`.

use crate::state::{display_pixel_rect, normalized_to_global, FRect};
use crate::{InputError, InputInjector, Result};
use aether_protocol::{
    DisplayInfo, Gesture, GesturePhase, HidUsage, InputEvent, Modifiers, MouseButton, ScrollUnit, SystemAction, TouchPhase,
};
use std::sync::{Arc, RwLock};

/// `AccessibilityService.GLOBAL_ACTION_*` equivalents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalAction {
    Back,
    Home,
    Recents,
    Notifications,
    QuickSettings,
    LockScreen,
    Screenshot,
}

impl GlobalAction {
    /// The `AccessibilityService.GLOBAL_ACTION_*` constant.
    pub fn android_constant(self) -> i32 {
        match self {
            GlobalAction::Back => 1,
            GlobalAction::Home => 2,
            GlobalAction::Recents => 3,
            GlobalAction::Notifications => 4,
            GlobalAction::QuickSettings => 5,
            GlobalAction::LockScreen => 8,
            GlobalAction::Screenshot => 9,
        }
    }
}

/// Phase of a direct touch contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchState {
    Down,
    Move,
    Up,
    Cancel,
}

/// One command for the Kotlin side. Coordinates are physical pixels of the shared display.
#[derive(Debug, Clone, PartialEq)]
pub enum AndroidInput {
    /// Primary pointer pressed (start of a tap or drag stroke).
    PointerDown { x: f32, y: f32 },
    /// Pointer moved while pressed (continues the stroke).
    PointerMove { x: f32, y: f32 },
    /// Primary pointer released (ends the stroke; short strokes become taps).
    PointerUp { x: f32, y: f32 },
    /// Secondary click → long press at the cursor.
    LongPress { x: f32, y: f32 },
    /// Scroll by `dx`/`dy` pixels at `x`/`y` (content moves like a wheel: positive `dy` = down).
    Scroll { x: f32, y: f32, dx: f32, dy: f32 },
    /// Pinch around `x`/`y`; `scale` > 1 zooms in.
    Pinch { x: f32, y: f32, scale: f32 },
    /// Direct multi-touch contact.
    Touch { id: u32, x: f32, y: f32, state: TouchState },
    /// `android.view.KeyEvent` key code + meta state.
    Key { keycode: i32, down: bool, meta_state: i32 },
    Text { text: String },
    Global(GlobalAction),
    /// Lift every pointer / key.
    ReleaseAll,
}

/// Implemented by the mobile app (Kotlin through JNI).
pub trait AndroidInputSink: Send + Sync + 'static {
    /// True when a backend (Shizuku or the accessibility service) is connected.
    fn ready(&self) -> bool;
    fn send(&self, cmd: AndroidInput) -> std::result::Result<(), String>;
}

static SINK: RwLock<Option<Arc<dyn AndroidInputSink>>> = RwLock::new(None);

/// Register (or clear) the app's input sink.
pub fn set_sink(sink: Option<Arc<dyn AndroidInputSink>>) {
    *SINK.write().unwrap_or_else(|p| p.into_inner()) = sink;
}

fn sink() -> Option<Arc<dyn AndroidInputSink>> {
    SINK.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Scroll pixels per wheel notch.
const LINE_PX: f32 = 48.0;

pub struct AndroidInjector {
    sink: Arc<dyn AndroidInputSink>,
    displays: Vec<DisplayInfo>,
    /// Cursor in global physical pixels.
    pos: (f64, f64),
    left_down: bool,
}

impl AndroidInjector {
    pub fn new(displays: Vec<DisplayInfo>) -> Result<Self> {
        let sink = sink().ok_or(InputError::Unsupported("the AetherLink app did not register an input backend"))?;
        if !sink.ready() {
            return Err(InputError::PermissionDenied(
                "enable the AetherLink accessibility service (or grant Shizuku access) on the phone",
            ));
        }
        let pos = displays
            .first()
            .map(|d| {
                let r = display_pixel_rect(d);
                (r.x + r.w / 2.0, r.y + r.h / 2.0)
            })
            .unwrap_or((0.0, 0.0));
        Ok(Self { sink, displays, pos, left_down: false })
    }

    fn rect(&self, display_id: u32) -> Result<FRect> {
        self.displays
            .iter()
            .find(|d| d.id == display_id)
            .or_else(|| if display_id == 0 { self.displays.first() } else { None })
            .map(display_pixel_rect)
            .ok_or(InputError::UnknownDisplay(display_id))
    }

    fn bounds(&self) -> FRect {
        self.displays.first().map(display_pixel_rect).unwrap_or(FRect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 })
    }

    fn send(&self, cmd: AndroidInput) -> Result<()> {
        self.sink.send(cmd).map_err(InputError::Os)
    }

    fn xy(&self) -> (f32, f32) {
        (self.pos.0 as f32, self.pos.1 as f32)
    }
}

impl InputInjector for AndroidInjector {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        // Kept in sync with what the sink can express; translation is pure (see `translate`).
        let cmds = translate(self, ev)?;
        for c in cmds {
            self.send(c)?;
        }
        Ok(())
    }

    fn update_displays(&mut self, d: Vec<DisplayInfo>) {
        self.displays = d;
    }
}

fn translate(inj: &mut AndroidInjector, ev: &InputEvent) -> Result<Vec<AndroidInput>> {
    let mut out = Vec::new();
    match ev {
        InputEvent::MouseMove { display_id, x, y } => {
            let r = inj.rect(*display_id)?;
            inj.pos = normalized_to_global(r, *x, *y);
            if inj.left_down {
                let (x, y) = inj.xy();
                out.push(AndroidInput::PointerMove { x, y });
            }
        }
        InputEvent::MouseMoveRelative { dx, dy } => {
            let b = inj.bounds();
            inj.pos.0 = (inj.pos.0 + *dx as f64).clamp(b.x, b.x + b.w - 1.0);
            inj.pos.1 = (inj.pos.1 + *dy as f64).clamp(b.y, b.y + b.h - 1.0);
            if inj.left_down {
                let (x, y) = inj.xy();
                out.push(AndroidInput::PointerMove { x, y });
            }
        }
        InputEvent::MouseButton { button, pressed } => {
            let (x, y) = inj.xy();
            match (button, pressed) {
                (MouseButton::Left, true) if !inj.left_down => {
                    inj.left_down = true;
                    out.push(AndroidInput::PointerDown { x, y });
                }
                (MouseButton::Left, false) if inj.left_down => {
                    inj.left_down = false;
                    out.push(AndroidInput::PointerUp { x, y });
                }
                (MouseButton::Right, true) => out.push(AndroidInput::LongPress { x, y }),
                (MouseButton::Back, true) => out.push(AndroidInput::Global(GlobalAction::Back)),
                (MouseButton::Middle, true) => out.push(AndroidInput::Global(GlobalAction::Home)),
                _ => {}
            }
        }
        InputEvent::Scroll { dx, dy, unit, phase } => {
            if matches!(phase, Some(GesturePhase::Momentum)) {
                return Ok(out);
            }
            let k = if *unit == ScrollUnit::Line { LINE_PX } else { 1.0 };
            let (x, y) = inj.xy();
            if *dx != 0.0 || *dy != 0.0 {
                out.push(AndroidInput::Scroll { x, y, dx: dx * k, dy: dy * k });
            }
        }
        InputEvent::Key { code, pressed, modifiers } => {
            if let Some(keycode) = hid_to_android_keycode(*code) {
                out.push(AndroidInput::Key { keycode, down: *pressed, meta_state: meta_state(*modifiers) });
            }
        }
        InputEvent::Text { text } => {
            if !text.is_empty() {
                out.push(AndroidInput::Text { text: text.clone() });
            }
        }
        InputEvent::Gesture { gesture: Gesture::Magnify { delta }, phase } => {
            if !matches!(phase, GesturePhase::Cancel | GesturePhase::Momentum) && *delta != 0.0 {
                let (x, y) = inj.xy();
                out.push(AndroidInput::Pinch { x, y, scale: (1.0 + delta).clamp(0.2, 5.0) });
            }
        }
        InputEvent::Gesture { gesture: Gesture::SwipeNavigate { dx, .. }, phase: GesturePhase::End } if *dx > 0.0 => {
            out.push(AndroidInput::Global(GlobalAction::Back));
        }
        InputEvent::Gesture { .. } => {}
        InputEvent::Touch { id, display_id, x, y, phase, .. } => {
            let r = inj.rect(*display_id)?;
            let (px, py) = normalized_to_global(r, *x, *y);
            let state = match phase {
                TouchPhase::Down => TouchState::Down,
                TouchPhase::Move => TouchState::Move,
                TouchPhase::Up => TouchState::Up,
                TouchPhase::Cancel => TouchState::Cancel,
            };
            out.push(AndroidInput::Touch { id: *id, x: px as f32, y: py as f32, state });
        }
        InputEvent::System(a) => {
            let g = match a {
                SystemAction::AndroidBack => Some(GlobalAction::Back),
                SystemAction::AndroidHome => Some(GlobalAction::Home),
                SystemAction::AndroidRecents | SystemAction::MissionControl => Some(GlobalAction::Recents),
                SystemAction::AndroidNotifications => Some(GlobalAction::Notifications),
                SystemAction::LockScreen => Some(GlobalAction::LockScreen),
                SystemAction::Screenshot => Some(GlobalAction::Screenshot),
                SystemAction::ShowDesktop => Some(GlobalAction::Home),
                SystemAction::CtrlAltDel => None,
            };
            if let Some(g) = g {
                out.push(AndroidInput::Global(g));
            }
        }
        InputEvent::ReleaseAll => {
            inj.left_down = false;
            out.push(AndroidInput::ReleaseAll);
        }
    }
    Ok(out)
}

/// `KeyEvent.META_*` state for HID modifiers.
pub fn meta_state(m: Modifiers) -> i32 {
    let mut s = 0;
    if m.contains(Modifiers::SHIFT) {
        s |= 0x1 | 0x40; // META_SHIFT_ON | META_SHIFT_LEFT_ON
    }
    if m.contains(Modifiers::ALT) {
        s |= 0x2 | 0x10; // META_ALT_ON | META_ALT_LEFT_ON
    }
    if m.contains(Modifiers::CTRL) {
        s |= 0x1000 | 0x2000; // META_CTRL_ON | META_CTRL_LEFT_ON
    }
    if m.contains(Modifiers::META) {
        s |= 0x10000 | 0x20000; // META_META_ON | META_META_LEFT_ON
    }
    s
}

/// HID usage (page 0x07) → `android.view.KeyEvent.KEYCODE_*`.
pub fn hid_to_android_keycode(u: HidUsage) -> Option<i32> {
    let c = u.0;
    Some(match c {
        0x04..=0x1D => 29 + (c - 0x04) as i32,  // A..Z
        0x1E..=0x26 => 8 + (c - 0x1E) as i32,   // 1..9
        0x27 => 7,                              // 0
        0x28 => 66,                             // ENTER
        0x29 => 111,                            // ESCAPE
        0x2A => 67,                             // DEL (backspace)
        0x2B => 61,                             // TAB
        0x2C => 62,                             // SPACE
        0x2D => 69,                             // MINUS
        0x2E => 70,                             // EQUALS
        0x2F => 71,                             // LEFT_BRACKET
        0x30 => 72,                             // RIGHT_BRACKET
        0x31 => 73,                             // BACKSLASH
        0x33 => 74,                             // SEMICOLON
        0x34 => 75,                             // APOSTROPHE
        0x35 => 68,                             // GRAVE
        0x36 => 55,                             // COMMA
        0x37 => 56,                             // PERIOD
        0x38 => 76,                             // SLASH
        0x39 => 115,                            // CAPS_LOCK
        0x3A..=0x45 => 131 + (c - 0x3A) as i32, // F1..F12
        0x46 => 120,                            // SYSRQ
        0x47 => 116,                            // SCROLL_LOCK
        0x48 => 121,                            // BREAK
        0x49 => 124,                            // INSERT
        0x4A => 122,                            // MOVE_HOME
        0x4B => 92,                             // PAGE_UP
        0x4C => 112,                            // FORWARD_DEL
        0x4D => 123,                            // MOVE_END
        0x4E => 93,                             // PAGE_DOWN
        0x4F => 22,                             // DPAD_RIGHT
        0x50 => 21,                             // DPAD_LEFT
        0x51 => 20,                             // DPAD_DOWN
        0x52 => 19,                             // DPAD_UP
        0x53 => 143,                            // NUM_LOCK
        0x54 => 154,                            // NUMPAD_DIVIDE
        0x55 => 155,                            // NUMPAD_MULTIPLY
        0x56 => 156,                            // NUMPAD_SUBTRACT
        0x57 => 157,                            // NUMPAD_ADD
        0x58 => 160,                            // NUMPAD_ENTER
        0x59..=0x61 => 145 + (c - 0x59) as i32, // NUMPAD_1..9
        0x62 => 144,                            // NUMPAD_0
        0x63 => 158,                            // NUMPAD_DOT
        0x64 => 73,                             // ISO backslash
        0x65 => 82,                             // MENU
        0x7F => 164,                            // VOLUME_MUTE
        0x80 => 24,                             // VOLUME_UP
        0x81 => 25,                             // VOLUME_DOWN
        0xE0 => 113,                            // CTRL_LEFT
        0xE1 => 59,                             // SHIFT_LEFT
        0xE2 => 57,                             // ALT_LEFT
        0xE3 => 117,                            // META_LEFT
        0xE4 => 114,                            // CTRL_RIGHT
        0xE5 => 60,                             // SHIFT_RIGHT
        0xE6 => 58,                             // ALT_RIGHT
        0xE7 => 118,                            // META_RIGHT
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_protocol::Rect;
    use std::sync::Mutex;

    struct Rec(Mutex<Vec<AndroidInput>>);
    impl AndroidInputSink for Rec {
        fn ready(&self) -> bool {
            true
        }
        fn send(&self, cmd: AndroidInput) -> std::result::Result<(), String> {
            self.0.lock().unwrap().push(cmd);
            Ok(())
        }
    }

    fn display() -> DisplayInfo {
        DisplayInfo {
            id: 1,
            name: "phone".into(),
            bounds: Rect::new(0, 0, 1080, 2400),
            scale: 3.0,
            refresh_hz: 60.0,
            primary: true,
            is_virtual: false,
        }
    }

    #[test]
    fn click_and_drag_become_a_stroke() {
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        set_sink(Some(rec.clone()));
        let mut inj = AndroidInjector::new(vec![display()]).unwrap();
        inj.inject(&InputEvent::MouseMove { display_id: 1, x: 0.5, y: 0.5 }).unwrap();
        inj.inject(&InputEvent::MouseButton { button: MouseButton::Left, pressed: true }).unwrap();
        inj.inject(&InputEvent::MouseMove { display_id: 1, x: 0.5, y: 0.25 }).unwrap();
        inj.inject(&InputEvent::MouseButton { button: MouseButton::Left, pressed: false }).unwrap();
        inj.inject(&InputEvent::System(SystemAction::AndroidBack)).unwrap();
        let got = rec.0.lock().unwrap().clone();
        assert_eq!(got[0], AndroidInput::PointerDown { x: 540.0, y: 1200.0 });
        assert_eq!(got[1], AndroidInput::PointerMove { x: 540.0, y: 600.0 });
        assert_eq!(got[2], AndroidInput::PointerUp { x: 540.0, y: 600.0 });
        assert_eq!(got[3], AndroidInput::Global(GlobalAction::Back));
        set_sink(None);
    }

    #[test]
    fn keycodes() {
        assert_eq!(hid_to_android_keycode(HidUsage::A), Some(29));
        assert_eq!(hid_to_android_keycode(HidUsage::Z), Some(54));
        assert_eq!(hid_to_android_keycode(HidUsage::DIGIT0), Some(7));
        assert_eq!(hid_to_android_keycode(HidUsage::DIGIT9), Some(16));
        assert_eq!(hid_to_android_keycode(HidUsage::F12), Some(142));
        assert_eq!(hid_to_android_keycode(HidUsage::ENTER), Some(66));
        assert_eq!(meta_state(Modifiers::CTRL) & 0x1000, 0x1000);
    }
}
