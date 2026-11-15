//! Host-side PTY for the context-aware Quake terminal.

use crate::{CoreError, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

fn err(e: impl std::fmt::Display) -> CoreError {
    CoreError::Terminal(e.to_string())
}

pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    cwd: PathBuf,
    shell: String,
}

/// Pick the user's shell.
pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").ok().filter(|s| Path::new(s).exists()).unwrap_or_else(|| "/bin/sh".into())
    }
}

impl PtySession {
    /// Spawn a shell in `cwd` (falls back to the home directory if it doesn't exist).
    /// `on_output` runs on a dedicated reader thread; `on_exit` fires once when the PTY closes.
    pub fn spawn(
        cols: u16,
        rows: u16,
        cwd: Option<PathBuf>,
        shell: Option<String>,
        mut on_output: impl FnMut(Vec<u8>) + Send + 'static,
        on_exit: impl FnOnce() + Send + 'static,
    ) -> Result<Self> {
        let pty = native_pty_system()
            .openpty(PtySize { rows: rows.max(2), cols: cols.max(10), pixel_width: 0, pixel_height: 0 })
            .map_err(err)?;
        let cwd = cwd
            .filter(|p| p.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        let shell = shell.unwrap_or_else(default_shell);
        let mut cmd = CommandBuilder::new(&shell);
        #[cfg(not(windows))]
        cmd.arg("-l");
        cmd.cwd(&cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("AETHERLINK_SESSION", "1");
        let child = pty.slave.spawn_command(cmd).map_err(err)?;
        drop(pty.slave);

        let mut reader = pty.master.try_clone_reader().map_err(err)?;
        let writer = pty.master.take_writer().map_err(err)?;
        std::thread::Builder::new().name("aether-pty-read".into()).spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => on_output(buf[..n].to_vec()),
                }
            }
            on_exit();
        })?;
        Ok(Self { master: pty.master, writer, child, cwd, shell })
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize { rows: rows.max(2), cols: cols.max(10), pixel_width: 0, pixel_height: 0 })
            .map_err(err)
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn shell(&self) -> &str {
        &self.shell
    }

    /// Exit status if the shell has terminated.
    pub fn try_exit_code(&mut self) -> Option<i32> {
        self.child.try_wait().ok().flatten().map(|s| s.exit_code() as i32)
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn runs_commands_in_requested_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let (etx, erx) = mpsc::channel::<()>();
        let mut pty = PtySession::spawn(
            80,
            24,
            Some(dir.path().to_path_buf()),
            Some("/bin/sh".into()),
            move |b| {
                let _ = tx.send(b);
            },
            move || {
                let _ = etx.send(());
            },
        )
        .unwrap();
        pty.write(b"echo AETHER_$((40+2)); pwd; exit\n").unwrap();
        erx.recv_timeout(Duration::from_secs(10)).expect("shell exited");
        let out: Vec<u8> = rx.try_iter().flatten().collect();
        let out = String::from_utf8_lossy(&out);
        assert!(out.contains("AETHER_42"), "output: {out}");
        let canon = dir.path().canonicalize().unwrap();
        assert!(out.contains(canon.to_str().unwrap()) || out.contains(dir.path().to_str().unwrap()), "output: {out}");
    }
}
