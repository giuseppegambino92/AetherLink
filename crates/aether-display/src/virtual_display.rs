//! Headless virtual displays ("Ghost Mode" without a monitor, closed-lid hosts).
//!
//! macOS: `CGVirtualDisplay` (CoreGraphics runtime classes `CGVirtualDisplayDescriptor`,
//! `CGVirtualDisplaySettings`, `CGVirtualDisplayMode`, available since macOS 11). They are not
//! in the public SDK headers, so they are resolved at runtime through the Objective-C runtime;
//! if Apple removes them, [`VirtualDisplay::create`] returns [`DisplayError::Unavailable`]
//! instead of crashing. The display exists exactly as long as the [`VirtualDisplay`] value.

use crate::{DisplayError, Result};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualDisplaySpec {
    /// Logical (point) size. With `hidpi` the mode is registered in pixels (2×) and macOS
    /// presents it as a Retina "looks like width×height" mode.
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f64,
    pub hidpi: bool,
    /// Physical pixels per inch used to derive the reported physical size (≥ ~200 for HiDPI).
    pub ppi: f64,
}

impl VirtualDisplaySpec {
    // The window server only brings HiDPI virtual displays online at Retina-class density
    // (verified on macOS 27: 220 ppi works, 110 ppi and below stay offline).
    pub const FHD_16_9: Self = Self { width: 1920, height: 1080, refresh_hz: 60.0, hidpi: true, ppi: 220.0 };
    pub const QHD_16_9: Self = Self { width: 2560, height: 1440, refresh_hz: 60.0, hidpi: true, ppi: 220.0 };
    /// Samsung Odyssey-class 32:9 dual-QHD.
    pub const DQHD_32_9: Self = Self { width: 5120, height: 1440, refresh_hz: 60.0, hidpi: false, ppi: 110.0 };

    pub fn validate(&self) -> Result<()> {
        if self.width < 640 || self.height < 480 || self.width > 8192 || self.height > 8192 {
            return Err(DisplayError::Os(format!("unsupported virtual display size {}x{}", self.width, self.height)));
        }
        if !(1.0..=240.0).contains(&self.refresh_hz) {
            return Err(DisplayError::Os(format!("unsupported refresh rate {}", self.refresh_hz)));
        }
        Ok(())
    }

    /// Physical size in millimetres for the descriptor.
    pub fn size_mm(&self) -> (f64, f64) {
        let scale = if self.hidpi { 2.0 } else { 1.0 };
        let mm = |px: u32| px as f64 * scale / self.ppi * 25.4;
        (mm(self.width), mm(self.height))
    }
}

pub struct VirtualDisplay {
    display_id: u32,
    spec: VirtualDisplaySpec,
    #[cfg(target_os = "macos")]
    _handle: imp::Handle,
}

impl std::fmt::Debug for VirtualDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualDisplay").field("display_id", &self.display_id).field("spec", &self.spec).finish()
    }
}

impl VirtualDisplay {
    /// Create and attach a virtual display; waits (≤ 2 s) until the window server lists it.
    pub fn create(name: &str, spec: VirtualDisplaySpec) -> Result<Self> {
        spec.validate()?;
        #[cfg(target_os = "macos")]
        {
            match imp::create(name, &spec) {
                Ok((handle, display_id)) => Ok(Self { display_id, spec, _handle: handle }),
                // A HiDPI mode the window server rejects leaves the display offline: retry at 1×.
                Err(DisplayError::Os(m)) if spec.hidpi && m.contains("did not come online") => {
                    tracing::warn!("HiDPI virtual display rejected ({m}); retrying at 1x");
                    let lo = VirtualDisplaySpec { hidpi: false, ..spec };
                    let (handle, display_id) = imp::create(name, &lo)?;
                    Ok(Self { display_id, spec: lo, _handle: handle })
                }
                Err(e) => Err(e),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = name;
            Err(DisplayError::Unsupported("virtual displays are implemented for macOS (Windows needs an IddCx driver)"))
        }
    }

    /// `CGDirectDisplayID` of the new display (usable as a capture target).
    pub fn display_id(&self) -> u32 {
        self.display_id
    }

    pub fn spec(&self) -> VirtualDisplaySpec {
        self.spec
    }
}

impl Drop for VirtualDisplay {
    /// Dropping detaches the display (macOS releases the `CGVirtualDisplay` right after this).
    fn drop(&mut self) {
        tracing::debug!(display_id = self.display_id, "detaching virtual display");
    }
}

/// True when the CGVirtualDisplay runtime classes exist on this OS.
pub fn is_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        imp::classes_present()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Number of online displays (used to auto-create a virtual display on headless hosts).
pub fn online_display_count() -> usize {
    #[cfg(target_os = "macos")]
    {
        imp::online_ids().len()
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::VirtualDisplaySpec;
    use crate::{DisplayError, Result};
    use objc2::rc::{Allocated, Retained};
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{ClassType, msg_send};
    use objc2_core_foundation::{CGPoint, CGSize};
    use objc2_foundation::{NSArray, NSObject, NSString};
    use std::time::{Duration, Instant};

    /// Keeps the `CGVirtualDisplay` alive; dropping it detaches the display.
    pub struct Handle(#[allow(dead_code)] Retained<AnyObject>);
    // SAFETY: CGVirtualDisplay is a plain NSObject subclass with no thread affinity; we only
    // retain/release it (and read `displayID`, an immutable property) from other threads.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    fn class(name: &std::ffi::CStr) -> Result<&'static AnyClass> {
        AnyClass::get(name).ok_or(DisplayError::Unavailable("CGVirtualDisplay"))
    }

    pub fn classes_present() -> bool {
        ["CGVirtualDisplay", "CGVirtualDisplayDescriptor", "CGVirtualDisplaySettings", "CGVirtualDisplayMode"]
            .iter()
            .all(|n| AnyClass::get(&std::ffi::CString::new(*n).unwrap()).is_some())
    }

    pub fn online_ids() -> Vec<u32> {
        let mut ids = [0u32; 32];
        let mut n = 0u32;
        // SAFETY: 32-entry buffer.
        unsafe { objc2_core_graphics::CGGetOnlineDisplayList(32, ids.as_mut_ptr(), &mut n) };
        ids[..n as usize].to_vec()
    }

    pub fn create(name: &str, spec: &VirtualDisplaySpec) -> Result<(Handle, u32)> {
        let desc_cls = class(c"CGVirtualDisplayDescriptor")?;
        let disp_cls = class(c"CGVirtualDisplay")?;
        let set_cls = class(c"CGVirtualDisplaySettings")?;
        let mode_cls = class(c"CGVirtualDisplayMode")?;
        let scale = if spec.hidpi { 2 } else { 1 };
        let (mm_w, mm_h) = spec.size_mm();
        let serial = std::process::id() ^ spec.width.rotate_left(8) ^ spec.height;
        // SAFETY: selectors/types follow the CGVirtualDisplay runtime interface (used by
        // Chromium's virtual display test utilities); every object is checked for nil.
        unsafe {
            let desc: Option<Retained<AnyObject>> = msg_send![desc_cls, new];
            let desc = desc.ok_or_else(|| DisplayError::Os("CGVirtualDisplayDescriptor alloc failed".into()))?;
            // Display reconfiguration callbacks are delivered on the main queue (as DeskPad /
            // Chromium do); the host app's AppKit run loop services it.
            let queue = dispatch2::DispatchQueue::main();
            let queue_obj = queue as *const dispatch2::DispatchQueue as *mut AnyObject;
            let _: () = msg_send![&*desc, setQueue: queue_obj];
            let ns_name = NSString::from_str(name);
            let _: () = msg_send![&*desc, setName: &*ns_name];
            let _: () = msg_send![&*desc, setMaxPixelsWide: spec.width * scale];
            let _: () = msg_send![&*desc, setMaxPixelsHigh: spec.height * scale];
            let _: () = msg_send![&*desc, setSizeInMillimeters: CGSize::new(mm_w, mm_h)];
            let _: () = msg_send![&*desc, setProductID: 0xAE71u32];
            let _: () = msg_send![&*desc, setVendorID: 0x1AE7u32];
            let _: () = msg_send![&*desc, setSerialNum: serial];
            // sRGB primaries / D65 white point.
            let _: () = msg_send![&*desc, setRedPrimary: CGPoint::new(0.6400, 0.3300)];
            let _: () = msg_send![&*desc, setGreenPrimary: CGPoint::new(0.3000, 0.6000)];
            let _: () = msg_send![&*desc, setBluePrimary: CGPoint::new(0.1500, 0.0600)];
            let _: () = msg_send![&*desc, setWhitePoint: CGPoint::new(0.3127, 0.3290)];

            let alloc: Allocated<AnyObject> = msg_send![disp_cls, alloc];
            let display: Option<Retained<AnyObject>> = msg_send![alloc, initWithDescriptor: &*desc];
            let display = display.ok_or_else(|| DisplayError::Os("CGVirtualDisplay initWithDescriptor returned nil".into()))?;

            let settings: Option<Retained<AnyObject>> = msg_send![set_cls, new];
            let settings = settings.ok_or_else(|| DisplayError::Os("CGVirtualDisplaySettings alloc failed".into()))?;
            let _: () = msg_send![&*settings, setHiDPI: spec.hidpi as u32];
            // HiDPI needs both the backing (pixel) mode and the "looks like" (point) mode.
            let mut mode_list = Vec::new();
            let sizes: &[(u32, u32)] = if spec.hidpi {
                &[(spec.width * 2, spec.height * 2), (spec.width, spec.height)]
            } else {
                &[(spec.width, spec.height)]
            };
            for &(w, h) in sizes {
                let mode_alloc: Allocated<AnyObject> = msg_send![mode_cls, alloc];
                let mode: Option<Retained<AnyObject>> =
                    msg_send![mode_alloc, initWithWidth: w, height: h, refreshRate: spec.refresh_hz];
                mode_list.push(mode.ok_or_else(|| DisplayError::Os("CGVirtualDisplayMode init returned nil".into()))?);
            }
            let modes: Retained<NSArray<AnyObject>> = NSArray::from_retained_slice(&mode_list);
            let _: () = msg_send![&*settings, setModes: &*modes];
            let applied: bool = msg_send![&*display, applySettings: &*settings];
            if !applied {
                return Err(DisplayError::Os("CGVirtualDisplay applySettings: failed".into()));
            }
            let id: u32 = msg_send![&*display, displayID];
            if id == 0 {
                return Err(DisplayError::Os("virtual display has no display id".into()));
            }
            let deadline = Instant::now() + Duration::from_millis(1500);
            while !online_ids().contains(&id) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if !online_ids().contains(&id) {
                return Err(DisplayError::Os(format!("virtual display {id} did not come online")));
            }
            let _ = NSObject::class(); // keep NSObject linked for the runtime lookups above
            Ok((Handle(display), id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_validation_and_size() {
        assert!(VirtualDisplaySpec::FHD_16_9.validate().is_ok());
        assert!(VirtualDisplaySpec::DQHD_32_9.validate().is_ok());
        assert!(VirtualDisplaySpec { width: 100, ..VirtualDisplaySpec::FHD_16_9 }.validate().is_err());
        let (w, _) = VirtualDisplaySpec::FHD_16_9.size_mm();
        assert!((440.0..445.0).contains(&w), "{w}"); // 3840 px at 220 ppi ≈ 443.3 mm
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn runtime_classes_present() {
        assert!(is_available(), "CGVirtualDisplay classes missing on this macOS");
    }

    /// Attaches a real 1920×1080 HiDPI display for a moment (windows may rearrange briefly).
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "attaches a real virtual display to this Mac"]
    fn create_and_destroy_virtual_display() {
        let before = online_display_count();
        let vd = VirtualDisplay::create("AetherLink Test", VirtualDisplaySpec::FHD_16_9).unwrap();
        assert!(vd.display_id() != 0);
        assert_eq!(online_display_count(), before + 1);
        drop(vd);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while online_display_count() != before && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(online_display_count(), before);
    }
}
