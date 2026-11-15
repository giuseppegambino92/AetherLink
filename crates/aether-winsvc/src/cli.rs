//! Command-line interface (portable, unit-tested).

use std::path::PathBuf;

pub const USAGE: &str = "\
aether-winsvc — AetherLink Windows service (runs as LocalSystem)

USAGE:
    aether-winsvc install   [OPTIONS] [-- <agent args>...]   register + start the service (admin)
    aether-winsvc uninstall                                  stop + remove the service (admin)
    aether-winsvc status                                     print the service state
    aether-winsvc run       [OPTIONS] [-- <agent args>...]   entry point used by the SCM
    aether-winsvc run --console [OPTIONS]                    run the supervisor in the foreground
                                                             (debugging, e.g. `psexec -s`)

OPTIONS:
    --agent <path>                    agent executable (default: AetherLink.exe next to this binary)
    --include-remote-sessions         also launch an agent into every active RDP session
    --allow-unprotected-agent-path    allow an agent outside %ProgramFiles% / %SystemRoot% (unsafe)
    --no-sas-policy                   install: do not enable SoftwareSASGeneration (Ctrl+Alt+Del)
";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Options {
    pub agent: Option<PathBuf>,
    pub include_remote: bool,
    pub allow_unprotected_agent: bool,
    pub set_sas_policy: bool,
    pub agent_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Install(Options),
    Uninstall,
    Status,
    Run { opts: Options, console: bool },
    Help,
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, String> {
    let mut it = args.into_iter();
    let Some(sub) = it.next() else { return Ok(Command::Help) };
    let mut opts = Options { set_sas_policy: true, ..Options::default() };
    let mut console = false;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--agent" => opts.agent = Some(PathBuf::from(it.next().ok_or("--agent needs a path")?)),
            "--include-remote-sessions" => opts.include_remote = true,
            "--allow-unprotected-agent-path" => opts.allow_unprotected_agent = true,
            "--no-sas-policy" => opts.set_sas_policy = false,
            "--console" => console = true,
            "--" => {
                opts.agent_args = it.by_ref().collect();
                break;
            }
            other => return Err(format!("unknown argument {other:?}\n\n{USAGE}")),
        }
    }
    match sub.as_str() {
        "install" if !console => Ok(Command::Install(opts)),
        "uninstall" | "status" if opts == Options { set_sas_policy: true, ..Options::default() } && !console => {
            Ok(if sub == "uninstall" { Command::Uninstall } else { Command::Status })
        }
        "run" => Ok(Command::Run { opts, console }),
        "help" | "--help" | "-h" => Ok(Command::Help),
        _ => Err(format!("invalid command line\n\n{USAGE}")),
    }
}

impl Options {
    /// Arguments the SCM passes to `aether-winsvc run` (the inverse of [`parse`]).
    pub fn to_run_args(&self) -> Vec<String> {
        let mut v = vec!["run".to_string()];
        if let Some(a) = &self.agent {
            v.push("--agent".into());
            v.push(a.to_string_lossy().into_owned());
        }
        if self.include_remote {
            v.push("--include-remote-sessions".into());
        }
        if self.allow_unprotected_agent {
            v.push("--allow-unprotected-agent-path".into());
        }
        if !self.agent_args.is_empty() {
            v.push("--".into());
            v.extend(self.agent_args.iter().cloned());
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &[&str]) -> Result<Command, String> {
        parse(s.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_commands() {
        assert_eq!(p(&[]).unwrap(), Command::Help);
        assert_eq!(p(&["uninstall"]).unwrap(), Command::Uninstall);
        assert_eq!(p(&["status"]).unwrap(), Command::Status);
        assert!(p(&["uninstall", "--agent", "x"]).is_err());
        assert!(p(&["install", "--bogus"]).is_err());
        assert!(p(&["install", "--agent"]).is_err());
        assert!(p(&["frobnicate"]).is_err());
        let Command::Install(o) = p(&["install", "--agent", r"C:\Program Files\A\a.exe", "--no-sas-policy"]).unwrap()
        else {
            panic!()
        };
        assert_eq!(o.agent.as_deref(), Some(std::path::Path::new(r"C:\Program Files\A\a.exe")));
        assert!(!o.set_sas_policy);
        let Command::Run { opts, console } =
            p(&["run", "--console", "--include-remote-sessions", "--", "--minimized", "--x"]).unwrap()
        else {
            panic!()
        };
        assert!(console && opts.include_remote);
        assert_eq!(opts.agent_args, vec!["--minimized".to_string(), "--x".to_string()]);
    }

    #[test]
    fn run_args_roundtrip() {
        let o = Options {
            agent: Some(PathBuf::from(r"C:\Program Files\AetherLink\AetherLink.exe")),
            include_remote: true,
            allow_unprotected_agent: false,
            set_sas_policy: true,
            agent_args: vec!["--service-mode".into(), "a b".into()],
        };
        let args = o.to_run_args();
        assert_eq!(args[0], "run");
        assert_eq!(parse(args).unwrap(), Command::Run { opts: o, console: false });
    }
}
