//! Drag-and-drop file transfer over a `TransferMessage` stream.
//!
//! Sender: [`send_paths`] walks the given files/directories (symlinks are skipped, not
//! followed), sends an `Offer`, then each file as `Chunk`s of [`TRANSFER_CHUNK_SIZE`]
//! followed by `EntryDone` with its BLAKE3 digest, then `Complete`.
//!
//! Receiver: [`receive`] validates every `rel_path` (relative, no `..`, NUL, drive/UNC
//! prefixes) and the offered sizes (checked arithmetic: a hostile offer cannot overflow
//! them), then writes the whole transfer into a private staging directory created in the
//! destination (`.aetherpart-<128-bit random>`, mode 0700 on Unix; the peer's transfer id
//! never influences a path). Files are created with `create_new` (nothing pre-existing is
//! ever truncated) and are bounded by their offered size. Only after `Complete`, with every
//! file's BLAKE3 digest verified, are the top-level items moved into the destination, each
//! under a non-colliding name (`file (1).txt`, `dir (1)`) using a no-replace rename, so an
//! existing entry is never overwritten. On any error or cancellation the staging directory
//! is removed and already published items are moved back: the destination is left as it was.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use aether_protocol::framing::{read_frame, write_frame};
use aether_protocol::{TRANSFER_CHUNK_SIZE, TransferEntry, TransferMessage};
use tokio::fs::{self, File};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Result, VfsError};
use crate::path::split_rel_path;

/// Cooperative cancellation flag shared between a transfer and its controller.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Progress report passed to the callback after every chunk and entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferProgress {
    pub transfer_id: u64,
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub entries_done: u32,
    pub entries_total: u32,
    /// `rel_path` of the entry being transferred.
    pub current: Option<String>,
}

/// Sender options.
#[derive(Debug, Clone, Default)]
pub struct SendOptions {
    pub transfer_id: u64,
    /// Forwarded in the `Offer` (host-local directory hint).
    pub drop_target: Option<String>,
    pub cancel: CancelToken,
}

/// Receiver options.
#[derive(Debug, Clone, Default)]
pub struct ReceiveOptions {
    pub cancel: CancelToken,
}

/// Result of a completed receive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveSummary {
    pub transfer_id: u64,
    /// Final paths of the top-level items (after collision renaming), in offer order.
    pub roots: Vec<PathBuf>,
    /// Final paths of every received file.
    pub files: Vec<PathBuf>,
    pub bytes: u64,
    pub drop_target: Option<String>,
}

struct SourceEntry {
    entry: TransferEntry,
    path: PathBuf,
}

fn mtime_ns(md: &std::fs::Metadata) -> i128 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

fn walk(paths: &[PathBuf]) -> Result<Vec<SourceEntry>> {
    let mut out = Vec::new();
    for p in paths {
        let md = std::fs::symlink_metadata(p)?;
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                VfsError::InvalidPath(format!("{} has no UTF-8 file name", p.display()))
            })?
            .to_string();
        walk_one(p, &md, name, &mut out)?;
    }
    Ok(out)
}

fn walk_one(
    p: &Path,
    md: &std::fs::Metadata,
    rel: String,
    out: &mut Vec<SourceEntry>,
) -> Result<()> {
    if md.file_type().is_symlink() {
        tracing::debug!("transfer: skipping symlink {}", p.display());
        return Ok(());
    }
    if md.is_dir() {
        out.push(SourceEntry {
            entry: TransferEntry {
                rel_path: rel.clone(),
                size: 0,
                is_dir: true,
                mtime_ns: mtime_ns(md),
            },
            path: p.to_path_buf(),
        });
        let mut children: Vec<_> = std::fs::read_dir(p)?.collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(|e| e.file_name());
        for c in children {
            let Ok(name) = c.file_name().into_string() else {
                tracing::debug!("transfer: skipping non-UTF-8 name in {}", p.display());
                continue;
            };
            let cmd = std::fs::symlink_metadata(c.path())?;
            walk_one(&c.path(), &cmd, format!("{rel}/{name}"), out)?;
        }
    } else if md.is_file() {
        out.push(SourceEntry {
            entry: TransferEntry {
                rel_path: rel,
                size: md.len(),
                is_dir: false,
                mtime_ns: mtime_ns(md),
            },
            path: p.to_path_buf(),
        });
    }
    Ok(())
}

/// Send `paths` (files and/or directories, recursively) as one transfer.
/// Returns the total number of payload bytes sent.
pub async fn send_paths<W, F>(
    paths: &[PathBuf],
    writer: &mut W,
    opts: SendOptions,
    mut progress: F,
) -> Result<u64>
where
    W: AsyncWrite + Unpin,
    F: FnMut(&TransferProgress),
{
    let list = {
        let paths = paths.to_vec();
        tokio::task::spawn_blocking(move || walk(&paths))
            .await
            .map_err(|e| VfsError::Io(std::io::Error::other(e)))??
    };
    let total_bytes: u64 = list.iter().map(|e| e.entry.size).sum();
    let files_total = list.iter().filter(|e| !e.entry.is_dir).count() as u32;
    write_frame(
        writer,
        &TransferMessage::Offer {
            transfer_id: opts.transfer_id,
            entries: list.iter().map(|e| e.entry.clone()).collect(),
            total_bytes,
            drop_target: opts.drop_target.clone(),
        },
    )
    .await?;

    let mut prog = TransferProgress {
        transfer_id: opts.transfer_id,
        bytes_done: 0,
        total_bytes,
        entries_done: 0,
        entries_total: files_total,
        current: None,
    };
    progress(&prog);
    let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];

    let res: Result<()> = async {
        for (idx, src) in list.iter().enumerate() {
            if src.entry.is_dir {
                continue;
            }
            prog.current = Some(src.entry.rel_path.clone());
            let mut f = File::open(&src.path).await?;
            let mut hasher = blake3::Hasher::new();
            let mut offset = 0u64;
            // Never send more than offered (the receiver enforces it too).
            while offset < src.entry.size {
                if opts.cancel.is_cancelled() {
                    return Err(VfsError::Cancelled("cancelled by sender".into()));
                }
                let want = ((src.entry.size - offset) as usize).min(buf.len());
                let n = f.read(&mut buf[..want]).await?;
                if n == 0 {
                    break; // file shrank since the offer
                }
                hasher.update(&buf[..n]);
                write_frame(
                    writer,
                    &TransferMessage::Chunk {
                        entry: idx as u32,
                        offset,
                        data: buf[..n].to_vec(),
                    },
                )
                .await?;
                offset += n as u64;
                prog.bytes_done += n as u64;
                progress(&prog);
            }
            write_frame(
                writer,
                &TransferMessage::EntryDone {
                    entry: idx as u32,
                    blake3: *hasher.finalize().as_bytes(),
                },
            )
            .await?;
            prog.entries_done += 1;
            progress(&prog);
        }
        Ok(())
    }
    .await;

    match res {
        Ok(()) => {
            write_frame(writer, &TransferMessage::Complete).await?;
            writer.flush().await?;
            prog.current = None;
            progress(&prog);
            Ok(prog.bytes_done)
        }
        Err(e) => {
            let _ = write_frame(
                writer,
                &TransferMessage::Cancel {
                    reason: e.to_string(),
                },
            )
            .await;
            let _ = writer.flush().await;
            Err(e)
        }
    }
}

/// `"name (n).ext"` / `"dir (n)"` variant of `name`.
fn numbered(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

/// Prefix of the private staging directory created inside the destination.
const STAGING_PREFIX: &str = ".aetherpart-";

/// Create the private staging directory (`0700` on Unix) with a random 128-bit name.
async fn create_staging(dest: &Path) -> Result<PathBuf> {
    for _ in 0..8 {
        let token: [u8; 16] = rand::random();
        let hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
        let dir = dest.join(format!("{STAGING_PREFIX}{hex}"));
        #[cfg_attr(not(unix), allow(unused_mut))]
        #[cfg(unix)]
        let b = {
            let mut b = fs::DirBuilder::new();
            b.mode(0o700);
            b
        };
        #[cfg(not(unix))]
        let b = fs::DirBuilder::new();
        match b.create(&dir).await {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(VfsError::Io(std::io::Error::other(
        "could not create a staging directory",
    )))
}

/// Rename `from` to `to`, failing with `AlreadyExists` instead of replacing `to`.
#[allow(clippy::needless_return)] // one cfg block per platform, each returning
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let f = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let t = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        if unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) } == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOTSUP) {
            return Err(e);
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let f = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let t = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        let r = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                f.as_ptr(),
                libc::AT_FDCWD,
                t.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if r == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if !matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS)) {
            return Err(e);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
        let w = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
        // No MOVEFILE_REPLACE_EXISTING: fails with ERROR_ALREADY_EXISTS.
        if unsafe { MoveFileExW(w(from).as_ptr(), w(to).as_ptr(), 0) } != 0 {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error());
    }
    #[cfg(not(windows))]
    {
        // Filesystem without an atomic no-replace rename: check, then rename.
        if std::fs::symlink_metadata(to).is_ok() {
            return Err(std::io::ErrorKind::AlreadyExists.into());
        }
        return std::fs::rename(from, to);
    }
}

fn is_collision(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::AlreadyExists
        || e.kind() == std::io::ErrorKind::DirectoryNotEmpty
}

/// Move each staged top-level item into `dest` under a non-colliding name, never replacing
/// an existing entry. Returns the final names (offer order). On failure, the items already
/// published are moved back into the staging directory.
fn publish(staging: &Path, dest: &Path, roots: &[String]) -> Result<Vec<String>> {
    let mut published: Vec<(String, String)> = Vec::new();
    let res = (|| -> Result<()> {
        for name in roots {
            let from = staging.join(name);
            let mut n = 0u32;
            loop {
                let cand = if n == 0 {
                    name.clone()
                } else {
                    numbered(name, n)
                };
                match rename_noreplace(&from, &dest.join(&cand)) {
                    Ok(()) => {
                        published.push((name.clone(), cand));
                        break;
                    }
                    Err(e) if is_collision(&e) && n < 10_000 => n += 1,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(())
    })();
    match res {
        Ok(()) => Ok(published.into_iter().map(|(_, f)| f).collect()),
        Err(e) => {
            for (orig, fin) in published.iter().rev() {
                if let Err(re) = std::fs::rename(dest.join(fin), staging.join(orig)) {
                    tracing::warn!("transfer: could not roll back {fin:?}: {re}");
                }
            }
            Err(e)
        }
    }
}

struct OpenFile {
    file: File,
    hasher: blake3::Hasher,
    written: u64,
}

struct Receiver {
    entries: Vec<TransferEntry>,
    /// Staged path per entry index (inside the private staging directory).
    staged: Vec<PathBuf>,
    open: HashMap<u32, OpenFile>,
    done: HashSet<u32>,
}

impl Receiver {
    fn entry(&self, idx: u32) -> Result<&TransferEntry> {
        self.entries
            .get(idx as usize)
            .ok_or_else(|| VfsError::Protocol(format!("transfer references unknown entry {idx}")))
    }

    async fn open_entry(&mut self, idx: u32) -> Result<&mut OpenFile> {
        if !self.open.contains_key(&idx) {
            let e = self.entry(idx)?;
            if e.is_dir {
                return Err(VfsError::Protocol(format!(
                    "data sent for directory entry {idx}"
                )));
            }
            if self.done.contains(&idx) {
                return Err(VfsError::Protocol(format!("entry {idx} already completed")));
            }
            let path = &self.staged[idx as usize];
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).await?;
            }
            // Never truncate or reuse an existing file (e.g. a case-insensitive duplicate).
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .await?;
            self.open.insert(
                idx,
                OpenFile {
                    file,
                    hasher: blake3::Hasher::new(),
                    written: 0,
                },
            );
        }
        Ok(self.open.get_mut(&idx).expect("just inserted"))
    }
}

/// Receive one transfer from `reader` into `dest_dir` (must exist).
pub async fn receive<R, F>(
    reader: &mut R,
    dest_dir: &Path,
    opts: ReceiveOptions,
    mut progress: F,
) -> Result<ReceiveSummary>
where
    R: AsyncRead + Unpin,
    F: FnMut(&TransferProgress),
{
    let dest = fs::canonicalize(dest_dir).await?;
    if !fs::metadata(&dest).await?.is_dir() {
        return Err(VfsError::InvalidPath(format!(
            "{} is not a directory",
            dest.display()
        )));
    }
    let Some(first) = read_frame::<_, TransferMessage>(reader).await? else {
        return Err(VfsError::Closed);
    };
    let (transfer_id, entries, total_bytes, drop_target) = match first {
        TransferMessage::Offer {
            transfer_id,
            entries,
            total_bytes,
            drop_target,
        } => (transfer_id, entries, total_bytes, drop_target),
        TransferMessage::Cancel { reason } => return Err(VfsError::Cancelled(reason)),
        other => return Err(VfsError::Protocol(format!("expected Offer, got {other:?}"))),
    };

    // Validate every path; remember the top-level names in offer order.
    let mut roots: Vec<String> = Vec::new();
    let mut root_set = HashSet::new();
    let mut rels: Vec<Vec<String>> = Vec::with_capacity(entries.len());
    let mut seen = HashSet::new();
    for e in &entries {
        let comps = split_rel_path(&e.rel_path).map_err(VfsError::InvalidPath)?;
        if comps.is_empty() {
            return Err(VfsError::InvalidPath(format!(
                "empty entry path {:?}",
                e.rel_path
            )));
        }
        if !seen.insert(comps.join("/")) {
            return Err(VfsError::Protocol(format!(
                "duplicate entry {:?}",
                e.rel_path
            )));
        }
        if root_set.insert(comps[0].to_string()) {
            roots.push(comps[0].to_string());
        }
        if e.is_dir && e.size != 0 {
            return Err(VfsError::Protocol(format!(
                "directory entry {:?} has a size",
                e.rel_path
            )));
        }
        rels.push(comps.iter().map(|c| c.to_string()).collect());
    }
    let files_total = entries.iter().filter(|e| !e.is_dir).count() as u32;
    let offered = entries
        .iter()
        .try_fold(0u64, |acc, e| acc.checked_add(e.size))
        .ok_or_else(|| VfsError::Protocol("offered sizes overflow".into()))?;
    if offered != total_bytes {
        return Err(VfsError::Protocol(
            "offer total does not match entry sizes".into(),
        ));
    }

    let staging = create_staging(&dest).await?;
    let staged: Vec<PathBuf> = rels
        .iter()
        .map(|c| {
            let mut p = staging.clone();
            p.extend(c);
            p
        })
        .collect();
    let mut rx = Receiver {
        entries,
        staged,
        open: HashMap::new(),
        done: HashSet::new(),
    };
    let mut prog = TransferProgress {
        transfer_id,
        bytes_done: 0,
        total_bytes,
        entries_done: 0,
        entries_total: files_total,
        current: None,
    };

    let res: Result<Vec<String>> = async {
        // Directories are created in the staging area too, so nothing is visible early.
        for (e, p) in rx.entries.iter().zip(&rx.staged) {
            if e.is_dir {
                fs::create_dir_all(p).await?;
            }
        }
        progress(&prog);
        loop {
            if opts.cancel.is_cancelled() {
                return Err(VfsError::Cancelled("cancelled by receiver".into()));
            }
            let Some(msg) = read_frame::<_, TransferMessage>(reader).await? else {
                return Err(VfsError::Protocol("stream ended before Complete".into()));
            };
            match msg {
                TransferMessage::Chunk {
                    entry,
                    offset,
                    data,
                } => {
                    let size = rx.entry(entry)?.size;
                    let rel = rx.entry(entry)?.rel_path.clone();
                    let of = rx.open_entry(entry).await?;
                    if offset != of.written {
                        return Err(VfsError::Protocol(format!(
                            "out-of-order chunk for entry {entry}"
                        )));
                    }
                    let chunk_len = u64::try_from(data.len())
                        .map_err(|_| VfsError::Protocol("chunk too large".into()))?;
                    let end = offset
                        .checked_add(chunk_len)
                        .ok_or_else(|| VfsError::Protocol("chunk offset overflows".into()))?;
                    if end > size {
                        return Err(VfsError::Protocol(format!(
                            "entry {entry} exceeds its offered size"
                        )));
                    }
                    of.file.write_all(&data).await?;
                    of.hasher.update(&data);
                    of.written = end;
                    // Bounded by the (checked) offer total.
                    prog.bytes_done += chunk_len;
                    prog.current = Some(rel);
                    progress(&prog);
                }
                TransferMessage::EntryDone {
                    entry,
                    blake3: digest,
                } => {
                    rx.open_entry(entry).await?;
                    let mut of = rx.open.remove(&entry).expect("opened");
                    of.file.flush().await?;
                    of.file.sync_all().await.ok();
                    drop(of.file);
                    if of.hasher.finalize().as_bytes() != &digest {
                        return Err(VfsError::Integrity(format!(
                            "BLAKE3 mismatch for {:?}",
                            rx.entries[entry as usize].rel_path
                        )));
                    }
                    rx.done.insert(entry);
                    prog.entries_done += 1;
                    progress(&prog);
                }
                TransferMessage::Complete => {
                    let missing = rx
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(i, e)| !e.is_dir && !rx.done.contains(&(*i as u32)))
                        .count();
                    if missing > 0 {
                        return Err(VfsError::Protocol(format!(
                            "Complete received with {missing} unfinished file(s)"
                        )));
                    }
                    // Everything is verified: publish atomically per top-level item.
                    let (st, de, ro) = (staging.clone(), dest.clone(), roots.clone());
                    return tokio::task::spawn_blocking(move || publish(&st, &de, &ro))
                        .await
                        .map_err(|e| VfsError::Io(std::io::Error::other(e)))?;
                }
                TransferMessage::Cancel { reason } => return Err(VfsError::Cancelled(reason)),
                TransferMessage::Offer { .. } => {
                    return Err(VfsError::Protocol("unexpected second Offer".into()));
                }
            }
        }
    }
    .await;

    // Close any half-written file, then drop the staging area (empty after a publish).
    rx.open.clear();
    if let Err(e) = fs::remove_dir_all(&staging).await {
        tracing::warn!("transfer: could not remove {}: {e}", staging.display());
    }
    let finals = res?;
    let rename: HashMap<&str, &str> = roots
        .iter()
        .map(String::as_str)
        .zip(finals.iter().map(String::as_str))
        .collect();
    let files = rx
        .entries
        .iter()
        .zip(&rels)
        .filter(|(e, _)| !e.is_dir)
        .map(|(_, c)| {
            let mut p = dest.join(rename[c[0].as_str()]);
            p.extend(&c[1..]);
            p
        })
        .collect();
    prog.current = None;
    progress(&prog);
    Ok(ReceiveSummary {
        transfer_id,
        roots: finals.iter().map(|f| dest.join(f)).collect(),
        files,
        bytes: prog.bytes_done,
        drop_target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbering() {
        assert_eq!(numbered("file.txt", 1), "file (1).txt");
        assert_eq!(numbered("archive.tar.gz", 2), "archive.tar (2).gz");
        assert_eq!(numbered("dir", 3), "dir (3)");
        assert_eq!(numbered(".bashrc", 1), ".bashrc (1)");
    }
}
