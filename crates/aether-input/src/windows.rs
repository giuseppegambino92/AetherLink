//! Windows injector: `SendInput` with scan codes (layout-independent, works
//! in games using raw input / DirectInput), `KEYEVENTF_UNICODE` for text,
//! absolute moves over the virtual desktop, and input-desktop following so
//! injection keeps working on the UAC secure desktop / lock screen when the
//! host runs as a service in the user's session (`OpenInputDesktop` +
//! `SetThreadDesktop`; the thread must own no windows or hooks).
//!
//! The process must be per-monitor DPI aware (application manifest) so
//! `DisplayInfo` physical pixels and `GetSystemMetrics(SM_*VIRTUALSCREEN)`
//! share one coordinate space.
//!
//! ## Input-desktop following
//!
//! Before injecting (at most every [`DESKTOP_RECHECK`], and again after any
//! `SendInput` failure) the thread re-attaches to the current input desktop
//! with `OpenInputDesktop` + `SetThreadDesktop`. Only a process running as
//! `LocalSystem` in the interactive session (what `aether-winsvc` launches)
//! may open `Winsta0\Winlogon`, so only then are the lock screen, the
//! Ctrl+Alt+Del screen and UAC prompts controllable; a normal user process
//! silently stays on `Default`.
//!
//! ## Ctrl+Alt+Del
//!
//! `SendSAS` is resolved at run time from `sas.dll` (absent on some SKUs), and
//! only called when the caller is allowed to: the process must run as
//! `LocalSystem` (`SendSAS(FALSE)`, needs the `SoftwareSASGeneration` policy
//! value 1 or 3) or be a signed `uiAccess` app (`SendSAS(TRUE)`, value 2 or 3).
//! `aether-winsvc install` sets the policy to 1. Otherwise a clear
//! [`InputError::PermissionDenied`] is returned instead of a silent no-op.
//!
//! Compile-verified with clippy for `x86_64-pc-windows-{msvc,gnu}`; not
//! run-tested from the macOS dev machine.

use crate::state::{
    display_pixel_rect, normalized_to_global, plan_key, to_virtual_desk_absolute, Accumulator, ModifierState,
};
use crate::{keymap, InputError, InputInjector, Result, INJECTED_EVENT_TAG};
use aether_protocol::{
    DisplayInfo, Gesture, GesturePhase, HidUsage, InputEvent, Modifiers, MouseButton, ScrollUnit, SystemAction,
};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, POINT};
use windows_sys::Win32::Security::{GetTokenInformation, IsWellKnownSid, TokenUser, WinLocalSystemSid, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
use windows_sys::Win32::System::Shutdown::LockWorkStation;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, HDESK, UOI_NAME,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};

const WHEEL_DELTA: f64 = 120.0;
/// `ScrollUnit::Pixel` → wheel units (120 = one notch ≈ 40 px).
const WHEEL_UNITS_PER_PIXEL: f64 = 3.0;
const EXTRA: usize = INJECTED_EVENT_TAG as usize;
/// Minimum interval between two input-desktop checks while injecting.
pub const DESKTOP_RECHECK: Duration = Duration::from_millis(100);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// True when the current process token is `NT AUTHORITY\SYSTEM`.
pub fn running_as_local_system() -> bool {
    // SAFETY: token handle is closed below; the TOKEN_USER buffer is u64-aligned and its
    // size is passed to GetTokenInformation, which validates it.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut buf = [0u64; 64];
        let mut len = 0u32;
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), std::mem::size_of_val(&buf) as u32, &mut len);
        CloseHandle(token);
        if ok == 0 {
            return false;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        IsWellKnownSid(user.User.Sid, WinLocalSystemSid) != 0
    }
}

/// `HKLM\...\Policies\System\SoftwareSASGeneration` (0 = none, 1 = services,
/// 2 = ease-of-access apps, 3 = both). `None` when unset (= 0).
pub fn software_sas_policy() -> Option<u32> {
    let key = wide(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System");
    let value = wide("SoftwareSASGeneration");
    let mut data = 0u32;
    let mut size = 4u32;
    // SAFETY: DWORD read into a 4-byte buffer whose size is passed.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (r == 0).then_some(data)
}

type SendSasFn = unsafe extern "system" fn(as_user: i32);

/// Resolves `SendSAS` from `%SystemRoot%\System32\sas.dll` (kept loaded for the process).
fn send_sas_fn() -> Option<SendSasFn> {
    let name = wide("sas.dll");
    // SAFETY: loading a system DLL from System32 only; the module is never freed, so the
    // function pointer stays valid. The transmute matches `VOID SendSAS(BOOL AsUser)`.
    unsafe {
        let module = LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if module.is_null() {
            return None;
        }
        let f = GetProcAddress(module, c"SendSAS".as_ptr().cast())?;
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, SendSasFn>(f))
    }
}

/// Generates the secure attention sequence (Ctrl+Alt+Del) when the process may.
pub fn send_secure_attention_sequence() -> Result<()> {
    let system = running_as_local_system();
    let policy = software_sas_policy().unwrap_or(0);
    // As SYSTEM the service flavour applies (AsUser = FALSE); otherwise only a uiAccess
    // application may succeed (AsUser = TRUE).
    let (allowed, as_user) = if system { (policy & 1 != 0, 0) } else { (policy & 2 != 0, 1) };
    if !allowed {
        return Err(InputError::PermissionDenied(if system {
            "Ctrl+Alt+Del needs the SoftwareSASGeneration policy set to 1 or 3 (run `aether-winsvc install`)"
        } else {
            "Ctrl+Alt+Del needs the AetherLink service (LocalSystem) or a uiAccess build with SoftwareSASGeneration 2/3"
        }));
    }
    let f = send_sas_fn().ok_or(InputError::Unsupported("sas.dll / SendSAS is not available on this Windows edition"))?;
    // SAFETY: valid function pointer resolved above; SendSAS has no failure result.
    unsafe { f(as_user) };
    Ok(())
}

fn kbd(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: EXTRA } },
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: EXTRA },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() {
        // Blocked by UIPI (target runs elevated) or by the secure desktop.
        return Err(InputError::Os(format!(
            "SendInput injected {n}/{} events: {}",
            inputs.len(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn key_input(u: HidUsage, pressed: bool) -> Result<INPUT> {
    let k = keymap::hid_to_windows(u)
        .ok_or_else(|| InputError::InvalidArgument(format!("HID usage {:#04x} has no Windows key", u.0)))?;
    let mut flags = if pressed { 0 } else { KEYEVENTF_KEYUP };
    if k.extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if keymap::windows_inject_by_vk(u) {
        Ok(kbd(k.vk, k.scancode, flags))
    } else {
        Ok(kbd(0, k.scancode, flags | KEYEVENTF_SCANCODE))
    }
}

fn desktop_name(h: HDESK) -> Option<Vec<u16>> {
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    let ok = unsafe {
        GetUserObjectInformationW(h as _, UOI_NAME, buf.as_mut_ptr().cast(), (buf.len() * 2) as u32, &mut needed)
    };
    if ok == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(buf[..len].to_vec())
}

pub struct WinInjector {
    displays: Vec<DisplayInfo>,
    mods: ModifierState,
    keys_down: BTreeSet<u16>,
    buttons: BTreeSet<u8>,
    rel_x: Accumulator,
    rel_y: Accumulator,
    wheel_v: Accumulator,
    wheel_h: Accumulator,
    zoom: Accumulator,
    /// Desktop handle we switched the thread to (closed on drop / next switch).
    desktop: HDESK,
    /// When the input desktop was last checked.
    desktop_checked: Option<Instant>,
}

// HDESK is a kernel handle usable from any thread; the injector is used by one thread at a time.
unsafe impl Send for WinInjector {}

impl Drop for WinInjector {
    fn drop(&mut self) {
        if !self.desktop.is_null() {
            unsafe { CloseDesktop(self.desktop) };
        }
    }
}

impl WinInjector {
    pub fn new(displays: Vec<DisplayInfo>) -> Result<Self> {
        Ok(Self {
            displays,
            mods: ModifierState::default(),
            keys_down: BTreeSet::new(),
            buttons: BTreeSet::new(),
            rel_x: Accumulator::default(),
            rel_y: Accumulator::default(),
            wheel_v: Accumulator::default(),
            wheel_h: Accumulator::default(),
            zoom: Accumulator::default(),
            desktop: std::ptr::null_mut(),
            desktop_checked: None,
        })
    }

    /// Follow the input desktop (Default ↔ Winlogon ↔ Screen-saver). Returns true when
    /// the thread switched to a different desktop.
    fn follow_input_desktop(&mut self) -> bool {
        self.desktop_checked = Some(Instant::now());
        // SAFETY: desktop handles are owned by `self.desktop` (closed on replacement / drop)
        // or closed right here; SetThreadDesktop only affects this thread.
        unsafe {
            let input = OpenInputDesktop(0, 0, GENERIC_ALL);
            if input.is_null() {
                return false; // no rights (not running as SYSTEM/service): stay where we are
            }
            let current = GetThreadDesktop(GetCurrentThreadId());
            if !current.is_null() && desktop_name(current) == desktop_name(input) {
                CloseDesktop(input);
                return false;
            }
            if SetThreadDesktop(input) != 0 {
                if !self.desktop.is_null() {
                    CloseDesktop(self.desktop);
                }
                self.desktop = input;
                true
            } else {
                CloseDesktop(input);
                false
            }
        }
    }

    fn maybe_follow_input_desktop(&mut self) {
        if self.desktop_checked.is_none_or(|t| t.elapsed() >= DESKTOP_RECHECK) {
            self.follow_input_desktop();
        }
    }


    fn virtual_screen() -> (i32, i32, i32, i32) {
        unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        }
    }

    fn move_abs(&mut self, display_id: u32, x: f32, y: f32) -> Result<()> {
        let d = self.displays.iter().find(|d| d.id == display_id).ok_or(InputError::UnknownDisplay(display_id))?;
        let (gx, gy) = normalized_to_global(display_pixel_rect(d), x, y);
        let (vx, vy, vw, vh) = Self::virtual_screen();
        let (ax, ay) = to_virtual_desk_absolute(gx, gy, vx, vy, vw, vh);
        send(&[mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)])
    }

    fn button(&mut self, b: MouseButton, pressed: bool) -> Result<()> {
        let (down, up, data) = match b {
            MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
            MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
            MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
            MouseButton::Back => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1 as i32),
            MouseButton::Forward => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2 as i32),
        };
        let bit = crate::state::button_bit(b);
        if pressed {
            self.buttons.insert(bit);
        } else {
            self.buttons.remove(&bit);
        }
        send(&[mouse(0, 0, data, if pressed { down } else { up })])
    }

    fn scroll(&mut self, dx: f32, dy: f32, unit: ScrollUnit, phase: Option<GesturePhase>) -> Result<()> {
        if phase == Some(GesturePhase::Cancel) {
            self.wheel_v.reset();
            self.wheel_h.reset();
            return Ok(());
        }
        let k = match unit {
            ScrollUnit::Line => WHEEL_DELTA,
            ScrollUnit::Pixel => WHEEL_UNITS_PER_PIXEL,
        };
        // DOM sign (+dy = down) → Windows (+wheel = away from user / up).
        let v = self.wheel_v.take(-(dy as f64) * k) as i32;
        let h = self.wheel_h.take(dx as f64 * k) as i32;
        let mut inputs = Vec::new();
        if v != 0 {
            inputs.push(mouse(0, 0, v, MOUSEEVENTF_WHEEL));
        }
        if h != 0 {
            inputs.push(mouse(0, 0, h, MOUSEEVENTF_HWHEEL));
        }
        send(&inputs)
    }

    fn key(&mut self, code: HidUsage, pressed: bool, modifiers: Modifiers) -> Result<()> {
        key_input(code, pressed)?; // validate before touching state
        let mut inputs = Vec::new();
        for s in plan_key(&mut self.mods, code, pressed, modifiers) {
            if !s.code.is_modifier() {
                if s.pressed {
                    self.keys_down.insert(s.code.0);
                } else {
                    self.keys_down.remove(&s.code.0);
                }
            }
            inputs.push(key_input(s.code, s.pressed)?);
        }
        send(&inputs)
    }

    fn combo(&mut self, mods: &[HidUsage], key: HidUsage) -> Result<()> {
        let mut inputs = Vec::new();
        let added: Vec<HidUsage> = mods.iter().copied().filter(|m| !self.mods.is_down(*m)).collect();
        for &m in &added {
            inputs.push(key_input(m, true)?);
        }
        inputs.push(key_input(key, true)?);
        inputs.push(key_input(key, false)?);
        for &m in added.iter().rev() {
            inputs.push(key_input(m, false)?);
        }
        send(&inputs)
    }

    fn text(&mut self, text: &str) -> Result<()> {
        let mut inputs = Vec::new();
        for c in text.chars() {
            match c {
                '\r' | '\n' => {
                    inputs.push(key_input(HidUsage::ENTER, true)?);
                    inputs.push(key_input(HidUsage::ENTER, false)?);
                }
                '\t' => {
                    inputs.push(key_input(HidUsage::TAB, true)?);
                    inputs.push(key_input(HidUsage::TAB, false)?);
                }
                _ => {
                    let mut buf = [0u16; 2];
                    for &u in c.encode_utf16(&mut buf).iter() {
                        inputs.push(kbd(0, u, KEYEVENTF_UNICODE));
                        inputs.push(kbd(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                    }
                }
            }
        }
        send(&inputs)
    }

    fn system(&mut self, a: SystemAction) -> Result<()> {
        match a {
            SystemAction::LockScreen => {
                if unsafe { LockWorkStation() } == 0 {
                    return Err(InputError::Os(format!("LockWorkStation: {}", std::io::Error::last_os_error())));
                }
                Ok(())
            }
            SystemAction::CtrlAltDel => send_secure_attention_sequence(),
            SystemAction::Screenshot => self.combo(&[HidUsage::META_LEFT], HidUsage::PRINT_SCREEN),
            SystemAction::ShowDesktop => self.combo(&[HidUsage::META_LEFT], HidUsage::D),
            SystemAction::MissionControl => self.combo(&[HidUsage::META_LEFT], HidUsage::TAB),
            SystemAction::AndroidBack
            | SystemAction::AndroidHome
            | SystemAction::AndroidRecents
            | SystemAction::AndroidNotifications => {
                Err(InputError::Unsupported("Android navigation actions are not available on Windows"))
            }
        }
    }

    fn release_all(&mut self) -> Result<()> {
        let mut inputs = Vec::new();
        for code in std::mem::take(&mut self.keys_down) {
            if let Ok(i) = key_input(HidUsage(code), false) {
                inputs.push(i);
            }
        }
        for m in self.mods.pressed() {
            inputs.push(key_input(m, false)?);
        }
        self.mods = ModifierState::default();
        for bit in std::mem::take(&mut self.buttons) {
            let (flags, data) = match bit {
                0x01 => (MOUSEEVENTF_LEFTUP, 0),
                0x02 => (MOUSEEVENTF_RIGHTUP, 0),
                0x04 => (MOUSEEVENTF_MIDDLEUP, 0),
                0x08 => (MOUSEEVENTF_XUP, XBUTTON1 as i32),
                _ => (MOUSEEVENTF_XUP, XBUTTON2 as i32),
            };
            inputs.push(mouse(0, 0, data, flags));
        }
        self.wheel_v.reset();
        self.wheel_h.reset();
        self.zoom.reset();
        send(&inputs)
    }
}

impl InputInjector for WinInjector {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        self.maybe_follow_input_desktop();
        match self.inject_on_current_desktop(ev) {
            // The desktop may have switched (UAC prompt, lock) since the last check: SendInput
            // into a non-input desktop fails. Re-attach and retry once.
            Err(InputError::Os(_)) if self.follow_input_desktop() => self.inject_on_current_desktop(ev),
            r => r,
        }
    }

    fn update_displays(&mut self, d: Vec<DisplayInfo>) {
        self.displays = d;
    }
}

impl WinInjector {
    fn inject_on_current_desktop(&mut self, ev: &InputEvent) -> Result<()> {
        match ev {
            InputEvent::MouseMove { display_id, x, y } => self.move_abs(*display_id, *x, *y),
            InputEvent::MouseMoveRelative { dx, dy } => {
                let ix = self.rel_x.take(*dx as f64) as i32;
                let iy = self.rel_y.take(*dy as f64) as i32;
                if ix == 0 && iy == 0 {
                    return Ok(());
                }
                send(&[mouse(ix, iy, 0, MOUSEEVENTF_MOVE)])
            }
            InputEvent::MouseButton { button, pressed } => self.button(*button, *pressed),
            InputEvent::Scroll { dx, dy, unit, phase } => self.scroll(*dx, *dy, *unit, *phase),
            InputEvent::Key { code, pressed, modifiers } => self.key(*code, *pressed, *modifiers),
            InputEvent::Text { text } => self.text(text),
            InputEvent::Gesture { gesture: Gesture::Magnify { delta }, .. } => {
                // Ctrl + wheel is the Windows convention for zoom.
                let units = self.zoom.take(*delta as f64 * 5.0 * WHEEL_DELTA) as i32;
                if units == 0 {
                    return Ok(());
                }
                let mut inputs = Vec::new();
                let ctrl_added = !self.mods.logical().contains(Modifiers::CTRL);
                if ctrl_added {
                    inputs.push(key_input(HidUsage::CONTROL_LEFT, true)?);
                }
                inputs.push(mouse(0, 0, units, MOUSEEVENTF_WHEEL));
                if ctrl_added {
                    inputs.push(key_input(HidUsage::CONTROL_LEFT, false)?);
                }
                send(&inputs)
            }
            InputEvent::Gesture { gesture: Gesture::SwipeNavigate { dx, dy }, phase } => {
                if *phase != GesturePhase::End || dx.abs() <= dy.abs() {
                    return Ok(());
                }
                // Alt+Left = back, Alt+Right = forward.
                let key = if *dx > 0.0 { HidUsage::ARROW_LEFT } else { HidUsage::ARROW_RIGHT };
                self.combo(&[HidUsage::ALT_LEFT], key)
            }
            InputEvent::Gesture { .. } => {
                Err(InputError::Unsupported("rotate / smart-zoom gestures have no Windows injection equivalent"))
            }
            InputEvent::Touch { .. } => Err(InputError::Unsupported(
                "touch injection (InjectTouchInput) is not implemented yet; send pointer events",
            )),
            InputEvent::System(a) => self.system(*a),
            InputEvent::ReleaseAll => self.release_all(),
        }
    }
}

#[allow(dead_code)]
fn cursor_pos() -> Option<(i32, i32)> {
    let mut p = POINT { x: 0, y: 0 };
    (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
}
