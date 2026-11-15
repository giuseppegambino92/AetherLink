//! OS privacy permissions needed by a host (macOS TCC).
//!
//! | kind              | check (never prompts)          | request (may prompt)                 | settings pane            |
//! |-------------------|--------------------------------|--------------------------------------|--------------------------|
//! | Accessibility     | `AXIsProcessTrusted`           | `AXIsProcessTrustedWithOptions(prompt)` | `Privacy_Accessibility` |
//! | Screen Recording  | `CGPreflightScreenCaptureAccess` | `CGRequestScreenCaptureAccess`     | `Privacy_ScreenCapture`  |
//! | Input Monitoring  | `IOHIDCheckAccess(ListenEvent)`  | `IOHIDRequestAccess(ListenEvent)`  | `Privacy_ListenEvent`    |
//!
//! On other OSes every permission is [`PermissionState::NotApplicable`].

use crate::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionKind {
    Accessibility,
    ScreenRecording,
    InputMonitoring,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionState {
    Granted,
    Denied,
    NotApplicable,
}

impl PermissionState {
    pub fn from_bool(granted: bool) -> Self {
        if granted { Self::Granted } else { Self::Denied }
    }
}

/// Serialized as `{screenRecording, accessibility, inputMonitoring}` for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionReport {
    pub screen_recording: PermissionState,
    pub accessibility: PermissionState,
    pub input_monitoring: PermissionState,
}

/// The `x-apple.systempreferences:` deep link for a permission pane.
pub fn settings_url(kind: PermissionKind) -> &'static str {
    match kind {
        PermissionKind::Accessibility => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        }
        PermissionKind::ScreenRecording => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        }
        PermissionKind::InputMonitoring => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;
    use core_foundation_sys::dictionary::CFDictionaryRef;
    use core_foundation_sys::string::CFStringRef;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOHIDCheckAccess(request: u32) -> u32;
        fn IOHIDRequestAccess(request: u32) -> bool;
    }
    const IOHID_REQUEST_LISTEN_EVENT: u32 = 1;
    const IOHID_ACCESS_GRANTED: u32 = 0;

    pub fn check(kind: PermissionKind) -> PermissionState {
        let granted = unsafe {
            match kind {
                PermissionKind::Accessibility => AXIsProcessTrusted(),
                PermissionKind::ScreenRecording => CGPreflightScreenCaptureAccess(),
                PermissionKind::InputMonitoring => {
                    IOHIDCheckAccess(IOHID_REQUEST_LISTEN_EVENT) == IOHID_ACCESS_GRANTED
                }
            }
        };
        PermissionState::from_bool(granted)
    }

    pub fn request(kind: PermissionKind) -> bool {
        unsafe {
            match kind {
                PermissionKind::Accessibility => {
                    let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
                    let opts = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), CFBoolean::true_value().as_CFType())]);
                    AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef())
                }
                PermissionKind::ScreenRecording => CGRequestScreenCaptureAccess(),
                PermissionKind::InputMonitoring => IOHIDRequestAccess(IOHID_REQUEST_LISTEN_EVENT),
            }
        }
    }

    pub fn open_settings(kind: PermissionKind) -> Result<()> {
        let status = std::process::Command::new("/usr/bin/open").arg(settings_url(kind)).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(crate::InputError::Os(format!("`open {}` failed: {status}", settings_url(kind))))
        }
    }
}

/// Current state of every permission. Never shows a prompt.
pub fn report() -> PermissionReport {
    PermissionReport {
        screen_recording: check(PermissionKind::ScreenRecording),
        accessibility: check(PermissionKind::Accessibility),
        input_monitoring: check(PermissionKind::InputMonitoring),
    }
}

/// State of a single permission. Never shows a prompt.
pub fn check(kind: PermissionKind) -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        imp::check(kind)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = kind;
        PermissionState::NotApplicable
    }
}

/// Ask the OS for the permission (shows the system prompt the first time).
/// Returns whether it is granted *now*; for Screen Recording / Input
/// Monitoring the app usually has to be relaunched after the user agrees.
/// Always `true` where the permission does not exist.
pub fn request(kind: PermissionKind) -> bool {
    #[cfg(target_os = "macos")]
    {
        imp::request(kind)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = kind;
        true
    }
}

/// Open the System Settings pane where the user grants `kind`.
pub fn open_settings(kind: PermissionKind) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        imp::open_settings(kind)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = kind;
        Err(crate::InputError::Unsupported("permission settings panes exist only on macOS"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_matches_ui_contract() {
        let r = PermissionReport {
            screen_recording: PermissionState::Granted,
            accessibility: PermissionState::Denied,
            input_monitoring: PermissionState::NotApplicable,
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"screenRecording":"granted","accessibility":"denied","inputMonitoring":"notApplicable"}"#
        );
        assert_eq!(serde_json::to_string(&PermissionKind::ScreenRecording).unwrap(), r#""screenRecording""#);
    }

    #[test]
    fn deep_links() {
        assert!(settings_url(PermissionKind::Accessibility).ends_with("?Privacy_Accessibility"));
        assert!(settings_url(PermissionKind::ScreenRecording).ends_with("?Privacy_ScreenCapture"));
        assert!(settings_url(PermissionKind::InputMonitoring).ends_with("?Privacy_ListenEvent"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn not_applicable_elsewhere() {
        assert_eq!(report().accessibility, PermissionState::NotApplicable);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "queries TCC state of the test binary (no prompt, but environment dependent)"]
    fn report_runs() {
        let r = report();
        assert_ne!(r.accessibility, PermissionState::NotApplicable);
    }
}
