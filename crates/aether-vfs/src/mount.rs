//! Native mounting of a remote share through a loopback NFSv3 server.
//!
//! [`mount()`] starts an `nfsserve` NFSv3 server on `127.0.0.1:<random port>` backed by a
//! [`VfsClient`], then asks the OS NFS client to mount it:
//!
//! * **macOS** — `mount_nfs -o nolocks,vers=3,tcp,rsize=131072,actimeo=1,port=P,mountport=P
//!   localhost:/ <mount point>`. Unprivileged users may mount onto a directory they own,
//!   so the default mount point is `~/AetherLink/<PeerName>` ([`default_mount_point`]).
//!   Mounting under `/Volumes` requires root (it is owned by root) and is therefore not
//!   the default.
//! * **Linux** — prefers a FUSE filesystem ([`MountBackend::Fuse`], `fuser` with its
//!   pure-Rust mount through the setuid `fusermount3` helper, so **no root** is needed;
//!   requires `/dev/fuse` and the `fuse3` package; no NFS server is started). When FUSE is
//!   unavailable or fails *and* the process is root, falls back to
//!   `mount -t nfs -o nolock,vers=3,tcp,port=P,mountport=P,… 127.0.0.1:/ <mp>` (the kernel
//!   only lets root mount NFS). Otherwise the error explains both options.
//! * **Windows** — no NFS (the "Client for NFS" is not installed by default and ignores
//!   custom ports). Instead the share is served by the [`crate::webdav`] loopback server and
//!   mapped as a drive letter (`X:` mount point, or the highest free letter) through the
//!   built-in WebClient redirector with `WNetAddConnection2W(\\localhost@PORT\<token>)`.
//!   WebClient caps file size at `FileSizeLimitInBytes` (default 50 MB); see
//!   [`crate::mount_windows`] for this and the other caveats.
//!
//! [`MountHandle::unmount`] runs `umount`, falling back to `diskutil unmount force`
//! (macOS) or `umount -l` (Linux), or `WNetCancelConnection2W` (Windows); a FUSE mount is
//! unmounted with `fusermount3 -u` and its session thread joined. Dropping the handle
//! unmounts best-effort.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::client::VfsClient;
use crate::error::{Result, VfsError};

/// Default mount point for a peer's share: `~/AetherLink/<PeerName>` (name sanitized).
pub fn default_mount_point(peer_name: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let mut name: String = peer_name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    name = name.trim().trim_matches('.').to_string();
    if name.is_empty() {
        name = "Peer".into();
    }
    home.join("AetherLink").join(name)
}

/// Which mechanism serves a [`MountHandle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MountBackend {
    /// Loopback NFSv3 server mounted with the OS NFS client (macOS; Linux root fallback).
    Nfs,
    /// Linux FUSE filesystem mounted through `fusermount3` (no root needed).
    Fuse,
    /// Loopback WebDAV server mapped with the WebClient redirector (Windows).
    WebDav,
}

/// A mounted remote share. Unmounts (best-effort) on drop.
#[derive(Debug)]
pub struct MountHandle {
    mount_point: PathBuf,
    port: u16,
    server: Option<JoinHandle<()>>,
    mounted: bool,
    /// Linux FUSE session (then `server` is `None` and `port` is 0).
    #[cfg(target_os = "linux")]
    fuse: Option<fuser::BackgroundSession>,
}

impl MountHandle {
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Loopback port of the NFS server (0 for a FUSE mount).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The mechanism serving this mount.
    pub fn backend(&self) -> MountBackend {
        #[cfg(target_os = "linux")]
        if self.fuse.is_some() {
            return MountBackend::Fuse;
        }
        if cfg!(windows) {
            MountBackend::WebDav
        } else {
            MountBackend::Nfs
        }
    }

    /// Unmount and stop the NFS server.
    pub async fn unmount(mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(session) = self.fuse.take() {
            self.mounted = false;
            return crate::fuse::unmount(session, &self.mount_point).await;
        }
        let res = if self.mounted {
            unmount_path(&self.mount_point).await
        } else {
            Ok(())
        };
        if res.is_ok() {
            self.mounted = false;
        }
        if let Some(s) = self.server.take() {
            if self.mounted {
                // Keep serving so a later forced unmount can still flush; Drop retries.
                self.server = Some(s);
            } else {
                s.abort();
            }
        }
        res
    }
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(session) = self.fuse.take() {
            // fusermount3 -u + join off the runtime: the session thread may be waiting on it.
            let mp = self.mount_point.clone();
            std::thread::spawn(move || {
                if let Err(e) = crate::fuse::unmount_blocking(session, &mp) {
                    tracing::warn!("unmount on drop failed: {e}");
                }
            });
            return;
        }
        let server = self.server.take();
        if !self.mounted {
            if let Some(s) = server {
                s.abort();
            }
            return;
        }
        // Unmount from a plain thread: the NFS server task must keep running on the
        // runtime while the OS flushes, so it is only aborted afterwards.
        let mp = self.mount_point.clone();
        #[cfg(windows)]
        std::thread::spawn(move || {
            let name = crate::mount_windows::connection_name(&mp);
            if let Err(e) = crate::mount_windows::sys::cancel_connection(&name) {
                tracing::warn!("unmount on drop failed: {e}");
            }
            if let Some(s) = server {
                s.abort();
            }
        });
        #[cfg(not(windows))]
        std::thread::spawn(move || {
            for args in force_unmount_commands(&mp) {
                let ok = std::process::Command::new(args[0])
                    .args(&args[1..])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok {
                    break;
                }
            }
            if let Some(s) = server {
                s.abort();
            }
        });
    }
}

#[cfg(not(windows))]
fn force_unmount_commands(mp: &Path) -> Vec<Vec<&str>> {
    let p = mp.to_str().unwrap_or_default();
    if cfg!(target_os = "macos") {
        vec![
            vec!["umount", p],
            vec!["diskutil", "unmount", "force", p],
            vec!["umount", "-f", p],
        ]
    } else {
        vec![vec!["umount", p], vec!["umount", "-l", p]]
    }
}

#[cfg(not(windows))]
async fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| VfsError::Mount(format!("`{cmd}` timed out")))?
    .map_err(|e| VfsError::Mount(format!("cannot run `{cmd}`: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(VfsError::Mount(format!(
            "`{cmd} {}` failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

#[cfg(windows)]
async fn unmount_path(mp: &Path) -> Result<()> {
    let name = crate::mount_windows::connection_name(mp);
    tokio::task::spawn_blocking(move || crate::mount_windows::sys::cancel_connection(&name))
        .await
        .map_err(|e| VfsError::Mount(format!("unmount task failed: {e}")))?
        .map_err(VfsError::Mount)
}

#[cfg(not(windows))]
async fn unmount_path(mp: &Path) -> Result<()> {
    let mut last = Err(VfsError::Mount("no unmount command".into()));
    for args in force_unmount_commands(mp) {
        last = run(args[0], &args[1..]).await;
        if last.is_ok() {
            break;
        }
    }
    last
}

/// Mount `share` of the remote VFS reachable through `client` at `mount_point`
/// (created if missing; must be empty). See the module docs for platform notes.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub async fn mount(client: Arc<VfsClient>, share: u32, mount_point: &Path) -> Result<MountHandle> {
    use nfsserve::tcp::{NFSTcp, NFSTcpListener};

    let info = client
        .list_shares()
        .await?
        .into_iter()
        .find(|s| s.id == share)
        .ok_or_else(|| VfsError::Mount(format!("remote has no share with id {share}")))?;
    tokio::fs::create_dir_all(mount_point).await?;
    let mount_point = tokio::fs::canonicalize(mount_point).await?;

    // Linux: FUSE first (no root needed); NFS only as a root fallback.
    #[cfg(target_os = "linux")]
    let fuse_err = match crate::fuse::availability() {
        Ok(()) => {
            match crate::fuse::mount(client.clone(), share, info.read_only, &mount_point).await {
                Ok(session) => {
                    tracing::info!("mounted share {share} at {} (fuse)", mount_point.display());
                    return Ok(MountHandle {
                        mount_point,
                        port: 0,
                        server: None,
                        mounted: true,
                        fuse: Some(session),
                    });
                }
                Err(e) => e.to_string(),
            }
        }
        Err(why) => format!("FUSE unavailable: {why}"),
    };
    #[cfg(target_os = "linux")]
    if !crate::fuse::is_root() {
        return Err(VfsError::Mount(format!(
            "{fuse_err}. Either make FUSE work (install `fuse3` for the setuid `fusermount3` \
             helper and make /dev/fuse accessible; no root needed) or run as root \
             (CAP_SYS_ADMIN) to use the kernel NFS client"
        )));
    }
    #[cfg(target_os = "linux")]
    tracing::info!("{fuse_err}; running as root, falling back to an NFS mount");

    let fs = crate::nfs::AetherNfs::new(client, share, info.read_only);
    let listener = NFSTcpListener::bind("127.0.0.1:0", fs).await?;
    let port = listener.get_listen_port();
    let server = tokio::spawn(async move {
        if let Err(e) = listener.handle_forever().await {
            tracing::warn!("nfs loopback server stopped: {e}");
        }
    });
    let mut handle = MountHandle {
        mount_point: mount_point.clone(),
        port,
        server: Some(server),
        mounted: false,
        #[cfg(target_os = "linux")]
        fuse: None,
    };

    let mp = mount_point
        .to_str()
        .ok_or_else(|| VfsError::Mount("mount point is not valid UTF-8".into()))?;
    let ro = if info.read_only { ",rdonly" } else { "" };
    let res = if cfg!(target_os = "macos") {
        let opts = format!(
            "nolocks,vers=3,tcp,rsize=131072,wsize=131072,actimeo=1,port={port},mountport={port}{ro}"
        );
        run("mount_nfs", &["-o", &opts, "localhost:/", mp]).await
    } else {
        let ro = if info.read_only { ",ro" } else { "" };
        let opts = format!(
            "nolock,vers=3,tcp,rsize=131072,wsize=131072,actimeo=1,port={port},mountport={port}{ro}"
        );
        run("mount", &["-t", "nfs", "-o", &opts, "127.0.0.1:/", mp])
            .await
            .map_err(|e| {
                VfsError::Mount(format!(
                    "{e} (mounting NFS on Linux requires root/CAP_SYS_ADMIN; retry via pkexec)"
                ))
            })
    };
    match res {
        Ok(()) => {
            handle.mounted = true;
            tracing::info!(
                "mounted share {share} at {} (nfs port {port})",
                mount_point.display()
            );
            Ok(handle)
        }
        Err(e) => {
            drop(handle); // stops the server
            #[cfg(target_os = "linux")]
            let e = VfsError::Mount(format!("{e}; FUSE was not usable either: {fuse_err}"));
            Err(e)
        }
    }
}

/// Windows: serve the share over loopback WebDAV and map it with the WebClient redirector.
/// `mount_point` `X:` / `X:\` selects the drive letter; anything else picks the highest free
/// letter (or a UNC-only connection). The returned [`MountHandle::mount_point`] is `X:\` or
/// the UNC path. See [`crate::mount_windows`].
#[cfg(windows)]
pub async fn mount(client: Arc<VfsClient>, share: u32, mount_point: &Path) -> Result<MountHandle> {
    use crate::mount_windows::{parse_drive_letter, size_limit_note, sys};

    let v4 = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = v4.local_addr()?.port();
    let mut listeners = vec![v4];
    // `localhost` may resolve to ::1 first; serve both loopbacks on the same port.
    if let Ok(v6) = tokio::net::TcpListener::bind(("::1", port)).await {
        listeners.push(v6);
    }
    let dav = crate::webdav::serve_listeners(client, share, listeners).await?;
    let unc = dav.unc_path();

    let webclient = tokio::task::spawn_blocking(sys::ensure_webclient)
        .await
        .map_err(|e| VfsError::Mount(format!("WebClient check failed: {e}")))?
        .map_err(VfsError::Mount)?;
    let limit = tokio::task::spawn_blocking(sys::webclient_file_size_limit).await.ok().flatten();
    let local = match parse_drive_letter(mount_point) {
        Some(l) => Some(l),
        None => tokio::task::spawn_blocking(sys::free_drive_letter).await.ok().flatten(),
    };
    let remote = unc.clone();
    tokio::time::timeout(
        Duration::from_secs(60),
        tokio::task::spawn_blocking(move || sys::add_connection(local, &remote, webclient)),
    )
    .await
    .map_err(|_| VfsError::Mount(format!("mapping {unc} timed out (WebClient did not answer)")))?
    .map_err(|e| VfsError::Mount(format!("mapping task failed: {e}")))?
    .map_err(|e| VfsError::Mount(format!("{e}. Note: {}", size_limit_note(limit))))?;

    let mp = match local {
        Some(l) => PathBuf::from(format!("{l}:\\")),
        None => PathBuf::from(&unc),
    };
    tracing::info!("mounted share {share} at {} via {unc}. {}", mp.display(), size_limit_note(limit));
    Ok(MountHandle { mount_point: mp, port, server: Some(dav.into_task()), mounted: true })
}

/// Mounting is not available on this platform.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub async fn mount(
    _client: Arc<VfsClient>,
    _share: u32,
    _mount_point: &Path,
) -> Result<MountHandle> {
    Err(VfsError::Unsupported(
        "native mount on this OS requires a WinFsp-based driver (not implemented)",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mount_point_is_sanitized() {
        let p = default_mount_point("../evil/Peer:1");
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), "_evil_Peer_1");
        assert_eq!(p.parent().unwrap().file_name().unwrap(), "AetherLink");
        assert!(default_mount_point("  ").ends_with("AetherLink/Peer"));
    }
}
