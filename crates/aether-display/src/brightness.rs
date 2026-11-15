//! Display backlight control.
//!
//! macOS: the private `DisplayServices` framework (loaded at runtime with `dlopen`, so the
//! binary still launches if Apple removes it) — `DisplayServicesGetBrightness` /
//! `DisplayServicesSetBrightness` / `DisplayServicesCanChangeBrightness`. This drives the
//! real hardware backlight of built-in panels (and Apple displays); third-party external
//! monitors expose no backlight API without DDC/CI and are reported as not dimmable.
//!
//! Windows (`brightness_windows.rs`): built-in panels through WMI (`root\WMI`
//! `WmiMonitorBrightness` / `WmiMonitorBrightnessMethods.WmiSetBrightness`, 0..100 %) and
//! external monitors through DDC/CI (`dxva2`: `GetPhysicalMonitorsFromHMONITOR` +
//! `GetMonitorBrightness` / `SetMonitorBrightness`). Ids are stable hashes of the WMI instance
//! name (high bit set) or of `\\.\DISPLAYn` + physical-monitor index, so a restore file
//! written before a crash still matches after a restart.
//!
//! Linux (`brightness_linux.rs`): `/sys/class/backlight/*/{brightness,max_brightness,type}` is
//! read directly (world-readable); changes go through systemd-logind
//! `org.freedesktop.login1.Session.SetBrightness("backlight", name, value)` on the system bus,
//! which logind allows for the active session without root (falls back to writing sysfs when
//! that is permitted, e.g. a udev rule). Only devices of the highest-priority type present
//! (`firmware` > `platform` > `raw`, the kernel's recommendation) are used, so the same panel
//! is not driven twice. Ids are hashes of the device name.
//!
//! Every change goes through a [`BrightnessSnapshot`] that can be persisted to disk before
//! dimming, so a crash while the screen is dark is undone at next launch
//! ([`restore_pending`]).

use crate::{DisplayError, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Saved backlight levels (`display id → 0.0..=1.0`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BrightnessSnapshot {
    pub levels: Vec<(u32, f32)>,
}

/// Displays whose backlight can be read and changed, with their current level.
pub fn read_all() -> Result<BrightnessSnapshot> {
    imp::read_all()
}

/// Set every listed display to `level` (0.0 = backlight off).
pub fn set_all(snapshot: &BrightnessSnapshot, level: f32) -> Result<()> {
    for (id, _) in &snapshot.levels {
        imp::set(*id, level.clamp(0.0, 1.0))?;
    }
    Ok(())
}

/// Restore exactly the saved levels.
pub fn restore(snapshot: &BrightnessSnapshot) -> Result<()> {
    let mut first_err = None;
    for (id, level) in &snapshot.levels {
        if let Err(e) = imp::set(*id, *level) {
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// Read current levels, persist them to `restore_file` (if given), then dim to 0.
pub fn dim(restore_file: Option<&Path>) -> Result<BrightnessSnapshot> {
    let snap = read_all()?;
    if let Some(p) = restore_file {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| DisplayError::Io(e.to_string()))?;
        }
        let json = serde_json::to_vec(&snap).map_err(|e| DisplayError::Io(e.to_string()))?;
        std::fs::write(p, json).map_err(|e| DisplayError::Io(e.to_string()))?;
    }
    set_all(&snap, 0.0)?;
    Ok(snap)
}

/// Restore and delete a snapshot left behind by a crash during Ghost Mode.
/// Returns `true` when something was restored.
pub fn restore_pending(restore_file: &Path) -> Result<bool> {
    let bytes = match std::fs::read(restore_file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(DisplayError::Io(e.to_string())),
    };
    let snap: BrightnessSnapshot = serde_json::from_slice(&bytes).map_err(|e| DisplayError::Io(e.to_string()))?;
    restore(&snap)?;
    let _ = std::fs::remove_file(restore_file);
    Ok(!snap.levels.is_empty())
}

#[cfg(target_os = "macos")]
mod imp {
    use super::BrightnessSnapshot;
    use crate::{DisplayError, Result};
    use std::ffi::{CStr, c_void};
    use std::sync::OnceLock;

    type GetFn = unsafe extern "C" fn(u32, *mut f32) -> i32;
    type SetFn = unsafe extern "C" fn(u32, f32) -> i32;
    type CanFn = unsafe extern "C" fn(u32) -> bool;

    struct Api {
        get: GetFn,
        set: SetFn,
        can: Option<CanFn>,
    }

    fn api() -> Result<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(|| unsafe {
            const PATH: &CStr = c"/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices";
            let h = libc::dlopen(PATH.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
            if h.is_null() {
                return None;
            }
            let sym = |name: &CStr| -> *mut c_void { libc::dlsym(h, name.as_ptr()) };
            let (g, s, c) = (
                sym(c"DisplayServicesGetBrightness"),
                sym(c"DisplayServicesSetBrightness"),
                sym(c"DisplayServicesCanChangeBrightness"),
            );
            if g.is_null() || s.is_null() {
                return None;
            }
            Some(Api {
                get: std::mem::transmute::<*mut c_void, GetFn>(g),
                set: std::mem::transmute::<*mut c_void, SetFn>(s),
                can: (!c.is_null()).then(|| std::mem::transmute::<*mut c_void, CanFn>(c)),
            })
        })
        .as_ref()
        .ok_or(DisplayError::Unavailable("DisplayServices brightness control"))
    }

    fn online_displays() -> Vec<u32> {
        let mut ids = [0u32; 32];
        let mut n = 0u32;
        // SAFETY: buffer of 32 ids, `n` receives the count.
        let err = unsafe { objc2_core_graphics::CGGetOnlineDisplayList(32, ids.as_mut_ptr(), &mut n) };
        if err != objc2_core_graphics::CGError::Success {
            return Vec::new();
        }
        ids[..n as usize].to_vec()
    }

    pub fn read_all() -> Result<BrightnessSnapshot> {
        let api = api()?;
        let mut levels = Vec::new();
        for id in online_displays() {
            // SAFETY: plain C calls with a valid display id / out-pointer.
            unsafe {
                if let Some(can) = api.can
                    && !can(id)
                {
                    continue;
                }
                let mut v = -1.0f32;
                if (api.get)(id, &mut v) == 0 && (0.0..=1.0).contains(&v) {
                    levels.push((id, v));
                }
            }
        }
        Ok(BrightnessSnapshot { levels })
    }

    pub fn set(id: u32, level: f32) -> Result<()> {
        let api = api()?;
        // SAFETY: plain C call.
        let rc = unsafe { (api.set)(id, level) };
        if rc == 0 { Ok(()) } else { Err(DisplayError::Os(format!("DisplayServicesSetBrightness({id}) = {rc}"))) }
    }
}

#[cfg(target_os = "windows")]
use crate::brightness_windows as imp;

#[cfg(target_os = "linux")]
use crate::brightness_linux as imp;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod imp {
    use super::BrightnessSnapshot;
    use crate::{DisplayError, Result};

    pub fn read_all() -> Result<BrightnessSnapshot> {
        Err(DisplayError::Unsupported("backlight control is implemented for macOS, Windows and Linux"))
    }

    pub fn set(_id: u32, _level: f32) -> Result<()> {
        Err(DisplayError::Unsupported("backlight control is implemented for macOS, Windows and Linux"))
    }
}

/// Stable 32-bit FNV-1a hash used to derive display ids from OS device names.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(crate) fn stable_id(name: &str) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for b in name.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// `raw` in `min..=max` → 0.0..=1.0.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(crate) fn to_level(raw: u32, min: u32, max: u32) -> f32 {
    if max <= min {
        return 0.0;
    }
    ((raw.clamp(min, max) - min) as f64 / (max - min) as f64) as f32
}

/// 0.0..=1.0 → raw value in `min..=max`, rounded so `to_raw(to_level(v)) == v`.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(crate) fn to_raw(level: f32, min: u32, max: u32) -> u32 {
    if max <= min {
        return min;
    }
    let l = if level.is_finite() { level.clamp(0.0, 1.0) as f64 } else { 0.0 };
    min + ((max - min) as f64 * l).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_ids_and_level_conversion_roundtrip() {
        // FNV-1a reference values.
        assert_eq!(stable_id(""), 0x811C_9DC5);
        assert_eq!(stable_id("a"), 0xE40C_292C);
        assert_ne!(stable_id("intel_backlight"), stable_id("acpi_video0"));
        for (min, max) in [(0, 100), (0, 255), (0, 19393), (0, 120000), (10, 90)] {
            for raw in [min, min + 1, (min + max) / 3, max - 1, max] {
                assert_eq!(to_raw(to_level(raw, min, max), min, max), raw, "{raw} in {min}..={max}");
            }
            assert_eq!(to_raw(0.0, min, max), min);
            assert_eq!(to_raw(f32::NAN, min, max), min);
            assert_eq!(to_raw(7.0, min, max), max);
        }
        assert_eq!(to_level(5, 0, 0), 0.0);
    }

    #[test]
    fn pending_restore_file_roundtrip_without_touching_hardware() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("restore.json");
        assert!(!restore_pending(&p).unwrap(), "missing file is a no-op");
        // An empty snapshot restores nothing and removes the file.
        std::fs::write(&p, serde_json::to_vec(&BrightnessSnapshot::default()).unwrap()).unwrap();
        assert!(!restore_pending(&p).unwrap());
        assert!(!p.exists());
    }

    /// Reads levels and writes the *same* value back — never changes what the user sees.
    #[cfg(target_os = "macos")]
    #[test]
    fn read_and_rewrite_same_levels() {
        let Ok(snap) = read_all() else { return };
        for (id, v) in &snap.levels {
            assert!((0.0..=1.0).contains(v), "display {id} level {v}");
        }
        restore(&snap).unwrap();
        let again = read_all().unwrap();
        for ((a, va), (b, vb)) in snap.levels.iter().zip(&again.levels) {
            assert_eq!(a, b);
            assert!((va - vb).abs() < 0.01);
        }
    }
}
