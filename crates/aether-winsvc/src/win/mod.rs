//! Windows implementation (service, installer, session launcher).

mod install;
mod launch;
mod service;

use crate::cli::{Command, USAGE};

/// Log to `%ProgramData%\AetherLink\logs\aether-winsvc.log` (services have no console);
/// console runs log to stderr as well.
fn init_logging(console: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AETHERLINK_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if console {
        let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
        return;
    }
    let dir = std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"))
        .join("AetherLink")
        .join("logs");
    let _ = std::fs::create_dir_all(&dir);
    match std::fs::OpenOptions::new().create(true).append(true).open(dir.join("aether-winsvc.log")) {
        Ok(f) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(f))
                .try_init();
        }
        Err(_) => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
    }
}

pub fn main(cmd: Command) -> Result<(), String> {
    match cmd {
        Command::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Command::Install(opts) => install::install(opts),
        Command::Uninstall => install::uninstall(),
        Command::Status => install::status(),
        Command::Run { opts, console } => {
            init_logging(console);
            let cfg = service::SupervisorConfig {
                agent: install::resolve_agent(&opts)?,
                agent_args: opts.agent_args.clone(),
                include_remote: opts.include_remote,
            };
            if console { service::run_console(cfg) } else { service::run_dispatcher(cfg) }
        }
    }
}
