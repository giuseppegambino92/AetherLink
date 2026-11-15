//! # aether-usb
//!
//! URB-level USB redirection for AetherLink.
//!
//! * [`descriptors`] — parsers for device/configuration/interface/endpoint descriptors.
//! * [`usbip`] — the Linux USB/IP wire format (`OP_REQ/REP_DEVLIST`, `OP_REQ/REP_IMPORT`,
//!   `USBIP_CMD_SUBMIT`/`RET_SUBMIT`/`CMD_UNLINK`/`RET_UNLINK`, big-endian) and converters
//!   between `UrbSubmit`/`UrbComplete` and `CMD_SUBMIT`/`RET_SUBMIT`.
//! * [`usbip_server`] — a USB/IP server ([`usbip_server::serve`]) over any [`UsbBackend`], so a
//!   Linux client can attach redirected devices with the stock tools:
//!   `sudo modprobe vhci-hcd && sudo usbip --tcp-port <port> attach -r 127.0.0.1 -b <busid>`.
//! * [`remote`] — the AetherLink side: [`remote::serve_usb_stream`] runs on the host over a
//!   `StreamKind::Usb` stream; [`remote::RemoteUsb`] is the client-side [`UsbBackend`] that
//!   forwards to it (plug it into the USB/IP server on the client).
//! * [`UsbHost`] (feature `libusb`, default) — the real device backend using `rusb` with a
//!   vendored libusb: enumeration, attach (kernel-driver detach where supported + interface
//!   claiming) and control/bulk/interrupt transfers on per-endpoint worker threads.
//!
//! ## Platform support (honest status)
//!
//! * **Host side** (sharing a physical device): works wherever libusb can claim the device.
//!   On Linux this needs permissions on `/dev/bus/usb` (udev rule or root); the kernel driver
//!   is detached automatically. On macOS, devices bound to an Apple kernel driver (HID
//!   keyboards/mice, mass storage, audio) cannot be claimed by a user process — claiming
//!   fails with `Access`/`Busy`; vendor-class devices generally work. On Windows the device
//!   must be bound to WinUSB (e.g. via Zadig).
//! * **Client side** (making a remote device appear locally): only Linux, through USB/IP and
//!   the in-tree `vhci-hcd` driver. macOS would need a signed DriverKit USB host-controller
//!   extension (entitlement `com.apple.developer.driverkit.family.usb` granted by Apple) and
//!   Windows a signed kernel-mode virtual host controller (like usbip-win). Neither exists
//!   here, so [`client_support`] returns [`UsbError::Unsupported`] on those systems.
//! * Isochronous transfers (webcams, audio) are not supported and complete with an error.

pub mod descriptors;
mod error;
#[cfg(all(feature = "libusb", not(target_os = "ios")))]
mod host;
pub mod remote;
pub mod usbip;
pub mod usbip_server;

pub use error::{Result, UsbError};
#[cfg(all(feature = "libusb", not(target_os = "ios")))]
pub use host::UsbHost;
#[cfg(all(feature = "libusb", target_os = "ios"))]
pub use ios_stub::UsbHost;

/// iOS: no USB host access for apps; `UsbHost::new` reports `Unsupported`.
#[cfg(all(feature = "libusb", target_os = "ios"))]
mod ios_stub {
    use crate::{Result, UsbBackend, UsbError};
    use aether_protocol::{UrbComplete, UrbSubmit, UsbDeviceInfo};

    /// Uninhabited: never constructed on iOS.
    #[derive(Debug)]
    pub enum UsbHost {}

    impl UsbHost {
        pub fn new() -> Result<Self> {
            Err(UsbError::Unsupported("USB redirection is not available on iOS"))
        }
    }

    #[async_trait::async_trait]
    impl UsbBackend for UsbHost {
        async fn list_devices(&self) -> Result<Vec<UsbDeviceInfo>> {
            match *self {}
        }
        async fn attach(&self, _device_id: u32) -> Result<()> {
            match *self {}
        }
        async fn detach(&self, _device_id: u32) -> Result<()> {
            match *self {}
        }
        async fn submit(&self, _device_id: u32, _urb: UrbSubmit) -> UrbComplete {
            match *self {}
        }
        async fn unlink(&self, _device_id: u32, _urb_id: u64) {
            match *self {}
        }
    }
}
pub use usbip_server::UsbBackend;

/// Can this machine make remote USB devices appear locally?
///
/// `Ok(())` on Linux (USB/IP + `vhci-hcd`, requires root to attach); `Unsupported`
/// elsewhere (needs a signed DriverKit / kernel virtual host controller driver).
pub fn client_support() -> Result<()> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else if cfg!(target_os = "macos") {
        Err(UsbError::Unsupported(
            "virtual USB devices on macOS require a signed DriverKit USB host-controller extension",
        ))
    } else if cfg!(target_os = "windows") {
        Err(UsbError::Unsupported(
            "virtual USB devices on Windows require a signed kernel-mode virtual host controller",
        ))
    } else {
        Err(UsbError::Unsupported(
            "virtual USB devices are not supported on this OS",
        ))
    }
}
