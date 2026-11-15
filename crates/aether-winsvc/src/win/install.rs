//! `install` / `uninstall` / `status` and the SoftwareSASGeneration policy.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, REG_DWORD, RRF_RT_REG_DWORD, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};

use super::service::{DISPLAY_NAME, SERVICE_NAME};
use crate::cli::Options;
use crate::policy::agent_path_is_protected;

const POLICY_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
const POLICY_VALUE: &str = "SoftwareSASGeneration";
const OUR_KEY: &str = r"SOFTWARE\AetherLink\Service";
/// Previous policy value saved at install time (`u32::MAX` = it did not exist).
const SAVED_VALUE: &str = "SoftwareSASGenerationBeforeInstall";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn reg_get_dword(key: &str, name: &str) -> Option<u32> {
    let (k, n) = (wide(key), wide(name));
    let mut data = 0u32;
    let mut size = 4u32;
    // SAFETY: DWORD read into a 4-byte buffer whose size is passed.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            n.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (r == 0).then_some(data)
}

fn reg_set_dword(key: &str, name: &str, v: u32) -> Result<(), String> {
    let (k, n) = (wide(key), wide(name));
    // SAFETY: 4-byte DWORD payload; RegSetKeyValueW creates the key if missing.
    let r = unsafe { RegSetKeyValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), n.as_ptr(), REG_DWORD, (&v as *const u32).cast(), 4) };
    if r == 0 {
        Ok(())
    } else {
        Err(format!("HKLM\\{key}\\{name}: {}", std::io::Error::from_raw_os_error(r as i32)))
    }
}

fn reg_delete_value(key: &str, name: &str) {
    let (k, n) = (wide(key), wide(name));
    // SAFETY: NUL-terminated strings.
    unsafe { RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), n.as_ptr()) };
}

/// Enables software Ctrl+Alt+Del for services (bit 1), remembering the previous value.
fn enable_sas_policy() -> Result<(), String> {
    let prev = reg_get_dword(POLICY_KEY, POLICY_VALUE);
    if reg_get_dword(OUR_KEY, SAVED_VALUE).is_none() {
        reg_set_dword(OUR_KEY, SAVED_VALUE, prev.unwrap_or(u32::MAX))?;
    }
    let new = prev.unwrap_or(0) | 1;
    if prev != Some(new) {
        reg_set_dword(POLICY_KEY, POLICY_VALUE, new)?;
    }
    Ok(())
}

fn restore_sas_policy() {
    let Some(saved) = reg_get_dword(OUR_KEY, SAVED_VALUE) else { return };
    if saved == u32::MAX {
        reg_delete_value(POLICY_KEY, POLICY_VALUE);
    } else if let Err(e) = reg_set_dword(POLICY_KEY, POLICY_VALUE, saved) {
        eprintln!("warning: cannot restore {POLICY_VALUE}: {e}");
    }
    reg_delete_value(OUR_KEY, SAVED_VALUE);
}

/// Directories only administrators can modify.
pub fn protected_roots() -> Vec<String> {
    ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "SystemRoot"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .collect()
}

/// Default agent: `AetherLink.exe` next to this binary.
pub fn resolve_agent(opts: &Options) -> Result<PathBuf, String> {
    let agent = match &opts.agent {
        Some(a) => a.clone(),
        None => std::env::current_exe()
            .map_err(|e| format!("current_exe: {e}"))?
            .with_file_name("AetherLink.exe"),
    };
    if !agent.is_absolute() {
        return Err(format!("agent path must be absolute: {}", agent.display()));
    }
    if !agent.is_file() {
        return Err(format!("agent executable not found: {}", agent.display()));
    }
    check_protected(&agent, opts.allow_unprotected_agent, "agent")?;
    Ok(agent)
}

fn check_protected(p: &Path, allow: bool, what: &str) -> Result<(), String> {
    let s = p.to_string_lossy();
    if agent_path_is_protected(&s, &protected_roots()) {
        return Ok(());
    }
    if allow {
        eprintln!(
            "warning: the {what} {s} is outside %ProgramFiles% / %SystemRoot%; anyone who can modify it gains SYSTEM rights"
        );
        Ok(())
    } else {
        Err(format!(
            "refusing: the {what} {s} is not under %ProgramFiles% or %SystemRoot%. The service runs it as SYSTEM, so a \
             user-writable location is a privilege escalation. Install into Program Files or pass \
             --allow-unprotected-agent-path"
        ))
    }
}

pub fn install(mut opts: Options) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    check_protected(&exe, opts.allow_unprotected_agent, "service binary")?;
    opts.agent = Some(resolve_agent(&opts)?);
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
        .map_err(|e| format!("cannot open the service manager (run as administrator): {e}"))?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: opts.to_run_args().into_iter().map(OsString::from).collect(),
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let svc = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS)
        .map_err(|e| format!("create service: {e}"))?;
    svc.set_description(
        "Launches the AetherLink agent into interactive sessions so remote control works on the lock screen and UAC prompts.",
    )
    .map_err(|e| format!("set description: {e}"))?;
    let restart = ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(5) };
    svc.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart.clone(), restart.clone(), restart]),
    })
    .map_err(|e| format!("failure actions: {e}"))?;
    svc.set_failure_actions_on_non_crash_failures(true).map_err(|e| format!("failure actions flag: {e}"))?;
    if opts.set_sas_policy {
        enable_sas_policy()?;
    }
    svc.start::<&str>(&[]).map_err(|e| format!("start service: {e}"))?;
    println!("{SERVICE_NAME} installed and started (agent: {})", opts.agent.as_deref().unwrap_or(Path::new("?")).display());
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| format!("cannot open the service manager (run as administrator): {e}"))?;
    let svc = manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)
        .map_err(|e| format!("open service: {e}"))?;
    if svc.query_status().map(|s| s.current_state != ServiceState::Stopped).unwrap_or(false) {
        let _ = svc.stop();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline
            && svc.query_status().map(|s| s.current_state != ServiceState::Stopped).unwrap_or(false)
        {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    svc.delete().map_err(|e| format!("delete service: {e}"))?;
    restore_sas_policy();
    println!("{SERVICE_NAME} removed");
    Ok(())
}

pub fn status() -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| format!("service manager: {e}"))?;
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG) {
        Ok(svc) => {
            let st = svc.query_status().map_err(|e| format!("query status: {e}"))?;
            let cfg = svc.query_config().map_err(|e| format!("query config: {e}"))?;
            println!("{SERVICE_NAME}: {:?} (pid {:?})", st.current_state, st.process_id);
            println!("command: {}", cfg.executable_path.display());
            println!("{POLICY_VALUE}: {:?}", reg_get_dword(POLICY_KEY, POLICY_VALUE));
            Ok(())
        }
        Err(_) => {
            println!("{SERVICE_NAME}: not installed");
            Ok(())
        }
    }
}
