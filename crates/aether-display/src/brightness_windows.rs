//! Windows backlight: WMI for built-in panels, DDC/CI for external monitors.
//!
//! * Built-in panels: `root\WMI` `WmiMonitorBrightness.CurrentBrightness` (0..100 %) to read,
//!   `WmiMonitorBrightnessMethods.WmiSetBrightness(Timeout, Brightness)` to write. Desktops without
//!   an ACPI/IGD backlight return `WBEM_E_NOT_SUPPORTED`, which simply means "no panels".
//! * External monitors: `GetPhysicalMonitorsFromHMONITOR` + `GetMonitorBrightness` /
//!   `SetMonitorBrightness` (dxva2, DDC/CI over the video cable). Monitors with DDC/CI disabled in
//!   their OSD fail the read and are skipped. DDC/CI transactions take 40–100 ms each.
//! * Ids: panels `0x8000_0000 | fnv(InstanceName)`, DDC monitors `fnv("\\.\DISPLAYn#index") &
//!   0x7FFF_FFFF` — stable across restarts, so the crash-restore file still matches.
//!
//! COM is initialised per call (multithreaded apartment; an existing STA on the calling thread is
//! used as is).

use crate::brightness::{BrightnessSnapshot, stable_id, to_level, to_raw};
use crate::{DisplayError, Result};
use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR,
    PHYSICAL_MONITOR, SetMonitorBrightness,
};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, EOAC_NONE,
    RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Wmi::{
    IEnumWbemClassObject, IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY,
    WBEM_GENERIC_FLAG_TYPE, WBEM_INFINITE, WbemLocator,
};
use windows::core::{BOOL, BSTR, PCWSTR, w};

const PANEL_FLAG: u32 = 0x8000_0000;
/// `RPC_C_AUTHN_WINNT` / `RPC_C_AUTHZ_NONE` (Win32_System_Rpc constants, inlined).
const RPC_C_AUTHN_WINNT: u32 = 10;
const RPC_C_AUTHZ_NONE: u32 = 0;

pub(crate) fn panel_id(instance_name: &str) -> u32 {
    PANEL_FLAG | stable_id(instance_name)
}

pub(crate) fn ddc_id(device: &str, index: usize) -> u32 {
    stable_id(&format!("{device}#{index}")) & !PANEL_FLAG
}

fn os<E: std::fmt::Display>(ctx: &'static str) -> impl FnOnce(E) -> DisplayError {
    move |e| DisplayError::Os(format!("{ctx}: {e}"))
}

pub fn read_all() -> Result<BrightnessSnapshot> {
    let mut levels = Vec::new();
    match wmi::panels() {
        Ok(p) => levels.extend(p.into_iter().map(|(name, pct)| (panel_id(&name), to_level(pct, 0, 100)))),
        Err(e) => tracing::debug!("WMI panel brightness unavailable: {e}"),
    }
    for m in ddc::monitors() {
        levels.push((m.id, to_level(m.current, m.min, m.max)));
    }
    Ok(BrightnessSnapshot { levels })
}

pub fn set(id: u32, level: f32) -> Result<()> {
    if id & PANEL_FLAG != 0 {
        let names = wmi::panels()?;
        let (name, _) = names
            .iter()
            .find(|(n, _)| panel_id(n) == id)
            .ok_or_else(|| DisplayError::Os(format!("panel {id:#010x} is no longer present")))?;
        wmi::set_panel(name, to_raw(level, 0, 100) as u8)
    } else {
        ddc::set(id, level)
    }
}

mod wmi {
    use super::*;

    struct Wmi {
        svc: IWbemServices,
    }

    fn com_init() {
        // SAFETY: balanced by process lifetime; S_FALSE / RPC_E_CHANGED_MODE mean COM is already
        // initialised on this thread, which is fine for in-process WMI calls.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    }

    impl Wmi {
        fn connect() -> Result<Self> {
            com_init();
            // SAFETY: standard WMI connection sequence with owned BSTRs.
            unsafe {
                let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).map_err(os("WbemLocator"))?;
                let svc = locator
                    .ConnectServer(&BSTR::from("root\\WMI"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None)
                    .map_err(os("ConnectServer(root\\WMI)"))?;
                CoSetProxyBlanket(
                    &svc,
                    RPC_C_AUTHN_WINNT,
                    RPC_C_AUTHZ_NONE,
                    PCWSTR::null(),
                    RPC_C_AUTHN_LEVEL_CALL,
                    RPC_C_IMP_LEVEL_IMPERSONATE,
                    None,
                    EOAC_NONE,
                )
                .map_err(os("CoSetProxyBlanket"))?;
                Ok(Self { svc })
            }
        }

        fn query(&self, wql: &str) -> Result<Vec<IWbemClassObject>> {
            // SAFETY: COM calls on a connected service; objects are owned by the returned Vec.
            unsafe {
                let flags = WBEM_GENERIC_FLAG_TYPE(WBEM_FLAG_FORWARD_ONLY.0 | WBEM_FLAG_RETURN_IMMEDIATELY.0);
                let e: IEnumWbemClassObject =
                    self.svc.ExecQuery(&BSTR::from("WQL"), &BSTR::from(wql), flags, None).map_err(os("ExecQuery"))?;
                let mut out = Vec::new();
                loop {
                    let mut row = [None];
                    let mut n = 0u32;
                    e.Next(WBEM_INFINITE, &mut row, &mut n).ok().map_err(os("IEnumWbemClassObject::Next"))?;
                    match row[0].take() {
                        Some(o) if n == 1 => out.push(o),
                        _ => break,
                    }
                }
                Ok(out)
            }
        }
    }

    fn get(obj: &IWbemClassObject, name: PCWSTR) -> Result<VARIANT> {
        let mut v = VARIANT::default();
        // SAFETY: `v` is a valid out VARIANT.
        unsafe { obj.Get(name, 0, &mut v, None, None) }.map_err(os("IWbemClassObject::Get"))?;
        Ok(v)
    }

    /// `(InstanceName, CurrentBrightness %)` of every active internal panel.
    pub fn panels() -> Result<Vec<(String, u32)>> {
        let wmi = Wmi::connect()?;
        let mut out = Vec::new();
        for o in wmi.query("SELECT InstanceName, CurrentBrightness, Active FROM WmiMonitorBrightness")? {
            let active = get(&o, w!("Active")).ok().and_then(|v| bool::try_from(&v).ok()).unwrap_or(true);
            let name = get(&o, w!("InstanceName")).ok().and_then(|v| BSTR::try_from(&v).ok()).map(|b| b.to_string());
            let pct = get(&o, w!("CurrentBrightness")).ok().and_then(|v| u32::try_from(&v).ok());
            if let (true, Some(name), Some(pct)) = (active, name, pct) {
                out.push((name, pct.min(100)));
            }
        }
        Ok(out)
    }

    pub fn set_panel(instance_name: &str, percent: u8) -> Result<()> {
        let wmi = Wmi::connect()?;
        let methods = wmi.query("SELECT * FROM WmiMonitorBrightnessMethods")?;
        let target = methods
            .iter()
            .find(|o| get(o, w!("InstanceName")).ok().and_then(|v| BSTR::try_from(&v).ok()).is_some_and(|b| b == instance_name))
            .ok_or_else(|| DisplayError::Os(format!("no WmiMonitorBrightnessMethods for {instance_name}")))?;
        let path = BSTR::try_from(&get(target, w!("__PATH"))?).map_err(os("__PATH"))?;
        // SAFETY: standard ExecMethod sequence: class → method signature → in-params instance.
        unsafe {
            let mut class = None;
            wmi.svc
                .GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), WBEM_GENERIC_FLAG_TYPE(0), None, Some(&mut class), None)
                .map_err(os("GetObject(WmiMonitorBrightnessMethods)"))?;
            let class: IWbemClassObject = class.ok_or(DisplayError::Os("WmiMonitorBrightnessMethods class missing".into()))?;
            let mut in_sig = None;
            class.GetMethod(w!("WmiSetBrightness"), 0, &mut in_sig, std::ptr::null_mut()).map_err(os("GetMethod"))?;
            let in_sig = in_sig.ok_or(DisplayError::Os("WmiSetBrightness has no input signature".into()))?;
            let params = in_sig.SpawnInstance(0).map_err(os("SpawnInstance"))?;
            // CIM uint32 travels as VT_I4, uint8 as VT_UI1.
            params.Put(w!("Timeout"), 0, &VARIANT::from(0i32), 0).map_err(os("Put(Timeout)"))?;
            params.Put(w!("Brightness"), 0, &VARIANT::from(percent), 0).map_err(os("Put(Brightness)"))?;
            wmi.svc
                .ExecMethod(&path, &BSTR::from("WmiSetBrightness"), WBEM_GENERIC_FLAG_TYPE(0), None, &params, None, None)
                .map_err(os("ExecMethod(WmiSetBrightness)"))?;
        }
        Ok(())
    }
}

mod ddc {
    use super::*;

    pub struct Monitor {
        pub id: u32,
        pub min: u32,
        pub current: u32,
        pub max: u32,
    }

    /// `(device name, HMONITOR)` of every display monitor.
    fn hmonitors() -> Vec<(String, HMONITOR)> {
        unsafe extern "system" fn cb(hmon: HMONITOR, _hdc: HDC, _rc: *mut RECT, data: LPARAM) -> BOOL {
            // SAFETY: `data` is the &mut Vec passed below, alive for the enumeration.
            let out = unsafe { &mut *(data.0 as *mut Vec<(String, HMONITOR)>) };
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            // SAFETY: MONITORINFOEXW starts with MONITORINFO and cbSize announces the larger struct.
            if unsafe { GetMonitorInfoW(hmon, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) }.as_bool() {
                let end = mi.szDevice.iter().position(|c| *c == 0).unwrap_or(mi.szDevice.len());
                out.push((String::from_utf16_lossy(&mi.szDevice[..end]), hmon));
            }
            BOOL(1)
        }
        let mut out: Vec<(String, HMONITOR)> = Vec::new();
        // SAFETY: the callback only touches `out` during the call.
        let _ = unsafe { EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut out as *mut _ as isize)) };
        out
    }

    /// Runs `f` on every DDC/CI-capable physical monitor; handles are released afterwards.
    fn for_each(mut f: impl FnMut(u32, &PHYSICAL_MONITOR, u32, u32, u32) -> bool) {
        for (device, hmon) in hmonitors() {
            let mut n = 0u32;
            // SAFETY: valid HMONITOR; the array is sized from the count and destroyed below.
            unsafe {
                if GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut n).is_err() || n == 0 {
                    continue;
                }
                let mut phys = vec![PHYSICAL_MONITOR::default(); n as usize];
                if GetPhysicalMonitorsFromHMONITOR(hmon, &mut phys).is_err() {
                    continue;
                }
                let mut done = false;
                for (i, p) in phys.iter().enumerate() {
                    let (mut min, mut cur, mut max) = (0u32, 0u32, 0u32);
                    if !done && GetMonitorBrightness(p.hPhysicalMonitor, &mut min, &mut cur, &mut max) != 0 && max > min {
                        done = !f(ddc_id(&device, i), p, min, cur, max);
                    }
                }
                let _ = DestroyPhysicalMonitors(&phys);
                if done {
                    return;
                }
            }
        }
    }

    pub fn monitors() -> Vec<Monitor> {
        let mut out = Vec::new();
        for_each(|id, _, min, current, max| {
            out.push(Monitor { id, min, current, max });
            true
        });
        out
    }

    pub fn set(id: u32, level: f32) -> Result<()> {
        let mut result = Err(DisplayError::Os(format!("DDC/CI monitor {id:#010x} is no longer present")));
        for_each(|mid, p, min, _, max| {
            if mid != id {
                return true;
            }
            // SAFETY: valid physical monitor handle for the duration of the callback.
            let ok = unsafe { SetMonitorBrightness(p.hPhysicalMonitor, to_raw(level, min, max)) } != 0;
            result = if ok { Ok(()) } else { Err(DisplayError::Os(format!("SetMonitorBrightness({id:#010x}) failed"))) };
            false
        });
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_and_ddc_ids_never_collide_and_are_stable() {
        let p = panel_id("DISPLAY\\BOE0900\\4&2d2a6a8b&0&UID8388688_0");
        let d = ddc_id("\\\\.\\DISPLAY1", 0);
        assert_ne!(p & PANEL_FLAG, 0);
        assert_eq!(d & PANEL_FLAG, 0);
        assert_eq!(p, panel_id("DISPLAY\\BOE0900\\4&2d2a6a8b&0&UID8388688_0"));
        assert_ne!(ddc_id("\\\\.\\DISPLAY1", 0), ddc_id("\\\\.\\DISPLAY1", 1));
        assert_ne!(ddc_id("\\\\.\\DISPLAY1", 0), ddc_id("\\\\.\\DISPLAY2", 0));
    }

    /// Real hardware: read every controllable display and write the same level back.
    #[test]
    #[ignore = "needs real Windows hardware (WMI panel or DDC/CI monitor)"]
    fn read_and_rewrite_same_levels() {
        let snap = read_all().unwrap();
        println!("{snap:?}");
        for (id, v) in &snap.levels {
            set(*id, *v).unwrap();
        }
        let again = read_all().unwrap();
        for ((a, va), (b, vb)) in snap.levels.iter().zip(&again.levels) {
            assert_eq!(a, b);
            assert!((va - vb).abs() < 0.011);
        }
    }

    /// Enumeration must never fail hard, even without panels / DDC (VMs, wine).
    #[test]
    #[ignore = "touches WMI / DDC/CI; run on a real Windows host"]
    fn enumeration_is_infallible() {
        let snap = read_all().unwrap();
        assert!(snap.levels.iter().all(|(_, v)| (0.0..=1.0).contains(v)));
    }
}
