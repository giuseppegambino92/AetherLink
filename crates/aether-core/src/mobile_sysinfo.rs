//! iOS stand-in for the few `sysinfo` APIs aether-core uses.
//!
//! `sysinfo` 0.37 does not build for iOS (its process/network modules use macOS-only
//! `libc` items), so on iOS `crate::sysinfo` points here. Host name and OS version are
//! real (`gethostname`, `kern.osproductversion`); iOS gives apps no system-wide CPU or
//! per-interface traffic counters, so those samples stay at zero (never invented).

use std::ffi::CStr;

pub struct System {
    total_memory: u64,
}

impl Default for System {
    fn default() -> Self {
        Self::new()
    }
}

fn sysctl_string(name: &CStr) -> Option<String> {
    let mut buf = [0u8; 256];
    let mut len = buf.len();
    // SAFETY: `buf`/`len` describe a valid writable buffer; `name` is NUL-terminated.
    let rc = unsafe { libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || len == 0 {
        return None;
    }
    let s = CStr::from_bytes_until_nul(&buf[..len]).ok()?.to_string_lossy().into_owned();
    (!s.is_empty()).then_some(s)
}

fn sysctl_u64(name: &CStr) -> Option<u64> {
    let mut v: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `v` is a valid u64 out-parameter of `len` bytes.
    let rc = unsafe { libc::sysctlbyname(name.as_ptr(), (&mut v as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) };
    (rc == 0).then_some(v)
}

impl System {
    pub fn new() -> Self {
        Self { total_memory: sysctl_u64(c"hw.memsize").unwrap_or(0) }
    }

    pub fn host_name() -> Option<String> {
        let mut buf = [0u8; 256];
        // SAFETY: valid buffer of the given length.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            return None;
        }
        let s = CStr::from_bytes_until_nul(&buf).ok()?.to_string_lossy().into_owned();
        (!s.is_empty()).then_some(s)
    }

    pub fn os_version() -> Option<String> {
        sysctl_string(c"kern.osproductversion")
    }

    pub fn refresh_cpu_usage(&mut self) {}

    pub fn refresh_memory(&mut self) {}

    pub fn global_cpu_usage(&self) -> f32 {
        0.0
    }

    pub fn total_memory(&self) -> u64 {
        self.total_memory
    }

    pub fn used_memory(&self) -> u64 {
        0
    }
}

pub struct NetworkData;

impl NetworkData {
    pub fn transmitted(&self) -> u64 {
        0
    }

    pub fn received(&self) -> u64 {
        0
    }
}

pub struct Networks;

impl Networks {
    pub fn new_with_refreshed_list() -> Self {
        Self
    }

    pub fn refresh(&mut self, _remove_not_listed: bool) {}

    pub fn iter(&self) -> std::iter::Empty<(&String, &NetworkData)> {
        std::iter::empty()
    }
}
