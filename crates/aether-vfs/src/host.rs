//! Host side: share registry + sandboxed request handler.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use aether_protocol::{
    DirEntry, FileAttr, FileKind, ShareInfo, VFS_CHUNK_SIZE, VfsErrorKind, VfsRequest, VfsResponse,
};
use parking_lot::RwLock;

use crate::error::{Result, VfsError};
use crate::path::{split_rel_path, symlink_target_is_contained};

/// Maximum number of bytes returned by a single `Read` request.
pub const MAX_READ_LEN: u32 = 4 * 1024 * 1024;

/// One exported directory.
#[derive(Debug)]
pub struct Share {
    pub id: u32,
    pub name: String,
    /// Canonical (symlink-free, absolute) root directory.
    pub root: PathBuf,
    pub read_only: bool,
    /// Serializes namespace-changing operations (see crate docs).
    ns_lock: RwLock<()>,
    /// Descriptor of the root directory: every operation resolves from it (see `beneath`).
    #[cfg(unix)]
    root_fd: std::os::fd::OwnedFd,
    #[cfg(unix)]
    root_dev: u64,
}

impl Share {
    pub fn info(&self) -> ShareInfo {
        ShareInfo {
            id: self.id,
            name: self.name.clone(),
            read_only: self.read_only,
        }
    }
}

/// Set of exported directories, keyed by share id.
#[derive(Debug, Default)]
pub struct ShareRegistry {
    shares: BTreeMap<u32, Arc<Share>>,
    next_id: u32,
}

impl ShareRegistry {
    pub fn new() -> Self {
        Self {
            shares: BTreeMap::new(),
            next_id: 1,
        }
    }

    /// Export `root` (must be an existing directory) under `name`; returns the new share id.
    pub fn add(
        &mut self,
        name: impl Into<String>,
        root: impl AsRef<Path>,
        read_only: bool,
    ) -> Result<u32> {
        let mut id = self.next_id.max(1);
        while self.shares.contains_key(&id) {
            id = id.wrapping_add(1).max(1);
        }
        self.add_with_id(id, name, root, read_only)?;
        Ok(id)
    }

    /// Export `root` with a caller-chosen id (replaces an existing share with the same id).
    pub fn add_with_id(
        &mut self,
        id: u32,
        name: impl Into<String>,
        root: impl AsRef<Path>,
        read_only: bool,
    ) -> Result<()> {
        let root = fs::canonicalize(root.as_ref())?;
        let md = fs::metadata(&root)?;
        if !md.is_dir() {
            return Err(VfsError::InvalidPath(format!(
                "share root {} is not a directory",
                root.display()
            )));
        }
        #[cfg(unix)]
        let (root_fd, root_dev) = {
            use std::os::fd::AsFd;
            let fd = crate::beneath::open_root(&root)?;
            let st = crate::beneath::fstat(fd.as_fd())?;
            (fd, st.st_dev as u64)
        };
        let share = Share {
            id,
            name: name.into(),
            root,
            read_only,
            ns_lock: RwLock::new(()),
            #[cfg(unix)]
            root_fd,
            #[cfg(unix)]
            root_dev,
        };
        self.shares.insert(id, Arc::new(share));
        self.next_id = self.next_id.max(id.wrapping_add(1));
        Ok(())
    }

    pub fn remove(&mut self, id: u32) -> bool {
        self.shares.remove(&id).is_some()
    }

    pub fn get(&self, id: u32) -> Option<Arc<Share>> {
        self.shares.get(&id).cloned()
    }

    pub fn list(&self) -> Vec<ShareInfo> {
        self.shares.values().map(|s| s.info()).collect()
    }

    pub fn len(&self) -> usize {
        self.shares.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shares.is_empty()
    }
}

type HResult<T> = std::result::Result<T, (VfsErrorKind, String)>;

fn err<T>(kind: VfsErrorKind, msg: impl Into<String>) -> HResult<T> {
    Err((kind, msg.into()))
}

/// Map an `io::Error` to the wire error kind.
pub(crate) fn io_kind(e: &io::Error) -> VfsErrorKind {
    use io::ErrorKind as K;
    match e.kind() {
        K::NotFound => VfsErrorKind::NotFound,
        K::PermissionDenied => VfsErrorKind::PermissionDenied,
        K::AlreadyExists => VfsErrorKind::AlreadyExists,
        K::NotADirectory => VfsErrorKind::NotADirectory,
        K::IsADirectory => VfsErrorKind::IsADirectory,
        K::DirectoryNotEmpty => VfsErrorKind::NotEmpty,
        K::ReadOnlyFilesystem => VfsErrorKind::ReadOnly,
        K::InvalidInput | K::InvalidFilename => VfsErrorKind::InvalidPath,
        _ => {
            #[cfg(unix)]
            if e.raw_os_error() == Some(libc::ELOOP) {
                // O_NOFOLLOW hit a symlink that appeared after the sandbox check.
                return VfsErrorKind::PermissionDenied;
            }
            VfsErrorKind::Io
        }
    }
}

fn io_err(e: io::Error) -> (VfsErrorKind, String) {
    (io_kind(&e), e.to_string())
}

/// Path-sandboxed VFS request handler.
#[derive(Debug, Default)]
pub struct VfsHost {
    registry: RwLock<ShareRegistry>,
}

impl VfsHost {
    pub fn new(reg: ShareRegistry) -> Self {
        Self {
            registry: RwLock::new(reg),
        }
    }

    /// Add a share at runtime.
    pub fn add_share(
        &self,
        name: impl Into<String>,
        root: impl AsRef<Path>,
        read_only: bool,
    ) -> Result<u32> {
        self.registry.write().add(name, root, read_only)
    }

    /// Remove a share at runtime. In-flight requests on it complete normally.
    pub fn remove_share(&self, id: u32) -> bool {
        self.registry.write().remove(id)
    }

    pub fn shares(&self) -> Vec<ShareInfo> {
        self.registry.read().list()
    }

    /// Handle one request. Blocking (filesystem I/O): call from a blocking context.
    pub fn handle(&self, r: VfsRequest) -> VfsResponse {
        match self.handle_inner(r) {
            Ok(resp) => resp,
            Err((kind, message)) => VfsResponse::Error { kind, message },
        }
    }

    fn share(&self, id: u32) -> HResult<Arc<Share>> {
        self.registry
            .read()
            .get(id)
            .ok_or((VfsErrorKind::NoSuchShare, format!("no share with id {id}")))
    }

    fn handle_inner(&self, r: VfsRequest) -> HResult<VfsResponse> {
        match r {
            VfsRequest::ListShares => Ok(VfsResponse::Shares(self.shares())),
            VfsRequest::Stat { share, path } => {
                let s = self.share(share)?;
                let _g = s.ns_lock.read();
                let comps = comps(&path)?;
                Ok(VfsResponse::Attr(ops::stat(&s, &comps, &path)?))
            }
            VfsRequest::ReadDir { share, path } => {
                let s = self.share(share)?;
                let _g = s.ns_lock.read();
                let comps = comps(&path)?;
                let mut entries = ops::read_dir(&s, &comps, &path)?;
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(VfsResponse::Entries(entries))
            }
            VfsRequest::Read {
                share,
                path,
                offset,
                len,
            } => {
                let s = self.share(share)?;
                let _g = s.ns_lock.read();
                let comps = comps(&path)?;
                let mut f = ops::open_existing(&s, &comps, false)?;
                let size = regular_file(&f, &path)?;
                let len = len.min(MAX_READ_LEN) as usize;
                let data = read_at(&mut f, offset, len).map_err(io_err)?;
                let eof = offset.saturating_add(data.len() as u64) >= size;
                Ok(VfsResponse::Data { data, eof })
            }
            VfsRequest::ChunkHashes { share, path } => {
                let s = self.share(share)?;
                let _g = s.ns_lock.read();
                let comps = comps(&path)?;
                let mut f = ops::open_existing(&s, &comps, false)?;
                regular_file(&f, &path)?;
                let mut hashes = Vec::new();
                let mut buf = vec![0u8; VFS_CHUNK_SIZE as usize];
                loop {
                    let n = read_full(&mut f, &mut buf).map_err(io_err)?;
                    if n == 0 {
                        break;
                    }
                    hashes.push(*blake3::hash(&buf[..n]).as_bytes());
                    if n < buf.len() {
                        break;
                    }
                }
                Ok(VfsResponse::Hashes {
                    chunk_size: VFS_CHUNK_SIZE,
                    hashes,
                })
            }
            VfsRequest::Write {
                share,
                path,
                offset,
                data,
            } => {
                let s = self.share(share)?;
                writable(&s)?;
                let _g = s.ns_lock.read();
                let comps = comps(&path)?;
                let len = u64::try_from(data.len())
                    .ok()
                    .filter(|l| offset.checked_add(*l).is_some())
                    .ok_or((VfsErrorKind::InvalidPath, "write range overflows".to_string()))?;
                let bytes = u32::try_from(len)
                    .map_err(|_| (VfsErrorKind::Io, "write too large".to_string()))?;
                let mut f = ops::open_existing(&s, &comps, true)?;
                regular_file(&f, &path)?;
                f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
                f.write_all(&data).map_err(io_err)?;
                Ok(VfsResponse::Written {
                    bytes,
                    attr: ops::file_attr(&s, &f, &path)?,
                })
            }
            VfsRequest::Truncate { share, path, size } => {
                let s = self.share(share)?;
                writable(&s)?;
                let _g = s.ns_lock.read();
                let comps = comps(&path)?;
                let f = ops::open_existing(&s, &comps, true)?;
                regular_file(&f, &path)?;
                f.set_len(size).map_err(io_err)?;
                Ok(VfsResponse::Attr(ops::file_attr(&s, &f, &path)?))
            }
            VfsRequest::Create {
                share,
                path,
                exclusive,
            } => {
                let s = self.share(share)?;
                writable(&s)?;
                let _g = s.ns_lock.write();
                let comps = nonroot(comps(&path)?)?;
                Ok(VfsResponse::Attr(ops::create(&s, &comps, exclusive, &path)?))
            }
            VfsRequest::Mkdir { share, path } => {
                let s = self.share(share)?;
                writable(&s)?;
                let _g = s.ns_lock.write();
                let comps = nonroot(comps(&path)?)?;
                Ok(VfsResponse::Attr(ops::mkdir(&s, &comps, &path)?))
            }
            VfsRequest::Remove { share, path } => {
                let s = self.share(share)?;
                writable(&s)?;
                let _g = s.ns_lock.write();
                let comps = nonroot(comps(&path)?)?;
                ops::remove(&s, &comps)?;
                Ok(VfsResponse::Ok)
            }
            VfsRequest::Rename { share, from, to } => {
                let s = self.share(share)?;
                writable(&s)?;
                let _g = s.ns_lock.write();
                let fc = nonroot(comps(&from)?)?;
                let tc = nonroot(comps(&to)?)?;
                ops::rename(&s, &fc, &tc)?;
                Ok(VfsResponse::Ok)
            }
            VfsRequest::Symlink {
                share,
                path,
                target,
            } => {
                let s = self.share(share)?;
                writable(&s)?;
                let _g = s.ns_lock.write();
                let comps = nonroot(comps(&path)?)?;
                symlink_target_is_contained(&comps[..comps.len() - 1], &target)
                    .map_err(|m| (VfsErrorKind::InvalidPath, m))?;
                Ok(VfsResponse::Attr(ops::symlink(&s, &comps, &target, &path)?))
            }
            VfsRequest::ReadLink { share, path } => {
                let s = self.share(share)?;
                let _g = s.ns_lock.read();
                let comps = nonroot(comps(&path)?)?;
                Ok(VfsResponse::Link {
                    target: ops::read_link(&s, &comps)?,
                })
            }
        }
    }
}

fn comps(path: &str) -> HResult<Vec<&str>> {
    split_rel_path(path).map_err(|m| (VfsErrorKind::InvalidPath, m))
}

fn nonroot(c: Vec<&str>) -> HResult<Vec<&str>> {
    if c.is_empty() {
        return err(
            VfsErrorKind::InvalidPath,
            "operation not allowed on the share root",
        );
    }
    Ok(c)
}

fn writable(s: &Share) -> HResult<()> {
    if s.read_only {
        return err(
            VfsErrorKind::ReadOnly,
            format!("share {:?} is read-only", s.name),
        );
    }
    Ok(())
}

/// Require an opened entry to be a regular file; returns its size.
fn regular_file(f: &File, path: &str) -> HResult<u64> {
    let md = f.metadata().map_err(io_err)?;
    if md.is_dir() {
        return err(
            VfsErrorKind::IsADirectory,
            format!("{path:?} is a directory"),
        );
    }
    if !md.is_file() {
        return err(VfsErrorKind::Io, format!("{path:?} is not a regular file"));
    }
    Ok(md.len())
}

#[cfg_attr(unix, allow(dead_code))]
fn join_rel(comps: &[&str], name: &str) -> String {
    let mut s = comps.join("/");
    if !s.is_empty() {
        s.push('/');
    }
    s.push_str(name);
    s
}

#[cfg(test)]
thread_local! {
    /// Test hook run between resolving a parent directory and operating in it, to
    /// simulate a concurrent local process deterministically.
    pub(crate) static RACE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn race_hook() {
    if let Some(f) = RACE_HOOK.with(|h| h.borrow_mut().take()) {
        f();
    }
}

#[cfg(not(test))]
#[inline(always)]
fn race_hook() {}

/// Unix: descriptor-relative operations (see [`crate::beneath`]); immune to directories
/// being swapped for symlinks between resolution and use.
#[cfg(unix)]
mod ops {
    use super::*;
    use crate::beneath as b;
    use std::os::fd::{AsFd, OwnedFd};

    fn bytes(s: &str) -> &[u8] {
        s.as_bytes()
    }

    fn parent(s: &Share, comps: &[&str]) -> HResult<OwnedFd> {
        b::open_dir(s.root_fd.as_fd(), &comps[..comps.len() - 1]).map_err(io_err)
    }

    fn last<'a>(comps: &[&'a str]) -> &'a str {
        comps.last().expect("non-empty components")
    }

    pub(super) fn stat(s: &Share, comps: &[&str], rel: &str) -> HResult<FileAttr> {
        if comps.is_empty() {
            let st = b::fstat(s.root_fd.as_fd()).map_err(io_err)?;
            return Ok(attr_of(s, &st, rel));
        }
        let dir = parent(s, comps)?;
        let st = b::lstatat(dir.as_fd(), bytes(last(comps))).map_err(io_err)?;
        Ok(attr_of(s, &st, rel))
    }

    pub(super) fn read_dir(s: &Share, comps: &[&str], _rel: &str) -> HResult<Vec<DirEntry>> {
        let dir = b::open_dir(s.root_fd.as_fd(), comps).map_err(io_err)?;
        let mut entries = Vec::new();
        for name in b::read_dir_names(dir.as_fd()).map_err(io_err)? {
            let Ok(name) = String::from_utf8(name) else {
                tracing::debug!("skipping non-UTF-8 file name");
                continue;
            };
            let Ok(st) = b::lstatat(dir.as_fd(), name.as_bytes()) else {
                continue;
            };
            let rel = join_rel(comps, &name);
            entries.push(DirEntry {
                attr: attr_of(s, &st, &rel),
                name,
            });
        }
        Ok(entries)
    }

    pub(super) fn open_existing(s: &Share, comps: &[&str], write: bool) -> HResult<File> {
        // O_NONBLOCK: never hang on a FIFO; non-regular files are refused by the caller.
        let flags = if write { libc::O_WRONLY } else { libc::O_RDONLY } | libc::O_NONBLOCK;
        match b::open_follow(s.root_fd.as_fd(), comps, flags) {
            Ok(fd) => Ok(File::from(fd)),
            // Opening a directory for writing.
            Err(e) if e.raw_os_error() == Some(libc::EISDIR) => err(
                VfsErrorKind::IsADirectory,
                format!("{:?} is a directory", comps.join("/")),
            ),
            Err(e) => Err(io_err(e)),
        }
    }

    pub(super) fn file_attr(s: &Share, f: &File, rel: &str) -> HResult<FileAttr> {
        let st = b::fstat(f.as_fd()).map_err(io_err)?;
        Ok(attr_of(s, &st, rel))
    }

    pub(super) fn create(
        s: &Share,
        comps: &[&str],
        exclusive: bool,
        rel: &str,
    ) -> HResult<FileAttr> {
        let dir = parent(s, comps)?;
        let name = bytes(last(comps));
        race_hook();
        match b::lstatat(dir.as_fd(), name) {
            Ok(_) if exclusive => err(
                VfsErrorKind::AlreadyExists,
                format!("{rel:?} already exists"),
            ),
            Ok(st) if b::is_dir(&st) => {
                err(VfsErrorKind::IsADirectory, format!("{rel:?} is a directory"))
            }
            Ok(st) if b::is_symlink(&st) => {
                // Non-exclusive create through an existing symlink: it must stay inside.
                let st = b::stat_follow(s.root_fd.as_fd(), comps).map_err(io_err)?;
                Ok(attr_of(s, &st, rel))
            }
            Ok(st) => Ok(attr_of(s, &st, rel)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let f = b::openat(
                    dir.as_fd(),
                    name,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                    0o666,
                )
                .map_err(io_err)?;
                let st = b::fstat(f.as_fd()).map_err(io_err)?;
                Ok(attr_of(s, &st, rel))
            }
            Err(e) => Err(io_err(e)),
        }
    }

    pub(super) fn mkdir(s: &Share, comps: &[&str], rel: &str) -> HResult<FileAttr> {
        let dir = parent(s, comps)?;
        let name = bytes(last(comps));
        race_hook();
        b::mkdirat(dir.as_fd(), name, 0o777).map_err(io_err)?;
        let st = b::lstatat(dir.as_fd(), name).map_err(io_err)?;
        Ok(attr_of(s, &st, rel))
    }

    pub(super) fn remove(s: &Share, comps: &[&str]) -> HResult<()> {
        let dir = parent(s, comps)?;
        let name = bytes(last(comps));
        race_hook();
        let st = b::lstatat(dir.as_fd(), name).map_err(io_err)?;
        b::unlinkat(dir.as_fd(), name, b::is_dir(&st)).map_err(io_err)
    }

    pub(super) fn rename(s: &Share, from: &[&str], to: &[&str]) -> HResult<()> {
        let fd = parent(s, from)?;
        let td = parent(s, to)?;
        race_hook();
        b::renameat(fd.as_fd(), bytes(last(from)), td.as_fd(), bytes(last(to))).map_err(io_err)
    }

    pub(super) fn symlink(
        s: &Share,
        comps: &[&str],
        target: &str,
        rel: &str,
    ) -> HResult<FileAttr> {
        let dir = parent(s, comps)?;
        let name = bytes(last(comps));
        race_hook();
        b::symlinkat(target.as_bytes(), dir.as_fd(), name).map_err(io_err)?;
        let st = b::lstatat(dir.as_fd(), name).map_err(io_err)?;
        Ok(attr_of(s, &st, rel))
    }

    pub(super) fn read_link(s: &Share, comps: &[&str]) -> HResult<String> {
        let dir = parent(s, comps)?;
        let t = b::readlinkat(dir.as_fd(), bytes(last(comps))).map_err(io_err)?;
        String::from_utf8(t).map_err(|_| {
            (
                VfsErrorKind::Io,
                "symlink target is not valid UTF-8".to_string(),
            )
        })
    }

    fn ts(sec: i64, nsec: i64) -> i128 {
        sec as i128 * 1_000_000_000 + nsec as i128
    }

    /// Wire attributes from a `stat` (`rel` unused on Unix: inode numbers are the ids).
    #[allow(clippy::unnecessary_cast)] // `stat` field types differ between Unixes
    pub(super) fn attr_of(s: &Share, st: &libc::stat, _rel: &str) -> FileAttr {
        let kind = if b::is_symlink(st) {
            FileKind::Symlink
        } else if b::is_dir(st) {
            FileKind::Directory
        } else {
            FileKind::File
        };
        let dev = st.st_dev as u64;
        let ino = st.st_ino as u64;
        let file_id = if dev == s.root_dev {
            ino
        } else {
            // Nested mount point: mix the device in to avoid inode collisions.
            let mut h = blake3::Hasher::new();
            h.update(&dev.to_le_bytes());
            h.update(&ino.to_le_bytes());
            u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap()) | (1 << 63)
        };
        let mode = (st.st_mode as u32) & 0o7777;
        FileAttr {
            file_id,
            kind,
            size: st.st_size.max(0) as u64,
            mode,
            mtime_ns: ts(st.st_mtime as i64, st.st_mtime_nsec as i64),
            ctime_ns: ts(st.st_ctime as i64, st.st_ctime_nsec as i64),
            read_only: s.read_only || mode & 0o222 == 0,
        }
    }
}

/// Other platforms (Windows): every directory on the way from the share root to the parent
/// of the target is *pinned* — opened, without following reparse points, with a share mode
/// that omits `FILE_SHARE_DELETE` — and held open for the whole operation. While a directory
/// is pinned, no process can rename it, delete it or replace it with a junction or symlink
/// (all of these need `DELETE` access, which the share mode refuses), and NTFS refuses to
/// rename an ancestor of an open handle. The path the operation then uses therefore names
/// the same directories that were checked: the Windows counterpart of the descriptor-relative
/// walk used on Unix. The final component is never followed by namespace operations
/// (create-new, mkdir, remove, rename and symlink act on the link itself); files that are
/// opened are additionally re-checked by handle (`GetFinalPathNameByHandleW`).
///
/// Links met while walking are followed only if they stay beneath the root, with the same
/// rules as on Unix (relative targets walked inside the share, at most 40 links; absolute
/// targets accepted only when they point inside the share root).
#[cfg(not(unix))]
mod ops {
    use super::*;
    use std::collections::VecDeque;
    use std::fs::{Metadata, OpenOptions};
    use std::path::Component;

    /// Maximum number of links expanded while resolving one path (as on Unix).
    const MAX_LINKS: u32 = 40;

    /// Directories held open (pinned) for the duration of one operation, root first.
    pub(super) struct Pinned {
        _held: Vec<File>,
        /// Path of the deepest pinned directory.
        pub(super) dir: PathBuf,
    }

    fn outside<T>() -> HResult<T> {
        err(
            VfsErrorKind::PermissionDenied,
            "path resolves outside of the share",
        )
    }

    /// Open `p` itself (a reparse point is not followed) so that it can be neither renamed
    /// nor deleted while the handle lives.
    #[cfg(windows)]
    fn open_pin(p: &Path) -> io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
            FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        // A data right (`FILE_LIST_DIRECTORY` = `FILE_READ_DATA`) is required: an open with
        // attribute rights only is not recorded for share-access checks and would pin nothing.
        OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(p)
    }

    #[cfg(not(windows))]
    fn open_pin(p: &Path) -> io::Result<File> {
        File::open(p)
    }

    /// Strip the `\\?\` verbatim prefix so link targets and the canonical root compare.
    fn plain(p: &Path) -> PathBuf {
        let s = p.as_os_str().to_string_lossy();
        match s.strip_prefix(r"\\?\UNC\") {
            Some(rest) => PathBuf::from(format!(r"\\{rest}")),
            None => PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()),
        }
    }

    /// Resolve and pin the directory `root/comps…`.
    pub(super) fn pin_dir(s: &Share, comps: &[&str]) -> HResult<Pinned> {
        let root = open_pin(&s.root).map_err(io_err)?;
        check_inside(s, &root)?;
        let md = root.metadata().map_err(io_err)?;
        if md.file_type().is_symlink() || !md.is_dir() {
            return err(VfsErrorKind::NotADirectory, "share root is not a directory");
        }
        let root_plain = plain(&s.root);
        let mut held = vec![root];
        let mut dir = s.root.clone();
        let mut queue: VecDeque<String> = comps.iter().map(|c| (*c).to_owned()).collect();
        let mut links = 0u32;
        while let Some(c) = queue.pop_front() {
            match c.as_str() {
                "" | "." => continue,
                ".." => {
                    // Applied to the walk's own stack; never above the root.
                    if held.len() == 1 {
                        return outside();
                    }
                    held.pop();
                    dir.pop();
                    continue;
                }
                _ => {}
            }
            let next = dir.join(&c);
            let f = open_pin(&next).map_err(io_err)?;
            let md = f.metadata().map_err(io_err)?;
            if md.file_type().is_symlink() {
                links += 1;
                if links > MAX_LINKS {
                    return err(VfsErrorKind::Io, "too many levels of symbolic links");
                }
                drop(f);
                let target = fs::read_link(&next).map_err(io_err)?;
                let parts = if target.has_root() {
                    // Junctions always hold absolute targets: accept those inside the share
                    // and restart the walk from the (still pinned) root.
                    let Ok(rest) = plain(&target).strip_prefix(&root_plain).map(Path::to_path_buf)
                    else {
                        return outside();
                    };
                    held.truncate(1);
                    dir = s.root.clone();
                    path_parts(&rest)?
                } else {
                    path_parts(&target)?
                };
                for t in parts.into_iter().rev() {
                    queue.push_front(t);
                }
                continue;
            }
            if !md.is_dir() {
                return err(VfsErrorKind::NotADirectory, "parent is not a directory");
            }
            held.push(f);
            dir = next;
        }
        Ok(Pinned { _held: held, dir })
    }

    /// Components of a relative link target (`..` kept for the walk, drive or root refused).
    fn path_parts(p: &Path) -> HResult<Vec<String>> {
        let mut out = Vec::new();
        for c in p.components() {
            match c {
                Component::Normal(n) => match n.to_str() {
                    Some(n) => out.push(n.to_owned()),
                    None => return err(VfsErrorKind::Io, "link target is not valid UTF-8"),
                },
                Component::ParentDir => out.push("..".to_owned()),
                Component::CurDir => {}
                Component::Prefix(_) | Component::RootDir => return outside(),
            }
        }
        Ok(out)
    }

    /// Pinned parent directory (inside the share) + final component (not followed).
    fn parent_and_name<'a>(s: &Share, comps: &[&'a str]) -> HResult<(Pinned, &'a str)> {
        let (name, parent) = comps.split_last().expect("non-empty components");
        Ok((pin_dir(s, parent)?, name))
    }

    fn open_file(s: &Share, p: &Path, opts: &OpenOptions) -> HResult<File> {
        let f = opts.open(p).map_err(io_err)?;
        check_inside(s, &f)?;
        Ok(f)
    }

    #[cfg(windows)]
    fn check_inside(s: &Share, f: &File) -> HResult<()> {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
        };
        let mut buf = vec![0u16; 512];
        loop {
            let n = unsafe {
                GetFinalPathNameByHandleW(
                    f.as_raw_handle() as _,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
                )
            } as usize;
            if n == 0 {
                return Err(io_err(io::Error::last_os_error()));
            }
            if n < buf.len() {
                buf.truncate(n);
                break;
            }
            buf.resize(n + 1, 0);
        }
        let real = PathBuf::from(OsString::from_wide(&buf));
        if !real.starts_with(&s.root) {
            return outside();
        }
        Ok(())
    }

    #[cfg(not(windows))]
    fn check_inside(_s: &Share, _f: &File) -> HResult<()> {
        Ok(())
    }

    pub(super) fn stat(s: &Share, comps: &[&str], rel: &str) -> HResult<FileAttr> {
        if comps.is_empty() {
            let md = fs::metadata(&s.root).map_err(io_err)?;
            return Ok(attr_of(s, &md, rel));
        }
        let (parent, name) = parent_and_name(s, comps)?;
        let md = fs::symlink_metadata(parent.dir.join(name)).map_err(io_err)?;
        Ok(attr_of(s, &md, rel))
    }

    pub(super) fn read_dir(s: &Share, comps: &[&str], rel: &str) -> HResult<Vec<DirEntry>> {
        let pinned = pin_dir(s, comps).map_err(|e| {
            if e.0 == VfsErrorKind::NotADirectory {
                (e.0, format!("{rel:?} is not a directory"))
            } else {
                e
            }
        })?;
        let dir = &pinned.dir;
        let mut entries = Vec::new();
        for ent in fs::read_dir(dir).map_err(io_err)? {
            let ent = ent.map_err(io_err)?;
            let Ok(name) = ent.file_name().into_string() else {
                tracing::debug!("skipping non-UTF-8 file name in {}", dir.display());
                continue;
            };
            let Ok(md) = ent.metadata() else { continue };
            let rel = join_rel(comps, &name);
            entries.push(DirEntry {
                attr: attr_of(s, &md, &rel),
                name,
            });
        }
        Ok(entries)
    }

    pub(super) fn open_existing(s: &Share, comps: &[&str], write: bool) -> HResult<File> {
        let Some((name, parent)) = comps.split_last() else {
            return err(VfsErrorKind::IsADirectory, "the share root is a directory");
        };
        let pinned = pin_dir(s, parent)?;
        let mut o = OpenOptions::new();
        if write {
            o.write(true);
        } else {
            o.read(true);
        }
        // A final link may be followed; the opened handle is then checked to be inside.
        open_file(s, &pinned.dir.join(name), &o)
    }

    pub(super) fn file_attr(s: &Share, f: &File, rel: &str) -> HResult<FileAttr> {
        let md = f.metadata().map_err(io_err)?;
        Ok(attr_of(s, &md, rel))
    }

    pub(super) fn create(
        s: &Share,
        comps: &[&str],
        exclusive: bool,
        rel: &str,
    ) -> HResult<FileAttr> {
        let (parent, name) = parent_and_name(s, comps)?;
        let full = parent.dir.join(name);
        race_hook();
        let md = match fs::symlink_metadata(&full) {
            Ok(_) if exclusive => {
                return err(
                    VfsErrorKind::AlreadyExists,
                    format!("{rel:?} already exists"),
                );
            }
            Ok(md) if md.is_dir() => {
                return err(VfsErrorKind::IsADirectory, format!("{rel:?} is a directory"));
            }
            Ok(md) if md.file_type().is_symlink() => {
                // Report the target's attributes, but only if the link stays inside.
                let f = open_file(s, &full, OpenOptions::new().read(true))?;
                f.metadata().map_err(io_err)?
            }
            Ok(md) => md,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // CREATE_NEW never follows a link planted at the final name.
                let f = open_file(s, &full, OpenOptions::new().write(true).create_new(true))?;
                f.metadata().map_err(io_err)?
            }
            Err(e) => return Err(io_err(e)),
        };
        Ok(attr_of(s, &md, rel))
    }

    pub(super) fn mkdir(s: &Share, comps: &[&str], rel: &str) -> HResult<FileAttr> {
        let (parent, name) = parent_and_name(s, comps)?;
        let full = parent.dir.join(name);
        race_hook();
        fs::create_dir(&full).map_err(io_err)?;
        let md = fs::symlink_metadata(&full).map_err(io_err)?;
        Ok(attr_of(s, &md, rel))
    }

    pub(super) fn remove(s: &Share, comps: &[&str]) -> HResult<()> {
        let (parent, name) = parent_and_name(s, comps)?;
        let full = parent.dir.join(name);
        race_hook();
        // DeleteFileW / RemoveDirectoryW act on a link itself, never on its target.
        let md = fs::symlink_metadata(&full).map_err(io_err)?;
        if md.is_dir() {
            fs::remove_dir(&full)
        } else {
            fs::remove_file(&full)
        }
        .map_err(io_err)
    }

    pub(super) fn rename(s: &Share, from: &[&str], to: &[&str]) -> HResult<()> {
        let (fp, fname) = parent_and_name(s, from)?;
        let (tp, tname) = parent_and_name(s, to)?;
        race_hook();
        // MoveFileExW moves a link itself, never its target.
        fs::rename(fp.dir.join(fname), tp.dir.join(tname)).map_err(io_err)
    }

    pub(super) fn symlink(
        s: &Share,
        comps: &[&str],
        target: &str,
        rel: &str,
    ) -> HResult<FileAttr> {
        let (parent, name) = parent_and_name(s, comps)?;
        let full = parent.dir.join(name);
        race_hook();
        make_symlink(target, &full).map_err(io_err)?;
        let md = fs::symlink_metadata(&full).map_err(io_err)?;
        Ok(attr_of(s, &md, rel))
    }

    pub(super) fn read_link(s: &Share, comps: &[&str]) -> HResult<String> {
        let (parent, name) = parent_and_name(s, comps)?;
        let t = fs::read_link(parent.dir.join(name)).map_err(io_err)?;
        Ok(t.to_str()
            .ok_or((
                VfsErrorKind::Io,
                "symlink target is not valid UTF-8".to_string(),
            ))?
            .replace(std::path::MAIN_SEPARATOR, "/"))
    }

    #[cfg(windows)]
    fn make_symlink(target: &str, link: &Path) -> io::Result<()> {
        // Requires SeCreateSymbolicLinkPrivilege or Developer Mode.
        std::os::windows::fs::symlink_file(target.replace('/', "\\"), link)
    }

    #[cfg(not(windows))]
    fn make_symlink(_target: &str, _link: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "symlinks are not supported on this platform",
        ))
    }

    /// Wire attributes for `md`. `rel` is the share-relative path (the file id is derived
    /// from it, as there are no inode numbers).
    pub(super) fn attr_of(s: &Share, md: &Metadata, rel: &str) -> FileAttr {
        let ft = md.file_type();
        let kind = if ft.is_symlink() {
            FileKind::Symlink
        } else if ft.is_dir() {
            FileKind::Directory
        } else {
            FileKind::File
        };
        let canonical_rel = split_rel_path(rel).map(|c| c.join("/")).unwrap_or_default();
        let mut h = blake3::Hasher::new();
        h.update(&s.id.to_le_bytes());
        h.update(canonical_rel.as_bytes());
        let file_id = u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap()).max(1);
        let mode = if md.is_dir() {
            0o755
        } else if md.permissions().readonly() {
            0o444
        } else {
            0o644
        };
        FileAttr {
            file_id,
            kind,
            size: md.len(),
            mode,
            mtime_ns: time_ns(md.modified()),
            ctime_ns: time_ns(md.created()),
            read_only: s.read_only || md.permissions().readonly(),
        }
    }
}

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn read_at(f: &mut File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let size = f.metadata()?.len();
    if offset >= size || len == 0 {
        return Ok(Vec::new());
    }
    let len = (len as u64).min(size - offset) as usize;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    let n = read_full(f, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

#[cfg_attr(unix, allow(dead_code))]
fn time_ns(t: io::Result<SystemTime>) -> i128 {
    match t {
        Ok(t) => match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        },
        Err(_) => 0,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn host_with_share(root: &Path) -> (VfsHost, u32) {
        let mut reg = ShareRegistry::new();
        let id = reg.add("s", root, false).unwrap();
        (VfsHost::new(reg), id)
    }

    fn denied(r: VfsResponse) {
        match r {
            VfsResponse::Error { kind, .. } => assert_eq!(kind, VfsErrorKind::PermissionDenied),
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
    }

    /// Swap `root/sub` for a symlink to `outside` (what a hostile local process would do).
    fn swap_hook(root: PathBuf, outside: PathBuf) {
        RACE_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                fs::rename(root.join("sub"), root.join("sub_moved")).unwrap();
                symlink(&outside, root.join("sub")).unwrap();
            }))
        });
    }

    #[test]
    fn directory_swapped_after_resolution_does_not_escape() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("share");
        let outside = t.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        type MakeRequest = Box<dyn Fn(u32) -> VfsRequest>;
        let ops: [(&str, MakeRequest); 3] = [
            (
                "create",
                Box::new(|share| VfsRequest::Create {
                    share,
                    path: "sub/new.txt".into(),
                    exclusive: true,
                }),
            ),
            (
                "mkdir",
                Box::new(|share| VfsRequest::Mkdir {
                    share,
                    path: "sub/new.txt".into(),
                }),
            ),
            (
                "symlink",
                Box::new(|share| VfsRequest::Symlink {
                    share,
                    path: "sub/new.txt".into(),
                    target: "x".into(),
                }),
            ),
        ];
        for (what, req) in ops {
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("sub")).unwrap();
            let (host, id) = host_with_share(&root);
            let root = fs::canonicalize(&root).unwrap();
            swap_hook(root.clone(), outside.clone());
            let r = host.handle(req(id));
            assert!(
                !matches!(r, VfsResponse::Error { .. }),
                "{what}: lands in the checked directory: {r:?}"
            );
            assert!(
                fs::symlink_metadata(root.join("sub_moved/new.txt")).is_ok(),
                "{what}"
            );
            assert!(
                fs::symlink_metadata(outside.join("new.txt")).is_err(),
                "{what} escaped"
            );
            // Any later request through the swapped-in symlink is refused.
            denied(host.handle(VfsRequest::Stat {
                share: id,
                path: "sub/new.txt".into(),
            }));
        }
    }

    #[test]
    fn remove_and_rename_cannot_be_redirected() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("share");
        let outside = t.path().join("outside");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("sub/victim"), b"in").unwrap();
        fs::write(outside.join("victim"), b"precious").unwrap();
        let (host, id) = host_with_share(&root);
        let root = fs::canonicalize(&root).unwrap();

        swap_hook(root.clone(), outside.clone());
        let r = host.handle(VfsRequest::Remove {
            share: id,
            path: "sub/victim".into(),
        });
        assert!(matches!(r, VfsResponse::Ok), "{r:?}");
        assert_eq!(fs::read(outside.join("victim")).unwrap(), b"precious");
        assert!(!root.join("sub_moved/victim").exists());

        // Rename into a directory that gets swapped: the file must not leave the share.
        fs::remove_file(root.join("sub")).unwrap();
        fs::rename(root.join("sub_moved"), root.join("sub")).unwrap();
        fs::write(root.join("f"), b"data").unwrap();
        swap_hook(root.clone(), outside.clone());
        let r = host.handle(VfsRequest::Rename {
            share: id,
            from: "f".into(),
            to: "sub/f".into(),
        });
        assert!(matches!(r, VfsResponse::Ok), "{r:?}");
        assert!(root.join("sub_moved/f").exists());
        assert!(!outside.join("f").exists());
    }

    #[test]
    fn reads_and_writes_never_follow_escaping_links() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("share");
        let outside = t.path().join("outside");
        fs::create_dir_all(root.join("d")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), b"top secret").unwrap();
        fs::write(root.join("d/ok"), b"fine").unwrap();
        symlink(outside.join("secret"), root.join("abs_link")).unwrap();
        symlink("../../outside/secret", root.join("d/rel_escape")).unwrap();
        symlink("../d/ok", root.join("d/rel_ok")).unwrap();
        let (host, id) = host_with_share(&root);
        for p in ["abs_link", "d/rel_escape"] {
            denied(host.handle(VfsRequest::Read {
                share: id,
                path: p.into(),
                offset: 0,
                len: 100,
            }));
            denied(host.handle(VfsRequest::Write {
                share: id,
                path: p.into(),
                offset: 0,
                data: b"pwned".to_vec(),
            }));
        }
        match host.handle(VfsRequest::Read {
            share: id,
            path: "d/rel_ok".into(),
            offset: 0,
            len: 100,
        }) {
            VfsResponse::Data { data, eof } => {
                assert_eq!(data, b"fine");
                assert!(eof);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"top secret");
        // A huge offset must not overflow (it used to be unchecked arithmetic on peer input).
        let r = host.handle(VfsRequest::Write {
            share: id,
            path: "d/ok".into(),
            offset: u64::MAX - 1,
            data: b"xyz".to_vec(),
        });
        assert!(matches!(r, VfsResponse::Error { .. }), "{r:?}");
        let r = host.handle(VfsRequest::Read {
            share: id,
            path: "d/ok".into(),
            offset: u64::MAX,
            len: u32::MAX,
        });
        assert!(
            matches!(r, VfsResponse::Data { ref data, eof: true } if data.is_empty()),
            "{r:?}"
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    /// While an operation runs, a hostile local process tries to move `sub` away (the first
    /// step of replacing it with a junction). Pinning must make that fail, and the operation
    /// must land in the directory that was checked.
    #[test]
    fn pinned_parent_cannot_be_swapped_during_an_operation() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("share");
        type MakeRequest = Box<dyn Fn(u32) -> VfsRequest>;
        let ops: [(&str, MakeRequest); 3] = [
            (
                "create",
                Box::new(|share| VfsRequest::Create {
                    share,
                    path: "sub/new.txt".into(),
                    exclusive: true,
                }),
            ),
            (
                "mkdir",
                Box::new(|share| VfsRequest::Mkdir {
                    share,
                    path: "sub/new.txt".into(),
                }),
            ),
            (
                "remove",
                Box::new(|share| VfsRequest::Remove {
                    share,
                    path: "sub/victim".into(),
                }),
            ),
        ];
        for (what, req) in ops {
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("sub")).unwrap();
            fs::write(root.join("sub/victim"), b"x").unwrap();
            let mut reg = ShareRegistry::new();
            let id = reg.add("s", &root, false).unwrap();
            let host = VfsHost::new(reg);
            let root = fs::canonicalize(&root).unwrap();
            let moved = Rc::new(Cell::new(false));
            let (r2, m2) = (root.clone(), moved.clone());
            RACE_HOOK.with(|h| {
                *h.borrow_mut() = Some(Box::new(move || {
                    m2.set(fs::rename(r2.join("sub"), r2.join("sub_moved")).is_ok());
                }))
            });
            let r = host.handle(req(id));
            assert!(!moved.get(), "{what}: a pinned directory was renamed mid-operation");
            assert!(!matches!(r, VfsResponse::Error { .. }), "{what}: {r:?}");
            assert!(root.join("sub").is_dir(), "{what}");
        }
    }

    #[test]
    fn links_outside_the_share_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("share");
        let outside = t.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        // Directory symlinks need Developer Mode or the privilege; skip quietly otherwise.
        if std::os::windows::fs::symlink_dir(&outside, root.join("out")).is_err() {
            return;
        }
        let mut reg = ShareRegistry::new();
        let id = reg.add("s", &root, false).unwrap();
        let host = VfsHost::new(reg);
        let r = host.handle(VfsRequest::Create {
            share: id,
            path: "out/escape.txt".into(),
            exclusive: true,
        });
        assert!(matches!(r, VfsResponse::Error { .. }), "{r:?}");
        assert!(!outside.join("escape.txt").exists());
    }
}
