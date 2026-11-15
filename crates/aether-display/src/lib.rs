//! # aether-display
//!
//! Host-side display control used by remote sessions:
//!
//! * [`ghost`] — **Ghost Mode**: an opaque privacy curtain on every screen plus optional
//!   hardware backlight dimming to 0 with exact, crash-safe restoration, kept out of the
//!   capture so the remote controller keeps seeing the real desktop.
//!   macOS: `NSWindow`s at `CGShieldingWindowLevel()` with `sharingType = none`.
//!   Windows: topmost popup windows with `WDA_EXCLUDEFROMCAPTURE` on a dedicated UI thread.
//!   Linux: X11 override-redirect windows, excluded by the AetherLink X11 capturer through the
//!   Composite extension (Wayland: [`DisplayError::Unsupported`]).
//! * [`brightness`] — backlight: DisplayServices (macOS), WMI + DDC/CI (Windows),
//!   sysfs + systemd-logind (Linux).
//! * [`virtual_display`] — on-demand headless HiDPI displays (16:9, 32:9 …) via the
//!   `CGVirtualDisplay` runtime classes, for Macs with no monitor attached or a closed lid
//!   (macOS only; Windows needs an IddCx driver).

pub mod brightness;
pub mod ghost;
pub mod virtual_display;

#[cfg(target_os = "macos")]
mod main_thread;

#[cfg(target_os = "windows")]
mod brightness_windows;
#[cfg(target_os = "windows")]
mod ghost_windows;

#[cfg(target_os = "linux")]
mod brightness_linux;
#[cfg(target_os = "linux")]
mod ghost_x11;

#[derive(Debug, Clone, thiserror::Error)]
pub enum DisplayError {
    #[error("unsupported on this platform: {0}")]
    Unsupported(&'static str),
    #[error("{0} is not available on this OS version")]
    Unavailable(&'static str),
    #[error("OS call failed: {0}")]
    Os(String),
    #[error("i/o: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, DisplayError>;
