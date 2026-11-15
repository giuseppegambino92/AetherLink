//! # aether-input
//!
//! Remote input for AetherLink hosts:
//!
//! * [`InputInjector`] / [`create_injector`] — OS-level injection of
//!   [`InputEvent`]s: CGEvent on macOS, `SendInput` on Windows, `uinput` /
//!   XTest / RemoteDesktop portal on Linux.
//! * [`keymap`] — pure HID usage → native key code tables.
//! * [`permissions`] — macOS TCC checks / prompts / System Settings deep links.
//! * [`hid_report`] — HID report descriptor + stateful `InputEvent` → report
//!   conversion (for BLE / USB HID emulation).
//! * [`ble_hid`] — BLE HID-over-GATT peripheral (see its docs for platform
//!   limits).
//! * [`bt_hid`] — "control an iPhone/iPad": present this machine as a
//!   Bluetooth Classic HID keyboard + mouse (IOBluetooth / BlueZ) or drive an
//!   ESP32-S3 BLE/USB HID dongle over serial.
//!
//! Every event injected by this crate carries [`INJECTED_EVENT_TAG`]
//! (macOS: `kCGEventSourceUserData`; Windows: `dwExtraInfo`) so AetherLink's own
//! capture/clipboard code can recognise and ignore it.

use aether_protocol::{DisplayInfo, InputEvent};

pub mod ble_hid;
pub mod bt_hid;
pub mod hid_report;
pub mod keymap;
pub mod permissions;
pub mod state;

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "ios")]
pub mod ios;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub mod linux_backend;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Tag stamped on every synthesized event ("AETHLINK" in ASCII).
pub const INJECTED_EVENT_TAG: i64 = 0x4145_5448_4C49_4E4B;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// The platform (or this build) cannot do this.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// A required OS permission (Accessibility, uinput access, …) is missing.
    #[error("permission denied: {0}")]
    PermissionDenied(&'static str),
    #[error("unknown display id {0}")]
    UnknownDisplay(u32),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("os error: {0}")]
    Os(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, InputError>;

/// Injects input events into the local OS session.
pub trait InputInjector: Send {
    fn inject(&mut self, ev: &InputEvent) -> Result<()>;
    /// Replace the display layout used to map normalized coordinates.
    fn update_displays(&mut self, d: Vec<DisplayInfo>);
}

/// Create the injector for the current OS.
///
/// * macOS: requires Accessibility permission for the events to be delivered
///   (see [`permissions`]); creation itself never prompts.
/// * Windows: `SendInput`; switches the injecting thread to the current input
///   desktop (UAC secure desktop / Winlogon) when running with enough rights.
/// * Linux: `uinput` when `/dev/uinput` is writable, else XTest (X11) or the
///   RemoteDesktop portal (Wayland); see `linux_backend` for the selection and
///   `linux_backend::active_backend()` for the backend in use.
pub fn create_injector(displays: Vec<DisplayInfo>) -> Result<Box<dyn InputInjector>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacInjector::new(displays)?))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WinInjector::new(displays)?))
    }
    #[cfg(target_os = "linux")]
    {
        linux_backend::create(displays)
    }
    #[cfg(target_os = "android")]
    {
        Ok(Box::new(android::AndroidInjector::new(displays)?))
    }
    #[cfg(target_os = "ios")]
    {
        ios::create(displays)
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "windows",
        target_os = "linux",
        target_os = "android",
        target_os = "ios"
    )))]
    {
        let _ = displays;
        Err(InputError::Unsupported(
            "input injection is not available on this OS (Android uses the AccessibilityService in the mobile app)",
        ))
    }
}
