//! macOS accessibility provider (`AXUIElement`, ApplicationServices).
//!
//! * Target: the focused application (system-wide `AXFocusedApplication`,
//!   falling back to the owner of the front-most on-screen window) — its menu
//!   bar plus its focused window, under a synthetic application root.
//! * Traversal: breadth-first, depth-limited, with a node budget
//!   ([`DEFAULT_NODE_BUDGET`]) and a wall-clock budget, one batched
//!   `AXUIElementCopyMultipleAttributeValues` per element and a global
//!   messaging timeout so a hung app cannot stall the host.
//! * Stable ids: elements are kept (retained) in a map keyed by
//!   `CFHash` + `CFEqual`, so the same UI element keeps its id across
//!   snapshots and diffs stay small. Entries unseen for a few snapshots are
//!   dropped.
//! * Bounds: AX reports global points; they are converted to the same
//!   "points × backing scale" pixel space `aether-capture` uses for
//!   `DisplayInfo` / `WindowInfo`.
//! * Secure text fields never expose their value.

use crate::{derive_container_names, prune_to_interactive, DomError, DomProvider, Result};
use aether_protocol::{DomAction, DomNode, DomRole, DomSnapshot, Rect};
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation_sys::base::{CFEqual, CFGetTypeID, CFHash, CFRelease, CFRetain, CFTypeID, CFTypeRef};
use core_foundation_sys::string::CFStringRef;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::time::{Duration, Instant};

pub const DEFAULT_NODE_BUDGET: usize = 5000;
pub const DEFAULT_TIME_BUDGET: Duration = Duration::from_millis(1500);
/// Per-message AX timeout (seconds) applied process-wide.
pub const MESSAGING_TIMEOUT_S: f32 = 0.35;
const MAX_VALUE_CHARS: usize = 256;
/// Snapshots an unseen element is remembered for (keeps ids stable while a
/// menu or a scrolled-away row briefly disappears).
const ID_RETENTION_GENERATIONS: u64 = 8;

type AXError = i32;
const AX_SUCCESS: AXError = 0;
const AX_ERR_INVALID_ELEMENT: AXError = -25202;
const AX_ERR_ATTRIBUTE_UNSUPPORTED: AXError = -25205;
const AX_ERR_ACTION_UNSUPPORTED: AXError = -25206;
const AX_ERR_API_DISABLED: AXError = -25211;
const AX_ERR_NO_VALUE: AXError = -25212;

const AX_VALUE_CGPOINT: u32 = 1;
const AX_VALUE_CGSIZE: u32 = 2;
const AX_VALUE_AXERROR: u32 = 5;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct CGPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct CGSize {
    width: f64,
    height: f64,
}
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(el: CFTypeRef, attr: CFStringRef, value: *mut CFTypeRef) -> AXError;
    fn AXUIElementCopyMultipleAttributeValues(
        el: CFTypeRef,
        attrs: CFArrayRef,
        options: u32,
        values: *mut CFArrayRef,
    ) -> AXError;
    fn AXUIElementCopyActionNames(el: CFTypeRef, names: *mut CFArrayRef) -> AXError;
    fn AXUIElementPerformAction(el: CFTypeRef, action: CFStringRef) -> AXError;
    fn AXUIElementSetAttributeValue(el: CFTypeRef, attr: CFStringRef, value: CFTypeRef) -> AXError;
    fn AXUIElementSetMessagingTimeout(el: CFTypeRef, seconds: f32) -> AXError;
    fn AXUIElementGetPid(el: CFTypeRef, pid: *mut i32) -> AXError;
    fn AXUIElementGetTypeID() -> CFTypeID;
    fn AXValueGetTypeID() -> CFTypeID;
    fn AXValueGetType(v: CFTypeRef) -> u32;
    fn AXValueGetValue(v: CFTypeRef, ty: u32, out: *mut c_void) -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayCopyDisplayMode(display: u32) -> *mut c_void;
    fn CGDisplayModeGetPixelWidth(mode: *mut c_void) -> usize;
    fn CGDisplayModeRelease(mode: *mut c_void);
}

// ---------------------------------------------------------------------------
// Element wrapper
// ---------------------------------------------------------------------------

/// Owned (retained) `AXUIElementRef` (or any CF object in tests).
pub struct Ax(CFTypeRef);

// CF objects have thread-safe reference counting and AX calls may be made from
// any thread; every use goes through `&mut AxProvider`.
unsafe impl Send for Ax {}

impl Ax {
    /// Take ownership of a +1 reference.
    unsafe fn from_create(r: CFTypeRef) -> Option<Self> {
        (!r.is_null()).then_some(Ax(r))
    }
    /// Retain a borrowed reference.
    unsafe fn from_get(r: CFTypeRef) -> Option<Self> {
        (!r.is_null()).then(|| Ax(unsafe { CFRetain(r) }))
    }
    fn raw(&self) -> CFTypeRef {
        self.0
    }
    fn cf_hash(&self) -> usize {
        unsafe { CFHash(self.0) }
    }
    fn cf_eq(&self, other: &Ax) -> bool {
        unsafe { CFEqual(self.0, other.0) != 0 }
    }
}

impl Clone for Ax {
    fn clone(&self) -> Self {
        Ax(unsafe { CFRetain(self.0) })
    }
}

impl Drop for Ax {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

fn ax_error(code: AXError, context: &'static str) -> DomError {
    if code == AX_ERR_API_DISABLED {
        DomError::PermissionDenied("Accessibility permission not granted")
    } else {
        DomError::Accessibility { code, context }
    }
}

fn copy_attr(el: &Ax, name: &str) -> std::result::Result<Option<CFType>, AXError> {
    let n = CFString::new(name);
    let mut v: CFTypeRef = std::ptr::null();
    let err = unsafe { AXUIElementCopyAttributeValue(el.raw(), n.as_concrete_TypeRef(), &mut v) };
    match err {
        AX_SUCCESS if !v.is_null() => Ok(Some(unsafe { CFType::wrap_under_create_rule(v) })),
        AX_SUCCESS | AX_ERR_NO_VALUE | AX_ERR_ATTRIBUTE_UNSUPPORTED => Ok(None),
        e => Err(e),
    }
}

fn set_attr(el: &Ax, name: &str, value: &CFType) -> std::result::Result<(), AXError> {
    let n = CFString::new(name);
    match unsafe { AXUIElementSetAttributeValue(el.raw(), n.as_concrete_TypeRef(), value.as_CFTypeRef()) } {
        AX_SUCCESS => Ok(()),
        e => Err(e),
    }
}

fn perform_action(el: &Ax, action: &str) -> std::result::Result<(), AXError> {
    let n = CFString::new(action);
    match unsafe { AXUIElementPerformAction(el.raw(), n.as_concrete_TypeRef()) } {
        AX_SUCCESS => Ok(()),
        e => Err(e),
    }
}

fn element_attr(el: &Ax, name: &str) -> Option<Ax> {
    let v = copy_attr(el, name).ok()??;
    is_ax_element(&v).then(|| unsafe { Ax::from_get(v.as_CFTypeRef()) }).flatten()
}

fn is_ax_element(v: &CFType) -> bool {
    v.type_of() == unsafe { AXUIElementGetTypeID() }
}

fn cf_string(v: &CFType) -> Option<String> {
    v.downcast::<CFString>().map(|s| s.to_string())
}

fn cf_bool(v: &CFType) -> Option<bool> {
    if let Some(b) = v.downcast::<CFBoolean>() {
        return Some(b.into());
    }
    v.downcast::<CFNumber>().and_then(|n| n.to_i64()).map(|i| i != 0)
}

fn cf_i64(v: &CFType) -> Option<i64> {
    v.downcast::<CFNumber>().and_then(|n| n.to_i64())
}

/// Human-readable form of an AXValue attribute (strings, numbers, booleans).
fn cf_value_string(v: &CFType) -> Option<String> {
    if let Some(s) = cf_string(v) {
        return Some(s);
    }
    if let Some(b) = v.downcast::<CFBoolean>() {
        return Some(bool::from(b).to_string());
    }
    if let Some(n) = v.downcast::<CFNumber>() {
        if let Some(i) = n.to_i64()
            && n.to_f64().is_none_or(|f| f == i as f64)
        {
            return Some(i.to_string());
        }
        return n.to_f64().map(|f| format!("{f:.2}").trim_end_matches('0').trim_end_matches('.').to_string());
    }
    None
}

fn ax_value<T: Default>(v: &CFType, ty: u32) -> Option<T> {
    unsafe {
        if v.type_of() != AXValueGetTypeID() || AXValueGetType(v.as_CFTypeRef()) != ty {
            return None;
        }
        let mut out = T::default();
        AXValueGetValue(v.as_CFTypeRef(), ty, (&mut out as *mut T).cast()).then_some(out)
    }
}

fn is_ax_error_value(v: &CFType) -> bool {
    unsafe { v.type_of() == AXValueGetTypeID() && AXValueGetType(v.as_CFTypeRef()) == AX_VALUE_AXERROR }
}

/// Elements of a CFArray of AXUIElements.
fn element_array(v: &CFType) -> Vec<Ax> {
    if v.type_of() != unsafe { core_foundation_sys::array::CFArrayGetTypeID() } {
        return Vec::new();
    }
    let arr = v.as_CFTypeRef() as CFArrayRef;
    let n = unsafe { CFArrayGetCount(arr) };
    let ax_type = unsafe { AXUIElementGetTypeID() };
    (0..n)
        .filter_map(|i| {
            let e = unsafe { CFArrayGetValueAtIndex(arr, i) };
            (!e.is_null() && unsafe { CFGetTypeID(e) } == ax_type).then(|| unsafe { Ax::from_get(e) }).flatten()
        })
        .collect()
}

fn string_array(arr: CFArrayRef) -> Vec<String> {
    let n = unsafe { CFArrayGetCount(arr) };
    (0..n)
        .filter_map(|i| {
            let e = unsafe { CFArrayGetValueAtIndex(arr, i) };
            (!e.is_null()).then(|| unsafe { CFType::wrap_under_get_rule(e) }).and_then(|t| cf_string(&t))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Pure mappings
// ---------------------------------------------------------------------------

/// Map `AXRole` (+ `AXSubrole`) to the portable role.
pub fn map_role(role: &str, subrole: Option<&str>) -> DomRole {
    match (role, subrole.unwrap_or("")) {
        ("AXWindow", "AXDialog" | "AXSystemDialog") => DomRole::Dialog,
        ("AXWindow", _) => DomRole::Window,
        ("AXSheet" | "AXDrawer" | "AXPopover", _) => DomRole::Dialog,
        ("AXButton", _) | ("AXDisclosureTriangle", _) | ("AXColorWell", _) => DomRole::Button,
        ("AXMenuButton" | "AXPopUpButton", _) => DomRole::PopUpButton,
        ("AXLink", _) => DomRole::Link,
        ("AXTextField", "AXSearchField") => DomRole::SearchField,
        ("AXTextField" | "AXDateField" | "AXTimeField", _) => DomRole::TextField,
        ("AXTextArea", _) => DomRole::TextArea,
        ("AXCheckBox", "AXSwitch" | "AXToggle") => DomRole::Switch,
        ("AXCheckBox", _) => DomRole::CheckBox,
        ("AXRadioButton", "AXTabButton") => DomRole::Tab,
        ("AXRadioButton", _) => DomRole::RadioButton,
        ("AXTab", _) => DomRole::Tab,
        ("AXSlider" | "AXIncrementor" | "AXStepper" | "AXLevelIndicator", _) => DomRole::Slider,
        ("AXComboBox", _) => DomRole::ComboBox,
        ("AXMenuBar", _) => DomRole::MenuBar,
        ("AXMenu", _) => DomRole::Menu,
        ("AXMenuItem" | "AXMenuBarItem", _) => DomRole::MenuItem,
        ("AXTabGroup", _) => DomRole::TabGroup,
        ("AXList", _) => DomRole::List,
        ("AXRow", "AXOutlineRow") => DomRole::TreeItem,
        ("AXRow", _) => DomRole::Row,
        ("AXCell", _) => DomRole::Cell,
        ("AXTable" | "AXGrid", _) => DomRole::Table,
        ("AXOutline" | "AXBrowser", _) => DomRole::Tree,
        ("AXToolbar", _) => DomRole::Toolbar,
        ("AXImage", _) => DomRole::Image,
        ("AXStaticText", _) => DomRole::StaticText,
        ("AXHeading", _) => DomRole::Heading,
        ("AXGroup", "AXDocumentArticle" | "AXLandmarkMain") => DomRole::Group,
        ("AXGroup" | "AXSplitGroup" | "AXRadioGroup" | "AXLayoutArea" | "AXApplication", _) => DomRole::Group,
        ("AXScrollArea", _) => DomRole::ScrollArea,
        ("AXWebArea", _) => DomRole::WebArea,
        _ => DomRole::Other,
    }
}

/// Map AX action names to portable actions.
pub fn map_actions(names: &[String], role: DomRole, disclosing: Option<bool>, selectable: bool) -> Vec<DomAction> {
    let mut out = Vec::new();
    let mut push = |a: DomAction| {
        if !out.contains(&a) {
            out.push(a);
        }
    };
    for n in names {
        match n.as_str() {
            "AXPress" => {
                push(DomAction::Press);
                if matches!(role, DomRole::CheckBox | DomRole::Switch | DomRole::RadioButton) {
                    push(DomAction::Toggle);
                }
            }
            "AXShowMenu" => push(DomAction::ShowMenu),
            "AXIncrement" => push(DomAction::Increment),
            "AXDecrement" => push(DomAction::Decrement),
            "AXScrollToVisible" => push(DomAction::ScrollIntoView),
            "AXRaise" => push(DomAction::Focus),
            "AXPick" => push(DomAction::Select),
            _ => {}
        }
    }
    if role.accepts_text() {
        push(DomAction::Focus);
    }
    match disclosing {
        Some(true) => push(DomAction::Collapse),
        Some(false) => push(DomAction::Expand),
        None => {}
    }
    if selectable && matches!(role, DomRole::Row | DomRole::TreeItem | DomRole::ListItem | DomRole::Tab | DomRole::Cell) {
        push(DomAction::Select);
    }
    out
}

/// Render a menu shortcut ("⌃⌥⇧⌘S") from `AXMenuItemCmdChar`,
/// `AXMenuItemCmdModifiers` (bit0 Shift, bit1 Option, bit2 Control,
/// bit3 *no* Command) and `AXMenuItemCmdVirtualKey`.
pub fn format_shortcut(cmd_char: Option<&str>, modifiers: Option<i64>, virtual_key: Option<i64>) -> Option<String> {
    let key = cmd_char
        .and_then(|c| {
            let mut it = c.chars();
            let ch = it.next()?;
            if it.next().is_some() {
                return Some(c.to_uppercase());
            }
            Some(
                match ch {
                    '\r' | '\u{3}' => "↩",
                    '\t' => "⇥",
                    ' ' => "Space",
                    '\u{8}' | '\u{7f}' => "⌫",
                    '\u{1b}' => "⎋",
                    '\u{f728}' => "⌦",
                    '\u{f700}' => "↑",
                    '\u{f701}' => "↓",
                    '\u{f702}' => "←",
                    '\u{f703}' => "→",
                    '\u{f729}' => "↖",
                    '\u{f72b}' => "↘",
                    '\u{f72c}' => "⇞",
                    '\u{f72d}' => "⇟",
                    c if ('\u{f704}'..='\u{f726}').contains(&c) => {
                        return Some(format!("F{}", c as u32 - 0xf704 + 1));
                    }
                    c if c.is_control() || ('\u{f700}'..='\u{f8ff}').contains(&c) => return None,
                    c => return Some(c.to_uppercase().collect()),
                }
                .to_string(),
            )
        })
        .or_else(|| {
            let k = match virtual_key? {
                0x24 => "↩",
                0x30 => "⇥",
                0x31 => "Space",
                0x33 => "⌫",
                0x35 => "⎋",
                0x75 => "⌦",
                0x7B => "←",
                0x7C => "→",
                0x7D => "↓",
                0x7E => "↑",
                0x73 => "↖",
                0x77 => "↘",
                0x74 => "⇞",
                0x79 => "⇟",
                0x7A => "F1",
                0x78 => "F2",
                0x63 => "F3",
                0x76 => "F4",
                0x60 => "F5",
                0x61 => "F6",
                0x62 => "F7",
                0x64 => "F8",
                0x65 => "F9",
                0x6D => "F10",
                0x67 => "F11",
                0x6F => "F12",
                _ => return None,
            };
            Some(k.to_string())
        })?;
    let m = modifiers.unwrap_or(0);
    let mut s = String::new();
    if m & 4 != 0 {
        s.push('⌃');
    }
    if m & 2 != 0 {
        s.push('⌥');
    }
    if m & 1 != 0 {
        s.push('⇧');
    }
    if m & 8 == 0 {
        s.push('⌘');
    }
    s.push_str(&key);
    Some(s)
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Stable ids
// ---------------------------------------------------------------------------

struct Entry {
    el: Ax,
    id: u64,
    last_seen: u64,
}

/// Element ↔ id map keyed by `CFHash` + `CFEqual`.
#[derive(Default)]
pub struct IdMap {
    by_hash: HashMap<usize, Vec<Entry>>,
    by_id: HashMap<u64, (usize, usize)>,
    next: u64,
    generation: u64,
}

impl IdMap {
    pub fn new() -> Self {
        Self { next: 1, ..Default::default() }
    }

    fn reindex(&mut self, h: usize) {
        if let Some(v) = self.by_hash.get(&h) {
            for (i, e) in v.iter().enumerate() {
                self.by_id.insert(e.id, (h, i));
            }
        }
    }

    /// Id of `el`, assigning a new one on first sight.
    pub fn id_for(&mut self, el: &Ax) -> u64 {
        let h = el.cf_hash();
        let gen_ = self.generation;
        let bucket = self.by_hash.entry(h).or_default();
        if let Some(e) = bucket.iter_mut().find(|e| e.el.cf_eq(el)) {
            e.last_seen = gen_;
            return e.id;
        }
        let id = self.next.max(1);
        self.next = id + 1;
        bucket.push(Entry { el: el.clone(), id, last_seen: gen_ });
        let idx = bucket.len() - 1;
        self.by_id.insert(id, (h, idx));
        id
    }

    pub fn element(&self, id: u64) -> Option<&Ax> {
        let (h, i) = *self.by_id.get(&id)?;
        self.by_hash.get(&h)?.get(i).map(|e| &e.el)
    }

    /// Start a new snapshot generation.
    pub fn begin(&mut self) {
        self.generation += 1;
    }

    /// Forget elements not seen in the last `keep` generations.
    pub fn prune(&mut self, keep: u64) {
        let min = self.generation.saturating_sub(keep);
        let mut touched = Vec::new();
        self.by_hash.retain(|h, v| {
            let before = v.len();
            v.retain(|e| e.last_seen >= min);
            if v.len() != before {
                touched.push(*h);
            }
            !v.is_empty()
        });
        if !touched.is_empty() {
            self.by_id.retain(|_, (h, _)| !touched.contains(h));
            for h in touched {
                self.reindex(h);
            }
        }
    }

    /// Forget everything but keep ids monotonic (new app → new ids).
    pub fn clear(&mut self) {
        self.by_hash.clear();
        self.by_id.clear();
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Displays (points → capture pixel space)
// ---------------------------------------------------------------------------

struct Screens(Vec<(CGRect, f64)>);

impl Screens {
    fn load() -> Self {
        let mut ids = [0u32; 32];
        let mut n = 0u32;
        if unsafe { CGGetActiveDisplayList(32, ids.as_mut_ptr(), &mut n) } != 0 {
            return Screens(Vec::new());
        }
        Screens(
            ids[..n as usize]
                .iter()
                .map(|&id| unsafe {
                    let b = CGDisplayBounds(id);
                    let mode = CGDisplayCopyDisplayMode(id);
                    let scale = if mode.is_null() || b.size.width <= 0.0 {
                        1.0
                    } else {
                        let px = CGDisplayModeGetPixelWidth(mode) as f64;
                        CGDisplayModeRelease(mode);
                        (px / b.size.width).max(1.0)
                    };
                    (b, scale)
                })
                .collect(),
        )
    }

    fn scale_at(&self, p: CGPoint) -> f64 {
        self.0
            .iter()
            .find(|(r, _)| {
                p.x >= r.origin.x
                    && p.y >= r.origin.y
                    && p.x < r.origin.x + r.size.width
                    && p.y < r.origin.y + r.size.height
            })
            .or(self.0.first())
            .map_or(1.0, |(_, s)| *s)
    }

    fn to_pixels(&self, p: CGPoint, s: CGSize) -> Rect {
        let k = self.scale_at(p);
        Rect::new(
            (p.x * k).round() as i32,
            (p.y * k).round() as i32,
            (s.width * k).round().max(0.0) as u32,
            (s.height * k).round().max(0.0) as u32,
        )
    }
}

// ---------------------------------------------------------------------------
// Front-most app helpers (shared with `context`)
// ---------------------------------------------------------------------------

/// PID of the front-most application: AX focused application when trusted,
/// otherwise the owner of the front-most normal (layer 0) on-screen window.
pub fn frontmost_pid() -> Option<u32> {
    if unsafe { AXIsProcessTrusted() } {
        let sys = unsafe { Ax::from_create(AXUIElementCreateSystemWide()) }?;
        if let Some(app) = element_attr(&sys, "AXFocusedApplication") {
            let mut pid = 0;
            if unsafe { AXUIElementGetPid(app.raw(), &mut pid) } == AX_SUCCESS && pid > 0 {
                return Some(pid as u32);
            }
        }
    }
    frontmost_pid_from_windows()
}

/// Owner PID of the front-most layer-0 window (no permission needed).
pub fn frontmost_pid_from_windows() -> Option<u32> {
    const ON_SCREEN_ONLY: u32 = 1;
    const EXCLUDE_DESKTOP: u32 = 16;
    let arr = unsafe { CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0) };
    if arr.is_null() {
        return None;
    }
    let arr_owned = unsafe { CFType::wrap_under_create_rule(arr as CFTypeRef) };
    let layer_key = CFString::from_static_string("kCGWindowLayer");
    let pid_key = CFString::from_static_string("kCGWindowOwnerPID");
    let n = unsafe { CFArrayGetCount(arr) };
    let get = |dict: CFTypeRef, key: &CFString| -> Option<i64> {
        let mut v: *const c_void = std::ptr::null();
        let found = unsafe {
            core_foundation_sys::dictionary::CFDictionaryGetValueIfPresent(
                dict as _,
                key.as_concrete_TypeRef() as *const c_void,
                &mut v,
            )
        };
        if found == 0 || v.is_null() {
            return None;
        }
        cf_i64(&unsafe { CFType::wrap_under_get_rule(v) })
    };
    let own = std::process::id() as i64;
    let res = (0..n).find_map(|i| {
        let d = unsafe { CFArrayGetValueAtIndex(arr, i) };
        (get(d, &layer_key) == Some(0)).then(|| get(d, &pid_key)).flatten().filter(|&p| p > 0 && p != own)
    });
    drop(arr_owned);
    res.map(|p| p as u32)
}

/// Executable path of a process.
pub fn pid_executable(pid: u32) -> Option<String> {
    let mut buf = vec![0u8; 4096];
    let n = unsafe { libc::proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok()
}

/// `(AXDocument, AXTitle)` of the focused window of `pid` (None without the
/// Accessibility permission).
pub fn focused_window_strings(pid: u32) -> (Option<String>, Option<String>) {
    if !unsafe { AXIsProcessTrusted() } {
        return (None, None);
    }
    let Some(app) = (unsafe { Ax::from_create(AXUIElementCreateApplication(pid as i32)) }) else {
        return (None, None);
    };
    unsafe { AXUIElementSetMessagingTimeout(app.raw(), MESSAGING_TIMEOUT_S) };
    let Some(win) = element_attr(&app, "AXFocusedWindow").or_else(|| element_attr(&app, "AXMainWindow")) else {
        return (None, None);
    };
    let doc = copy_attr(&win, "AXDocument").ok().flatten().and_then(|v| cf_string(&v));
    let title = copy_attr(&win, "AXTitle").ok().flatten().and_then(|v| cf_string(&v));
    (doc, title)
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

const ATTRS: [&str; 17] = [
    "AXRole",
    "AXSubrole",
    "AXTitle",
    "AXDescription",
    "AXValue",
    "AXHelp",
    "AXEnabled",
    "AXFocused",
    "AXPosition",
    "AXSize",
    "AXChildren",
    "AXPlaceholderValue",
    "AXMenuItemCmdChar",
    "AXMenuItemCmdModifiers",
    "AXMenuItemCmdVirtualKey",
    "AXDisclosing",
    "AXSelected",
];
const A_ROLE: usize = 0;
const A_SUBROLE: usize = 1;
const A_TITLE: usize = 2;
const A_DESC: usize = 3;
const A_VALUE: usize = 4;
const A_HELP: usize = 5;
const A_ENABLED: usize = 6;
const A_FOCUSED: usize = 7;
const A_POS: usize = 8;
const A_SIZE: usize = 9;
const A_CHILDREN: usize = 10;
const A_PLACEHOLDER: usize = 11;
const A_CMD_CHAR: usize = 12;
const A_CMD_MODS: usize = 13;
const A_CMD_VKEY: usize = 14;
const A_DISCLOSING: usize = 15;
const A_SELECTED: usize = 16;

/// Batched attribute values (None = missing / error).
fn fetch_attrs(el: &Ax, names: &CFType) -> std::result::Result<Vec<Option<CFType>>, AXError> {
    let mut out: CFArrayRef = std::ptr::null();
    let err = unsafe { AXUIElementCopyMultipleAttributeValues(el.raw(), names.as_CFTypeRef() as CFArrayRef, 0, &mut out) };
    if err != AX_SUCCESS {
        return Err(err);
    }
    if out.is_null() {
        return Ok(vec![None; ATTRS.len()]);
    }
    let owned = unsafe { CFType::wrap_under_create_rule(out as CFTypeRef) };
    let n = unsafe { CFArrayGetCount(out) } as usize;
    let null_id = unsafe { core_foundation_sys::base::CFNullGetTypeID() };
    let vals = (0..ATTRS.len())
        .map(|i| {
            if i >= n {
                return None;
            }
            let v = unsafe { CFArrayGetValueAtIndex(out, i as isize) };
            if v.is_null() || unsafe { CFGetTypeID(v) } == null_id {
                return None;
            }
            let t = unsafe { CFType::wrap_under_get_rule(v) };
            (!is_ax_error_value(&t)).then_some(t)
        })
        .collect();
    drop(owned);
    Ok(vals)
}

fn attr_names_array() -> CFType {
    let strings: Vec<CFString> = ATTRS.iter().map(|s| CFString::new(s)).collect();
    core_foundation::array::CFArray::from_CFTypes(&strings).as_CFType()
}

pub struct AxProvider {
    ids: IdMap,
    revision: u64,
    pid: Option<u32>,
    node_budget: usize,
    time_budget: Duration,
}

impl Default for AxProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AxProvider {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_NODE_BUDGET, DEFAULT_TIME_BUDGET)
    }

    pub fn with_limits(node_budget: usize, time_budget: Duration) -> Self {
        Self { ids: IdMap::new(), revision: 0, pid: None, node_budget: node_budget.max(2), time_budget, }
    }

    fn element(&self, node: u64) -> Result<Ax> {
        self.ids.element(node).cloned().ok_or(DomError::UnknownNode(node))
    }

    fn map_err(node: u64, action: DomAction, code: AXError, ctx: &'static str) -> DomError {
        match code {
            AX_ERR_ACTION_UNSUPPORTED | AX_ERR_ATTRIBUTE_UNSUPPORTED => DomError::ActionUnsupported { node, action },
            AX_ERR_INVALID_ELEMENT => DomError::UnknownNode(node),
            c => ax_error(c, ctx),
        }
    }

    fn build_node(
        &self,
        id: u64,
        parent: Option<u64>,
        el: &Ax,
        v: &[Option<CFType>],
        screens: &Screens,
    ) -> (DomNode, Vec<Ax>) {
        let s = |i: usize| v[i].as_ref().and_then(cf_string).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let role_s = s(A_ROLE).unwrap_or_else(|| "AXUnknown".into());
        let subrole = s(A_SUBROLE);
        let role = map_role(&role_s, subrole.as_deref());
        let secure = subrole.as_deref() == Some("AXSecureTextField");
        let raw_value = if secure { None } else { v[A_VALUE].as_ref().and_then(cf_value_string) };
        let title = s(A_TITLE);
        let desc = s(A_DESC);
        let help = s(A_HELP);
        let mut name_from_desc = false;
        let mut name_from_value = false;
        let mut name = title.clone().or_else(|| {
            name_from_desc = desc.is_some();
            desc.clone()
        });
        if name.is_none() && role.accepts_text() {
            name = s(A_PLACEHOLDER);
        }
        if name.is_none() && (role.accepts_text() || matches!(role, DomRole::Slider | DomRole::PopUpButton | DomRole::CheckBox)) {
            // Label from the linked title element (one extra round-trip, only for unnamed inputs).
            name = element_attr(el, "AXTitleUIElement").and_then(|t| {
                copy_attr(&t, "AXValue").ok().flatten().or_else(|| copy_attr(&t, "AXTitle").ok().flatten())
            })
            .and_then(|x| cf_string(&x))
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty());
        }
        if name.is_none() && role == DomRole::StaticText {
            name_from_value = true;
            name = raw_value.clone().map(|x| x.trim().to_string());
        }
        let description = help.or_else(|| desc.filter(|d| !name_from_desc && Some(d) != title.as_ref()));
        let value = if name_from_value { None } else { raw_value.map(|x| truncate_chars(&x, MAX_VALUE_CHARS)) };
        let shortcut = format_shortcut(
            v[A_CMD_CHAR].as_ref().and_then(cf_string).as_deref(),
            v[A_CMD_MODS].as_ref().and_then(cf_i64),
            v[A_CMD_VKEY].as_ref().and_then(cf_i64),
        );
        let pos: Option<CGPoint> = v[A_POS].as_ref().and_then(|x| ax_value(x, AX_VALUE_CGPOINT));
        let size: Option<CGSize> = v[A_SIZE].as_ref().and_then(|x| ax_value(x, AX_VALUE_CGSIZE));
        let bounds = match (pos, size) {
            (Some(p), Some(sz)) => screens.to_pixels(p, sz),
            _ => Rect::default(),
        };
        let disclosing = v[A_DISCLOSING].as_ref().and_then(cf_bool);
        let selectable = v[A_SELECTED].is_some();
        let fetch_actions = role.is_interactive() || role.accepts_text() || matches!(role, DomRole::Window | DomRole::Dialog);
        let action_names = if fetch_actions {
            let mut arr: CFArrayRef = std::ptr::null();
            if unsafe { AXUIElementCopyActionNames(el.raw(), &mut arr) } == AX_SUCCESS && !arr.is_null() {
                let names = string_array(arr);
                unsafe { CFRelease(arr as CFTypeRef) };
                names
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        let children = v[A_CHILDREN].as_ref().map(element_array).unwrap_or_default();
        let native_role = match &subrole {
            Some(sr) => format!("{role_s}:{sr}"),
            None => role_s,
        };
        (
            DomNode {
                id,
                parent,
                role,
                native_role,
                name: name.unwrap_or_default(),
                value,
                description,
                shortcut,
                bounds,
                actions: map_actions(&action_names, role, disclosing, selectable),
                enabled: v[A_ENABLED].as_ref().and_then(cf_bool).unwrap_or(true),
                focused: v[A_FOCUSED].as_ref().and_then(cf_bool).unwrap_or(false),
                children: Vec::new(),
            },
            children,
        )
    }

    fn focused_app() -> Result<(Ax, u32)> {
        let sys = unsafe { Ax::from_create(AXUIElementCreateSystemWide()) }
            .ok_or(DomError::Os("AXUIElementCreateSystemWide failed".into()))?;
        // Global (process-wide) messaging timeout.
        unsafe { AXUIElementSetMessagingTimeout(sys.raw(), MESSAGING_TIMEOUT_S) };
        if let Some(app) = element_attr(&sys, "AXFocusedApplication") {
            let mut pid = 0;
            if unsafe { AXUIElementGetPid(app.raw(), &mut pid) } == AX_SUCCESS && pid > 0 {
                return Ok((app, pid as u32));
            }
        }
        let pid = frontmost_pid_from_windows().ok_or(DomError::NoFocusedApp)?;
        let app = unsafe { Ax::from_create(AXUIElementCreateApplication(pid as i32)) }.ok_or(DomError::NoFocusedApp)?;
        Ok((app, pid))
    }

    fn snapshot_impl(&mut self, max_depth: u16, interactive_only: bool) -> Result<DomSnapshot> {
        if !unsafe { AXIsProcessTrusted() } {
            return Err(DomError::PermissionDenied("Accessibility permission not granted"));
        }
        let deadline = Instant::now() + self.time_budget;
        let (app, pid) = Self::focused_app()?;
        if self.pid != Some(pid) {
            self.ids.clear();
            self.pid = Some(pid);
        }
        self.ids.begin();
        let screens = Screens::load();
        let names = attr_names_array();

        let app_name = copy_attr(&app, "AXTitle")
            .map_err(|e| ax_error(e, "AXTitle"))?
            .and_then(|v| cf_string(&v))
            .unwrap_or_default();
        let window = element_attr(&app, "AXFocusedWindow").or_else(|| element_attr(&app, "AXMainWindow"));
        let menubar = element_attr(&app, "AXMenuBar");
        let window_title =
            window.as_ref().and_then(|w| copy_attr(w, "AXTitle").ok().flatten()).and_then(|v| cf_string(&v)).unwrap_or_default();

        let root_id = self.ids.id_for(&app);
        let mut root = DomNode {
            id: root_id,
            parent: None,
            role: DomRole::Group,
            native_role: "AXApplication".into(),
            name: app_name.clone(),
            value: None,
            description: None,
            shortcut: None,
            bounds: Rect::default(),
            actions: Vec::new(),
            enabled: true,
            focused: false,
            children: Vec::new(),
        };
        let mut nodes: Vec<DomNode> = Vec::new();
        let mut seen: HashSet<u64> = HashSet::from([root_id]);
        let mut queue: VecDeque<(Ax, u64, u64, u16)> = VecDeque::new();
        let mut allocated = 1usize;
        if max_depth >= 1 {
            for el in [menubar, window].into_iter().flatten() {
                let id = self.ids.id_for(&el);
                if seen.insert(id) {
                    root.children.push(id);
                    queue.push_back((el, id, root_id, 1));
                    allocated += 1;
                }
            }
        }
        nodes.push(root);
        while let Some((el, id, parent, depth)) = queue.pop_front() {
            if Instant::now() > deadline {
                break;
            }
            let vals = match fetch_attrs(&el, &names) {
                Ok(v) => v,
                Err(AX_ERR_API_DISABLED) => {
                    return Err(DomError::PermissionDenied("Accessibility permission not granted"));
                }
                Err(_) => continue, // element vanished or timed out: dropped below
            };
            let (mut node, kids) = self.build_node(id, Some(parent), &el, &vals, &screens);
            if depth < max_depth {
                for k in kids {
                    if allocated >= self.node_budget {
                        break;
                    }
                    let kid = self.ids.id_for(&k);
                    if seen.insert(kid) {
                        node.children.push(kid);
                        queue.push_back((k, kid, id, depth + 1));
                        allocated += 1;
                    }
                }
            }
            nodes.push(node);
        }
        // Drop links to nodes that were never materialised (deadline / errors).
        let present: HashSet<u64> = nodes.iter().map(|n| n.id).collect();
        for n in &mut nodes {
            n.children.retain(|c| present.contains(c));
        }
        derive_container_names(&mut nodes);
        if interactive_only {
            nodes = prune_to_interactive(nodes, root_id);
        }
        self.ids.prune(ID_RETENTION_GENERATIONS);
        self.revision += 1;
        Ok(DomSnapshot { revision: self.revision, app_name, window_title, pid, root: root_id, nodes })
    }

    fn set_bool(el: &Ax, attr: &str, v: bool) -> std::result::Result<(), AXError> {
        set_attr(el, attr, &CFBoolean::from(v).as_CFType())
    }
}

impl DomProvider for AxProvider {
    fn snapshot(&mut self, max_depth: u16, interactive_only: bool) -> Result<DomSnapshot> {
        self.snapshot_impl(max_depth, interactive_only)
    }

    fn perform(&mut self, node: u64, action: DomAction) -> Result<()> {
        let el = self.element(node)?;
        let err = |code, ctx| Self::map_err(node, action, code, ctx);
        match action {
            DomAction::Press | DomAction::Toggle => perform_action(&el, "AXPress").map_err(|e| err(e, "AXPress")),
            DomAction::ShowMenu => perform_action(&el, "AXShowMenu").map_err(|e| err(e, "AXShowMenu")),
            DomAction::Increment => perform_action(&el, "AXIncrement").map_err(|e| err(e, "AXIncrement")),
            DomAction::Decrement => perform_action(&el, "AXDecrement").map_err(|e| err(e, "AXDecrement")),
            DomAction::ScrollIntoView => {
                perform_action(&el, "AXScrollToVisible").map_err(|e| err(e, "AXScrollToVisible"))
            }
            DomAction::Focus => {
                let role = copy_attr(&el, "AXRole").ok().flatten().and_then(|v| cf_string(&v)).unwrap_or_default();
                if role == "AXWindow" {
                    let _ = Self::set_bool(&el, "AXMain", true);
                    perform_action(&el, "AXRaise").map_err(|e| err(e, "AXRaise"))
                } else {
                    Self::set_bool(&el, "AXFocused", true).map_err(|e| err(e, "AXFocused"))
                }
            }
            DomAction::Expand | DomAction::Collapse => {
                let open = action == DomAction::Expand;
                Self::set_bool(&el, "AXDisclosing", open)
                    .or_else(|_| Self::set_bool(&el, "AXExpanded", open))
                    .or_else(|e| if open { perform_action(&el, "AXPress") } else { Err(e) })
                    .map_err(|e| err(e, "AXDisclosing"))
            }
            DomAction::Select => Self::set_bool(&el, "AXSelected", true)
                .or_else(|_| perform_action(&el, "AXPick"))
                .or_else(|_| perform_action(&el, "AXPress"))
                .map_err(|e| err(e, "AXSelected")),
        }
    }

    fn set_value(&mut self, node: u64, value: &str) -> Result<()> {
        let el = self.element(node)?;
        let v = CFString::new(value).as_CFType();
        match set_attr(&el, "AXValue", &v) {
            Ok(()) => Ok(()),
            Err(AX_ERR_INVALID_ELEMENT) => Err(DomError::UnknownNode(node)),
            Err(AX_ERR_API_DISABLED) => Err(DomError::PermissionDenied("Accessibility permission not granted")),
            Err(_) => {
                // Fallback: focus the element, select all and type the text.
                Self::set_bool(&el, "AXFocused", true).map_err(|e| Self::map_err(node, DomAction::Focus, e, "AXFocused"))?;
                use aether_protocol::{HidUsage, InputEvent, Modifiers};
                let mut inj = aether_input::create_injector(Vec::new()).map_err(|e| DomError::Os(e.to_string()))?;
                let evs = [
                    InputEvent::Key { code: HidUsage::A, pressed: true, modifiers: Modifiers::META },
                    InputEvent::Key { code: HidUsage::A, pressed: false, modifiers: Modifiers::META },
                    InputEvent::Key { code: HidUsage::META_LEFT, pressed: false, modifiers: Modifiers::NONE },
                    InputEvent::Text { text: value.to_string() },
                ];
                for e in &evs {
                    inj.inject(e).map_err(|e| DomError::Os(e.to_string()))?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles() {
        assert_eq!(map_role("AXButton", None), DomRole::Button);
        assert_eq!(map_role("AXTextField", Some("AXSearchField")), DomRole::SearchField);
        assert_eq!(map_role("AXTextField", Some("AXSecureTextField")), DomRole::TextField);
        assert_eq!(map_role("AXCheckBox", Some("AXSwitch")), DomRole::Switch);
        assert_eq!(map_role("AXRadioButton", Some("AXTabButton")), DomRole::Tab);
        assert_eq!(map_role("AXRow", Some("AXOutlineRow")), DomRole::TreeItem);
        assert_eq!(map_role("AXWindow", Some("AXStandardWindow")), DomRole::Window);
        assert_eq!(map_role("AXWindow", Some("AXDialog")), DomRole::Dialog);
        assert_eq!(map_role("AXMenuBarItem", None), DomRole::MenuItem);
        assert_eq!(map_role("AXPopUpButton", None), DomRole::PopUpButton);
        assert_eq!(map_role("AXWhatever", None), DomRole::Other);
    }

    #[test]
    fn actions() {
        let n = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            map_actions(&n(&["AXPress", "AXShowMenu"]), DomRole::Button, None, false),
            vec![DomAction::Press, DomAction::ShowMenu]
        );
        assert_eq!(
            map_actions(&n(&["AXPress"]), DomRole::CheckBox, None, false),
            vec![DomAction::Press, DomAction::Toggle]
        );
        assert_eq!(
            map_actions(&n(&["AXIncrement", "AXDecrement"]), DomRole::Slider, None, false),
            vec![DomAction::Increment, DomAction::Decrement]
        );
        assert_eq!(map_actions(&[], DomRole::TextField, None, false), vec![DomAction::Focus]);
        assert_eq!(
            map_actions(&[], DomRole::TreeItem, Some(false), true),
            vec![DomAction::Expand, DomAction::Select]
        );
        assert_eq!(map_actions(&n(&["AXCustomThing"]), DomRole::Group, Some(true), false), vec![DomAction::Collapse]);
    }

    #[test]
    fn shortcuts() {
        assert_eq!(format_shortcut(Some("s"), Some(0), None).as_deref(), Some("⌘S"));
        assert_eq!(format_shortcut(Some("S"), Some(1), None).as_deref(), Some("⇧⌘S"));
        assert_eq!(format_shortcut(Some("F"), Some(4 | 2), None).as_deref(), Some("⌃⌥⌘F"));
        assert_eq!(format_shortcut(Some("Q"), Some(8 | 4), None).as_deref(), Some("⌃Q"));
        assert_eq!(format_shortcut(None, Some(0), Some(0x7E)).as_deref(), Some("⌘↑"));
        assert_eq!(format_shortcut(Some("\u{f704}"), None, None).as_deref(), Some("⌘F1"));
        assert_eq!(format_shortcut(Some("\u{8}"), Some(0), None).as_deref(), Some("⌘⌫"));
        assert_eq!(format_shortcut(None, Some(0), None), None);
        assert_eq!(format_shortcut(Some(""), None, Some(0x99)), None);
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_chars("perché", 4), "perc…");
        assert_eq!(truncate_chars("abc", 3), "abc");
    }

    #[test]
    fn id_map_is_stable_and_prunes() {
        // CFHash/CFEqual work on any CF object; strings stand in for elements.
        let mk = |s: &str| {
            let cf = CFString::new(s);
            unsafe { Ax::from_get(cf.as_CFTypeRef()) }.unwrap()
        };
        let mut m = IdMap::new();
        m.begin();
        let a1 = m.id_for(&mk("button-a"));
        let b1 = m.id_for(&mk("button-b"));
        assert_ne!(a1, b1);
        m.begin();
        assert_eq!(m.id_for(&mk("button-a")), a1, "equal element keeps its id");
        assert!(m.element(b1).is_some());
        for _ in 0..3 {
            m.begin();
            m.id_for(&mk("button-a"));
        }
        m.prune(2);
        assert!(m.element(b1).is_none(), "b unseen for 4 generations");
        assert_eq!(m.id_for(&mk("button-a")), a1);
        let b2 = m.id_for(&mk("button-b"));
        assert!(b2 > b1, "ids are never reused");
        assert_eq!(m.len(), 2);
        m.clear();
        assert!(m.is_empty());
        assert!(m.id_for(&mk("button-a")) > b2);
    }

    #[test]
    fn own_executable_path() {
        let exe = pid_executable(std::process::id()).unwrap();
        assert!(exe.contains("aether_dom"), "{exe}");
    }

    #[test]
    #[ignore = "reads the live window list of the desktop session"]
    fn live_frontmost_pid() {
        let pid = frontmost_pid_from_windows().expect("a front window");
        assert!(pid_executable(pid).is_some());
    }

    #[test]
    #[ignore = "walks the live accessibility tree; needs the Accessibility permission"]
    fn live_snapshot() {
        let mut p = AxProvider::new();
        let s = p.snapshot(12, false).unwrap();
        assert!(!s.nodes.is_empty());
        let s2 = p.snapshot(12, false).unwrap();
        let d = crate::diff(&s, &s2);
        eprintln!("{} nodes, diff upserts {}", s2.nodes.len(), d.upserted.len());
        let mut t = crate::DomTree::from_snapshot(s);
        t.apply_diff(&d).unwrap();
        eprintln!("{:#?}", t.search("file", 5));
    }
}
