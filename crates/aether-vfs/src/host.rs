//! Host side: share registry + sandboxed request handler.

use std::collections::BTreeMap;
use std::fs::{self, File, Metadata, OpenOptions};
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
        let share = Share {
            id,
            name: name.into(),
            root,
            read_only,
            ns_lock: RwLock::new(()),
            #[cfg(unix)]
            root_dev: std::os::unix::fs::MetadataExt::dev(&md),
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
                if comps.is_empty() {
                    let md = fs::metadata(&s.root).map_err(io_err)?;
                    return Ok(VfsResponse::Attr(attr_of(&s, &md, &path)));
                }
                let (parent, name) = parent_and_name(&s, &comps)?;
                let md = fs::symlink_metadata(parent.join(name)).map_err(io_err)?;
                Ok(VfsResponse::Attr(attr_of(&s, &md, &path)))
            }
            VfsRequest::ReadDir { share, path } => {
                let s = self.share(share)?;
                let _g = s.ns_lock.read();
                let comps = comps(&path)?;
                let dir = canon_existing(&s, &comps)?;
                let md = fs::metadata(&dir).map_err(io_err)?;
                if !md.is_dir() {
                    return err(
                        VfsErrorKind::NotADirectory,
                        format!("{path:?} is not a directory"),
                    );
                }
                let mut entries = Vec::new();
                for ent in fs::read_dir(&dir).map_err(io_err)? {
                    let ent = ent.map_err(io_err)?;
                    let Ok(name) = ent.file_name().into_string() else {
                        tracing::debug!("skipping non-UTF-8 file name in {}", dir.display());
                        continue;
                    };
                    let Ok(md) = ent.metadata() else { continue };
                    let rel = join_rel(&comps, &name);
                    entries.push(DirEntry {
                        attr: attr_of(&s, &md, &rel),
                        name,
                    });
                }
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
                let p = canon_existing(&s, &comps)?;
                let mut f = open_file(&p, OpenOptions::new().read(true))?;
                let md = f.metadata().map_err(io_err)?;
                if md.is_dir() {
                    return err(
                        VfsErrorKind::IsADirectory,
                        format!("{path:?} is a directory"),
                    );
                }
                let len = len.min(MAX_READ_LEN) as usize;
                let data = read_at(&mut f, offset, len).map_err(io_err)?;
                let eof = offset.saturating_add(data.len() as u64) >= md.len();
                Ok(VfsResponse::Data { data, eof })
            }
            VfsRequest::ChunkHashes { share, path } => {
                let s = self.share(share)?;
                let _g = s.ns_lock.read();
                let comps = comps(&path)?;
                let p = canon_existing(&s, &comps)?;
                let mut f = open_file(&p, OpenOptions::new().read(true))?;
                if f.metadata().map_err(io_err)?.is_dir() {
                    return err(
                        VfsErrorKind::IsADirectory,
                        format!("{path:?} is a directory"),
                    );
                }
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
                let p = canon_existing(&s, &comps)?;
                let mut f = open_file(&p, OpenOptions::new().write(true))?;
                f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
                f.write_all(&data).map_err(io_err)?;
                let md = f.metadata().map_err(io_err)?;
                Ok(VfsResponse::Written {
                    bytes: data.len() as u32,
                    attr: attr_of(&s, &md, &path),
                })
            }
            VfsRequest::Truncate { share, path, size } => {
                let s = self.share(share)?;
                writable(&s)?;
                let _g = s.ns_lock.read();
                let comps = comps(&path)?;
                let p = canon_existing(&s, &comps)?;
                let f = open_file(&p, OpenOptions::new().write(true))?;
                f.set_len(size).map_err(io_err)?;
                let md = f.metadata().map_err(io_err)?;
                Ok(VfsResponse::Attr(attr_of(&s, &md, &path)))
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
                let (parent, name) = parent_and_name(&s, &comps)?;
                let full = parent.join(name);
                let md = match fs::symlink_metadata(&full) {
                    Ok(_) if exclusive => {
                        return err(
                            VfsErrorKind::AlreadyExists,
                            format!("{path:?} already exists"),
                        );
                    }
                    Ok(md) if md.is_dir() => {
                        return err(
                            VfsErrorKind::IsADirectory,
                            format!("{path:?} is a directory"),
                        );
                    }
                    Ok(md) if md.file_type().is_symlink() => {
                        // Non-exclusive create through an existing symlink: target must stay inside.
                        let target = canon_existing(&s, &comps)?;
                        fs::metadata(target).map_err(io_err)?
                    }
                    Ok(md) => md,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        let f = open_file(&full, OpenOptions::new().write(true).create_new(true))?;
                        f.metadata().map_err(io_err)?
                    }
                    Err(e) => return Err(io_err(e)),
                };
                Ok(VfsResponse::Attr(attr_of(&s, &md, &path)))
            }
            VfsRequest::Mkdir { share, path } => {
                let s = self.share(share)?;
                writable(&s)?;
                let _g = s.ns_lock.write();
                let comps = nonroot(comps(&path)?)?;
                let (parent, name) = parent_and_name(&s, &comps)?;
                let full = parent.join(name);
                fs::create_dir(&full).map_err(io_err)?;
                let md = fs::symlink_metadata(&full).map_err(io_err)?;
                Ok(VfsResponse::Attr(attr_of(&s, &md, &path)))
            }
            VfsRequest::Remove { share, path } => {
                let s = self.share(share)?;
                writable(&s)?;
                let _g = s.ns_lock.write();
                let comps = nonroot(comps(&path)?)?;
                let (parent, name) = parent_and_name(&s, &comps)?;
                let full = parent.join(name);
                let md = fs::symlink_metadata(&full).map_err(io_err)?;
                if md.is_dir() {
                    fs::remove_dir(&full)
                } else {
                    fs::remove_file(&full)
                }
                .map_err(io_err)?;
                Ok(VfsResponse::Ok)
            }
            VfsRequest::Rename { share, from, to } => {
                let s = self.share(share)?;
                writable(&s)?;
                let _g = s.ns_lock.write();
                let fc = nonroot(comps(&from)?)?;
                let tc = nonroot(comps(&to)?)?;
                let (fp, fname) = parent_and_name(&s, &fc)?;
                let (tp, tname) = parent_and_name(&s, &tc)?;
                fs::rename(fp.join(fname), tp.join(tname)).map_err(io_err)?;
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
                let (parent, name) = parent_and_name(&s, &comps)?;
                let full = parent.join(name);
                make_symlink(&target, &full).map_err(io_err)?;
                let md = fs::symlink_metadata(&full).map_err(io_err)?;
                Ok(VfsResponse::Attr(attr_of(&s, &md, &path)))
            }
            VfsRequest::ReadLink { share, path } => {
                let s = self.share(share)?;
                let _g = s.ns_lock.read();
                let comps = nonroot(comps(&path)?)?;
                let (parent, name) = parent_and_name(&s, &comps)?;
                let t = fs::read_link(parent.join(name)).map_err(io_err)?;
                let target = t
                    .to_str()
                    .ok_or((
                        VfsErrorKind::Io,
                        "symlink target is not valid UTF-8".to_string(),
                    ))?
                    .replace(std::path::MAIN_SEPARATOR, "/");
                Ok(VfsResponse::Link { target })
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

fn join_rel(comps: &[&str], name: &str) -> String {
    let mut s = comps.join("/");
    if !s.is_empty() {
        s.push('/');
    }
    s.push_str(name);
    s
}

/// Canonicalize `root/comps…` (following symlinks) and require it to stay inside the root.
fn canon_existing(s: &Share, comps: &[&str]) -> HResult<PathBuf> {
    let mut p = s.root.clone();
    p.extend(comps);
    let c = fs::canonicalize(&p).map_err(io_err)?;
    if !c.starts_with(&s.root) {
        return err(
            VfsErrorKind::PermissionDenied,
            "path resolves outside of the share",
        );
    }
    Ok(c)
}

/// Canonical parent directory (inside the share) + final component (not followed).
fn parent_and_name<'a>(s: &Share, comps: &[&'a str]) -> HResult<(PathBuf, &'a str)> {
    let (name, parent) = comps.split_last().expect("non-empty components");
    let parent = canon_existing(s, parent)?;
    if !fs::metadata(&parent).map_err(io_err)?.is_dir() {
        return err(VfsErrorKind::NotADirectory, "parent is not a directory");
    }
    Ok((parent, name))
}

/// Open a file whose path is already canonical; refuse to follow a symlink in the final
/// component (it could only appear through an outside race).
fn open_file(p: &Path, opts: &mut OpenOptions) -> HResult<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(p).map_err(io_err)
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
    let len = len.min((size - offset) as usize);
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    let n = read_full(f, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

#[cfg(unix)]
fn make_symlink(target: &str, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_symlink(target: &str, link: &Path) -> io::Result<()> {
    // Requires SeCreateSymbolicLinkPrivilege or Developer Mode.
    std::os::windows::fs::symlink_file(target.replace('/', "\\"), link)
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_target: &str, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlinks are not supported on this platform",
    ))
}

fn time_ns(t: io::Result<SystemTime>) -> i128 {
    match t {
        Ok(t) => match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        },
        Err(_) => 0,
    }
}

/// Build the wire attributes for `md`. `rel` is the share-relative path (used for the
/// file id on platforms without inode numbers).
pub(crate) fn attr_of(s: &Share, md: &Metadata, rel: &str) -> FileAttr {
    let ft = md.file_type();
    let kind = if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_dir() {
        FileKind::Directory
    } else {
        FileKind::File
    };
    #[cfg(unix)]
    let (file_id, mode, ctime_ns) = {
        use std::os::unix::fs::MetadataExt;
        let id = if md.dev() == s.root_dev {
            md.ino()
        } else {
            // Nested mount point: mix the device in to avoid inode collisions.
            let mut h = blake3::Hasher::new();
            h.update(&md.dev().to_le_bytes());
            h.update(&md.ino().to_le_bytes());
            u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap()) | (1 << 63)
        };
        (
            id,
            md.mode() & 0o7777,
            md.ctime() as i128 * 1_000_000_000 + md.ctime_nsec() as i128,
        )
    };
    #[cfg(not(unix))]
    let (file_id, mode, ctime_ns) = {
        let canonical_rel = split_rel_path(rel).map(|c| c.join("/")).unwrap_or_default();
        let mut h = blake3::Hasher::new();
        h.update(&s.id.to_le_bytes());
        h.update(canonical_rel.as_bytes());
        let id = u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap()).max(1);
        let mode = if md.is_dir() {
            0o755
        } else if md.permissions().readonly() {
            0o444
        } else {
            0o644
        };
        (id, mode, time_ns(md.created()))
    };
    let _ = rel;
    FileAttr {
        file_id,
        kind,
        size: md.len(),
        mode,
        mtime_ns: time_ns(md.modified()),
        ctime_ns,
        read_only: s.read_only || md.permissions().readonly(),
    }
}
