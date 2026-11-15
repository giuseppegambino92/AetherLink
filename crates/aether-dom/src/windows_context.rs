//! Windows strategy for [`crate::context::active_working_directory`].
//!
//! By foreground window:
//! * **Explorer** (`CabinetWClass` / `ExploreWClass`) — `IShellWindows` → the entry whose
//!   `HWND` is the foreground window (and, on Windows 11 tabbed Explorer, whose shell browser
//!   window is the active `ShellTabWindowClass` tab) → `IShellBrowser` →
//!   `IFolderView::GetFolder<IPersistFolder2>` → `GetCurFolder` → `SHGetPathFromIDListW`;
//!   falling back to the `LocationURL` (`file:///C:/…`).
//! * **Desktop** (`Progman` / `WorkerW`) — the user's Desktop known folder.
//! * **Terminals** (Windows Terminal, conhost, PowerShell, cmd, WezTerm, Alacritty, mintty…) —
//!   absolute paths in the window title (prompts such as `C:\src>` or
//!   `Administrator: C:\…`), else the current directory of the window's shell process or
//!   its newest descendant shell, read from the process PEB
//!   (`NtQueryInformationProcess` + `ReadProcessMemory`, same-bitness 64-bit targets only).
//! * **IDEs** (VS Code & forks, Visual Studio, JetBrains, Zed, Sublime, Notepad++) and any
//!   other app — absolute paths in the window title (JetBrains shows `[C:\dev\app]`); a file
//!   resolves to its git work tree or parent directory.
//!
//! The pure parsing helpers are portable and unit-tested on every OS.

/// Foreground-application families with a dedicated strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinAppKind {
    Explorer,
    Desktop,
    Terminal,
    Ide,
    Other,
}

const SHELLS: &[&str] = &["cmd", "powershell", "pwsh", "bash", "zsh", "fish", "nu", "sh", "tcc", "elvish", "xonsh"];

/// True for shell executables (`pwsh.exe`, `CMD.EXE` …).
pub fn is_shell_exe(exe: &str) -> bool {
    SHELLS.contains(&exe_stem_lower(exe).as_str())
}

/// Lower-cased file stem of an executable path or name.
pub fn exe_stem_lower(exe: &str) -> String {
    let file = exe.rsplit(['\\', '/']).next().unwrap_or(exe).to_lowercase();
    match file.strip_suffix(".exe") {
        Some(s) => s.to_string(),
        None => file,
    }
}

/// Classify the foreground window by its class name and process executable.
pub fn classify_window(class: &str, exe: &str) -> WinAppKind {
    const TERMINALS: &[&str] = &[
        "windowsterminal", "wt", "conhost", "openconsole", "wezterm-gui", "alacritty", "mintty", "tabby", "hyper",
        "terminus", "fluent terminal", "conemu64", "conemu", "cmder", "kitty",
    ];
    const IDES: &[&str] = &[
        "code", "code - insiders", "codium", "vscodium", "cursor", "windsurf", "devenv", "zed", "sublime_text",
        "notepad++", "studio64", "fleet", "rider64", "idea64", "pycharm64", "webstorm64", "clion64", "goland64",
        "rustrover64", "phpstorm64", "rubymine64", "datagrip64", "dataspell64",
    ];
    match class {
        "CabinetWClass" | "ExploreWClass" => return WinAppKind::Explorer,
        "Progman" | "WorkerW" => return WinAppKind::Desktop,
        "ConsoleWindowClass" | "CASCADIA_HOSTING_WINDOW_CLASS" | "PseudoConsoleWindow" => {
            return WinAppKind::Terminal;
        }
        _ => {}
    }
    let stem = exe_stem_lower(exe);
    if stem == "explorer" {
        WinAppKind::Explorer
    } else if TERMINALS.contains(&stem.as_str()) || SHELLS.contains(&stem.as_str()) {
        WinAppKind::Terminal
    } else if IDES.contains(&stem.as_str()) {
        WinAppKind::Ide
    } else {
        WinAppKind::Other
    }
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
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
    String::from_utf8(out).ok()
}

/// Strip a trailing separator except on a drive root (`C:\`).
fn trim_trailing_sep(mut s: String) -> String {
    while (s.ends_with('\\') || s.ends_with('/')) && !(s.len() == 3 && s.as_bytes()[1] == b':') && s.len() > 2 {
        s.pop();
    }
    s
}

/// `file:///C:/a%20b/` → `C:\a b`; `file://server/share/x` → `\\server\share\x`.
pub fn win_file_url_to_path(url: &str) -> Option<String> {
    let rest = url.get(..7).filter(|p| p.eq_ignore_ascii_case("file://")).map(|_| &url[7..])?;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let decoded = percent_decode(rest)?;
    let path = if let Some(local) = decoded.strip_prefix('/') {
        // file:///C:/… (also tolerate the legacy file:///C|/ form)
        let lb = local.as_bytes();
        if lb.len() >= 2 && lb[0].is_ascii_alphabetic() && (lb[1] == b':' || lb[1] == b'|') {
            format!("{}:{}", &local[..1], &local[2..])
        } else {
            return None;
        }
    } else if let Some(local) = decoded.strip_prefix("localhost/") {
        return win_file_url_to_path(&format!("file:///{local}"));
    } else if decoded.is_empty() {
        return None;
    } else {
        format!("//{decoded}")
    };
    let mut p = path.replace('/', "\\");
    if p.len() == 2 {
        p.push('\\');
    }
    Some(trim_trailing_sep(p))
}

/// Characters that end a path embedded in a window title.
fn ends_path(c: char) -> bool {
    matches!(c, ']' | '>' | '"' | '|' | '*' | '?' | '<' | '\n' | '\r' | '\t' | '(' | ')' | '[')
}

const TITLE_SEPARATORS: &[&str] = &[" - ", " — ", " – ", " • ", " · "];

/// Absolute Windows paths (`C:\…`, `C:/…`, `\\server\share\…`) appearing in a window title,
/// in order of appearance, without duplicates. Paths end at bracket/prompt characters or at
/// a ` - ` / ` — ` title separator.
pub fn win_paths_from_title(title: &str) -> Vec<String> {
    let chars: Vec<(usize, char)> = title.char_indices().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (start, c) = chars[i];
        let prev_ok = i == 0 || !chars[i - 1].1.is_alphanumeric();
        let drive = c.is_ascii_alphabetic()
            && prev_ok
            && chars.get(i + 1).is_some_and(|x| x.1 == ':')
            && chars.get(i + 2).is_some_and(|x| x.1 == '\\' || x.1 == '/');
        let unc = c == '\\'
            && prev_ok
            && chars.get(i + 1).is_some_and(|x| x.1 == '\\')
            && chars.get(i + 2).is_some_and(|x| x.1.is_alphanumeric());
        if !(drive || unc) {
            i += 1;
            continue;
        }
        let tail = &title[start..];
        let mut end = tail.find(ends_path).unwrap_or(tail.len());
        for sep in TITLE_SEPARATORS {
            if let Some(p) = tail[..end].find(sep) {
                end = end.min(p);
            }
        }
        let raw = tail[..end].trim_end().trim_end_matches(['.', ',', ';', ':']);
        let mut p = raw.replace('/', "\\");
        if p.len() == 2 {
            p.push('\\');
        }
        let p = trim_trailing_sep(p);
        if p.len() >= 3 && !out.contains(&p) {
            out.push(p);
        }
        i += tail[..end].chars().count().max(1);
    }
    out
}

/// True for paths that name an executable (`…\cmd.exe`) — terminal titles often show the
/// shell binary, which says nothing about the working directory.
pub fn is_executable_path(p: &str) -> bool {
    let l = p.to_lowercase();
    [".exe", ".com", ".bat", ".cmd", ".ps1"].iter().any(|e| l.ends_with(e))
}

/// Shell processes among the descendants (≤ 3 generations) of `root` in a
/// `(pid, parent pid, exe)` process table, nearest generation first. `root` itself is
/// included when it is a shell.
pub fn descendant_shells(table: &[(u32, u32, String)], root: u32) -> Vec<u32> {
    let mut out = Vec::new();
    if table.iter().any(|(p, _, exe)| *p == root && is_shell_exe(exe)) {
        out.push(root);
    }
    let mut frontier = vec![root];
    for _ in 0..3 {
        let mut next = Vec::new();
        for (pid, ppid, exe) in table {
            if *pid != root && frontier.contains(ppid) && !next.contains(pid) {
                next.push(*pid);
                if is_shell_exe(exe) && !out.contains(pid) {
                    out.push(*pid);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    out
}

#[cfg(target_os = "windows")]
pub(crate) use imp::detect;

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use crate::windows::{ensure_com, foreground_window, process_image_path, wide_to_string, window_text};
    use std::path::{Path, PathBuf};
    use windows::core::{w, Interface, PCWSTR};
    use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
    use windows::core::BOOL;
    use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND};
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, IServiceProvider, CLSCTX_ALL};
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetProcessTimes, IsWow64Process, OpenProcess, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_INFORMATION,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Shell::{
        FOLDERID_Desktop, IFolderView, IPersistFolder2, IShellBrowser, IShellWindows, IWebBrowserApp,
        SHGetKnownFolderPath, SHGetPathFromIDListW, ShellWindows, KF_FLAG_DEFAULT, SID_STopLevelBrowser,
    };
    use windows::Win32::UI::WindowsAndMessaging::FindWindowExW;

    fn home() -> PathBuf {
        std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\"))
    }

    /// Directory for a path found in a title: itself, or a file's git root / parent.
    fn resolve(p: &str, home: &Path) -> Option<PathBuf> {
        if is_executable_path(p) {
            return None;
        }
        let p = PathBuf::from(p);
        if p.is_dir() {
            Some(p)
        } else if p.is_file() {
            crate::context::dir_for_document(&p, &|q: &Path| q.is_dir(), Some(home))
        } else {
            None
        }
    }

    fn from_title(hwnd: HWND, home: &Path) -> Option<PathBuf> {
        win_paths_from_title(&window_text(hwnd)).iter().find_map(|p| resolve(p, home))
    }

    fn desktop_folder() -> Option<PathBuf> {
        unsafe {
            let p = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            s.map(PathBuf::from)
        }
    }

    unsafe fn browser_folder(browser: &IShellBrowser) -> Option<PathBuf> {
        unsafe {
            let view = browser.QueryActiveShellView().ok()?;
            let fv: IFolderView = view.cast().ok()?;
            let pf: IPersistFolder2 = fv.GetFolder().ok()?;
            let pidl = pf.GetCurFolder().ok()?;
            if pidl.is_null() {
                return None;
            }
            let mut buf = [0u16; 260];
            let ok = SHGetPathFromIDListW(pidl, &mut buf).as_bool();
            CoTaskMemFree(Some(pidl as *const _));
            let s = wide_to_string(&buf);
            (ok && !s.is_empty()).then(|| PathBuf::from(s))
        }
    }

    fn explorer_folder(hwnd: HWND) -> Option<PathBuf> {
        ensure_com().ok()?;
        unsafe {
            let shell: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
            // Windows 11 tabbed Explorer: one shell window per tab, all with the same top-level
            // HWND; the first ShellTabWindowClass child is the active tab.
            let active_tab = FindWindowExW(Some(hwnd), None, w!("ShellTabWindowClass"), PCWSTR::null()).ok();
            let count = shell.Count().ok()?;
            let mut url_fallback = None;
            for i in 0..count {
                let Ok(disp) = shell.Item(&VARIANT::from(i)) else { continue };
                let Ok(app) = disp.cast::<IWebBrowserApp>() else { continue };
                if app.HWND().map(|h| h.0) != Ok(hwnd.0 as isize) {
                    continue;
                }
                let browser = app
                    .cast::<IServiceProvider>()
                    .ok()
                    .and_then(|sp| sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser).ok());
                if let (Some(tab), Some(b)) = (active_tab, browser.as_ref())
                    && let Ok(w) = b.GetWindow()
                    && w != tab
                {
                    continue;
                }
                if let Some(p) = browser.as_ref().and_then(|b| browser_folder(b)) {
                    return Some(p);
                }
                if let Ok(url) = app.LocationURL()
                    && let Some(p) = win_file_url_to_path(&url.to_string())
                {
                    url_fallback.get_or_insert(PathBuf::from(p));
                }
            }
            url_fallback
        }
    }

    fn process_table() -> Vec<(u32, u32, String)> {
        let mut out = Vec::new();
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            if Process32FirstW(snap, &mut e).is_ok() {
                loop {
                    out.push((e.th32ProcessID, e.th32ParentProcessID, wide_to_string(&e.szExeFile)));
                    if Process32NextW(snap, &mut e).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
        }
        out
    }

    fn creation_time(pid: u32) -> u64 {
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return 0 };
            let (mut c, mut e, mut k, mut u) =
                (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).is_ok();
            let _ = CloseHandle(h);
            if ok { ((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64 } else { 0 }
        }
    }

    unsafe fn read<T: Copy + Default>(h: HANDLE, addr: usize) -> Option<T> {
        let mut v = T::default();
        unsafe {
            ReadProcessMemory(h, addr as *const _, &mut v as *mut T as *mut _, std::mem::size_of::<T>(), None).ok()?;
        }
        Some(v)
    }

    /// Current directory of another (native 64-bit) process, from its PEB.
    #[cfg(target_pointer_width = "64")]
    fn process_cwd(pid: u32) -> Option<PathBuf> {
        // x64 offsets: PEB.ProcessParameters = 0x20;
        // RTL_USER_PROCESS_PARAMETERS.CurrentDirectory.DosPath (UNICODE_STRING) = 0x38.
        const PEB_PROCESS_PARAMETERS: usize = 0x20;
        const PARAMS_CURRENT_DIRECTORY: usize = 0x38;
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid).ok()?;
            let res = (|| {
                let mut wow = BOOL(0);
                IsWow64Process(h, &mut wow).ok()?;
                if wow.as_bool() {
                    return None; // 32-bit PEB layout; not worth a second code path
                }
                let mut pbi: PROCESS_BASIC_INFORMATION = std::mem::zeroed();
                let mut len = 0u32;
                let st = NtQueryInformationProcess(
                    h,
                    ProcessBasicInformation,
                    &mut pbi as *mut _ as *mut _,
                    std::mem::size_of::<PROCESS_BASIC_INFORMATION>() as u32,
                    &mut len,
                );
                if st.is_err() || pbi.PebBaseAddress.is_null() {
                    return None;
                }
                let params: usize = read(h, pbi.PebBaseAddress as usize + PEB_PROCESS_PARAMETERS)?;
                let byte_len: u16 = read(h, params + PARAMS_CURRENT_DIRECTORY)?;
                let buffer: usize = read(h, params + PARAMS_CURRENT_DIRECTORY + 8)?;
                if params == 0 || buffer == 0 || byte_len == 0 || byte_len > 2 * 32_767 {
                    return None;
                }
                let mut w = vec![0u16; byte_len as usize / 2];
                ReadProcessMemory(h, buffer as *const _, w.as_mut_ptr() as *mut _, byte_len as usize, None).ok()?;
                Some(PathBuf::from(trim_trailing_sep(String::from_utf16_lossy(&w))))
            })();
            let _ = CloseHandle(h);
            res
        }
    }

    #[cfg(not(target_pointer_width = "64"))]
    fn process_cwd(_pid: u32) -> Option<PathBuf> {
        None
    }

    fn terminal_cwd(pid: u32) -> Option<PathBuf> {
        let table = process_table();
        let mut shells = descendant_shells(&table, pid);
        // Newest shell first: in a multi-tab terminal it is the most likely active one.
        shells.sort_by_key(|&p| std::cmp::Reverse(creation_time(p)));
        shells.into_iter().find_map(|p| process_cwd(p).filter(|d| d.is_dir()))
    }

    pub(crate) fn detect() -> Option<PathBuf> {
        let home = home();
        let (hwnd, pid) = foreground_window()?;
        let class = crate::windows::window_class(hwnd);
        let exe = process_image_path(pid).unwrap_or_default();
        match classify_window(&class, &exe) {
            WinAppKind::Explorer => explorer_folder(hwnd).or_else(|| from_title(hwnd, &home)),
            WinAppKind::Desktop => desktop_folder(),
            WinAppKind::Terminal => from_title(hwnd, &home).or_else(|| terminal_cwd(pid)),
            WinAppKind::Ide | WinAppKind::Other => from_title(hwnd, &home),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify() {
        assert_eq!(classify_window("CabinetWClass", r"C:\Windows\explorer.exe"), WinAppKind::Explorer);
        assert_eq!(classify_window("Progman", r"C:\Windows\explorer.exe"), WinAppKind::Desktop);
        assert_eq!(classify_window("WorkerW", ""), WinAppKind::Desktop);
        assert_eq!(classify_window("ConsoleWindowClass", r"C:\Windows\System32\cmd.exe"), WinAppKind::Terminal);
        assert_eq!(
            classify_window("CASCADIA_HOSTING_WINDOW_CLASS", r"C:\Program Files\WindowsApps\WindowsTerminal.exe"),
            WinAppKind::Terminal
        );
        assert_eq!(classify_window("Chrome_WidgetWin_1", r"C:\Users\me\AppData\Local\Programs\Microsoft VS Code\Code.exe"), WinAppKind::Ide);
        assert_eq!(classify_window("SunAwtFrame", r"C:\JetBrains\RustRover\bin\rustrover64.exe"), WinAppKind::Ide);
        assert_eq!(classify_window("Chrome_WidgetWin_1", r"C:\Program Files\Google\Chrome\chrome.exe"), WinAppKind::Other);
        assert!(is_shell_exe("PWSH.EXE"));
        assert!(is_shell_exe(r"C:\Windows\System32\cmd.exe"));
        assert!(!is_shell_exe("OpenConsole.exe"));
    }

    #[test]
    fn file_urls() {
        assert_eq!(win_file_url_to_path("file:///C:/Users/me/My%20Dir/").as_deref(), Some(r"C:\Users\me\My Dir"));
        assert_eq!(win_file_url_to_path("file:///D:/perch%C3%A9").as_deref(), Some(r"D:\perché"));
        assert_eq!(win_file_url_to_path("file:///C:/").as_deref(), Some(r"C:\"));
        assert_eq!(win_file_url_to_path("FILE:///c|/x").as_deref(), Some(r"c:\x"));
        assert_eq!(win_file_url_to_path("file://server/share/dir").as_deref(), Some(r"\\server\share\dir"));
        assert_eq!(win_file_url_to_path("file://localhost/C:/tmp").as_deref(), Some(r"C:\tmp"));
        assert_eq!(win_file_url_to_path("file:///usr/lib"), None);
        assert_eq!(win_file_url_to_path("https://example.com/"), None);
        assert_eq!(win_file_url_to_path("file:///C:/a%2"), Some(r"C:\a%2".to_string()));
    }

    #[test]
    fn title_paths() {
        assert_eq!(win_paths_from_title(r"aetherlink [C:\dev\aetherlink] – lib.rs"), vec![r"C:\dev\aetherlink"]);
        assert_eq!(win_paths_from_title(r"Administrator: C:\Windows\system32\cmd.exe"), vec![
            r"C:\Windows\system32\cmd.exe"
        ]);
        assert_eq!(win_paths_from_title(r"C:\src\my project>"), vec![r"C:\src\my project"]);
        assert_eq!(win_paths_from_title(r"PS D:\work> "), vec![r"D:\work"]);
        assert_eq!(win_paths_from_title(r"main.rs - D:/code/x - Visual Studio Code"), vec![r"D:\code\x"]);
        assert_eq!(win_paths_from_title(r"\\nas\share\docs - Explorer"), vec![r"\\nas\share\docs"]);
        assert_eq!(win_paths_from_title(r"C:\"), vec![r"C:\"]);
        assert!(win_paths_from_title("main.rs - aetherlink - Visual Studio Code").is_empty());
        assert!(win_paths_from_title("ratio 16:9 and http://x").is_empty());
        assert_eq!(win_paths_from_title(r"C:\a - C:\b"), vec![r"C:\a", r"C:\b"]);
        assert!(is_executable_path(r"C:\Windows\system32\cmd.exe"));
        assert!(!is_executable_path(r"C:\src"));
    }

    #[test]
    fn shells_under_terminal() {
        let t = |v: &[(u32, u32, &str)]| v.iter().map(|(a, b, c)| (*a, *b, c.to_string())).collect::<Vec<_>>();
        let table = t(&[
            (10, 1, "WindowsTerminal.exe"),
            (11, 10, "OpenConsole.exe"),
            (12, 10, "pwsh.exe"),
            (13, 12, "git.exe"),
            (14, 10, "cmd.exe"),
            (15, 14, "bash.exe"),
            (20, 1, "cmd.exe"),
        ]);
        assert_eq!(descendant_shells(&table, 10), vec![12, 14, 15]);
        assert_eq!(descendant_shells(&table, 20), vec![20]);
        assert!(descendant_shells(&table, 99).is_empty());
    }
}
