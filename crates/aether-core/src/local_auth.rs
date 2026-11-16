//! Local owner verification (Touch ID / Apple Watch / account password on macOS, Windows Hello
//! on Windows) used to gate trusted-device sessions when `require_biometric` is on.
//!
//! Everything here fails closed: an error, a timeout, a cancelled prompt or a platform without
//! support all mean "not verified".

use std::time::Duration;

/// Longest a prompt may stay open before it counts as declined.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(90);

/// Whether this platform can verify the local owner at all (the settings UI disables the
/// option where it cannot, since enabling it would lock out every trusted device).
pub fn is_available() -> bool {
    imp::is_available()
}

/// Ask the person at this computer to confirm `reason`. Blocking; call from a blocking thread.
pub fn verify(reason: &str) -> Result<(), String> {
    imp::verify(reason)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::PROMPT_TIMEOUT;
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::msg_send;
    use objc2_foundation::{NSError, NSString};
    use std::ptr;

    #[link(name = "LocalAuthentication", kind = "framework")]
    unsafe extern "C" {}

    /// `LAPolicyDeviceOwnerAuthentication`: biometrics, Apple Watch or the account password.
    const POLICY_DEVICE_OWNER: isize = 2;

    fn context() -> Option<Retained<AnyObject>> {
        let cls = AnyClass::get(c"LAContext")?;
        // SAFETY: `+[LAContext new]` returns a retained instance.
        unsafe { msg_send![cls, new] }
    }

    pub fn is_available() -> bool {
        let Some(ctx) = context() else { return false };
        let mut err: *mut NSError = ptr::null_mut();
        // SAFETY: documented selector; `err` is an out-parameter.
        let ok: Bool = unsafe { msg_send![&*ctx, canEvaluatePolicy: POLICY_DEVICE_OWNER, error: &mut err] };
        ok.as_bool()
    }

    pub fn verify(reason: &str) -> Result<(), String> {
        let ctx = context().ok_or("LocalAuthentication is unavailable")?;
        let mut err: *mut NSError = ptr::null_mut();
        // SAFETY: as above.
        let can: Bool = unsafe { msg_send![&*ctx, canEvaluatePolicy: POLICY_DEVICE_OWNER, error: &mut err] };
        if !can.as_bool() {
            return Err("owner authentication is not configured on this Mac".into());
        }
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let reply = RcBlock::new(move |success: Bool, error: *mut NSError| {
            let r = if success.as_bool() {
                Ok(())
            } else if error.is_null() {
                Err("not verified".to_owned())
            } else {
                // SAFETY: non-null NSError handed to the reply block.
                Err(unsafe { &*error }.localizedDescription().to_string())
            };
            let _ = tx.send(r);
        });
        let reason = NSString::from_str(reason);
        // SAFETY: documented selector; the block is copied by LocalAuthentication.
        let _: () = unsafe {
            msg_send![&*ctx, evaluatePolicy: POLICY_DEVICE_OWNER, localizedReason: &*reason, reply: &*reply]
        };
        match rx.recv_timeout(PROMPT_TIMEOUT) {
            Ok(r) => r,
            Err(_) => {
                // SAFETY: cancels the pending prompt; the reply block then fires with an error.
                let _: () = unsafe { msg_send![&*ctx, invalidate] };
                Err("owner verification timed out".into())
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use windows::Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    };
    use windows::core::HSTRING;

    pub fn is_available() -> bool {
        UserConsentVerifier::CheckAvailabilityAsync()
            .and_then(|op| op.join())
            .is_ok_and(|a| a == UserConsentVerifierAvailability::Available)
    }

    pub fn verify(reason: &str) -> Result<(), String> {
        if !is_available() {
            return Err("Windows Hello is not set up on this PC".into());
        }
        let result = UserConsentVerifier::RequestVerificationAsync(&HSTRING::from(reason))
            .and_then(|op| op.join())
            .map_err(|e| e.to_string())?;
        if result == UserConsentVerificationResult::Verified {
            Ok(())
        } else {
            Err(format!("Windows Hello: {result:?}"))
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    pub fn is_available() -> bool {
        false
    }

    pub fn verify(_reason: &str) -> Result<(), String> {
        Err("owner verification is not available on this platform".into())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn unsupported_platforms_fail_closed() {
        assert!(!super::is_available());
        assert!(super::verify("test").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn availability_query_does_not_prompt() {
        // Only asks whether a policy *can* be evaluated; never shows UI.
        let _ = super::is_available();
    }
}
