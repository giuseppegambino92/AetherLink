//! Service entry point (SCM dispatcher) and the session supervisor loop.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    SessionChangeReason,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use super::launch::{self, Child, Handle};
use crate::policy::{Action, Backoff, desired_sessions, reconcile};

pub const SERVICE_NAME: &str = "AetherLinkService";
pub const DISPLAY_NAME: &str = "AetherLink Remote Desktop Service";

/// Resolved settings for the supervisor (set by `main` before the dispatcher starts).
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub agent: PathBuf,
    pub agent_args: Vec<String>,
    pub include_remote: bool,
}

static CONFIG: OnceLock<SupervisorConfig> = OnceLock::new();

pub enum Event {
    Stop,
    /// Something changed; reconcile now.
    Wake,
    /// Restart the agent of this session (logon / logoff changed its desktop & user).
    Restart(u32),
}

define_windows_service!(ffi_service_main, service_main);

/// Hands the process to the SCM; returns when the service stopped.
pub fn run_dispatcher(cfg: SupervisorConfig) -> Result<(), String> {
    let _ = CONFIG.set(cfg);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| format!("cannot start the service dispatcher (run this through the SCM, or use `run --console`): {e}"))
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!("service failed: {e}");
    }
}

fn status(state: ServiceState, exit: u32) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN | ServiceControlAccept::SESSION_CHANGE
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(exit),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    }
}

fn run_service() -> windows_service::Result<()> {
    let cfg = CONFIG.get().cloned().expect("config set before dispatch");
    let (tx, rx) = channel();
    let handle = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = tx.send(Event::Stop);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(p) => {
            let ev = match p.reason {
                SessionChangeReason::SessionLogon | SessionChangeReason::SessionLogoff => {
                    Event::Restart(p.notification.session_id)
                }
                _ => Event::Wake,
            };
            let _ = tx.send(ev);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    handle.set_service_status(status(ServiceState::Running, 0))?;
    let exit = match supervise(&cfg, &rx) {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("supervisor: {e}");
            1
        }
    };
    handle.set_service_status(status(ServiceState::Stopped, exit))?;
    Ok(())
}

/// Foreground mode (`run --console`): same loop, stopped by killing the process (the job
/// object then takes the agents down too).
pub fn run_console(cfg: SupervisorConfig) -> Result<(), String> {
    let (_tx, rx) = channel();
    supervise(&cfg, &rx)
}

struct Slot {
    backoff: Backoff,
    not_before: Option<Instant>,
}

/// Keeps one agent per desired session alive until `Event::Stop`.
pub fn supervise(cfg: &SupervisorConfig, rx: &Receiver<Event>) -> Result<(), String> {
    let job = launch::kill_on_close_job().map_err(|e| format!("job object: {e}"))?;
    let mut children: BTreeMap<u32, Child> = BTreeMap::new();
    let mut slots: BTreeMap<u32, Slot> = BTreeMap::new();
    tracing::info!(agent = %cfg.agent.display(), include_remote = cfg.include_remote, "supervisor started");
    loop {
        // Reap agents that exited (crash or normal exit) and schedule their relaunch.
        let exited: Vec<u32> = children.iter().filter(|(_, c)| c.exit_code().is_some()).map(|(s, _)| *s).collect();
        for s in exited {
            let c = children.remove(&s).expect("present");
            let code = c.exit_code().unwrap_or(u32::MAX);
            let slot = slots.entry(s).or_insert_with(|| Slot { backoff: Backoff::default(), not_before: None });
            let delay = slot.backoff.next_delay(c.started.elapsed());
            slot.not_before = Some(Instant::now() + delay);
            tracing::warn!(session = c.session, pid = c.pid, code, ?delay, "agent exited; relaunching after delay");
        }

        let remote = if cfg.include_remote { launch::active_sessions() } else { Vec::new() };
        let desired = desired_sessions(launch::console_session(), &remote, cfg.include_remote);
        let running: BTreeSet<u32> = children.keys().copied().collect();
        for action in reconcile(&desired, &running) {
            match action {
                Action::Stop(s) => {
                    if let Some(c) = children.remove(&s) {
                        tracing::info!(session = s, pid = c.pid, "session no longer targeted; stopping agent");
                        c.terminate();
                    }
                    slots.remove(&s);
                }
                Action::Launch(s) => {
                    let slot = slots.entry(s).or_insert_with(|| Slot { backoff: Backoff::default(), not_before: None });
                    if slot.not_before.is_some_and(|t| Instant::now() < t) {
                        continue;
                    }
                    match launch::launch(s, &cfg.agent, &cfg.agent_args, &job) {
                        Ok(c) => {
                            slot.not_before = None;
                            children.insert(s, c);
                        }
                        Err(e) => {
                            let delay = slot.backoff.next_delay(Duration::ZERO);
                            slot.not_before = Some(Instant::now() + delay);
                            tracing::error!(session = s, ?delay, "cannot launch agent: {e}");
                        }
                    }
                }
            }
        }

        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::Stop) => break,
            Ok(Event::Restart(s)) => {
                if let Some(c) = children.remove(&s) {
                    tracing::info!(session = s, "logon/logoff: restarting agent");
                    c.terminate();
                }
                if let Some(slot) = slots.get_mut(&s) {
                    slot.backoff.reset();
                    slot.not_before = None;
                }
            }
            Ok(Event::Wake) | Err(RecvTimeoutError::Timeout) => {}
            // Console mode keeps the sender alive; a disconnected channel means shutdown.
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    for (_, c) in std::mem::take(&mut children) {
        c.terminate();
    }
    drop::<Handle>(job);
    tracing::info!("supervisor stopped");
    Ok(())
}
