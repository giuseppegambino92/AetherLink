//! Windows accessibility provider (Microsoft UI Automation, `IUIAutomation`).
//!
//! * Target: the foreground top-level window (`GetForegroundWindow` → `GetAncestor(GA_ROOT)`),
//!   which becomes the snapshot root. Menus, toolbars and the client area are all part of the
//!   window's UIA subtree.
//! * Traversal: `ElementFromHandleBuildCache` + a Control-view `IUIAutomationTreeWalker`
//!   (`GetFirstChildElementBuildCache` / `GetNextSiblingElementBuildCache`), breadth-first,
//!   depth-limited, with a node budget ([`DEFAULT_NODE_BUDGET`]) and a wall-clock budget
//!   ([`DEFAULT_TIME_BUDGET`]). Every element arrives with one `IUIAutomationCacheRequest`
//!   (name, control type, bounds, enabled/focus state, automation id, accelerator keys,
//!   help text, password flag, Value, Toggle / ExpandCollapse state, pattern availability,
//!   RuntimeId) so reading a node costs no extra cross-process calls. Connection and
//!   transaction timeouts (`IUIAutomation2`) keep a hung application from stalling the host.
//! * Stable ids: a 64-bit hash of the element's `RuntimeId`
//!   ([`runtime_id_hash`](crate::windows_roles::runtime_id_hash)); the element pointers are
//!   kept in a map so `perform` / `set_value` reach the live element. Entries unseen for a
//!   few snapshots are released.
//! * Bounds: `BoundingRectangle` is in physical screen pixels when the process is
//!   per-monitor DPI aware (the AetherLink manifest declares it), i.e. the same space as
//!   `aether-capture`'s `DisplayInfo`.
//! * Password fields never expose their value.
//!
//! ## Threading
//! COM is initialised as MTA on whichever thread calls the provider (once per thread; a
//! thread that is already STA is tolerated — `RPC_E_CHANGED_MODE`). The UIA client objects
//! (`CUIAutomation8`) are free-threaded: they aggregate the free-threaded marshaler, so the
//! interface pointers held by [`UiaProvider`] may be used from any COM-initialised thread.
//! That is why `UiaProvider` is `Send` although `windows-rs` interface types are not.
//!
//! Compile-verified for `x86_64-pc-windows-*`; not exercised on a real Windows desktop from
//! the macOS development machine.

use crate::windows_roles::{
    actions_for, control_type, control_type_name, refine_role, runtime_id_hash, truncate_chars, Patterns, RoleHints,
};
use crate::{derive_container_names, prune_to_interactive, DomError, DomProvider, Result};
use aether_protocol::{DomAction, DomNode, DomRole, DomSnapshot, Rect};
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};
use windows::core::{Interface, BSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, E_NOINTERFACE, E_NOTIMPL, E_POINTER, HWND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, SAFEARRAY};
use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::{VARIANT, VT_ARRAY, VT_BOOL, VT_BSTR, VT_I4, VT_R8};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, SetForegroundWindow,
    GA_ROOT,
};

pub const DEFAULT_NODE_BUDGET: usize = 5000;
pub const DEFAULT_TIME_BUDGET: Duration = Duration::from_millis(1500);
/// Per-call UIA transaction timeout (ms) — a hung target fails fast instead of blocking.
const TRANSACTION_TIMEOUT_MS: u32 = 1000;
const CONNECTION_TIMEOUT_MS: u32 = 2000;
const MAX_VALUE_CHARS: usize = 256;
/// Snapshots an unseen element is remembered for.
const ID_RETENTION_GENERATIONS: u64 = 8;

// ---------------------------------------------------------------------------
// COM / Win32 helpers
// ---------------------------------------------------------------------------

thread_local! {
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

/// Initialise COM (MTA) on the current thread once. An existing STA is accepted.
pub(crate) fn ensure_com() -> Result<()> {
    if COM_READY.with(Cell::get) {
        return Ok(());
    }
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_ok() || hr == RPC_E_CHANGED_MODE {
        // The apartment stays initialised for the life of the thread (the provider may be
        // called on it again); it is torn down by the OS at thread exit.
        COM_READY.with(|c| c.set(true));
        Ok(())
    } else {
        Err(DomError::Accessibility { code: hr.0, context: "CoInitializeEx" })
    }
}

fn hr_err(e: windows::core::Error, context: &'static str) -> DomError {
    DomError::Accessibility { code: e.code().0, context }
}

pub(crate) fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

pub(crate) fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    wide_to_string(&buf[..n.max(0) as usize])
}

pub(crate) fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    wide_to_string(&buf[..n.max(0) as usize])
}

/// Foreground top-level window and its process id.
pub(crate) fn foreground_window() -> Option<(HWND, u32)> {
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_invalid() {
        return None;
    }
    let root = unsafe { GetAncestor(fg, GA_ROOT) };
    let hwnd = if root.is_invalid() { fg } else { root };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    Some((hwnd, pid))
}

/// Full Win32 path of a process image (`QueryFullProcessImageNameW`).
pub(crate) fn process_image_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// `C:\Program Files\App\app.exe` → `app`.
fn exe_stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    match file.rfind('.') {
        Some(i) if i > 0 => file[..i].to_string(),
        _ => file.to_string(),
    }
}

// --- VARIANT readers (type-checked; anything else, incl. "not supported", → None) ---

fn var_bool(v: &VARIANT) -> Option<bool> {
    (v.vt() == VT_BOOL).then(|| unsafe { v.Anonymous.Anonymous.Anonymous.boolVal.0 != 0 })
}

fn var_i32(v: &VARIANT) -> Option<i32> {
    (v.vt() == VT_I4).then(|| unsafe { v.Anonymous.Anonymous.Anonymous.lVal })
}

fn var_f64(v: &VARIANT) -> Option<f64> {
    (v.vt() == VT_R8).then(|| unsafe { v.Anonymous.Anonymous.Anonymous.dblVal })
}

fn var_string(v: &VARIANT) -> Option<String> {
    (v.vt() == VT_BSTR).then(|| unsafe { (*v.Anonymous.Anonymous.Anonymous.bstrVal).to_string() })
}

/// Reads a one-dimensional `VT_I4` SAFEARRAY (not destroyed).
unsafe fn safearray_i32(psa: *const SAFEARRAY) -> Option<Vec<i32>> {
    if psa.is_null() {
        return None;
    }
    unsafe {
        let lo = SafeArrayGetLBound(psa, 1).ok()?;
        let hi = SafeArrayGetUBound(psa, 1).ok()?;
        let n = (hi - lo + 1).max(0) as usize;
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        SafeArrayAccessData(psa, &mut data).ok()?;
        let out = if data.is_null() { Vec::new() } else { std::slice::from_raw_parts(data as *const i32, n).to_vec() };
        let _ = SafeArrayUnaccessData(psa);
        Some(out)
    }
}

fn var_i32_array(v: &VARIANT) -> Option<Vec<i32>> {
    if v.vt() != (VT_ARRAY | VT_I4) {
        return None;
    }
    unsafe { safearray_i32(v.Anonymous.Anonymous.Anonymous.parray) }
}

fn bstr_opt(r: windows::core::Result<BSTR>) -> Option<String> {
    r.ok().map(|b| b.to_string().trim().to_string()).filter(|s| !s.is_empty())
}

fn rect_from(r: windows::Win32::Foundation::RECT) -> Rect {
    Rect::new(r.left, r.top, (r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32)
}

// ---------------------------------------------------------------------------
// Stable ids
// ---------------------------------------------------------------------------

struct Entry {
    el: IUIAutomationElement,
    last_seen: u64,
}

#[derive(Default)]
struct IdMap {
    map: HashMap<u64, Entry>,
    generation: u64,
}

impl IdMap {
    fn begin(&mut self) {
        self.generation += 1;
    }

    fn insert(&mut self, id: u64, el: &IUIAutomationElement) {
        self.map.insert(id, Entry { el: el.clone(), last_seen: self.generation });
    }

    fn element(&self, id: u64) -> Option<IUIAutomationElement> {
        self.map.get(&id).map(|e| e.el.clone())
    }

    fn prune(&mut self, keep: u64) {
        let min = self.generation.saturating_sub(keep);
        self.map.retain(|_, e| e.last_seen >= min);
    }

    fn clear(&mut self) {
        self.map.clear();
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Properties fetched for every element (one cross-process round trip per element).
const CACHED_PROPERTIES: &[UIA_PROPERTY_ID] = &[
    UIA_RuntimeIdPropertyId,
    UIA_NamePropertyId,
    UIA_ControlTypePropertyId,
    UIA_LocalizedControlTypePropertyId,
    UIA_BoundingRectanglePropertyId,
    UIA_IsEnabledPropertyId,
    UIA_HasKeyboardFocusPropertyId,
    UIA_IsKeyboardFocusablePropertyId,
    UIA_AutomationIdPropertyId,
    UIA_ClassNamePropertyId,
    UIA_AcceleratorKeyPropertyId,
    UIA_AccessKeyPropertyId,
    UIA_HelpTextPropertyId,
    UIA_IsPasswordPropertyId,
    UIA_IsValuePatternAvailablePropertyId,
    UIA_ValueValuePropertyId,
    UIA_ValueIsReadOnlyPropertyId,
    UIA_IsInvokePatternAvailablePropertyId,
    UIA_IsTogglePatternAvailablePropertyId,
    UIA_ToggleToggleStatePropertyId,
    UIA_IsExpandCollapsePatternAvailablePropertyId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId,
    UIA_IsSelectionItemPatternAvailablePropertyId,
    UIA_IsLegacyIAccessiblePatternAvailablePropertyId,
    UIA_LegacyIAccessibleDefaultActionPropertyId,
    UIA_IsRangeValuePatternAvailablePropertyId,
    UIA_RangeValueValuePropertyId,
    UIA_IsScrollItemPatternAvailablePropertyId,
];

/// Newer properties (Windows 10 1803/1809+); added best-effort.
const OPTIONAL_PROPERTIES: &[UIA_PROPERTY_ID] = &[UIA_HeadingLevelPropertyId, UIA_IsDialogPropertyId];

/// UI Automation [`DomProvider`] for the foreground window.
pub struct UiaProvider {
    automation: IUIAutomation,
    walker: IUIAutomationTreeWalker,
    cache: IUIAutomationCacheRequest,
    ids: IdMap,
    revision: u64,
    hwnd: isize,
    node_budget: usize,
    time_budget: Duration,
}

// SAFETY: see the module docs — CUIAutomation objects are free-threaded (FTM-aggregated) and
// every entry point initialises COM on the calling thread before touching them. The provider
// is only ever used through `&mut self`, so there is no concurrent access.
unsafe impl Send for UiaProvider {}

impl UiaProvider {
    pub fn new() -> Result<Self> {
        Self::with_limits(DEFAULT_NODE_BUDGET, DEFAULT_TIME_BUDGET)
    }

    pub fn with_limits(node_budget: usize, time_budget: Duration) -> Result<Self> {
        ensure_com()?;
        let automation: IUIAutomation = unsafe {
            CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
                .or_else(|_| CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER))
                .map_err(|e| hr_err(e, "CoCreateInstance(CUIAutomation)"))?
        };
        if let Ok(a2) = automation.cast::<IUIAutomation2>() {
            unsafe {
                let _ = a2.SetConnectionTimeout(CONNECTION_TIMEOUT_MS);
                let _ = a2.SetTransactionTimeout(TRANSACTION_TIMEOUT_MS);
            }
        }
        let walker = unsafe { automation.ControlViewWalker() }.map_err(|e| hr_err(e, "ControlViewWalker"))?;
        let cache = unsafe { automation.CreateCacheRequest() }.map_err(|e| hr_err(e, "CreateCacheRequest"))?;
        unsafe {
            for &p in CACHED_PROPERTIES {
                cache.AddProperty(p).map_err(|e| hr_err(e, "IUIAutomationCacheRequest::AddProperty"))?;
            }
            for &p in OPTIONAL_PROPERTIES {
                let _ = cache.AddProperty(p);
            }
            cache.SetTreeScope(TreeScope_Element).map_err(|e| hr_err(e, "SetTreeScope"))?;
            cache
                .SetAutomationElementMode(AutomationElementMode_Full)
                .map_err(|e| hr_err(e, "SetAutomationElementMode"))?;
        }
        Ok(Self {
            automation,
            walker,
            cache,
            ids: IdMap::default(),
            revision: 0,
            hwnd: 0,
            node_budget: node_budget.max(2),
            time_budget,
        })
    }

    fn cached(el: &IUIAutomationElement, p: UIA_PROPERTY_ID) -> Option<VARIANT> {
        unsafe { el.GetCachedPropertyValueEx(p, false) }.ok()
    }

    fn cached_bool(el: &IUIAutomationElement, p: UIA_PROPERTY_ID) -> bool {
        Self::cached(el, p).as_ref().and_then(var_bool).unwrap_or(false)
    }

    /// Id for an element: RuntimeId hash, falling back to its position in the tree.
    fn element_id(el: &IUIAutomationElement, parent: Option<u64>, index: usize, ct: i32) -> u64 {
        let rid = Self::cached(el, UIA_RuntimeIdPropertyId)
            .as_ref()
            .and_then(var_i32_array)
            .filter(|r| !r.is_empty())
            .or_else(|| unsafe {
                let psa = el.GetRuntimeId().ok()?;
                let v = safearray_i32(psa);
                let _ = windows::Win32::System::Ole::SafeArrayDestroy(psa);
                v
            });
        match rid {
            Some(r) => runtime_id_hash(&r),
            None => {
                let p = parent.unwrap_or(0);
                runtime_id_hash(&[(p >> 32) as i32, p as i32, index as i32, ct, -1])
            }
        }
    }

    fn build_node(el: &IUIAutomationElement, id: u64, parent: Option<u64>) -> DomNode {
        unsafe {
            let ct = el.CachedControlType().map(|c| c.0).unwrap_or(control_type::CUSTOM);
            let name = bstr_opt(el.CachedName());
            let localized = bstr_opt(el.CachedLocalizedControlType()).unwrap_or_default();
            let automation_id = bstr_opt(el.CachedAutomationId()).unwrap_or_default();
            let class_name = bstr_opt(el.CachedClassName()).unwrap_or_default();
            let help = bstr_opt(el.CachedHelpText());
            let accel = bstr_opt(el.CachedAcceleratorKey()).or_else(|| bstr_opt(el.CachedAccessKey()));
            let enabled = el.CachedIsEnabled().map(|b| b.as_bool()).unwrap_or(true);
            let focused = el.CachedHasKeyboardFocus().map(|b| b.as_bool()).unwrap_or(false);
            let password = el.CachedIsPassword().map(|b| b.as_bool()).unwrap_or(false);
            let bounds = el.CachedBoundingRectangle().map(rect_from).unwrap_or_default();

            let legacy_action = if Self::cached_bool(el, UIA_IsLegacyIAccessiblePatternAvailablePropertyId) {
                Self::cached(el, UIA_LegacyIAccessibleDefaultActionPropertyId).as_ref().and_then(var_string)
            } else {
                None
            };
            let p = Patterns {
                invoke: Self::cached_bool(el, UIA_IsInvokePatternAvailablePropertyId),
                toggle: Self::cached_bool(el, UIA_IsTogglePatternAvailablePropertyId),
                expand_collapse: Self::cached_bool(el, UIA_IsExpandCollapsePatternAvailablePropertyId),
                selection_item: Self::cached_bool(el, UIA_IsSelectionItemPatternAvailablePropertyId),
                legacy: Self::cached_bool(el, UIA_IsLegacyIAccessiblePatternAvailablePropertyId),
                legacy_default_action: legacy_action.as_deref().is_some_and(|s| !s.trim().is_empty()),
                range_value: Self::cached_bool(el, UIA_IsRangeValuePatternAvailablePropertyId),
                scroll_item: Self::cached_bool(el, UIA_IsScrollItemPatternAvailablePropertyId),
                value: Self::cached_bool(el, UIA_IsValuePatternAvailablePropertyId),
                value_read_only: Self::cached_bool(el, UIA_ValueIsReadOnlyPropertyId),
                keyboard_focusable: el.CachedIsKeyboardFocusable().map(|b| b.as_bool()).unwrap_or(false),
            };
            let heading = Self::cached(el, UIA_HeadingLevelPropertyId)
                .as_ref()
                .and_then(var_i32)
                .is_some_and(|l| l != HeadingLevel_None.0 && l != 0);
            let hints = RoleHints {
                is_dialog: Self::cached_bool(el, UIA_IsDialogPropertyId),
                is_heading: heading,
                automation_id: &automation_id,
                class_name: &class_name,
                localized_type: &localized,
            };
            let role = refine_role(ct, &p, &hints);

            let raw_value: Option<String> = if password {
                None
            } else if p.value {
                Self::cached(el, UIA_ValueValuePropertyId).as_ref().and_then(var_string)
            } else if p.range_value {
                Self::cached(el, UIA_RangeValueValuePropertyId).as_ref().and_then(var_f64).map(|v| {
                    if v.fract() == 0.0 { format!("{v:.0}") } else { format!("{v}") }
                })
            } else if p.toggle {
                Self::cached(el, UIA_ToggleToggleStatePropertyId).as_ref().and_then(var_i32).map(|s| {
                    if s == ToggleState_On.0 {
                        "on"
                    } else if s == ToggleState_Off.0 {
                        "off"
                    } else {
                        "mixed"
                    }
                    .to_string()
                })
            } else {
                None
            };
            let raw_value = raw_value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            let (name, value) = match (name, raw_value) {
                (None, Some(v)) if role == DomRole::StaticText => (Some(v), None),
                (n, v) => (n, v.map(|v| truncate_chars(&v, MAX_VALUE_CHARS))),
            };
            let description = help.filter(|h| Some(h) != name.as_ref());
            let native_role = match control_type_name(ct) {
                "Unknown" => format!("ControlType({ct})"),
                n => n.to_string(),
            };
            DomNode {
                id,
                parent,
                role,
                native_role,
                name: name.unwrap_or_default(),
                value,
                description,
                shortcut: accel,
                bounds,
                actions: actions_for(role, &p, enabled),
                enabled,
                focused,
                children: Vec::new(),
            }
        }
    }

    fn snapshot_impl(&mut self, max_depth: u16, interactive_only: bool) -> Result<DomSnapshot> {
        ensure_com()?;
        let deadline = Instant::now() + self.time_budget;
        let (hwnd, pid) = foreground_window().ok_or(DomError::NoFocusedApp)?;
        if self.hwnd != hwnd.0 as isize {
            self.ids.clear();
            self.hwnd = hwnd.0 as isize;
        }
        self.ids.begin();
        let app_name = process_image_path(pid).map(|p| exe_stem(&p)).unwrap_or_default();
        let root_el = unsafe { self.automation.ElementFromHandleBuildCache(hwnd, &self.cache) }.map_err(|e| {
            if e.code().0 as u32 == UIA_E_ELEMENTNOTAVAILABLE { DomError::NoFocusedApp } else { hr_err(e, "ElementFromHandle") }
        })?;
        let ct = unsafe { root_el.CachedControlType() }.map(|c| c.0).unwrap_or(control_type::WINDOW);
        let root_id = Self::element_id(&root_el, None, 0, ct);
        let mut window_title = window_text(hwnd);

        let mut nodes: Vec<DomNode> = Vec::new();
        let mut seen: HashSet<u64> = HashSet::from([root_id]);
        let mut queue: VecDeque<(IUIAutomationElement, u64, Option<u64>, u16)> = VecDeque::new();
        queue.push_back((root_el, root_id, None, 0));
        let mut allocated = 1usize;
        while let Some((el, id, parent, depth)) = queue.pop_front() {
            if Instant::now() > deadline && parent.is_some() {
                break;
            }
            self.ids.insert(id, &el);
            let mut node = Self::build_node(&el, id, parent);
            if parent.is_none() {
                if window_title.is_empty() {
                    window_title = node.name.clone();
                }
                if node.role != DomRole::Dialog {
                    node.role = DomRole::Window;
                }
            }
            if depth < max_depth {
                let mut child = unsafe { self.walker.GetFirstChildElementBuildCache(&el, &self.cache) }.ok();
                let mut index = 0usize;
                while let Some(c) = child {
                    if allocated >= self.node_budget || Instant::now() > deadline {
                        break;
                    }
                    let cct = unsafe { c.CachedControlType() }.map(|t| t.0).unwrap_or(0);
                    let mut kid = Self::element_id(&c, Some(id), index, cct);
                    // Guard against providers that reuse RuntimeIds (ids must be unique).
                    let mut salt = 0i32;
                    while seen.contains(&kid) {
                        salt += 1;
                        kid = runtime_id_hash(&[(kid >> 32) as i32, kid as i32, salt]);
                    }
                    seen.insert(kid);
                    node.children.push(kid);
                    allocated += 1;
                    index += 1;
                    let next = unsafe { self.walker.GetNextSiblingElementBuildCache(&c, &self.cache) }.ok();
                    queue.push_back((c, kid, Some(id), depth + 1));
                    child = next;
                }
            }
            nodes.push(node);
        }
        // Drop links to nodes that were never materialised (deadline).
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

    fn element(&self, node: u64) -> Result<IUIAutomationElement> {
        self.ids.element(node).ok_or(DomError::UnknownNode(node))
    }

    fn map_err(node: u64, action: DomAction, e: windows::core::Error, ctx: &'static str) -> DomError {
        let code = e.code();
        match code.0 as u32 {
            UIA_E_ELEMENTNOTAVAILABLE => DomError::UnknownNode(node),
            UIA_E_NOTSUPPORTED | UIA_E_ELEMENTNOTENABLED | UIA_E_INVALIDOPERATION => {
                DomError::ActionUnsupported { node, action }
            }
            _ if code == E_NOINTERFACE || code == E_POINTER || code == E_NOTIMPL => {
                DomError::ActionUnsupported { node, action }
            }
            _ => hr_err(e, ctx),
        }
    }

    fn pattern<T: Interface>(el: &IUIAutomationElement, id: UIA_PATTERN_ID) -> Option<T> {
        unsafe { el.GetCurrentPatternAs::<T>(id) }.ok()
    }

    fn step_range(el: &IUIAutomationElement, up: bool) -> Option<windows::core::Result<()>> {
        let rv: IUIAutomationRangeValuePattern = Self::pattern(el, UIA_RangeValuePatternId)?;
        Some(unsafe {
            (|| {
                let v = rv.CurrentValue()?;
                let (min, max) = (rv.CurrentMinimum()?, rv.CurrentMaximum()?);
                let mut step = rv.CurrentSmallChange().unwrap_or(0.0);
                if step <= 0.0 || !step.is_finite() {
                    step = if max > min { ((max - min) / 10.0).max(f64::EPSILON) } else { 1.0 };
                }
                let target = if up { v + step } else { v - step };
                rv.SetValue(if max > min { target.clamp(min, max) } else { target })
            })()
        })
    }

    fn press(el: &IUIAutomationElement) -> Option<windows::core::Result<()>> {
        unsafe {
            if let Some(p) = Self::pattern::<IUIAutomationInvokePattern>(el, UIA_InvokePatternId) {
                return Some(p.Invoke());
            }
            if let Some(p) = Self::pattern::<IUIAutomationTogglePattern>(el, UIA_TogglePatternId) {
                return Some(p.Toggle());
            }
            if let Some(p) = Self::pattern::<IUIAutomationSelectionItemPattern>(el, UIA_SelectionItemPatternId) {
                return Some(p.Select());
            }
            if let Some(p) = Self::pattern::<IUIAutomationExpandCollapsePattern>(el, UIA_ExpandCollapsePatternId) {
                return Some(if p.CurrentExpandCollapseState() == Ok(ExpandCollapseState_Collapsed) {
                    p.Expand()
                } else {
                    p.Collapse()
                });
            }
            Self::pattern::<IUIAutomationLegacyIAccessiblePattern>(el, UIA_LegacyIAccessiblePatternId)
                .map(|p| p.DoDefaultAction())
        }
    }

    fn focus(el: &IUIAutomationElement) -> windows::core::Result<()> {
        unsafe {
            let is_window = el.CurrentControlType().map(|c| c.0) == Ok(control_type::WINDOW);
            if is_window
                && let Ok(h) = el.CurrentNativeWindowHandle()
                && !h.is_invalid()
            {
                let _ = SetForegroundWindow(h);
            }
            el.SetFocus()
        }
    }

    fn perform_impl(&mut self, node: u64, action: DomAction) -> Result<()> {
        ensure_com()?;
        let el = self.element(node)?;
        let unsupported = DomError::ActionUnsupported { node, action };
        let res: Option<windows::core::Result<()>> = unsafe {
            match action {
                DomAction::Press => Self::press(&el),
                DomAction::Toggle => Self::pattern::<IUIAutomationTogglePattern>(&el, UIA_TogglePatternId)
                    .map(|p| p.Toggle())
                    .or_else(|| Self::press(&el)),
                DomAction::Expand | DomAction::ShowMenu => {
                    Self::pattern::<IUIAutomationExpandCollapsePattern>(&el, UIA_ExpandCollapsePatternId)
                        .map(|p| p.Expand())
                }
                DomAction::Collapse => {
                    Self::pattern::<IUIAutomationExpandCollapsePattern>(&el, UIA_ExpandCollapsePatternId)
                        .map(|p| p.Collapse())
                }
                DomAction::Select => {
                    Self::pattern::<IUIAutomationSelectionItemPattern>(&el, UIA_SelectionItemPatternId)
                        .map(|p| p.Select())
                        .or_else(|| {
                            Self::pattern::<IUIAutomationLegacyIAccessiblePattern>(&el, UIA_LegacyIAccessiblePatternId)
                                .map(|p| p.Select((SELFLAG_TAKEFOCUS | SELFLAG_TAKESELECTION) as i32))
                        })
                }
                DomAction::Increment => Self::step_range(&el, true),
                DomAction::Decrement => Self::step_range(&el, false),
                DomAction::ScrollIntoView => {
                    Self::pattern::<IUIAutomationScrollItemPattern>(&el, UIA_ScrollItemPatternId)
                        .map(|p| p.ScrollIntoView())
                }
                DomAction::Focus => Some(Self::focus(&el)),
            }
        };
        match res {
            None => Err(unsupported),
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(Self::map_err(node, action, e, "UIA pattern call")),
        }
    }

    fn set_value_impl(&mut self, node: u64, value: &str) -> Result<()> {
        ensure_com()?;
        let el = self.element(node)?;
        let as_err = |e: windows::core::Error, ctx| Self::map_err(node, DomAction::Focus, e, ctx);
        unsafe {
            if let Some(p) = Self::pattern::<IUIAutomationValuePattern>(&el, UIA_ValuePatternId)
                && !p.CurrentIsReadOnly().map(|b| b.as_bool()).unwrap_or(false)
            {
                match p.SetValue(&BSTR::from(value)) {
                    Ok(()) => return Ok(()),
                    Err(e) if e.code().0 as u32 == UIA_E_ELEMENTNOTAVAILABLE => {
                        return Err(DomError::UnknownNode(node));
                    }
                    Err(_) => {} // fall through to the other strategies
                }
            }
            if let Ok(v) = value.trim().parse::<f64>()
                && let Some(p) = Self::pattern::<IUIAutomationRangeValuePattern>(&el, UIA_RangeValuePatternId)
                && p.SetValue(v).is_ok()
            {
                return Ok(());
            }
            if let Some(p) =
                Self::pattern::<IUIAutomationLegacyIAccessiblePattern>(&el, UIA_LegacyIAccessiblePatternId)
            {
                let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                if p.SetValue(PCWSTR(wide.as_ptr())).is_ok() {
                    return Ok(());
                }
            }
            // Fallback: focus the element, select all and type the text.
            el.SetFocus().map_err(|e| as_err(e, "SetFocus"))?;
        }
        use aether_protocol::{HidUsage, InputEvent, Modifiers};
        let mut inj = aether_input::create_injector(Vec::new()).map_err(|e| DomError::Os(e.to_string()))?;
        let evs = [
            InputEvent::Key { code: HidUsage::A, pressed: true, modifiers: Modifiers::CTRL },
            InputEvent::Key { code: HidUsage::A, pressed: false, modifiers: Modifiers::CTRL },
            InputEvent::Key { code: HidUsage::CONTROL_LEFT, pressed: false, modifiers: Modifiers::NONE },
            InputEvent::Text { text: value.to_string() },
        ];
        for e in &evs {
            inj.inject(e).map_err(|e| DomError::Os(e.to_string()))?;
        }
        Ok(())
    }
}

impl DomProvider for UiaProvider {
    fn snapshot(&mut self, max_depth: u16, interactive_only: bool) -> Result<DomSnapshot> {
        self.snapshot_impl(max_depth, interactive_only)
    }

    fn perform(&mut self, node: u64, action: DomAction) -> Result<()> {
        self.perform_impl(node, action)
    }

    fn set_value(&mut self, node: u64, value: &str) -> Result<()> {
        self.set_value_impl(node, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_type_ids_match_sdk() {
        use crate::windows_roles::control_type as ct;
        assert_eq!(UIA_ButtonControlTypeId.0, ct::BUTTON);
        assert_eq!(UIA_EditControlTypeId.0, ct::EDIT);
        assert_eq!(UIA_DocumentControlTypeId.0, ct::DOCUMENT);
        assert_eq!(UIA_WindowControlTypeId.0, ct::WINDOW);
        assert_eq!(UIA_DataItemControlTypeId.0, ct::DATA_ITEM);
        assert_eq!(UIA_SplitButtonControlTypeId.0, ct::SPLIT_BUTTON);
        assert_eq!(UIA_AppBarControlTypeId.0, ct::APP_BAR);
    }

    #[test]
    fn exe_names() {
        assert_eq!(exe_stem(r"C:\Program Files\Microsoft VS Code\Code.exe"), "Code");
        assert_eq!(exe_stem("notepad"), "notepad");
    }

    #[test]
    #[ignore = "needs an interactive Windows desktop"]
    fn live_snapshot_of_foreground_window() {
        let mut p = UiaProvider::new().expect("UIA available");
        let s = p.snapshot(12, false).expect("snapshot");
        assert!(!s.nodes.is_empty());
        let again = p.snapshot(12, false).expect("snapshot");
        assert_eq!(s.root, again.root, "root id is stable");
        assert!(again.revision > s.revision);
    }
}
