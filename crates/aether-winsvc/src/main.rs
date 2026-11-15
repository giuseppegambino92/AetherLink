//! `aether-winsvc` — the AetherLink Windows service.
//!
//! Runs as `LocalSystem`, watches interactive sessions (`WTSGetActiveConsoleSessionId`,
//! `SERVICE_CONTROL_SESSIONCHANGE`, optionally every active RDP session) and keeps one
//! AetherLink agent per session alive: the agent gets a duplicate of the service's SYSTEM
//! token moved into the target session (`DuplicateTokenEx` + `SetTokenInformation(
//! TokenSessionId)` + `CreateProcessAsUserW`) on `winsta0\default` (user logged on) or
//! `winsta0\winlogon` (logon screen), so it can capture and control the secure desktop
//! (lock screen, Ctrl+Alt+Del, UAC). Crashed agents are relaunched with exponential
//! backoff; all agents live in a kill-on-close job object. See `README.md` for the security
//! model.
//!
//! The binary builds on every OS (the CLI and policy logic are portable and unit-tested),
//! but only does something on Windows.

// The portable CLI/policy code is only driven by the Windows backend (tests run everywhere).
#![cfg_attr(not(windows), allow(dead_code))]

mod cli;
mod policy;
#[cfg(windows)]
mod win;

fn main() {
    let cmd = match cli::parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    #[cfg(windows)]
    {
        if let Err(e) = win::main(cmd) {
            eprintln!("aether-winsvc: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(windows))]
    {
        if cmd == cli::Command::Help {
            print!("{}", cli::USAGE);
            return;
        }
        eprintln!("aether-winsvc only runs on Windows");
        std::process::exit(2);
    }
}
