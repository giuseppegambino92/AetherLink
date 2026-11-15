//! Best guess of the directory the user is currently working in, used as the
//! starting cwd of the drop-down ("Quake") terminal.
//!
//! macOS strategy, by frontmost application:
//! * **Finder** — AppleScript: POSIX path of the front Finder window's target
//!   (the desktop when no window is open).
//! * **Terminal / iTerm2** — the window's `AXDocument` (Terminal publishes the
//!   shell cwd there via OSC 7); otherwise the selected tab's tty (AppleScript)
//!   → foreground shell on that tty (`ps`) → its cwd (`lsof -d cwd`). iTerm2
//!   also answers `session.path` directly.
//! * **IDEs** (VS Code & forks, Xcode, JetBrains, Zed) and any other app —
//!   the focused window's `AXDocument` file URL → enclosing git work tree,
//!   else the file's directory; then window-title heuristics (JetBrains shows
//!   `[~/path]`, many editors show absolute paths).
//! * Fallback: `$HOME`.
//!
//! AppleScript calls trigger the macOS *Automation* consent prompt the first
//! time per target app; AX reads need the Accessibility permission (skipped
//! when not granted).
//!
//! Windows: Explorer's current folder (Shell windows), the Desktop folder,
//! terminal shells' current directory and title paths (see
//! `windows_context.rs`). Other OSes return `$HOME` / `%USERPROFILE%`.

use std::path::{Path, PathBuf};

/// Frontmost-app families with a dedicated strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppKind {
    Finder,
    Terminal,
    ITerm,
    /// Other terminal emulators (Ghostty, WezTerm, Alacritty, kitty, Warp).
    OtherTerminal,
    VsCode,
    Xcode,
    JetBrains,
    Zed,
    Other,
}

/// The `.app` bundle name ("Visual Studio Code") contained in an executable path.
pub fn bundle_name(exe: &str) -> Option<&str> {
    let end = exe.find(".app/").or_else(|| exe.strip_suffix(".app").map(|s| s.len()))?;
    let start = exe[..end].rfind('/').map_or(0, |i| i + 1);
    Some(&exe[start..end])
}

/// Classify an application by its executable path.
pub fn classify_app(exe: &str) -> AppKind {
    let Some(name) = bundle_name(exe) else { return AppKind::Other };
    const JETBRAINS: &[&str] = &[
        "IntelliJ IDEA", "PyCharm", "WebStorm", "CLion", "GoLand", "RustRover", "PhpStorm", "Rider", "RubyMine",
        "DataGrip", "DataSpell", "Fleet", "Android Studio", "Aqua", "Writerside",
    ];
    match name {
        "Finder" => AppKind::Finder,
        "Terminal" => AppKind::Terminal,
        "iTerm" | "iTerm2" => AppKind::ITerm,
        "Ghostty" | "WezTerm" | "Alacritty" | "kitty" | "Warp" | "Hyper" | "Tabby" => AppKind::OtherTerminal,
        "Visual Studio Code" | "Visual Studio Code - Insiders" | "Code" | "Code - Insiders" | "VSCodium"
        | "Cursor" | "Windsurf" => AppKind::VsCode,
        "Xcode" | "Xcode-beta" => AppKind::Xcode,
        "Zed" | "Zed Preview" | "Zed Nightly" => AppKind::Zed,
        n if JETBRAINS.iter().any(|j| n.starts_with(j)) => AppKind::JetBrains,
        _ => AppKind::Other,
    }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `file:///Users/me/My%20Dir/x.rs` → `/Users/me/My Dir/x.rs`.
pub fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let b = rest.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    let s = String::from_utf8(out).ok()?;
    let p = PathBuf::from(s);
    // Trailing slash (directory URL) is normalised away by PathBuf components.
    Some(p.components().collect())
}

/// Working directory for a document: itself when it is a directory, otherwise
/// the enclosing git work tree (stopping at `stop`), otherwise its parent.
pub fn dir_for_document(doc: &Path, is_dir: &dyn Fn(&Path) -> bool, stop: Option<&Path>) -> Option<PathBuf> {
    if is_dir(doc) {
        return Some(doc.to_path_buf());
    }
    let parent = doc.parent()?.to_path_buf();
    let mut cur = Some(parent.as_path());
    while let Some(d) = cur {
        if stop.is_some_and(|s| d == s) || d.parent().is_none() {
            break;
        }
        if is_dir(&d.join(".git")) || d.join(".git").is_file() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    Some(parent)
}

/// Expand a leading `~` / `~/`.
pub fn expand_tilde(s: &str, home: &Path) -> PathBuf {
    if s == "~" {
        home.to_path_buf()
    } else if let Some(r) = s.strip_prefix("~/") {
        home.join(r)
    } else {
        PathBuf::from(s)
    }
}

/// Candidate paths found in a window title, most specific first:
/// `[~/dev/app]` (JetBrains), and title segments separated by ` — `, ` – `,
/// ` - ` that look like absolute or `~` paths.
pub fn paths_from_title(title: &str, home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut rest = title;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find(']') else { break };
        let inner = rest[open + 1..open + close].trim();
        if inner.starts_with('/') || inner.starts_with('~') {
            out.push(expand_tilde(inner, home));
        }
        rest = &rest[open + close + 1..];
    }
    let mut segs: Vec<&str> = vec![title];
    for sep in [" — ", " – ", " - ", " : "] {
        segs = segs.into_iter().flat_map(|s| s.split(sep)).collect();
    }
    for s in segs {
        let s = s.trim().trim_matches(|c| c == '[' || c == ']');
        if (s.starts_with('/') || s.starts_with("~/") || s == "~") && !s.contains('\n') {
            let p = expand_tilde(s, home);
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// Parse `lsof -a -p PID -d cwd -Fn` output.
pub fn parse_lsof_cwd(out: &str) -> Option<PathBuf> {
    out.lines().find_map(|l| l.strip_prefix('n')).filter(|p| p.starts_with('/')).map(PathBuf::from)
}

/// Pick the process whose cwd represents a terminal tab from
/// `ps -t TTY -o pid=,stat=,comm=` output: prefer the foreground (`+`) shell,
/// else any foreground process, else the first shell.
pub fn pick_tty_process(ps: &str) -> Option<u32> {
    const SHELLS: &[&str] = &["zsh", "bash", "fish", "sh", "nu", "tcsh", "ksh", "xonsh", "pwsh"];
    let rows: Vec<(u32, String, String)> = ps
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let stat = it.next()?.to_string();
            let comm = it.collect::<Vec<_>>().join(" ");
            let comm = comm.rsplit('/').next().unwrap_or("").trim_start_matches('-').to_string();
            Some((pid, stat, comm))
        })
        .collect();
    let is_shell = |c: &str| SHELLS.contains(&c);
    let fg = |st: &str| st.contains('+');
    rows.iter()
        .rev()
        .find(|(_, st, c)| fg(st) && is_shell(c))
        .or_else(|| rows.iter().rev().find(|(_, st, _)| fg(st)))
        .or_else(|| rows.iter().find(|(_, _, c)| is_shell(c)))
        .map(|r| r.0)
}

/// Normalise a tty name from AppleScript (`/dev/ttys003`) for `ps -t`.
pub fn tty_for_ps(tty: &str) -> Option<String> {
    let t = tty.trim().trim_start_matches("/dev/");
    (!t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric())).then(|| t.to_string())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// Directory the user is most likely working in (see module docs).
pub fn active_working_directory() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        mac::detect().or_else(home_dir)
    }
    #[cfg(target_os = "windows")]
    {
        crate::windows_context::detect().or_else(home_dir)
    }
    #[cfg(target_os = "linux")]
    {
        linux::detect().or_else(home_dir)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        home_dir()
    }
}

/// Linux strategy (terminal shell cwd via `/proc`, file-manager / IDE title
/// heuristics); the pure helpers compile on every OS for testing.
#[path = "linux_context.rs"]
pub mod linux;

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use crate::macos::{focused_window_strings, frontmost_pid, pid_executable};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Run a command with a timeout, returning trimmed stdout on success.
    pub(super) fn run(cmd: &str, args: &[&str], timeout: Duration) -> Option<String> {
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(st)) if st.success() => break,
                Ok(Some(_)) => return None,
                Ok(None) if start.elapsed() > timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(15)),
                Err(_) => return None,
            }
        }
        let out = child.wait_with_output().ok()?;
        let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    fn osascript(script: &str) -> Option<String> {
        run("/usr/bin/osascript", &["-e", script], Duration::from_secs(3))
    }

    fn existing_dir(p: PathBuf) -> Option<PathBuf> {
        p.is_dir().then_some(p)
    }

    fn cwd_of_pid(pid: u32) -> Option<PathBuf> {
        let out = run("/usr/sbin/lsof", &["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"], Duration::from_secs(2))?;
        parse_lsof_cwd(&out).and_then(existing_dir)
    }

    fn cwd_of_tty(tty: &str) -> Option<PathBuf> {
        let tty = tty_for_ps(tty)?;
        let ps = run("/bin/ps", &["-t", &tty, "-o", "pid=,stat=,comm="], Duration::from_secs(2))?;
        cwd_of_pid(pick_tty_process(&ps)?)
    }

    fn from_document(pid: u32, home: &Path) -> Option<PathBuf> {
        let (doc, title) = focused_window_strings(pid);
        if let Some(p) = doc.as_deref().and_then(file_url_to_path)
            && let Some(d) = dir_for_document(&p, &|q: &Path| q.is_dir(), Some(home)).and_then(existing_dir)
        {
            return Some(d);
        }
        let title = title?;
        paths_from_title(&title, home).into_iter().find_map(|p| {
            if p.is_dir() {
                Some(p)
            } else if p.is_file() {
                p.parent().map(Path::to_path_buf)
            } else {
                None
            }
        })
    }

    pub(super) fn detect() -> Option<PathBuf> {
        let home = home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let pid = frontmost_pid()?;
        let kind = pid_executable(pid).map(|e| classify_app(&e)).unwrap_or(AppKind::Other);
        match kind {
            AppKind::Finder => osascript(
                "tell application \"Finder\"\n\
                 if (count of Finder windows) > 0 then\n\
                 get POSIX path of (target of front Finder window as alias)\n\
                 else\n\
                 get POSIX path of (desktop as alias)\n\
                 end if\n\
                 end tell",
            )
            .map(PathBuf::from)
            .and_then(existing_dir),
            AppKind::Terminal => from_document(pid, &home).or_else(|| {
                osascript("tell application \"Terminal\" to get tty of selected tab of front window")
                    .and_then(|t| cwd_of_tty(&t))
            }),
            AppKind::ITerm => osascript(
                "tell application \"iTerm2\" to tell current session of current window to get variable named \"session.path\"",
            )
            .map(PathBuf::from)
            .and_then(existing_dir)
            .or_else(|| {
                osascript("tell application \"iTerm2\" to get tty of current session of current window")
                    .and_then(|t| cwd_of_tty(&t))
            })
            .or_else(|| from_document(pid, &home)),
            _ => from_document(pid, &home),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify() {
        assert_eq!(classify_app("/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder"), AppKind::Finder);
        assert_eq!(
            classify_app("/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal"),
            AppKind::Terminal
        );
        assert_eq!(classify_app("/Applications/iTerm.app/Contents/MacOS/iTerm2"), AppKind::ITerm);
        assert_eq!(
            classify_app("/Applications/Visual Studio Code.app/Contents/MacOS/Electron"),
            AppKind::VsCode
        );
        assert_eq!(classify_app("/Applications/Cursor.app/Contents/MacOS/Cursor"), AppKind::VsCode);
        assert_eq!(classify_app("/Applications/Xcode.app/Contents/MacOS/Xcode"), AppKind::Xcode);
        assert_eq!(
            classify_app("/Users/me/Applications/IntelliJ IDEA Ultimate.app/Contents/MacOS/idea"),
            AppKind::JetBrains
        );
        assert_eq!(classify_app("/Applications/RustRover.app/Contents/MacOS/rustrover"), AppKind::JetBrains);
        assert_eq!(classify_app("/Applications/Zed.app/Contents/MacOS/zed"), AppKind::Zed);
        assert_eq!(classify_app("/Applications/Ghostty.app/Contents/MacOS/ghostty"), AppKind::OtherTerminal);
        assert_eq!(classify_app("/usr/bin/vim"), AppKind::Other);
        assert_eq!(bundle_name("/Applications/Safari.app"), Some("Safari"));
    }

    #[test]
    fn file_urls() {
        assert_eq!(
            file_url_to_path("file:///Users/me/My%20Dir/perch%C3%A9.rs"),
            Some(PathBuf::from("/Users/me/My Dir/perché.rs"))
        );
        assert_eq!(file_url_to_path("file://localhost/tmp/"), Some(PathBuf::from("/tmp")));
        assert_eq!(file_url_to_path("file:///a/b%2"), Some(PathBuf::from("/a/b%2")));
        assert_eq!(file_url_to_path("https://example.com/x"), None);
        assert_eq!(file_url_to_path("file://server/share"), None);
    }

    #[test]
    fn document_dir_prefers_git_root() {
        let dirs = ["/u/me/proj", "/u/me/proj/.git", "/u/me/proj/src", "/u/me/other"];
        let is_dir = |p: &Path| dirs.iter().any(|d| Path::new(d) == p);
        let home = Path::new("/u/me");
        assert_eq!(
            dir_for_document(Path::new("/u/me/proj/src/main.rs"), &is_dir, Some(home)),
            Some(PathBuf::from("/u/me/proj"))
        );
        assert_eq!(
            dir_for_document(Path::new("/u/me/other/notes.txt"), &is_dir, Some(home)),
            Some(PathBuf::from("/u/me/other"))
        );
        assert_eq!(
            dir_for_document(Path::new("/u/me/proj/src"), &is_dir, Some(home)),
            Some(PathBuf::from("/u/me/proj/src"))
        );
    }

    #[test]
    fn title_heuristics() {
        let home = Path::new("/Users/me");
        assert_eq!(
            paths_from_title("aetherlink [~/dev/aetherlink] – lib.rs", home),
            vec![PathBuf::from("/Users/me/dev/aetherlink")]
        );
        assert_eq!(
            paths_from_title("main.rs — /Users/me/dev/x — Zed", home),
            vec![PathBuf::from("/Users/me/dev/x")]
        );
        assert_eq!(paths_from_title("~/notes - vim", home), vec![PathBuf::from("/Users/me/notes")]);
        assert!(paths_from_title("lib.rs — aetherlink — Visual Studio Code", home).is_empty());
    }

    #[test]
    fn lsof_and_ps_parsing() {
        assert_eq!(parse_lsof_cwd("p123\nfcwd\nn/Users/me/dev\n"), Some(PathBuf::from("/Users/me/dev")));
        assert_eq!(parse_lsof_cwd("p123\n"), None);
        let ps = "  501 Ss   login\n  502 S    -zsh\n  900 S+   /usr/bin/vim\n";
        assert_eq!(pick_tty_process(ps), Some(900));
        let ps = "  501 Ss   login\n  502 S+   -zsh\n";
        assert_eq!(pick_tty_process(ps), Some(502));
        let ps = "  501 Ss   login\n  502 S    -zsh\n";
        assert_eq!(pick_tty_process(ps), Some(502));
        assert_eq!(pick_tty_process(""), None);
        assert_eq!(tty_for_ps("/dev/ttys003\n"), Some("ttys003".into()));
        assert_eq!(tty_for_ps("; rm -rf /"), None);
    }

    #[test]
    fn tilde() {
        let h = Path::new("/h");
        assert_eq!(expand_tilde("~", h), PathBuf::from("/h"));
        assert_eq!(expand_tilde("~/a", h), PathBuf::from("/h/a"));
        assert_eq!(expand_tilde("/x", h), PathBuf::from("/x"));
    }

    #[test]
    #[ignore = "reads the live frontmost app (may run osascript → Automation prompt)"]
    fn live_cwd() {
        let d = active_working_directory().expect("at least $HOME");
        assert!(d.is_dir());
    }
}
