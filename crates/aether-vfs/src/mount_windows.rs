//! Windows drive mapping of a remote share through the built-in WebDAV redirector.
//!
//! [`crate::mount()`] on Windows starts the [`crate::webdav`] loopback server (on
//! `127.0.0.1` and, best-effort, `[::1]` with the same port, because `localhost` may resolve
//! to IPv6 first), makes sure the **WebClient** service runs, and maps
//! `\\localhost@<port>\<token>` with `WNetAddConnection2W` (`CONNECT_TEMPORARY`, not
//! restored at next logon):
//!
//! * mount point `X:` / `X:\` → that drive letter (fails if taken);
//! * any other mount point → the highest free letter from `Z:` down to `D:`, or a UNC-only
//!   connection when every letter is taken. [`crate::MountHandle::mount_point`] is then
//!   `X:\` or the UNC path.
//!
//! Unmounting calls `WNetCancelConnection2W(name, 0, force = TRUE)`.
//!
//! ## Caveats (documented, not hidden)
//!
//! * The mapping lives in the **calling user's logon session**: a process running as
//!   `LocalSystem` (e.g. an agent launched by `aether-winsvc`) creates a drive that the
//!   interactive user's Explorer cannot see. Mount from the per-user desktop app.
//! * WebClient refuses files larger than `FileSizeLimitInBytes` (registry
//!   `HKLM\SYSTEM\CurrentControlSet\Services\WebClient\Parameters`, default 50 000 000
//!   bytes ≈ 47.7 MiB) — raise it as administrator and restart WebClient for larger files.
//! * WebClient is "Manual (Trigger Start)". Starting it needs rights a standard user may
//!   not have; in that case the mapping itself usually trigger-starts it. If it is not
//!   installed (Windows Server without the *WebDAV Redirector* feature) mounting fails
//!   with an explicit message.
//! * The first connection can stall for seconds when "Automatically detect settings"
//!   (WPAD) is on in the Internet options; loopback is otherwise fast.

// The pure helpers are compiled (and unit-tested) everywhere; only Windows calls them.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::Path;

/// Registry location of the WebClient tunables.
pub const WEBCLIENT_PARAMETERS_KEY: &str = r"SYSTEM\CurrentControlSet\Services\WebClient\Parameters";

/// `X:`, `x:\`, `X:/` → `'X'`.
pub(crate) fn parse_drive_letter(p: &Path) -> Option<char> {
    let s = p.to_str()?;
    let b = s.as_bytes();
    let ok = match b.len() {
        2 => b[1] == b':',
        3 => b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'),
        _ => false,
    };
    (ok && b[0].is_ascii_alphabetic()).then(|| (b[0] as char).to_ascii_uppercase())
}

/// Highest free drive letter in `Z..=D` given the `GetLogicalDrives` bit mask (bit 0 = A).
pub(crate) fn pick_free_drive(used_mask: u32) -> Option<char> {
    (b'D'..=b'Z').rev().find(|l| used_mask & (1 << (l - b'A')) == 0).map(|l| l as char)
}

/// Human-readable text for the size limit, used in logs and error messages.
pub(crate) fn size_limit_note(limit: Option<u64>) -> String {
    let limit = limit.unwrap_or(crate::webdav::WINDOWS_WEBCLIENT_DEFAULT_FILE_SIZE_LIMIT);
    format!(
        "Windows WebClient refuses files larger than {limit} bytes ({:.1} MiB); raise \
         HKLM\\{WEBCLIENT_PARAMETERS_KEY}\\FileSizeLimitInBytes (max 4294967295) and restart the \
         WebClient service for larger files",
        limit as f64 / (1024.0 * 1024.0)
    )
}

#[cfg(windows)]
pub(crate) mod sys {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_ALREADY_ASSIGNED, ERROR_BAD_NET_NAME, ERROR_BAD_NETPATH, ERROR_EXTENDED_ERROR,
        ERROR_NO_NET_OR_BAD_PATH, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, GetLastError,
    };
    use windows_sys::Win32::NetworkManagement::WNet::{
        CONNECT_TEMPORARY, NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection2W, WNetCancelConnection2W,
        WNetGetLastErrorW,
    };
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_HANDLE, SC_MANAGER_CONNECT,
        SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START, SERVICE_STATUS, StartServiceW,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct Sc(SC_HANDLE);
    impl Drop for Sc {
        fn drop(&mut self) {
            // SAFETY: handle returned by OpenSCManagerW / OpenServiceW, closed once.
            unsafe { CloseServiceHandle(self.0) };
        }
    }

    /// State of the WebClient service after [`ensure_webclient`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum WebClient {
        Running,
        /// Not running and we may not start it (the mapping may still trigger-start it).
        NotRunningNoRights,
    }

    fn running(svc: &Sc) -> bool {
        // SAFETY: valid service handle; SERVICE_STATUS is plain data.
        let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
        (unsafe { QueryServiceStatus(svc.0, &mut st) } != 0) && st.dwCurrentState == SERVICE_RUNNING
    }

    /// Makes sure the WebClient service runs, starting it when permitted.
    pub(crate) fn ensure_webclient() -> Result<WebClient, String> {
        // SAFETY: plain Win32 calls with valid (NUL-terminated) arguments; handles are wrapped
        // in `Sc` and closed on every path.
        unsafe {
            let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
            if scm.is_null() {
                return Err(format!("cannot open the service manager: {}", std::io::Error::last_os_error()));
            }
            let scm = Sc(scm);
            let name = wide("WebClient");
            let mut can_start = true;
            let mut svc = OpenServiceW(scm.0, name.as_ptr(), SERVICE_QUERY_STATUS | SERVICE_START);
            if svc.is_null() && GetLastError() == ERROR_ACCESS_DENIED {
                can_start = false;
                svc = OpenServiceW(scm.0, name.as_ptr(), SERVICE_QUERY_STATUS);
            }
            if svc.is_null() {
                let e = GetLastError();
                return Err(if e == ERROR_SERVICE_DOES_NOT_EXIST {
                    "the WebClient service (WebDAV redirector) is not installed; on Windows Server add the \
                     'WebDAV Redirector' feature (Install-WindowsFeature WebDAV-Redirector) and reboot"
                        .to_string()
                } else {
                    format!("cannot open the WebClient service: {}", std::io::Error::from_raw_os_error(e as i32))
                });
            }
            let svc = Sc(svc);
            if running(&svc) {
                return Ok(WebClient::Running);
            }
            if !can_start {
                return Ok(WebClient::NotRunningNoRights);
            }
            if StartServiceW(svc.0, 0, std::ptr::null()) == 0 {
                let e = GetLastError();
                if e == ERROR_ACCESS_DENIED {
                    return Ok(WebClient::NotRunningNoRights);
                }
                if e != ERROR_SERVICE_ALREADY_RUNNING {
                    return Err(format!(
                        "cannot start the WebClient service: {} (start it with `sc start WebClient` as administrator \
                         or set it to Automatic)",
                        std::io::Error::from_raw_os_error(e as i32)
                    ));
                }
            }
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                if running(&svc) {
                    return Ok(WebClient::Running);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err("the WebClient service did not reach the running state within 15 s".into())
        }
    }

    /// `FileSizeLimitInBytes` of the WebClient service, if readable.
    pub(crate) fn webclient_file_size_limit() -> Option<u64> {
        let key = wide(super::WEBCLIENT_PARAMETERS_KEY);
        let value = wide("FileSizeLimitInBytes");
        let mut data = 0u32;
        let mut size = 4u32;
        // SAFETY: DWORD-typed read into a 4-byte buffer whose size is passed in `size`.
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut data as *mut u32).cast(),
                &mut size,
            )
        };
        (r == 0).then_some(data as u64)
    }

    pub(crate) fn free_drive_letter() -> Option<char> {
        // SAFETY: no arguments.
        super::pick_free_drive(unsafe { GetLogicalDrives() })
    }

    fn extended_error() -> String {
        let mut code = 0u32;
        let mut msg = [0u16; 512];
        let mut provider = [0u16; 128];
        // SAFETY: buffers and their lengths (in characters) are passed together.
        let r = unsafe {
            WNetGetLastErrorW(&mut code, msg.as_mut_ptr(), msg.len() as u32, provider.as_mut_ptr(), provider.len() as u32)
        };
        if r != 0 {
            return "unknown provider error".into();
        }
        let s = |b: &[u16]| String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]);
        format!("{} (provider {}, code {code})", s(&msg).trim(), s(&provider))
    }

    /// Maps `remote` (`\\localhost@port\token`) to `local` (`'Z'`) or as a UNC-only connection.
    pub(crate) fn add_connection(local: Option<char>, remote: &str, webclient: WebClient) -> Result<(), String> {
        let mut local_w = local.map(|l| wide(&format!("{l}:")));
        let mut remote_w = wide(remote);
        let nr = NETRESOURCEW {
            dwScope: 0,
            dwType: RESOURCETYPE_DISK,
            dwDisplayType: 0,
            dwUsage: 0,
            lpLocalName: local_w.as_mut().map_or(std::ptr::null_mut(), |v| v.as_mut_ptr()),
            lpRemoteName: remote_w.as_mut_ptr(),
            lpComment: std::ptr::null_mut(),
            lpProvider: std::ptr::null_mut(),
        };
        // SAFETY: `nr` points at NUL-terminated buffers that outlive the call; no credentials.
        let r = unsafe { WNetAddConnection2W(&nr, std::ptr::null(), std::ptr::null(), CONNECT_TEMPORARY) };
        if r == 0 {
            return Ok(());
        }
        let not_running_hint = if webclient == WebClient::NotRunningNoRights {
            " — the WebClient service is not running and this account may not start it: run \
             `sc start WebClient` as administrator or set the service to Automatic"
        } else {
            ""
        };
        Err(match r {
            ERROR_ALREADY_ASSIGNED => format!("drive {}: is already in use", local.unwrap_or('?')),
            ERROR_BAD_NET_NAME | ERROR_BAD_NETPATH | ERROR_NO_NET_OR_BAD_PATH => format!(
                "the WebDAV redirector could not reach {remote} ({}){not_running_hint}",
                std::io::Error::from_raw_os_error(r as i32)
            ),
            ERROR_EXTENDED_ERROR => format!("WNetAddConnection2W: {}{not_running_hint}", extended_error()),
            _ => format!(
                "WNetAddConnection2W({remote}): {}{not_running_hint}",
                std::io::Error::from_raw_os_error(r as i32)
            ),
        })
    }

    /// Cancels a mapping by drive (`Z:`) or UNC name, forcing open files closed.
    pub(crate) fn cancel_connection(name: &str) -> Result<(), String> {
        let w = wide(name);
        // SAFETY: NUL-terminated name; flags 0 = do not touch the persistent profile.
        let r = unsafe { WNetCancelConnection2W(w.as_ptr(), 0, 1) };
        if r == 0 { Ok(()) } else { Err(format!("WNetCancelConnection2W({name}): {}", std::io::Error::from_raw_os_error(r as i32))) }
    }
}

/// Name to pass to `WNetCancelConnection2W` for a mount point produced by `mount()`:
/// `Z:\` → `Z:`, UNC paths unchanged.
pub(crate) fn connection_name(mount_point: &Path) -> String {
    match parse_drive_letter(mount_point) {
        Some(l) => format!("{l}:"),
        None => mount_point.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_letters() {
        assert_eq!(parse_drive_letter(Path::new("z:")), Some('Z'));
        assert_eq!(parse_drive_letter(Path::new("Q:\\")), Some('Q'));
        assert_eq!(parse_drive_letter(Path::new("Q:/")), Some('Q'));
        assert_eq!(parse_drive_letter(Path::new("Q:\\x")), None);
        assert_eq!(parse_drive_letter(Path::new("1:")), None);
        assert_eq!(parse_drive_letter(Path::new("/home/me/AetherLink/Peer")), None);
        // A..C used, Z used → Y.
        assert_eq!(pick_free_drive(0b111 | 1 << 25), Some('Y'));
        assert_eq!(pick_free_drive(0), Some('Z'));
        assert_eq!(pick_free_drive(u32::MAX), None);
        // Only A..C free → none (A–C are never auto-picked).
        assert_eq!(pick_free_drive(!0b111), None);
        assert_eq!(connection_name(Path::new("Y:\\")), "Y:");
        assert_eq!(connection_name(Path::new(r"\\localhost@1234\abc")), r"\\localhost@1234\abc");
    }

    #[test]
    fn size_limit_message() {
        let m = size_limit_note(None);
        assert!(m.contains("50000000") && m.contains("FileSizeLimitInBytes"), "{m}");
        assert!(size_limit_note(Some(4_294_967_295)).contains("4096.0 MiB"));
    }
}
