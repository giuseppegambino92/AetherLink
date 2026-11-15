//! iOS: no software input injection.
//!
//! iOS gives apps no API to synthesize touches or key presses outside their own process
//! (no accessibility-service equivalent, no `IOHIDEvent` access for third-party apps), so an
//! iPhone/iPad host is **view-only**. To control one remotely, use a hardware path that iOS
//! treats as a real accessory:
//!
//! * an Android phone running AetherLink as a Bluetooth HID keyboard + mouse
//!   (`BluetoothHidDevice`, using [`crate::hid_report::REPORT_DESCRIPTOR`]), with
//!   *AssistiveTouch* enabled on the iPhone for the pointer;
//! * a USB/BLE HID dongle (nRF52 / ESP32) driven by AetherLink.
//!
//! The mobile app shows this explanation (localized) on its host screen.

use crate::{InputError, InputInjector, Result};
use aether_protocol::DisplayInfo;

/// Human-readable reason returned by [`create`]; the app shows a localized version.
pub const REASON: &str = "iOS does not allow apps to inject input; use a Bluetooth HID controller or dongle";

pub fn create(_displays: Vec<DisplayInfo>) -> Result<Box<dyn InputInjector>> {
    Err(InputError::Unsupported(REASON))
}
