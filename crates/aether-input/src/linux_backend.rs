//! Linux injector selection: `uinput` → (Wayland: RemoteDesktop portal | X11: XTest).
//!
//! * [`LinuxInputBackend::Uinput`] — kernel virtual devices; works everywhere (X11, Wayland,
//!   console, lock screen) but needs write access to `/dev/uinput` (root or the `input`/`uinput`
//!   group, usually via a udev rule).
//! * [`LinuxInputBackend::XTest`] — X11 sessions without uinput access (also types arbitrary
//!   Unicode text).
//! * [`LinuxInputBackend::RemoteDesktopPortal`] — Wayland sessions without uinput access; asks
//!   for consent once (restore token).
//!
//! `AETHERLINK_INPUT_BACKEND=uinput|xtest|portal` forces a backend. The choice made by the
//! last successful [`create`] is reported by [`active_backend`].

use std::sync::atomic::{AtomicU8, Ordering};

use aether_protocol::DisplayInfo;

use crate::{InputError, InputInjector, Result};

#[path = "linux_portal.rs"]
mod portal;
#[path = "linux_xtest.rs"]
mod xtest;

pub use portal::{PortalInjector, PortalStream, restore_token_path as portal_restore_token_path};
pub use xtest::{XTestInjector, char_to_keysym};

/// Concrete Linux injection backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxInputBackend {
    Uinput,
    XTest,
    RemoteDesktopPortal,
}

impl LinuxInputBackend {
    pub fn name(self) -> &'static str {
        match self {
            LinuxInputBackend::Uinput => "uinput",
            LinuxInputBackend::XTest => "xtest",
            LinuxInputBackend::RemoteDesktopPortal => "remote-desktop-portal",
        }
    }

    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Uinput),
            2 => Some(Self::XTest),
            3 => Some(Self::RemoteDesktopPortal),
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::Uinput => 1,
            Self::XTest => 2,
            Self::RemoteDesktopPortal => 3,
        }
    }
}

static ACTIVE: AtomicU8 = AtomicU8::new(0);

/// Backend of the most recently created injector in this process.
pub fn active_backend() -> Option<LinuxInputBackend> {
    LinuxInputBackend::from_u8(ACTIVE.load(Ordering::Relaxed))
}

/// True when this process may open `/dev/uinput` for writing.
pub fn uinput_writable() -> bool {
    // SAFETY: access(2) with a NUL-terminated literal.
    unsafe { libc::access(c"/dev/uinput".as_ptr(), libc::W_OK) == 0 }
}

fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
}

fn has_x_display() -> bool {
    std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
}

/// Order in which backends are tried for the current environment.
pub fn candidate_order() -> Vec<LinuxInputBackend> {
    use LinuxInputBackend::*;
    match std::env::var("AETHERLINK_INPUT_BACKEND").map(|v| v.to_ascii_lowercase()).as_deref() {
        Ok("uinput") => return vec![Uinput],
        Ok("xtest" | "x11") => return vec![XTest],
        Ok("portal" | "remote-desktop" | "remotedesktop") => return vec![RemoteDesktopPortal],
        _ => {}
    }
    let mut v = Vec::new();
    if uinput_writable() {
        v.push(Uinput);
    }
    if is_wayland() {
        v.push(RemoteDesktopPortal);
        if has_x_display() {
            v.push(XTest); // reaches Xwayland clients only
        }
    } else {
        if has_x_display() {
            v.push(XTest);
        }
        v.push(RemoteDesktopPortal);
    }
    v
}

/// Creates a specific backend.
pub fn create_backend(backend: LinuxInputBackend, displays: Vec<DisplayInfo>) -> Result<Box<dyn InputInjector>> {
    let inj: Box<dyn InputInjector> = match backend {
        LinuxInputBackend::Uinput => Box::new(crate::linux::UinputInjector::new(displays)?),
        LinuxInputBackend::XTest => Box::new(XTestInjector::new(displays)?),
        LinuxInputBackend::RemoteDesktopPortal => Box::new(PortalInjector::new(displays)?),
    };
    ACTIVE.store(backend.to_u8(), Ordering::Relaxed);
    Ok(inj)
}

/// Creates the first working backend of [`candidate_order`] and returns which one it is.
pub fn create_with_backend(displays: Vec<DisplayInfo>) -> Result<(Box<dyn InputInjector>, LinuxInputBackend)> {
    let mut last = InputError::PermissionDenied(
        "no Linux input backend available: grant write access to /dev/uinput, or run inside an X11 session (XTest) or a Wayland desktop with xdg-desktop-portal RemoteDesktop",
    );
    for b in candidate_order() {
        match create_backend(b, displays.clone()) {
            Ok(i) => {
                tracing::info!(backend = b.name(), "input injection backend selected");
                return Ok((i, b));
            }
            Err(e) => {
                tracing::info!(backend = b.name(), "input backend unavailable: {e}");
                last = e;
            }
        }
    }
    Err(last)
}

/// Creates the first working backend (see module docs).
pub fn create(displays: Vec<DisplayInfo>) -> Result<Box<dyn InputInjector>> {
    create_with_backend(displays).map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_backend_order() {
        let old = std::env::var_os("AETHERLINK_INPUT_BACKEND");
        unsafe { std::env::set_var("AETHERLINK_INPUT_BACKEND", "xtest") };
        assert_eq!(candidate_order(), vec![LinuxInputBackend::XTest]);
        unsafe { std::env::set_var("AETHERLINK_INPUT_BACKEND", "portal") };
        assert_eq!(candidate_order(), vec![LinuxInputBackend::RemoteDesktopPortal]);
        match old {
            Some(v) => unsafe { std::env::set_var("AETHERLINK_INPUT_BACKEND", v) },
            None => unsafe { std::env::remove_var("AETHERLINK_INPUT_BACKEND") },
        }
        let auto = {
            unsafe { std::env::remove_var("AETHERLINK_INPUT_BACKEND") };
            candidate_order()
        };
        assert!(auto.contains(&LinuxInputBackend::RemoteDesktopPortal));
        assert_eq!(auto.contains(&LinuxInputBackend::Uinput), uinput_writable());
    }

    #[test]
    #[ignore = "needs an X server with XTEST and no writable /dev/uinput (ci/linux-test.sh)"]
    fn falls_back_to_xtest_without_uinput() {
        if uinput_writable() {
            return;
        }
        let (_inj, b) = create_with_backend(Vec::new()).unwrap();
        assert_eq!(b, LinuxInputBackend::XTest);
        assert_eq!(active_backend(), Some(LinuxInputBackend::XTest));
    }
}
