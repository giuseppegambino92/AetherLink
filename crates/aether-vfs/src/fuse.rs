//! Linux FUSE backend (`fuser`, pure-Rust mount through the setuid `fusermount3` helper).
//!
//! Mirrors the NFS adapter ([`crate::nfs`]): inode numbers are assigned locally
//! (path-interned, root = 1) and renames remap the ids of the moved subtree; attributes are
//! cached for [`TTL`] (the kernel entry/attr TTL is the same); read-only shares report
//! their write bits cleared and fail writes with `EROFS`; reads go through
//! [`VfsClient::read_cached`] when the client has a chunk cache; mode/owner/time `setattr`
//! is accepted as a no-op and a size change becomes a truncate.
//!
//! `fuser` dispatches requests synchronously on its own thread while [`VfsClient`] is async.
//! Read-only requests (lookup, getattr, readlink, read, opendir, statfs) are spawned onto
//! the tokio runtime captured at mount time and reply from the task (`Reply*` objects are
//! `Send`), so a slow read does not stall the dispatch loop. Namespace changes and writes
//! are run with [`Handle::block_on`] on the fuser thread (which is not a runtime worker),
//! keeping them in kernel order.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aether_protocol::{FileAttr, FileKind, VfsErrorKind};
use fuser::{
    AccessFlags, BackgroundSession, Config, Errno, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, KernelConfig, LockOwner, MountOption, OpenAccMode, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, Request, SessionACL, TimeOrNow, WriteFlags,
};
use parking_lot::Mutex;
use tokio::runtime::Handle;

use crate::client::VfsClient;
use crate::error::{Result, VfsError};
use crate::path::validate_name;

const ROOT_INO: u64 = 1;
/// Attribute cache lifetime, also used as the kernel entry/attr TTL.
const TTL: Duration = Duration::from_secs(1);
const BLKSIZE: u32 = 128 * 1024;
const GEN: Generation = Generation(0);

type R<T> = std::result::Result<T, Errno>;

/// Path ↔ inode-number interning (root = 1). Ids are never reused within a mount.
#[derive(Default)]
struct IdMap {
    by_id: HashMap<u64, String>,
    by_path: HashMap<String, u64>,
    next: u64,
}

impl IdMap {
    fn new() -> Self {
        let mut m = IdMap {
            next: ROOT_INO + 1,
            ..Default::default()
        };
        m.by_id.insert(ROOT_INO, String::new());
        m.by_path.insert(String::new(), ROOT_INO);
        m
    }

    fn intern(&mut self, path: &str) -> u64 {
        if let Some(id) = self.by_path.get(path) {
            return *id;
        }
        let id = self.next;
        self.next += 1;
        self.by_id.insert(id, path.to_string());
        self.by_path.insert(path.to_string(), id);
        id
    }

    fn forget(&mut self, path: &str) {
        if path.is_empty() {
            return; // the root is permanent
        }
        if let Some(id) = self.by_path.remove(path) {
            self.by_id.remove(&id);
        }
    }

    /// Move `from` (and its descendants) to `to`, keeping their ids.
    fn rename(&mut self, from: &str, to: &str) {
        self.forget(to);
        let prefix = format!("{from}/");
        let moved: Vec<(String, u64)> = self
            .by_path
            .iter()
            .filter(|(p, _)| p.as_str() == from || p.starts_with(&prefix))
            .map(|(p, id)| (p.clone(), *id))
            .collect();
        for (old, id) in moved {
            let new = format!("{to}{}", &old[from.len()..]);
            self.by_path.remove(&old);
            self.by_path.insert(new.clone(), id);
            self.by_id.insert(id, new);
        }
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

/// Map a VFS error to the errno reported to the kernel.
pub(crate) fn errno_of(e: &VfsError) -> Errno {
    match e.kind() {
        Some(VfsErrorKind::NotFound) => Errno::ENOENT,
        Some(VfsErrorKind::PermissionDenied) => Errno::EACCES,
        Some(VfsErrorKind::AlreadyExists) => Errno::EEXIST,
        Some(VfsErrorKind::NotADirectory) => Errno::ENOTDIR,
        Some(VfsErrorKind::IsADirectory) => Errno::EISDIR,
        Some(VfsErrorKind::NotEmpty) => Errno::ENOTEMPTY,
        Some(VfsErrorKind::ReadOnly) => Errno::EROFS,
        Some(VfsErrorKind::InvalidPath) => Errno::EINVAL,
        Some(VfsErrorKind::NoSuchShare) => Errno::ESTALE,
        Some(VfsErrorKind::Io) | None => match e {
            VfsError::InvalidPath(_) => Errno::EINVAL,
            _ => {
                tracing::debug!("fuse adapter error: {e}");
                Errno::EIO
            }
        },
    }
}

fn errno(e: VfsError) -> Errno {
    errno_of(&e)
}

fn systime(ns: i128) -> SystemTime {
    if ns <= 0 {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH + Duration::from_nanos(ns.min(u64::MAX as i128) as u64)
    }
}

fn file_type(k: FileKind) -> FileType {
    match k {
        FileKind::File => FileType::RegularFile,
        FileKind::Directory => FileType::Directory,
        FileKind::Symlink => FileType::Symlink,
    }
}

fn wants_write(flags: i32) -> bool {
    OpenFlags(flags).acc_mode() != OpenAccMode::O_RDONLY
}

struct DirSnap {
    ino: u64,
    kind: FileType,
    name: String,
}

struct Inner {
    client: Arc<VfsClient>,
    share: u32,
    read_only: bool,
    ids: Mutex<IdMap>,
    attrs: Mutex<HashMap<String, (Instant, FileAttr)>>,
    /// Directory listings snapshotted at `opendir`, keyed by file handle.
    dirs: Mutex<HashMap<u64, Arc<Vec<DirSnap>>>>,
    next_fh: AtomicU64,
    uid: u32,
    gid: u32,
}

impl Inner {
    fn path_of(&self, ino: INodeNo) -> R<String> {
        self.ids
            .lock()
            .by_id
            .get(&ino.0)
            .cloned()
            .ok_or(Errno::ESTALE)
    }

    fn rw(&self) -> R<()> {
        if self.read_only {
            Err(Errno::EROFS)
        } else {
            Ok(())
        }
    }

    /// `dir/name` for a name received from the kernel. `lookup` reports invalid names as
    /// `ENOENT` (they cannot exist), creating operations as `EINVAL`.
    fn child(&self, dir: INodeNo, name: &OsStr, invalid: Errno) -> R<String> {
        let dir = self.path_of(dir)?;
        let name = name.to_str().ok_or(invalid)?;
        validate_name(name).map_err(|_| invalid)?;
        Ok(join(&dir, name))
    }

    fn remember(&self, path: &str, attr: FileAttr) {
        let mut a = self.attrs.lock();
        if a.len() > 65_536 {
            a.clear();
        }
        a.insert(path.to_string(), (Instant::now(), attr));
    }

    fn forget_attr(&self, path: &str) {
        self.attrs.lock().remove(path);
    }

    async fn stat_path(&self, path: &str) -> R<FileAttr> {
        if let Some((t, a)) = self.attrs.lock().get(path)
            && t.elapsed() < TTL
        {
            return Ok(a.clone());
        }
        let a = self.client.stat(self.share, path).await.map_err(errno)?;
        self.remember(path, a.clone());
        Ok(a)
    }

    fn fattr(&self, ino: u64, a: &FileAttr) -> fuser::FileAttr {
        let nlink = if a.kind == FileKind::Directory { 2 } else { 1 };
        let mut perm = (a.mode & 0o7777) as u16;
        if self.read_only {
            perm &= !0o222;
        }
        let mtime = systime(a.mtime_ns);
        let ctime = systime(a.ctime_ns);
        fuser::FileAttr {
            ino: INodeNo(ino),
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime,
            crtime: ctime,
            kind: file_type(a.kind),
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLKSIZE,
            flags: 0,
        }
    }

    fn new_entry(&self, path: &str, attr: FileAttr) -> fuser::FileAttr {
        let ino = self.ids.lock().intern(path);
        self.forget_attr(parent(path));
        let fa = self.fattr(ino, &attr);
        self.remember(path, attr);
        fa
    }

    async fn lookup(&self, dir: INodeNo, name: &OsStr) -> R<fuser::FileAttr> {
        let path = match name.to_str() {
            Some("." | "") => self.path_of(dir)?,
            Some("..") => parent(&self.path_of(dir)?).to_string(),
            _ => self.child(dir, name, Errno::ENOENT)?,
        };
        let a = self.stat_path(&path).await?;
        let ino = self.ids.lock().intern(&path);
        Ok(self.fattr(ino, &a))
    }

    async fn getattr(&self, ino: INodeNo) -> R<fuser::FileAttr> {
        let path = self.path_of(ino)?;
        let a = self.stat_path(&path).await?;
        Ok(self.fattr(ino.0, &a))
    }

    async fn setattr(&self, ino: INodeNo, size: Option<u64>) -> R<fuser::FileAttr> {
        let path = self.path_of(ino)?;
        if let Some(size) = size {
            self.rw()?;
            let a = self
                .client
                .truncate(self.share, &path, size)
                .await
                .map_err(errno)?;
            self.remember(&path, a.clone());
            return Ok(self.fattr(ino.0, &a));
        }
        // Mode/owner/time changes are not part of the VFS protocol; accept them as no-ops
        // (so `cp -p`, `touch` keep working) and report the current attributes.
        self.forget_attr(&path);
        self.getattr(ino).await
    }

    async fn readlink(&self, ino: INodeNo) -> R<Vec<u8>> {
        let path = self.path_of(ino)?;
        let t = self
            .client
            .read_link(self.share, &path)
            .await
            .map_err(errno)?;
        Ok(t.into_bytes())
    }

    async fn mkdir(&self, dir: INodeNo, name: &OsStr) -> R<fuser::FileAttr> {
        self.rw()?;
        let path = self.child(dir, name, Errno::EINVAL)?;
        let a = self.client.mkdir(self.share, &path).await.map_err(errno)?;
        Ok(self.new_entry(&path, a))
    }

    async fn remove(&self, dir: INodeNo, name: &OsStr, is_dir: bool) -> R<()> {
        self.rw()?;
        let path = self.child(dir, name, Errno::EINVAL)?;
        let a = self.stat_path(&path).await?;
        match (is_dir, a.kind == FileKind::Directory) {
            (true, false) => return Err(Errno::ENOTDIR),
            (false, true) => return Err(Errno::EISDIR),
            _ => {}
        }
        self.client.remove(self.share, &path).await.map_err(errno)?;
        self.ids.lock().forget(&path);
        self.forget_attr(&path);
        self.forget_attr(parent(&path));
        Ok(())
    }

    async fn symlink(&self, dir: INodeNo, name: &OsStr, target: &Path) -> R<fuser::FileAttr> {
        self.rw()?;
        let path = self.child(dir, name, Errno::EINVAL)?;
        let target = target.to_str().ok_or(Errno::EINVAL)?;
        let a = self
            .client
            .symlink(self.share, &path, target)
            .await
            .map_err(errno)?;
        Ok(self.new_entry(&path, a))
    }

    async fn rename(
        &self,
        dir: INodeNo,
        name: &OsStr,
        newdir: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
    ) -> R<()> {
        self.rw()?;
        // Atomic exchange, whiteouts (and unknown flags) cannot be expressed with the VFS protocol.
        if flags.bits() & !RenameFlags::RENAME_NOREPLACE.bits() != 0 {
            return Err(Errno::EINVAL);
        }
        let from = self.child(dir, name, Errno::EINVAL)?;
        let to = self.child(newdir, newname, Errno::EINVAL)?;
        if flags.contains(RenameFlags::RENAME_NOREPLACE) {
            // Emulated with a stat check (not atomic against concurrent remote changes).
            match self.client.stat(self.share, &to).await {
                Ok(_) => return Err(Errno::EEXIST),
                Err(e) if e.kind() == Some(VfsErrorKind::NotFound) => {}
                Err(e) => return Err(errno(e)),
            }
        }
        self.client
            .rename(self.share, &from, &to)
            .await
            .map_err(errno)?;
        self.ids.lock().rename(&from, &to);
        // Cached attributes of the moved subtree are keyed by path: drop them all.
        self.attrs.lock().clear();
        Ok(())
    }

    async fn open(&self, ino: INodeNo, flags: i32) -> R<()> {
        if wants_write(flags) || flags & libc::O_TRUNC != 0 {
            self.rw()?;
        }
        if flags & libc::O_TRUNC != 0 {
            let path = self.path_of(ino)?;
            let a = self
                .client
                .truncate(self.share, &path, 0)
                .await
                .map_err(errno)?;
            self.remember(&path, a);
        }
        Ok(())
    }

    async fn read(&self, ino: INodeNo, offset: u64, size: u32) -> R<Vec<u8>> {
        let path = self.path_of(ino)?;
        let attr = self.stat_path(&path).await?;
        if attr.kind == FileKind::Directory {
            return Err(Errno::EISDIR);
        }
        if self.client.cache().is_some() && attr.kind == FileKind::File {
            return self
                .client
                .read_cached(self.share, &path, offset, size)
                .await
                .map_err(errno);
        }
        // FUSE expects the full requested length unless at EOF.
        let mut out = Vec::with_capacity(size as usize);
        while out.len() < size as usize {
            let (d, eof) = self
                .client
                .read(
                    self.share,
                    &path,
                    offset + out.len() as u64,
                    size - out.len() as u32,
                )
                .await
                .map_err(errno)?;
            let done = eof || d.is_empty();
            out.extend_from_slice(&d);
            if done {
                break;
            }
        }
        Ok(out)
    }

    async fn write(&self, ino: INodeNo, offset: u64, data: Vec<u8>) -> R<u32> {
        self.rw()?;
        let path = self.path_of(ino)?;
        let len = data.len() as u32;
        let a = self
            .client
            .write(self.share, &path, offset, data)
            .await
            .map_err(errno)?;
        self.remember(&path, a);
        Ok(len)
    }

    async fn create(&self, dir: INodeNo, name: &OsStr, flags: i32) -> R<fuser::FileAttr> {
        self.rw()?;
        let path = self.child(dir, name, Errno::EINVAL)?;
        let exclusive = flags & libc::O_EXCL != 0;
        let mut a = self
            .client
            .create(self.share, &path, exclusive)
            .await
            .map_err(errno)?;
        if !exclusive && flags & libc::O_TRUNC != 0 && a.size != 0 {
            a = self
                .client
                .truncate(self.share, &path, 0)
                .await
                .map_err(errno)?;
        }
        Ok(self.new_entry(&path, a))
    }

    async fn opendir(&self, ino: INodeNo) -> R<u64> {
        let dir = self.path_of(ino)?;
        let entries = self
            .client
            .read_dir(self.share, &dir)
            .await
            .map_err(errno)?;
        let mut snap = Vec::with_capacity(entries.len() + 2);
        {
            let mut ids = self.ids.lock();
            snap.push(DirSnap {
                ino: ino.0,
                kind: FileType::Directory,
                name: ".".into(),
            });
            snap.push(DirSnap {
                ino: ids.intern(parent(&dir)),
                kind: FileType::Directory,
                name: "..".into(),
            });
            for e in &entries {
                snap.push(DirSnap {
                    ino: ids.intern(&join(&dir, &e.name)),
                    kind: file_type(e.attr.kind),
                    name: e.name.clone(),
                });
            }
        }
        for e in entries {
            self.remember(&join(&dir, &e.name), e.attr);
        }
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        let mut dirs = self.dirs.lock();
        dirs.insert(fh, Arc::new(snap));
        Ok(fh)
    }
}

/// The `fuser` filesystem: shared state plus the runtime used to drive [`VfsClient`].
pub(crate) struct AetherFuse {
    inner: Arc<Inner>,
    rt: Handle,
}

impl AetherFuse {
    pub(crate) fn new(client: Arc<VfsClient>, share: u32, read_only: bool, rt: Handle) -> Self {
        // SAFETY: getuid/getgid have no preconditions and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            inner: Arc::new(Inner {
                client,
                share,
                read_only,
                ids: Mutex::new(IdMap::new()),
                attrs: Mutex::new(HashMap::new()),
                dirs: Mutex::new(HashMap::new()),
                next_fh: AtomicU64::new(1),
                uid,
                gid,
            }),
            rt,
        }
    }

    /// Run `f` on the runtime; it replies when done (non-blocking for the fuser thread).
    fn spawn<F, Fut>(&self, f: F)
    where
        F: FnOnce(Arc<Inner>) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.rt.spawn(f(self.inner.clone()));
    }

    /// Run `f` to completion on the fuser thread (ordered with respect to other requests).
    fn block<T>(&self, f: impl Future<Output = T>) -> T {
        self.rt.block_on(f)
    }
}

impl Filesystem for AetherFuse {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::io::Result<()> {
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = name.to_os_string();
        self.spawn(move |fs| async move {
            match fs.lookup(parent, &name).await {
                Ok(a) => reply.entry(&TTL, &a, GEN),
                Err(e) => reply.error(e),
            }
        });
    }

    fn forget(&self, _req: &Request, _ino: INodeNo, _nlookup: u64) {
        // Ids are path-interned and stay valid for the lifetime of the mount.
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.spawn(move |fs| async move {
            match fs.getattr(ino).await {
                Ok(a) => reply.attr(&TTL, &a),
                Err(e) => reply.error(e),
            }
        });
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        match self.block(self.inner.setattr(ino, size)) {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        self.spawn(move |fs| async move {
            match fs.readlink(ino).await {
                Ok(t) => reply.data(&t),
                Err(e) => reply.error(e),
            }
        });
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        match self.block(self.inner.mkdir(parent, name)) {
            Ok(a) => reply.entry(&TTL, &a, GEN),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.block(self.inner.remove(parent, name, false)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.block(self.inner.remove(parent, name, true)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        match self.block(self.inner.symlink(parent, link_name, target)) {
            Ok(a) => reply.entry(&TTL, &a, GEN),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        match self.block(self.inner.rename(parent, name, newparent, newname, flags)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        match self.block(self.inner.open(ino, flags.0)) {
            Ok(()) => reply.opened(FileHandle(0), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        self.spawn(move |fs| async move {
            match fs.read(ino, offset, size).await {
                Ok(d) => reply.data(&d),
                Err(e) => reply.error(e),
            }
        });
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.block(self.inner.write(ino, offset, data.to_vec())) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // Writes are forwarded synchronously; nothing is buffered here.
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.spawn(move |fs| async move {
            match fs.opendir(ino).await {
                Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
                Err(e) => reply.error(e),
            }
        });
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(snap) = self.inner.dirs.lock().get(&fh.0).cloned() else {
            reply.error(Errno::EBADF);
            return;
        };
        // Entry `i` is reported with offset `i + 1`: the offset of the *next* entry.
        for (i, e) in snap.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(e.ino), i as u64 + 1, e.kind, &e.name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.inner.dirs.lock().remove(&fh.0);
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // The VFS protocol has no free-space query: report a large, mostly free volume so
        // tools that check for space before copying proceed (the host enforces real limits).
        let blocks: u64 = 1 << 32; // 16 TiB in 4 KiB blocks
        let free = if self.inner.read_only { 0 } else { blocks / 2 };
        reply.statfs(blocks, free, free, 1 << 24, 1 << 23, 4096, 255, 4096);
    }

    fn access(&self, _req: &Request, _ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        // Only called without `default_permissions`; the kernel checks modes otherwise.
        if mask.contains(AccessFlags::W_OK) && self.inner.read_only {
            reply.error(Errno::EROFS);
        } else {
            reply.ok();
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        match self.block(self.inner.create(parent, name, flags)) {
            Ok(a) => reply.created(&TTL, &a, GEN, FileHandle(0), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }
}

pub(crate) fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// The setuid `fusermount3` / `fusermount` helper, if installed.
fn find_fusermount() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FUSERMOUNT_PATH") {
        return Some(PathBuf::from(p));
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from));
    ["fusermount3", "fusermount"]
        .iter()
        .flat_map(|name| dirs.iter().map(move |d| d.join(name)))
        .find(|p| p.is_file())
}

/// Whether a FUSE mount can be attempted: `/dev/fuse` must exist and, unless running as
/// root (which mounts directly), the `fusermount3` helper must be installed.
pub(crate) fn availability() -> std::result::Result<(), String> {
    if !Path::new("/dev/fuse").exists() {
        return Err(
            "/dev/fuse is missing (fuse kernel module not loaded, or not passed \
                    into the container)"
                .into(),
        );
    }
    if !is_root() && find_fusermount().is_none() {
        return Err("the setuid `fusermount3` helper is not installed (package `fuse3`)".into());
    }
    Ok(())
}

/// Mount `share` at `mount_point` (an existing, canonical directory) with FUSE.
pub(crate) async fn mount(
    client: Arc<VfsClient>,
    share: u32,
    read_only: bool,
    mount_point: &Path,
) -> Result<BackgroundSession> {
    let fs = AetherFuse::new(client, share, read_only, Handle::current());
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("aetherlink".into()),
        MountOption::Subtype("aetherlink".into()),
        MountOption::DefaultPermissions,
        MountOption::NoAtime,
        MountOption::NoDev,
        MountOption::NoSuid,
        if read_only {
            MountOption::RO
        } else {
            MountOption::RW
        },
    ];
    // Owner-only access: `allow_other` / `auto_unmount` would need `user_allow_other`
    // in /etc/fuse.conf for unprivileged users.
    config.acl = SessionACL::Owner;
    let mp = mount_point.to_path_buf();
    // Mounting (fusermount3 + the INIT handshake) blocks: keep it off the runtime workers.
    tokio::task::spawn_blocking(move || fuser::spawn_mount(fs, &mp, &config))
        .await
        .map_err(|e| VfsError::Mount(format!("FUSE mount task failed: {e}")))?
        .map_err(|e| VfsError::Mount(format!("FUSE mount failed: {e}")))
}

/// Unmount (via `fusermount3 -u` when unprivileged) and join the session thread. If the
/// regular unmount fails (e.g. busy as root), detach lazily instead.
pub(crate) fn unmount_blocking(session: BackgroundSession, mount_point: &Path) -> Result<()> {
    match session.umount_and_join() {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::debug!("fuse unmount of {} failed: {e}", mount_point.display());
            let lazy = if is_root() {
                std::process::Command::new("umount")
                    .arg("-l")
                    .arg(mount_point)
                    .status()
            } else {
                std::process::Command::new(
                    find_fusermount().unwrap_or_else(|| PathBuf::from("fusermount3")),
                )
                .args(["-u", "-z", "--"])
                .arg(mount_point)
                .status()
            };
            match lazy {
                Ok(s) if s.success() => Ok(()),
                _ => Err(VfsError::Mount(format!(
                    "cannot unmount {}: {e}",
                    mount_point.display()
                ))),
            }
        }
    }
}

/// Async wrapper around [`unmount_blocking`]. A lazily detached mount keeps its session
/// alive until the last open file is closed; after 10 s we stop waiting for that.
pub(crate) async fn unmount(session: BackgroundSession, mount_point: &Path) -> Result<()> {
    let mp = mount_point.to_path_buf();
    let task = tokio::task::spawn_blocking(move || unmount_blocking(session, &mp));
    match tokio::time::timeout(Duration::from_secs(10), task).await {
        Ok(Ok(res)) => res,
        Ok(Err(e)) => Err(VfsError::Mount(format!("FUSE unmount task failed: {e}"))),
        Err(_) => {
            tracing::warn!(
                "{} detached; FUSE session ends when its open files are closed",
                mount_point.display()
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_map_interns_and_renames_subtree() {
        let mut m = IdMap::new();
        assert_eq!(m.intern(""), ROOT_INO);
        let a = m.intern("a");
        let ab = m.intern("a/b");
        let ax = m.intern("ax");
        assert_eq!(m.intern("a"), a, "interning is idempotent");
        m.rename("a", "z/a");
        assert_eq!(m.by_id[&a], "z/a");
        assert_eq!(m.by_id[&ab], "z/a/b");
        assert_eq!(
            m.by_id[&ax], "ax",
            "sibling with a common prefix is untouched"
        );
        assert_eq!(m.intern("z/a/b"), ab);
        assert!(!m.by_path.contains_key("a/b"));
        m.forget("z/a/b");
        assert!(!m.by_id.contains_key(&ab));
        let fresh = m.intern("z/a/b");
        assert_ne!(fresh, ab, "ids are not reused");
        m.forget("");
        assert_eq!(m.by_id[&ROOT_INO], "", "root is permanent");
    }

    #[test]
    fn rename_over_existing_target_drops_its_id() {
        let mut m = IdMap::new();
        let src = m.intern("src");
        let dst = m.intern("dst");
        m.rename("src", "dst");
        assert_eq!(m.by_path["dst"], src);
        assert!(!m.by_id.contains_key(&dst));
    }

    #[test]
    fn errno_mapping() {
        let remote = |kind| VfsError::Remote {
            kind,
            message: String::new(),
        };
        for (kind, want) in [
            (VfsErrorKind::NotFound, Errno::ENOENT),
            (VfsErrorKind::PermissionDenied, Errno::EACCES),
            (VfsErrorKind::AlreadyExists, Errno::EEXIST),
            (VfsErrorKind::NotADirectory, Errno::ENOTDIR),
            (VfsErrorKind::IsADirectory, Errno::EISDIR),
            (VfsErrorKind::NotEmpty, Errno::ENOTEMPTY),
            (VfsErrorKind::ReadOnly, Errno::EROFS),
            (VfsErrorKind::InvalidPath, Errno::EINVAL),
            (VfsErrorKind::NoSuchShare, Errno::ESTALE),
            (VfsErrorKind::Io, Errno::EIO),
        ] {
            assert_eq!(errno_of(&remote(kind)), want, "{kind:?}");
        }
        assert_eq!(errno_of(&VfsError::Closed), Errno::EIO);
        assert_eq!(errno_of(&VfsError::InvalidPath("x".into())), Errno::EINVAL);
    }

    #[test]
    fn helpers() {
        assert_eq!(join("", "a"), "a");
        assert_eq!(join("a/b", "c"), "a/b/c");
        assert_eq!(parent("a/b/c"), "a/b");
        assert_eq!(parent("a"), "");
        assert_eq!(systime(-5), UNIX_EPOCH);
        assert_eq!(
            systime(1_500_000_000),
            UNIX_EPOCH + Duration::from_millis(1500)
        );
        assert!(wants_write(libc::O_WRONLY));
        assert!(wants_write(libc::O_RDWR | libc::O_APPEND));
        assert!(!wants_write(libc::O_RDONLY));
    }
}
