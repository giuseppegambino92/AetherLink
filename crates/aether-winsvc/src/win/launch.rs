//! Session enumeration and launching the agent into a session with a SYSTEM token.

use std::io;
use std::path::Path;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId, TOKEN_ADJUST_SESSIONID,
    TOKEN_ALL_ACCESS, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSActive, WTSEnumerateSessionsW, WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
    WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, ResumeThread, TerminateProcess,
    WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::policy::{build_command_line, desktop_for};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// An owned kernel handle.
pub struct Handle(pub HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: owned handle, closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

// Kernel handles may be used from any thread.
unsafe impl Send for Handle {}

pub fn console_session() -> u32 {
    // SAFETY: no arguments.
    unsafe { WTSGetActiveConsoleSessionId() }
}

/// Ids of sessions in the `WTSActive` state (console and RDP).
pub fn active_sessions() -> Vec<u32> {
    let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: on success the API allocates `count` entries that we free with WTSFreeMemory.
    unsafe {
        if WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut info, &mut count) == 0 || info.is_null() {
            return Vec::new();
        }
        let v = std::slice::from_raw_parts(info, count as usize)
            .iter()
            .filter(|s| s.State == WTSActive)
            .map(|s| s.SessionId)
            .collect();
        WTSFreeMemory(info.cast());
        v
    }
}

/// True when an interactive user is logged on in `session` (needs SE_TCB_NAME, i.e. SYSTEM).
pub fn user_logged_on(session: u32) -> bool {
    let mut tok: HANDLE = std::ptr::null_mut();
    // SAFETY: the returned token is closed immediately.
    unsafe {
        if WTSQueryUserToken(session, &mut tok) == 0 {
            return false;
        }
        CloseHandle(tok);
    }
    true
}

/// A job object that kills every assigned process when the service exits (even on crash).
pub fn kill_on_close_job() -> io::Result<Handle> {
    // SAFETY: anonymous job; the limit struct is fully initialised (zeroed + flag).
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Handle(job);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }
}

/// A running agent.
pub struct Child {
    pub session: u32,
    pub pid: u32,
    pub started: Instant,
    process: Handle,
}

impl Child {
    /// `Some(exit code)` once the process has exited.
    pub fn exit_code(&self) -> Option<u32> {
        // SAFETY: valid process handle owned by `self`.
        unsafe {
            if WaitForSingleObject(self.process.0, 0) != WAIT_OBJECT_0 {
                return None;
            }
            let mut code = 0u32;
            if GetExitCodeProcess(self.process.0, &mut code) == 0 {
                return Some(u32::MAX);
            }
            (code != STILL_ACTIVE as u32).then_some(code)
        }
    }

    /// Terminates the agent and waits up to 5 s for it to go away.
    pub fn terminate(&self) {
        // SAFETY: valid process handle owned by `self`.
        unsafe {
            TerminateProcess(self.process.0, 1);
            WaitForSingleObject(self.process.0, 5000);
        }
    }
}

/// Environment block of `token` plus our variables, as a double-NUL-terminated UTF-16 list.
fn environment(token: HANDLE, extra: &[(&str, String)]) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::new();
    let mut block: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: CreateEnvironmentBlock returns a double-NUL-terminated UTF-16 block that we
    // walk up to the terminator and release with DestroyEnvironmentBlock.
    unsafe {
        if CreateEnvironmentBlock(&mut block, token, 0) != 0 && !block.is_null() {
            let p = block as *const u16;
            let mut i = 0usize;
            loop {
                let start = i;
                while *p.add(i) != 0 {
                    i += 1;
                }
                if i == start {
                    break;
                }
                let var = std::slice::from_raw_parts(p.add(start), i - start);
                let skip = extra.iter().any(|(k, _)| {
                    String::from_utf16_lossy(var).split('=').next().is_some_and(|n| n.eq_ignore_ascii_case(k))
                });
                if !skip {
                    out.extend_from_slice(var);
                    out.push(0);
                }
                i += 1;
            }
            DestroyEnvironmentBlock(block);
        }
    }
    for (k, v) in extra {
        out.extend(format!("{k}={v}").encode_utf16());
        out.push(0);
    }
    out.push(0);
    out
}

/// Launches `agent args…` into `session` with a duplicate of this service's LocalSystem
/// token whose session id is set to `session`, on `winsta0\default` when a user is logged on
/// (else `winsta0\winlogon`), inside `job`.
pub fn launch(session: u32, agent: &Path, args: &[String], job: &Handle) -> io::Result<Child> {
    let agent_s = agent.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "agent path is not UTF-8"))?;
    let logged_on = user_logged_on(session);
    let desktop = desktop_for(logged_on);
    // SAFETY: every handle created here is wrapped in `Handle` (closed on all paths); all
    // strings passed to the API are NUL-terminated and outlive the calls; the command-line
    // buffer is mutable as CreateProcessAsUserW requires.
    unsafe {
        let mut own: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_SESSIONID,
            &mut own,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let own = Handle(own);
        let mut dup: HANDLE = std::ptr::null_mut();
        if DuplicateTokenEx(own.0, TOKEN_ALL_ACCESS, std::ptr::null(), SecurityImpersonation, TokenPrimary, &mut dup)
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        let dup = Handle(dup);
        // Moving a token to another session requires SeTcbPrivilege (held by LocalSystem).
        if SetTokenInformation(dup.0, TokenSessionId, (&session as *const u32).cast(), 4) == 0 {
            return Err(io::Error::last_os_error());
        }
        let env = environment(
            dup.0,
            &[
                ("AETHERLINK_LAUNCHED_BY_SERVICE", "1".to_string()),
                ("AETHERLINK_SESSION_ID", session.to_string()),
                ("AETHERLINK_USER_LOGGED_ON", (logged_on as u8).to_string()),
            ],
        );
        let mut desktop_w = wide(desktop);
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = desktop_w.as_mut_ptr();
        let app = wide(agent_s);
        let mut cmd = wide(&build_command_line(agent_s, args));
        let cwd = agent.parent().and_then(|p| p.to_str()).map(wide);
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessAsUserW(
            dup.0,
            app.as_ptr(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            env.as_ptr().cast(),
            cwd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            &si,
            &mut pi,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let process = Handle(pi.hProcess);
        let thread = Handle(pi.hThread);
        if AssignProcessToJobObject(job.0, process.0) == 0 {
            let e = io::Error::last_os_error();
            TerminateProcess(process.0, 1);
            return Err(e);
        }
        ResumeThread(thread.0);
        tracing::info!(session, pid = pi.dwProcessId, desktop, "agent launched");
        Ok(Child { session, pid: pi.dwProcessId, started: Instant::now(), process })
    }
}
