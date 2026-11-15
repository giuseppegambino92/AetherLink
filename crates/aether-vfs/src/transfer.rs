//! Drag-and-drop file transfer over a `TransferMessage` stream.
//!
//! Sender: [`send_paths`] walks the given files/directories (symlinks are skipped, not
//! followed), sends an `Offer`, then each file as `Chunk`s of [`TRANSFER_CHUNK_SIZE`]
//! followed by `EntryDone` with its BLAKE3 digest, then `Complete`.
//!
//! Receiver: [`receive`] validates every `rel_path` (relative, no `..`, NUL, drive/UNC
//! prefixes), gives each top-level item a non-colliding name in the destination
//! (`file (1).txt`, `dir (1)`), writes files to hidden temp files next to their final
//! location and atomically renames them only after the BLAKE3 digest matches. Files are
//! bounded by their offered size. On error or cancellation, temp files are removed;
//! already verified files are kept.

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

async fn unique_name(dir: &Path, name: &str, taken: &HashSet<String>) -> String {
    let mut n = 0;
    loop {
        let cand = if n == 0 {
            name.to_string()
        } else {
            numbered(name, n)
        };
        if !taken.contains(&cand) && fs::symlink_metadata(dir.join(&cand)).await.is_err() {
            return cand;
        }
        n += 1;
    }
}

struct OpenFile {
    tmp: PathBuf,
    file: File,
    hasher: blake3::Hasher,
    written: u64,
}

struct Receiver {
    entries: Vec<TransferEntry>,
    /// Final path per entry index.
    finals: Vec<PathBuf>,
    open: HashMap<u32, OpenFile>,
    done: HashSet<u32>,
    files: Vec<PathBuf>,
    transfer_id: u64,
}

impl Receiver {
    async fn cleanup(&mut self) {
        for (_, of) in self.open.drain() {
            drop(of.file);
            let _ = fs::remove_file(&of.tmp).await;
        }
    }

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
            let fin = &self.finals[idx as usize];
            let parent = fin.parent().expect("final path has a parent").to_path_buf();
            fs::create_dir_all(&parent).await?;
            let tmp = parent.join(format!(
                ".{}.aetherpart-{:x}-{idx}",
                fin.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
                self.transfer_id
            ));
            let file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .await?;
            self.open.insert(
                idx,
                OpenFile {
                    tmp,
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

    // Validate every path and map top-level names to non-colliding destinations.
    let mut root_map: HashMap<String, String> = HashMap::new();
    let mut taken = HashSet::new();
    let mut roots = Vec::new();
    let mut finals = Vec::with_capacity(entries.len());
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
        let root = match root_map.get(comps[0]) {
            Some(r) => r.clone(),
            None => {
                let r = unique_name(&dest, comps[0], &taken).await;
                taken.insert(r.clone());
                root_map.insert(comps[0].to_string(), r.clone());
                roots.push(dest.join(&r));
                r
            }
        };
        let mut p = dest.join(root);
        p.extend(&comps[1..]);
        finals.push(p);
    }
    let files_total = entries.iter().filter(|e| !e.is_dir).count() as u32;
    let offered: u64 = entries.iter().map(|e| e.size).sum();
    if offered != total_bytes {
        return Err(VfsError::Protocol(
            "offer total does not match entry sizes".into(),
        ));
    }

    // Directories are created up front (they live under freshly reserved root names).
    for (e, p) in entries.iter().zip(&finals) {
        if e.is_dir {
            fs::create_dir_all(p).await?;
        }
    }

    let mut rx = Receiver {
        entries,
        finals,
        open: HashMap::new(),
        done: HashSet::new(),
        files: Vec::new(),
        transfer_id,
    };
    let mut prog = TransferProgress {
        transfer_id,
        bytes_done: 0,
        total_bytes,
        entries_done: 0,
        entries_total: files_total,
        current: None,
    };
    progress(&prog);

    let res: Result<()> = async {
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
                    if offset + data.len() as u64 > size {
                        return Err(VfsError::Protocol(format!(
                            "entry {entry} exceeds its offered size"
                        )));
                    }
                    of.file.write_all(&data).await?;
                    of.hasher.update(&data);
                    of.written += data.len() as u64;
                    prog.bytes_done += data.len() as u64;
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
                        let _ = fs::remove_file(&of.tmp).await;
                        return Err(VfsError::Integrity(format!(
                            "BLAKE3 mismatch for {:?}",
                            rx.entries[entry as usize].rel_path
                        )));
                    }
                    let mut fin = rx.finals[entry as usize].clone();
                    if fs::symlink_metadata(&fin).await.is_ok() {
                        // Something appeared at the reserved name meanwhile: pick another.
                        let parent = fin.parent().expect("has parent").to_path_buf();
                        let name = fin
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("file")
                            .to_string();
                        fin = parent.join(unique_name(&parent, &name, &HashSet::new()).await);
                    }
                    fs::rename(&of.tmp, &fin).await?;
                    rx.done.insert(entry);
                    rx.files.push(fin);
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
                    return Ok(());
                }
                TransferMessage::Cancel { reason } => return Err(VfsError::Cancelled(reason)),
                TransferMessage::Offer { .. } => {
                    return Err(VfsError::Protocol("unexpected second Offer".into()));
                }
            }
        }
    }
    .await;

    match res {
        Ok(()) => {
            prog.current = None;
            progress(&prog);
            Ok(ReceiveSummary {
                transfer_id,
                roots,
                files: std::mem::take(&mut rx.files),
                bytes: prog.bytes_done,
                drop_target,
            })
        }
        Err(e) => {
            rx.cleanup().await;
            Err(e)
        }
    }
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
