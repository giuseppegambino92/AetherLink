//! Portable supervisor logic (unit-tested on every OS): restart backoff, session
//! reconciliation, Windows command-line quoting and the agent-path safety check.

use std::collections::BTreeSet;
use std::time::Duration;

/// Exponential restart backoff: 1 s, 2 s, 4 s … capped at `max`; a child that ran for at
/// least `stable_after` resets the sequence.
#[derive(Debug, Clone)]
pub struct Backoff {
    attempt: u32,
    pub base: Duration,
    pub max: Duration,
    pub stable_after: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { attempt: 0, base: Duration::from_secs(1), max: Duration::from_secs(60), stable_after: Duration::from_secs(60) }
    }
}

impl Backoff {
    /// Delay before relaunching a child that exited after running for `ran_for`.
    pub fn next_delay(&mut self, ran_for: Duration) -> Duration {
        if ran_for >= self.stable_after {
            self.attempt = 0;
        }
        let d = self.base.saturating_mul(1u32 << self.attempt.min(16));
        self.attempt = self.attempt.saturating_add(1);
        d.min(self.max)
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// What the supervisor must do to reach the desired set of sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    /// Terminate the agent in this session (session gone / no longer targeted).
    Stop(u32),
    /// Launch an agent into this session.
    Launch(u32),
}

/// `0xFFFF_FFFF` from `WTSGetActiveConsoleSessionId` means "no console session" (switching).
pub const NO_SESSION: u32 = 0xFFFF_FFFF;

/// Sessions that should run an agent: the console session (also while nobody is logged on,
/// so the logon screen is reachable) and, optionally, every active remote session.
/// Session 0 (services) never gets one.
pub fn desired_sessions(console: u32, active_remote: &[u32], include_remote: bool) -> BTreeSet<u32> {
    let mut s = BTreeSet::new();
    if console != NO_SESSION && console != 0 {
        s.insert(console);
    }
    if include_remote {
        s.extend(active_remote.iter().copied().filter(|&id| id != 0 && id != NO_SESSION));
    }
    s
}

/// Stops first (sorted), then launches.
pub fn reconcile(desired: &BTreeSet<u32>, running: &BTreeSet<u32>) -> Vec<Action> {
    let mut v: Vec<Action> = running.difference(desired).map(|&s| Action::Stop(s)).collect();
    v.extend(desired.difference(running).map(|&s| Action::Launch(s)));
    v
}

/// Desktop the agent starts on: the user's desktop when someone is logged on, otherwise
/// the logon (secure) desktop. The agent follows desktop switches by itself afterwards.
pub fn desktop_for(user_logged_on: bool) -> &'static str {
    if user_logged_on { "winsta0\\default" } else { "winsta0\\winlogon" }
}

/// Quotes one argument per the `CommandLineToArgvW` / MSVC CRT rules.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Full command line: quoted program followed by quoted arguments.
pub fn build_command_line(program: &str, args: &[String]) -> String {
    let mut s = quote_arg(program);
    for a in args {
        s.push(' ');
        s.push_str(&quote_arg(a));
    }
    s
}

fn normalize_windows_path(p: &str) -> Option<String> {
    let p = p.replace('/', "\\");
    // Reject relative, UNC, device and parent-traversal paths outright.
    let b = p.as_bytes();
    let absolute_drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
    if !absolute_drive || p.split('\\').any(|c| c == "..") {
        return None;
    }
    Some(p.trim_end_matches('\\').to_ascii_lowercase())
}

/// True when `agent` lies below one of `protected_roots` (e.g. `%ProgramFiles%`,
/// `%ProgramFiles(x86)%`, `%SystemRoot%`) — directories only administrators can write, so a
/// standard user cannot swap the binary the SYSTEM service launches. Case-insensitive.
pub fn agent_path_is_protected(agent: &str, protected_roots: &[String]) -> bool {
    let Some(agent) = normalize_windows_path(agent) else { return false };
    protected_roots.iter().filter_map(|r| normalize_windows_path(r)).any(|root| {
        agent.len() > root.len() + 1 && agent.starts_with(&root) && agent.as_bytes()[root.len()] == b'\\'
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_caps_and_resets() {
        let mut b = Backoff::default();
        let short = Duration::from_secs(1);
        let delays: Vec<u64> = (0..8).map(|_| b.next_delay(short).as_secs()).collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(b.next_delay(Duration::from_secs(120)), Duration::from_secs(1));
        assert_eq!(b.next_delay(short), Duration::from_secs(2));
        b.reset();
        assert_eq!(b.next_delay(short), Duration::from_secs(1));
        for _ in 0..100 {
            b.next_delay(short);
        }
        assert_eq!(b.next_delay(short), Duration::from_secs(60));
    }

    #[test]
    fn sessions_and_reconcile() {
        assert_eq!(desired_sessions(1, &[2, 3], false), BTreeSet::from([1]));
        assert_eq!(desired_sessions(1, &[0, 2, 3], true), BTreeSet::from([1, 2, 3]));
        assert!(desired_sessions(NO_SESSION, &[], true).is_empty());
        assert!(desired_sessions(0, &[], false).is_empty());
        let desired = BTreeSet::from([2, 3]);
        let running = BTreeSet::from([1, 2]);
        assert_eq!(reconcile(&desired, &running), vec![Action::Stop(1), Action::Launch(3)]);
        assert!(reconcile(&desired, &desired).is_empty());
        assert_eq!(desktop_for(true), "winsta0\\default");
        assert_eq!(desktop_for(false), "winsta0\\winlogon");
    }

    #[test]
    fn windows_quoting() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r"C:\Program Files\"), r#""C:\Program Files\\""#);
        assert_eq!(quote_arg(r"a\\b c"), r#""a\\b c""#);
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(
            build_command_line(r"C:\Program Files\AetherLink\AetherLink.exe", &["--agent".into(), "x y".into()]),
            r#""C:\Program Files\AetherLink\AetherLink.exe" --agent "x y""#
        );
    }

    #[test]
    fn agent_path_protection() {
        let roots = vec![r"C:\Program Files".to_string(), r"C:\Windows\".to_string()];
        assert!(agent_path_is_protected(r"C:\Program Files\AetherLink\AetherLink.exe", &roots));
        assert!(agent_path_is_protected(r"c:/program files/aetherlink/a.exe", &roots));
        assert!(!agent_path_is_protected(r"C:\Program Files (x86)\x.exe", &roots));
        assert!(!agent_path_is_protected(r"C:\Program FilesEvil\x.exe", &roots));
        assert!(!agent_path_is_protected(r"C:\Users\me\Downloads\AetherLink.exe", &roots));
        assert!(!agent_path_is_protected(r"C:\Program Files\..\Users\me\x.exe", &roots));
        assert!(!agent_path_is_protected(r"AetherLink.exe", &roots));
        assert!(!agent_path_is_protected(r"\\server\share\Program Files\x.exe", &roots));
        assert!(!agent_path_is_protected(r"C:\Program Files", &roots));
    }
}
