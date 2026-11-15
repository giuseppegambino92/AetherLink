//! Linux strategy for [`super::active_working_directory`].
//!
//! Focused window = X11 `_NET_ACTIVE_WINDOW` → `_NET_WM_PID` + `_NET_WM_NAME`
//! when `$DISPLAY` is set and the active window is an X11 client, else the
//! AT-SPI active frame (pid via the a11y bus). Then, by process:
//! * **Terminals** — a path in the window title (`user@host: ~/dir`, shells'
//!   OSC 0/7 titles), else the cwd of the most recently started shell child
//!   of the terminal process (`/proc/<pid>/stat` ppid walk → `/proc/<pid>/cwd`).
//! * **File managers** (Nautilus, Dolphin, Thunar, Nemo, PCManFM, Caja…) —
//!   title heuristics: full paths, else the folder name resolved against
//!   `$HOME` and the XDG user dirs.
//! * **IDEs** (VS Code & forks, JetBrains, Zed, Sublime…) and other apps —
//!   paths in the title (`[~/dev/app]`, `file (~/dir) - gedit`), IDEs then
//!   fall back to the process cwd.
//!
//! The parsing / classification helpers are pure and compiled on every OS.

use super::{dir_for_document, expand_tilde, paths_from_title};
use std::path::{Path, PathBuf};

/// Focused-application families with a dedicated strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxAppKind {
    Terminal,
    FileManager,
    Ide,
    Other,
}

const TERMINALS: &[&str] = &[
    "gnome-terminal-server", "gnome-terminal", "gnome-terminal-", "kgx", "gnome-console", "ptyxis", "ptyxis-agent",
    "konsole", "xterm", "uxterm", "alacritty", "kitty", "wezterm-gui", "wezterm", "foot", "footclient", "tilix",
    "xfce4-terminal", "terminator", "ghostty", "lxterminal", "mate-terminal", "qterminal", "terminology", "urxvt",
    "rxvt", "st", "sakura", "guake", "tilda", "yakuake", "blackbox", "cool-retro-term", "contour", "rio", "warp",
    "deepin-terminal", "io.elementary.terminal", "kmscon", "wave",
];
const FILE_MANAGERS: &[&str] = &[
    "nautilus", "dolphin", "thunar", "nemo", "pcmanfm", "pcmanfm-qt", "caja", "spacefm", "krusader",
    "io.elementary.files", "peony", "dde-file-manager", "konqueror",
];
const IDES: &[&str] = &[
    "code", "code-oss", "code-insiders", "codium", "vscodium", "cursor", "windsurf", "zed", "zed-editor", "zeditor",
    "idea", "pycharm", "webstorm", "clion", "goland", "rustrover", "phpstorm", "rider", "rubymine", "datagrip",
    "dataspell", "fleet", "android-studio", "studio", "gnome-builder", "sublime_text", "subl", "kate", "kdevelop",
    "lapce", "helix", "emacs", "gvim", "neovide",
];
const SHELLS: &[&str] =
    &["bash", "zsh", "fish", "sh", "dash", "nu", "tcsh", "csh", "ksh", "mksh", "xonsh", "pwsh", "elvish", "oil", "ion"];

fn base(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s).trim_start_matches('-')
}

/// Classify a process by any of its names (exe path, `comm`, argv[0],
/// argv[1] — interpreters like `python3 /usr/bin/terminator`).
pub fn classify_process(names: &[&str]) -> LinuxAppKind {
    let names: Vec<String> = names.iter().map(|n| base(n.trim()).to_lowercase()).filter(|n| !n.is_empty()).collect();
    let any = |list: &[&str]| names.iter().any(|n| list.contains(&n.as_str()));
    if any(TERMINALS) {
        LinuxAppKind::Terminal
    } else if any(FILE_MANAGERS) {
        LinuxAppKind::FileManager
    } else if any(IDES) || names.iter().any(|n| n.starts_with("jetbrains") || n.starts_with("intellij")) {
        LinuxAppKind::Ide
    } else {
        LinuxAppKind::Other
    }
}

pub fn is_shell(comm: &str) -> bool {
    SHELLS.contains(&base(comm))
}

/// The fields of `/proc/<pid>/stat` we use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: u32,
    pub comm: String,
    pub ppid: u32,
    pub pgrp: i32,
    pub tty_nr: i32,
    /// Foreground process group of the controlling terminal.
    pub tpgid: i32,
    /// Clock ticks since boot.
    pub starttime: u64,
}

/// Parse `/proc/<pid>/stat` (`comm` may contain spaces and parentheses).
pub fn parse_proc_stat(s: &str) -> Option<ProcStat> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let pid = s[..open].trim().parse().ok()?;
    let comm = s.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = s[close + 1..].split_whitespace().collect();
    // rest[0] = state (field 3) … rest[19] = starttime (field 22).
    Some(ProcStat {
        pid,
        comm,
        ppid: rest.get(1)?.parse().ok()?,
        pgrp: rest.get(2)?.parse().ok()?,
        tty_nr: rest.get(4)?.parse().ok()?,
        tpgid: rest.get(5)?.parse().ok()?,
        starttime: rest.get(19)?.parse().ok()?,
    })
}

/// The shell whose cwd represents the terminal `terminal_pid`: shells among
/// its children (else grandchildren, e.g. behind a mux server), preferring
/// ones with a controlling tty, then the most recently started.
pub fn pick_shell(procs: &[ProcStat], terminal_pid: u32) -> Option<u32> {
    let children: Vec<&ProcStat> = procs.iter().filter(|p| p.ppid == terminal_pid).collect();
    let best = |c: &[&ProcStat]| {
        c.iter().filter(|p| is_shell(&p.comm)).max_by_key(|p| (p.tty_nr != 0, p.starttime, p.pid)).map(|p| p.pid)
    };
    best(&children).or_else(|| {
        let grand: Vec<&ProcStat> =
            procs.iter().filter(|p| children.iter().any(|c| c.pid == p.ppid)).collect();
        best(&grand)
    })
}

fn push_unique(out: &mut Vec<PathBuf>, p: PathBuf) {
    if !out.contains(&p) {
        out.push(p);
    }
}

fn looks_like_path(s: &str) -> bool {
    (s.starts_with('/') || s.starts_with("~/") || s == "~") && !s.contains('\n')
}

/// Candidate paths in a Linux window title, most specific first:
/// [`paths_from_title`] plus `user@host: ~/dir` (bash/zsh titles),
/// `file (~/dir) - gedit` and a whole-title path.
pub fn title_paths(title: &str, home: &Path) -> Vec<PathBuf> {
    let mut out = paths_from_title(title, home);
    let mut rest = title;
    while let Some(i) = rest.find(": ") {
        let tail = rest[i + 2..].trim();
        let cand = tail.split([' ', '\t']).next().unwrap_or("");
        if looks_like_path(tail) {
            push_unique(&mut out, expand_tilde(tail, home));
        } else if looks_like_path(cand) {
            push_unique(&mut out, expand_tilde(cand, home));
        }
        rest = &rest[i + 2..];
    }
    let mut rest = title;
    while let Some(open) = rest.find('(') {
        let Some(close) = rest[open..].find(')') else { break };
        let inner = rest[open + 1..open + close].trim();
        if looks_like_path(inner) {
            push_unique(&mut out, expand_tilde(inner, home));
        }
        rest = &rest[open + close + 1..];
    }
    let whole = title.trim();
    if looks_like_path(whole) {
        push_unique(&mut out, expand_tilde(whole, home));
    }
    out
}

/// `~/.config/user-dirs.dirs` → the XDG user directories.
pub fn parse_user_dirs(content: &str, home: &Path) -> Vec<PathBuf> {
    content
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            if !(k.starts_with("XDG_") && k.ends_with("_DIR")) {
                return None;
            }
            let v = v.trim().trim_matches('"');
            if let Some(r) = v.strip_prefix("$HOME") {
                let r = r.trim_start_matches('/');
                Some(if r.is_empty() { home.to_path_buf() } else { home.join(r) })
            } else {
                v.starts_with('/').then(|| PathBuf::from(v))
            }
        })
        .collect()
}

/// Candidate directories for a file-manager window title: explicit paths,
/// then the folder name (`Downloads — Dolphin`, `Documents - Thunar`) as
/// home / root / an XDG user dir / `~/<name>`.
pub fn file_manager_candidates(title: &str, home: &Path, user_dirs: &[PathBuf], user: Option<&str>) -> Vec<PathBuf> {
    let mut out = title_paths(title, home);
    let name = [" — ", " – ", " - "].iter().fold(title.trim(), |s, sep| s.split(*sep).next().unwrap_or(s)).trim();
    if name.is_empty() {
        return out;
    }
    match name {
        "Home" | "Home Folder" | "Personal Folder" => push_unique(&mut out, home.to_path_buf()),
        "File System" | "Filesystem" | "Computer" | "Other Locations" => push_unique(&mut out, PathBuf::from("/")),
        n if user == Some(n) => push_unique(&mut out, home.to_path_buf()),
        _ => {}
    }
    for d in user_dirs {
        if d.file_name().is_some_and(|f| f == name) {
            push_unique(&mut out, d.clone());
        }
    }
    if !name.contains('/') {
        push_unique(&mut out, home.join(name));
    }
    out
}

/// First candidate that is a directory (a file → its project dir / parent).
pub fn first_dir(cands: &[PathBuf], is_dir: &dyn Fn(&Path) -> bool, is_file: &dyn Fn(&Path) -> bool, home: &Path) -> Option<PathBuf> {
    cands.iter().find_map(|p| {
        if is_dir(p) {
            Some(p.clone())
        } else if is_file(p) {
            dir_for_document(p, is_dir, Some(home))
        } else {
            None
        }
    })
}

#[cfg(target_os = "linux")]
pub use imp::{detect, shell_cwd_under};

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::time::Duration;

    /// `(pid, title)` of the X11 active window (EWMH), if any.
    fn x11_active_window() -> Option<(u32, String)> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
        std::env::var_os("DISPLAY")?;
        let (conn, screen) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        let atom = |n: &[u8]| conn.intern_atom(false, n).ok()?.reply().ok().map(|r| r.atom);
        let (active, wm_pid, wm_name, utf8) =
            (atom(b"_NET_ACTIVE_WINDOW")?, atom(b"_NET_WM_PID")?, atom(b"_NET_WM_NAME")?, atom(b"UTF8_STRING")?);
        let prop = |w: u32, a: u32, ty: u32, len: u32| conn.get_property(false, w, a, ty, 0, len).ok()?.reply().ok();
        let win = prop(root, active, AtomEnum::WINDOW.into(), 1)?.value32()?.next()?;
        if win == 0 {
            return None;
        }
        let pid = prop(win, wm_pid, AtomEnum::CARDINAL.into(), 1)?.value32()?.next()?;
        let title = prop(win, wm_name, utf8, 1024)
            .filter(|r| !r.value.is_empty())
            .or_else(|| prop(win, AtomEnum::WM_NAME.into(), AtomEnum::STRING.into(), 1024))
            .map(|r| String::from_utf8_lossy(&r.value).into_owned())
            .unwrap_or_default();
        (pid > 0).then_some((pid, title))
    }

    fn process_names(pid: u32) -> Vec<String> {
        let mut names = Vec::new();
        if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
            names.push(exe.to_string_lossy().trim_end_matches(" (deleted)").to_string());
        }
        if let Ok(c) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            names.push(c.trim().to_string());
        }
        if let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) {
            names.extend(cmd.split(|b| *b == 0).take(2).map(|a| String::from_utf8_lossy(a).into_owned()));
        }
        names
    }

    fn all_procs() -> Vec<ProcStat> {
        let Ok(rd) = std::fs::read_dir("/proc") else { return Vec::new() };
        rd.filter_map(|e| {
            let e = e.ok()?;
            e.file_name().to_str()?.parse::<u32>().ok()?;
            parse_proc_stat(&std::fs::read_to_string(e.path().join("stat")).ok()?)
        })
        .collect()
    }

    fn cwd_of(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok().filter(|p| p.is_dir())
    }

    /// Cwd of the shell representing terminal `pid` (see [`pick_shell`]).
    pub fn shell_cwd_under(pid: u32) -> Option<PathBuf> {
        cwd_of(pick_shell(&all_procs(), pid)?)
    }

    pub fn detect() -> Option<PathBuf> {
        let home = super::super::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let (pid, title) = x11_active_window()
            .or_else(|| crate::linux::active_window(Duration::from_millis(1500)).map(|w| (w.pid, w.title)))?;
        let names = process_names(pid);
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let is_dir = |p: &Path| p.is_dir();
        let is_file = |p: &Path| p.is_file();
        let from_title = |cands: &[PathBuf]| first_dir(cands, &is_dir, &is_file, &home);
        match classify_process(&refs) {
            LinuxAppKind::Terminal => from_title(&title_paths(&title, &home)).or_else(|| shell_cwd_under(pid)),
            LinuxAppKind::FileManager => {
                let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
                let dirs = std::fs::read_to_string(xdg.join("user-dirs.dirs"))
                    .map(|c| parse_user_dirs(&c, &home))
                    .unwrap_or_default();
                let user = std::env::var("USER").ok();
                from_title(&file_manager_candidates(&title, &home, &dirs, user.as_deref()))
            }
            LinuxAppKind::Ide => {
                from_title(&title_paths(&title, &home)).or_else(|| cwd_of(pid).filter(|p| p != Path::new("/")))
            }
            LinuxAppKind::Other => from_title(&title_paths(&title, &home)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify() {
        assert_eq!(classify_process(&["/usr/libexec/gnome-terminal-server", "gnome-terminal-"]), LinuxAppKind::Terminal);
        assert_eq!(classify_process(&["/usr/bin/python3.12", "python3", "/usr/bin/python3", "/usr/bin/terminator"]), LinuxAppKind::Terminal);
        assert_eq!(classify_process(&["/usr/bin/kitty"]), LinuxAppKind::Terminal);
        assert_eq!(classify_process(&["/usr/bin/wezterm-gui"]), LinuxAppKind::Terminal);
        assert_eq!(classify_process(&["/usr/bin/nautilus"]), LinuxAppKind::FileManager);
        assert_eq!(classify_process(&["/usr/bin/dolphin"]), LinuxAppKind::FileManager);
        assert_eq!(classify_process(&["/usr/share/code/code", "code"]), LinuxAppKind::Ide);
        assert_eq!(classify_process(&["/opt/jetbrains-toolbox/apps/idea/bin/idea"]), LinuxAppKind::Ide);
        assert_eq!(classify_process(&["/usr/bin/zeditor"]), LinuxAppKind::Ide);
        assert_eq!(classify_process(&["/usr/bin/gedit"]), LinuxAppKind::Other);
        assert_eq!(classify_process(&[]), LinuxAppKind::Other);
        assert!(is_shell("-bash") && is_shell("/usr/bin/zsh") && !is_shell("vim"));
    }

    #[test]
    fn proc_stat() {
        let s = "4242 (my (weird) sh) S 100 4242 4242 34817 4300 4194304 1 2 3 4 5 6 7 8 20 0 1 0 987654 1000 2 18446744073709551615";
        let p = parse_proc_stat(s).unwrap();
        assert_eq!(
            p,
            ProcStat { pid: 4242, comm: "my (weird) sh".into(), ppid: 100, pgrp: 4242, tty_nr: 34817, tpgid: 4300, starttime: 987654 }
        );
        assert_eq!(parse_proc_stat("garbage"), None);
        assert_eq!(parse_proc_stat("1 (x) S 0"), None);
    }

    fn ps(pid: u32, comm: &str, ppid: u32, tty: i32, start: u64) -> ProcStat {
        ProcStat { pid, comm: comm.into(), ppid, pgrp: pid as i32, tty_nr: tty, tpgid: pid as i32, starttime: start }
    }

    #[test]
    fn shell_choice() {
        let procs = vec![
            ps(100, "gnome-terminal-", 1, 0, 10),
            ps(200, "bash", 100, 34816, 20),
            ps(300, "zsh", 100, 34817, 30),
            ps(301, "vim", 300, 34817, 40),
            ps(400, "sh", 100, 0, 50), // helper without a tty
            ps(500, "bash", 999, 34818, 60),
        ];
        assert_eq!(pick_shell(&procs, 100), Some(300));
        assert_eq!(pick_shell(&procs, 999), Some(500));
        assert_eq!(pick_shell(&procs, 300), None);
        // Mux server in between (terminal → server → shell).
        let mux = vec![ps(10, "wezterm-gui", 1, 0, 1), ps(11, "wezterm-mux-ser", 10, 0, 2), ps(12, "fish", 11, 5, 3)];
        assert_eq!(pick_shell(&mux, 10), Some(12));
    }

    #[test]
    fn titles() {
        let home = Path::new("/home/me");
        assert_eq!(title_paths("me@box: ~/dev/aether", home), vec![PathBuf::from("/home/me/dev/aether")]);
        assert_eq!(title_paths("me@box: /etc", home), vec![PathBuf::from("/etc")]);
        assert_eq!(title_paths("notes.txt (~/Documents) - gedit", home), vec![PathBuf::from("/home/me/Documents")]);
        assert_eq!(title_paths("/home/me/Downloads", home), vec![PathBuf::from("/home/me/Downloads")]);
        assert_eq!(title_paths("aether [~/dev/aether] – main.rs", home), vec![PathBuf::from("/home/me/dev/aether")]);
        assert!(title_paths("main.rs - aether - Visual Studio Code", home).is_empty());
    }

    #[test]
    fn file_managers() {
        let home = Path::new("/home/me");
        let dirs = parse_user_dirs(
            "# comment\nXDG_DESKTOP_DIR=\"$HOME/Scrivania\"\nXDG_DOWNLOAD_DIR=\"$HOME/Scaricati\"\nXDG_MUSIC_DIR=\"/data/music\"\nXDG_PUBLICSHARE_DIR=\"$HOME/\"\n",
            home,
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/me/Scrivania"),
                PathBuf::from("/home/me/Scaricati"),
                PathBuf::from("/data/music"),
                PathBuf::from("/home/me"),
            ]
        );
        assert_eq!(file_manager_candidates("Home", home, &dirs, None)[0], PathBuf::from("/home/me"));
        assert_eq!(file_manager_candidates("me", home, &dirs, Some("me"))[0], PathBuf::from("/home/me"));
        assert_eq!(file_manager_candidates("music — Dolphin", home, &dirs, None)[0], PathBuf::from("/data/music"));
        assert_eq!(file_manager_candidates("Scaricati - Thunar", home, &dirs, None), vec![PathBuf::from("/home/me/Scaricati")]);
        assert_eq!(file_manager_candidates("/srv/www - File Manager", home, &dirs, None)[0], PathBuf::from("/srv/www"));
        let existing = ["/home/me/proj", "/home/me/proj/.git", "/home/me/proj/src"];
        let is_dir = |p: &Path| existing.iter().any(|d| Path::new(d) == p);
        let is_file = |p: &Path| p == Path::new("/home/me/proj/src/main.rs");
        let c = vec![PathBuf::from("/nope"), PathBuf::from("/home/me/proj/src/main.rs")];
        assert_eq!(first_dir(&c, &is_dir, &is_file, home), Some(PathBuf::from("/home/me/proj")));
        assert_eq!(first_dir(&[PathBuf::from("/nope")], &is_dir, &is_file, home), None);
    }

    /// Real /proc: a shell spawned under this test process (the "terminal")
    /// in a known directory is found and its cwd read back.
    #[cfg(target_os = "linux")]
    #[test]
    fn proc_shell_cwd() {
        let dir = std::env::temp_dir().join(format!("aether-dom-shell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let expected = dir.canonicalize().unwrap();
        let mut child =
            std::process::Command::new("sh").args(["-c", "sleep 30; :"]).current_dir(&dir).spawn().expect("spawn sh");
        let mut found = None;
        for _ in 0..50 {
            found = shell_cwd_under(std::process::id());
            if found.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let found = found.map(|p| p.canonicalize().unwrap_or(p));
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir(&dir);
        assert_eq!(found, Some(expected));
    }
}
