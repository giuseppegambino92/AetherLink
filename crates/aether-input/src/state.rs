//! Platform-independent injector state machines: modifier tracking, click
//! counting and coordinate mapping. Kept pure so they are unit-tested on
//! every OS; the platform back-ends only translate their output to native
//! events.

use aether_protocol::{DisplayInfo, HidUsage, Modifiers, MouseButton};
use std::time::{Duration, Instant};

/// A rectangle in floating-point desktop coordinates (points on macOS).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl FRect {
    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

/// Map a normalized position (0..1) inside `r` to global coordinates. The
/// result is clamped so `1.0` lands on the last row/column of the display
/// instead of the neighbouring display.
pub fn normalized_to_global(r: FRect, nx: f32, ny: f32) -> (f64, f64) {
    let nx = if nx.is_finite() { nx.clamp(0.0, 1.0) as f64 } else { 0.0 };
    let ny = if ny.is_finite() { ny.clamp(0.0, 1.0) as f64 } else { 0.0 };
    let max_x = r.x + (r.w - 1.0).max(0.0);
    let max_y = r.y + (r.h - 1.0).max(0.0);
    ((r.x + nx * r.w).min(max_x), (r.y + ny * r.h).min(max_y))
}

/// Fallback conversion of a [`DisplayInfo`] (physical pixels) to logical
/// points, used when the OS cannot be asked for the display's point bounds.
pub fn display_points_rect(d: &DisplayInfo) -> FRect {
    let s = if d.scale.is_finite() && d.scale > 0.0 { d.scale as f64 } else { 1.0 };
    FRect {
        x: d.bounds.x as f64 / s,
        y: d.bounds.y as f64 / s,
        w: d.bounds.width as f64 / s,
        h: d.bounds.height as f64 / s,
    }
}

/// Convert a global pixel position into Windows `MOUSEEVENTF_ABSOLUTE |
/// MOUSEEVENTF_VIRTUALDESK` coordinates (0..=65535 across the virtual screen
/// `vx, vy, vw, vh`).
pub fn to_virtual_desk_absolute(px: f64, py: f64, vx: i32, vy: i32, vw: i32, vh: i32) -> (i32, i32) {
    let map = |p: f64, o: i32, len: i32| -> i32 {
        if len <= 1 {
            return 0;
        }
        let v = ((p - o as f64) * 65535.0 / (len - 1) as f64).round();
        v.clamp(0.0, 65535.0) as i32
    };
    (map(px, vx, vw), map(py, vy, vh))
}

/// Pixel rectangle of a display (no scaling), for hosts whose input space is pixels.
pub fn display_pixel_rect(d: &DisplayInfo) -> FRect {
    FRect { x: d.bounds.x as f64, y: d.bounds.y as f64, w: d.bounds.width as f64, h: d.bounds.height as f64 }
}

/// Clamp a point to the union of `rects` (keeps relative motion on-screen).
pub fn clamp_to_displays(rects: &[FRect], x: f64, y: f64) -> (f64, f64) {
    if rects.is_empty() || rects.iter().any(|r| r.contains(x, y)) {
        return (x, y);
    }
    // Nearest point on any display.
    let mut best = (x, y);
    let mut best_d = f64::INFINITY;
    for r in rects {
        let cx = x.clamp(r.x, r.x + (r.w - 1.0).max(0.0));
        let cy = y.clamp(r.y, r.y + (r.h - 1.0).max(0.0));
        let d = (cx - x).powi(2) + (cy - y).powi(2);
        if d < best_d {
            best_d = d;
            best = (cx, cy);
        }
    }
    best
}

/// Bit index (0..8) of a modifier usage 0xE0..=0xE7.
fn mod_bit(u: HidUsage) -> Option<u8> {
    u.is_modifier().then(|| (u.0 - 0xE0) as u8)
}

/// Tracks which physical modifier keys (left/right distinguished) are down.
/// The bit layout is the HID boot-keyboard modifier byte (bit i = usage 0xE0+i).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModifierState(pub u8);

/// (left usage, right usage, logical flag) for the four modifier groups.
const GROUPS: [(HidUsage, HidUsage, Modifiers); 4] = [
    (HidUsage::CONTROL_LEFT, HidUsage::CONTROL_RIGHT, Modifiers::CTRL),
    (HidUsage::SHIFT_LEFT, HidUsage::SHIFT_RIGHT, Modifiers::SHIFT),
    (HidUsage::ALT_LEFT, HidUsage::ALT_RIGHT, Modifiers::ALT),
    (HidUsage::META_LEFT, HidUsage::META_RIGHT, Modifiers::META),
];

impl ModifierState {
    pub fn is_down(self, u: HidUsage) -> bool {
        mod_bit(u).is_some_and(|b| self.0 & (1 << b) != 0)
    }

    pub fn set(&mut self, u: HidUsage, down: bool) {
        if let Some(b) = mod_bit(u) {
            if down {
                self.0 |= 1 << b;
            } else {
                self.0 &= !(1 << b);
            }
        }
    }

    /// Logical modifiers (left/right folded).
    pub fn logical(self) -> Modifiers {
        let mut m = Modifiers::NONE;
        for (l, r, flag) in GROUPS {
            if self.is_down(l) || self.is_down(r) {
                m = m | flag;
            }
        }
        m
    }

    /// Pressed modifier usages.
    pub fn pressed(self) -> Vec<HidUsage> {
        (0..8).filter(|b| self.0 & (1 << b) != 0).map(|b| HidUsage(0xE0 + b as u16)).collect()
    }
}

/// One key transition to emit natively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStep {
    pub code: HidUsage,
    pub pressed: bool,
}

/// Plan the native key transitions for a protocol key event so the host's
/// modifier state matches the client's `modifiers` field (which is the
/// authoritative logical state, e.g. after the client window lost focus while
/// Shift was held). Updates `state`.
///
/// For a modifier key itself the group of that key is driven by the key event,
/// the other groups are synchronised.
pub fn plan_key(state: &mut ModifierState, code: HidUsage, pressed: bool, wanted: Modifiers) -> Vec<KeyStep> {
    let mut steps = Vec::new();
    let own_group = GROUPS.iter().position(|(l, r, _)| *l == code || *r == code);
    for (i, (l, r, flag)) in GROUPS.iter().enumerate() {
        if Some(i) == own_group {
            continue;
        }
        let want = wanted.contains(*flag);
        let have = state.is_down(*l) || state.is_down(*r);
        if want && !have {
            state.set(*l, true);
            steps.push(KeyStep { code: *l, pressed: true });
        } else if !want && have {
            for u in [*l, *r] {
                if state.is_down(u) {
                    state.set(u, false);
                    steps.push(KeyStep { code: u, pressed: false });
                }
            }
        }
    }
    if code.is_modifier() {
        state.set(code, pressed);
    }
    steps.push(KeyStep { code, pressed });
    steps
}

/// Bit for a mouse button in a 5-button mask (HID order).
pub fn button_bit(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 0x01,
        MouseButton::Right => 0x02,
        MouseButton::Middle => 0x04,
        MouseButton::Back => 0x08,
        MouseButton::Forward => 0x10,
    }
}

/// Double/triple-click detection: consecutive presses of the same button
/// within `interval` and `max_distance` increase the click count.
#[derive(Debug, Clone)]
pub struct ClickTracker {
    pub interval: Duration,
    pub max_distance: f64,
    last: Option<(MouseButton, Instant, f64, f64)>,
    count: i64,
}

impl Default for ClickTracker {
    fn default() -> Self {
        Self::new(Duration::from_millis(500), 5.0)
    }
}

impl ClickTracker {
    pub fn new(interval: Duration, max_distance: f64) -> Self {
        Self { interval, max_distance, last: None, count: 0 }
    }

    /// Register a button press; returns the click count (1, 2, 3, …).
    pub fn press(&mut self, button: MouseButton, now: Instant, x: f64, y: f64) -> i64 {
        let continues = match self.last {
            Some((b, t, lx, ly)) => {
                b == button
                    && now.saturating_duration_since(t) <= self.interval
                    && ((x - lx).powi(2) + (y - ly).powi(2)).sqrt() <= self.max_distance
            }
            None => false,
        };
        self.count = if continues { self.count + 1 } else { 1 };
        self.last = Some((button, now, x, y));
        self.count
    }

    /// Click count to stamp on the matching release / drag events.
    pub fn current(&self) -> i64 {
        self.count.max(1)
    }

    pub fn reset(&mut self) {
        self.last = None;
        self.count = 0;
    }
}

/// Accumulates fractional deltas so slow sub-unit motion is not lost when the
/// target only accepts integers.
#[derive(Debug, Clone, Copy, Default)]
pub struct Accumulator {
    frac: f64,
}

impl Accumulator {
    /// Add `v` and return the whole part to emit now (rounded toward zero).
    pub fn take(&mut self, v: f64) -> i64 {
        if !v.is_finite() {
            return 0;
        }
        self.frac += v;
        let whole = self.frac.trunc();
        self.frac -= whole;
        whole as i64
    }

    pub fn reset(&mut self) {
        self.frac = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_protocol::Rect;

    #[test]
    fn normalized_mapping_and_clamp() {
        let r = FRect { x: 1440.0, y: -200.0, w: 1920.0, h: 1080.0 };
        assert_eq!(normalized_to_global(r, 0.0, 0.0), (1440.0, -200.0));
        assert_eq!(normalized_to_global(r, 0.5, 0.5), (2400.0, 340.0));
        assert_eq!(normalized_to_global(r, 1.0, 1.0), (1440.0 + 1919.0, -200.0 + 1079.0));
        assert_eq!(normalized_to_global(r, 2.0, -1.0), (1440.0 + 1919.0, -200.0));
        assert_eq!(normalized_to_global(r, f32::NAN, 0.0), (1440.0, -200.0));
    }

    #[test]
    fn retina_pixels_to_points() {
        let d = DisplayInfo {
            id: 1,
            name: "Built-in".into(),
            bounds: Rect::new(0, 0, 3024, 1964),
            scale: 2.0,
            refresh_hz: 120.0,
            primary: true,
            is_virtual: false,
        };
        assert_eq!(display_points_rect(&d), FRect { x: 0.0, y: 0.0, w: 1512.0, h: 982.0 });
        let ext = DisplayInfo { bounds: Rect::new(3024, 0, 3840, 2160), scale: 2.0, ..d.clone() };
        assert_eq!(display_points_rect(&ext), FRect { x: 1512.0, y: 0.0, w: 1920.0, h: 1080.0 });
        let bad = DisplayInfo { scale: 0.0, ..d };
        assert_eq!(display_points_rect(&bad).w, 3024.0);
    }

    #[test]
    fn windows_virtual_desk() {
        // Virtual screen spanning a left monitor at negative x.
        assert_eq!(to_virtual_desk_absolute(-1920.0, 0.0, -1920, 0, 3840, 1080), (0, 0));
        assert_eq!(to_virtual_desk_absolute(1919.0, 1079.0, -1920, 0, 3840, 1080), (65535, 65535));
        let (x, _) = to_virtual_desk_absolute(0.0, 0.0, -1920, 0, 3840, 1080);
        assert!((32775..=32785).contains(&x), "{x}");
        assert_eq!(to_virtual_desk_absolute(5000.0, -5.0, 0, 0, 1920, 1080), (65535, 0));
        assert_eq!(to_virtual_desk_absolute(10.0, 10.0, 0, 0, 0, 0), (0, 0));
    }

    #[test]
    fn clamp_union() {
        let a = FRect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let b = FRect { x: 100.0, y: -50.0, w: 100.0, h: 100.0 };
        assert_eq!(clamp_to_displays(&[a, b], 50.0, 50.0), (50.0, 50.0));
        assert_eq!(clamp_to_displays(&[a, b], 150.0, -20.0), (150.0, -20.0));
        assert_eq!(clamp_to_displays(&[a, b], -10.0, 50.0), (0.0, 50.0));
        assert_eq!(clamp_to_displays(&[a, b], 150.0, 500.0), (99.0, 99.0));
        assert_eq!(clamp_to_displays(&[a, b], 190.0, -400.0), (190.0, -50.0));
        assert_eq!(clamp_to_displays(&[], 7.0, 8.0), (7.0, 8.0));
    }

    #[test]
    fn modifier_sync_presses_missing_and_releases_stale() {
        let mut st = ModifierState::default();
        // Client says Cmd+Shift held, we have nothing: press both, then key.
        let steps = plan_key(&mut st, HidUsage::S, true, Modifiers::META | Modifiers::SHIFT);
        assert_eq!(
            steps,
            vec![
                KeyStep { code: HidUsage::SHIFT_LEFT, pressed: true },
                KeyStep { code: HidUsage::META_LEFT, pressed: true },
                KeyStep { code: HidUsage::S, pressed: true },
            ]
        );
        assert_eq!(st.logical(), Modifiers::META | Modifiers::SHIFT);
        // Shift released on the client side without us seeing the key-up.
        let steps = plan_key(&mut st, HidUsage::S, false, Modifiers::META);
        assert_eq!(
            steps,
            vec![KeyStep { code: HidUsage::SHIFT_LEFT, pressed: false }, KeyStep { code: HidUsage::S, pressed: false }]
        );
    }

    #[test]
    fn modifier_key_drives_its_own_group() {
        let mut st = ModifierState::default();
        let steps = plan_key(&mut st, HidUsage::SHIFT_RIGHT, true, Modifiers::SHIFT);
        assert_eq!(steps, vec![KeyStep { code: HidUsage::SHIFT_RIGHT, pressed: true }]);
        assert!(st.is_down(HidUsage::SHIFT_RIGHT));
        assert!(!st.is_down(HidUsage::SHIFT_LEFT));
        let steps = plan_key(&mut st, HidUsage::SHIFT_RIGHT, false, Modifiers::NONE);
        assert_eq!(steps, vec![KeyStep { code: HidUsage::SHIFT_RIGHT, pressed: false }]);
        assert_eq!(st, ModifierState(0));
        // Both shifts down, client reports none: both released.
        st.set(HidUsage::SHIFT_LEFT, true);
        st.set(HidUsage::SHIFT_RIGHT, true);
        let steps = plan_key(&mut st, HidUsage::A, true, Modifiers::NONE);
        assert_eq!(steps.len(), 3);
        assert_eq!(st.pressed(), vec![]);
    }

    #[test]
    fn click_counting() {
        let mut c = ClickTracker::default();
        let t0 = Instant::now();
        assert_eq!(c.press(MouseButton::Left, t0, 10.0, 10.0), 1);
        assert_eq!(c.press(MouseButton::Left, t0 + Duration::from_millis(200), 11.0, 10.0), 2);
        assert_eq!(c.press(MouseButton::Left, t0 + Duration::from_millis(400), 12.0, 11.0), 3);
        assert_eq!(c.current(), 3);
        // Too slow.
        assert_eq!(c.press(MouseButton::Left, t0 + Duration::from_millis(1200), 12.0, 11.0), 1);
        // Moved too far.
        assert_eq!(c.press(MouseButton::Left, t0 + Duration::from_millis(1300), 40.0, 11.0), 1);
        // Different button.
        assert_eq!(c.press(MouseButton::Right, t0 + Duration::from_millis(1400), 40.0, 11.0), 1);
        c.reset();
        assert_eq!(c.current(), 1);
    }

    #[test]
    fn accumulator_keeps_fractions() {
        let mut a = Accumulator::default();
        let total: i64 = (0..10).map(|_| a.take(0.25)).sum();
        assert_eq!(total, 2);
        assert_eq!(a.take(-0.75), 0);
        assert_eq!(a.take(-0.5), 0);
        assert_eq!(a.take(-0.5), -1);
        assert_eq!(a.take(f64::NAN), 0);
    }

    #[test]
    fn buttons() {
        assert_eq!(button_bit(MouseButton::Left) | button_bit(MouseButton::Forward), 0x11);
    }
}
