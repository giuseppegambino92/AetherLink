//! `nfsserve::NFSFileSystem` adapter on top of [`VfsClient`].
//!
//! NFS file ids are assigned locally (path-interned, root = 1) rather than reusing host
//! inode numbers, so ids are unique per mount even across nested host mount points and
//! hard links. Renames remap the ids of the moved subtree. Attributes are cached for
//! [`ATTR_TTL`] (the OS client is mounted with `actimeo=1` as well).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_protocol::{FileAttr, FileKind, VfsErrorKind};
use async_trait::async_trait;
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfsstring, nfstime3, sattr3, set_size3,
    specdata3,
};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use parking_lot::Mutex;

use crate::client::VfsClient;
use crate::error::VfsError;
use crate::path::validate_name;

const ROOT_ID: fileid3 = 1;
const ATTR_TTL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct IdMap {
    by_id: HashMap<fileid3, String>,
    by_path: HashMap<String, fileid3>,
    next: fileid3,
}

impl IdMap {
    fn new() -> Self {
        let mut m = IdMap {
            next: ROOT_ID + 1,
            ..Default::default()
        };
        m.by_id.insert(ROOT_ID, String::new());
        m.by_path.insert(String::new(), ROOT_ID);
        m
    }

    fn intern(&mut self, path: &str) -> fileid3 {
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
        if let Some(id) = self.by_path.remove(path) {
            self.by_id.remove(&id);
        }
    }

    /// Move `from` (and its descendants) to `to`, keeping their ids.
    fn rename(&mut self, from: &str, to: &str) {
        self.forget(to);
        let prefix = format!("{from}/");
        let moved: Vec<(String, fileid3)> = self
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

pub(crate) struct AetherNfs {
    client: Arc<VfsClient>,
    share: u32,
    read_only: bool,
    ids: Mutex<IdMap>,
    attrs: Mutex<HashMap<String, (Instant, FileAttr)>>,
    uid: u32,
    gid: u32,
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

fn to_nfs(e: VfsError) -> nfsstat3 {
    match e.kind() {
        Some(VfsErrorKind::NotFound) => nfsstat3::NFS3ERR_NOENT,
        Some(VfsErrorKind::PermissionDenied) => nfsstat3::NFS3ERR_ACCES,
        Some(VfsErrorKind::AlreadyExists) => nfsstat3::NFS3ERR_EXIST,
        Some(VfsErrorKind::NotADirectory) => nfsstat3::NFS3ERR_NOTDIR,
        Some(VfsErrorKind::IsADirectory) => nfsstat3::NFS3ERR_ISDIR,
        Some(VfsErrorKind::NotEmpty) => nfsstat3::NFS3ERR_NOTEMPTY,
        Some(VfsErrorKind::ReadOnly) => nfsstat3::NFS3ERR_ROFS,
        Some(VfsErrorKind::InvalidPath) => nfsstat3::NFS3ERR_INVAL,
        Some(VfsErrorKind::NoSuchShare) => nfsstat3::NFS3ERR_STALE,
        Some(VfsErrorKind::Io) | None => {
            tracing::debug!("nfs adapter error: {e}");
            nfsstat3::NFS3ERR_IO
        }
    }
}

fn nfstime(ns: i128) -> nfstime3 {
    let ns = ns.max(0);
    nfstime3 {
        seconds: (ns / 1_000_000_000).min(u32::MAX as i128) as u32,
        nseconds: (ns % 1_000_000_000) as u32,
    }
}

fn name_str(n: &filename3) -> Result<&str, nfsstat3> {
    let s = std::str::from_utf8(&n.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
    Ok(s)
}

fn checked_name(n: &filename3) -> Result<&str, nfsstat3> {
    let s = name_str(n)?;
    validate_name(s).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
    Ok(s)
}

impl AetherNfs {
    pub(crate) fn new(client: Arc<VfsClient>, share: u32, read_only: bool) -> Self {
        #[cfg(unix)]
        // SAFETY: getuid/getgid have no preconditions and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        #[cfg(not(unix))]
        let (uid, gid) = (0, 0);
        Self {
            client,
            share,
            read_only,
            ids: Mutex::new(IdMap::new()),
            attrs: Mutex::new(HashMap::new()),
            uid,
            gid,
        }
    }

    fn path_of(&self, id: fileid3) -> Result<String, nfsstat3> {
        self.ids
            .lock()
            .by_id
            .get(&id)
            .cloned()
            .ok_or(nfsstat3::NFS3ERR_STALE)
    }

    fn rw(&self) -> Result<(), nfsstat3> {
        if self.read_only {
            Err(nfsstat3::NFS3ERR_ROFS)
        } else {
            Ok(())
        }
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

    async fn stat_path(&self, path: &str) -> Result<FileAttr, nfsstat3> {
        if let Some((t, a)) = self.attrs.lock().get(path)
            && t.elapsed() < ATTR_TTL
        {
            return Ok(a.clone());
        }
        let a = self.client.stat(self.share, path).await.map_err(to_nfs)?;
        self.remember(path, a.clone());
        Ok(a)
    }

    fn fattr(&self, id: fileid3, a: &FileAttr) -> fattr3 {
        let (ftype, nlink) = match a.kind {
            FileKind::File => (ftype3::NF3REG, 1),
            FileKind::Directory => (ftype3::NF3DIR, 2),
            FileKind::Symlink => (ftype3::NF3LNK, 1),
        };
        let mut mode = a.mode & 0o7777;
        if self.read_only {
            mode &= !0o222;
        }
        fattr3 {
            ftype,
            mode,
            nlink,
            uid: self.uid,
            gid: self.gid,
            size: a.size,
            used: a.size,
            rdev: specdata3::default(),
            fsid: self.share as u64,
            fileid: id,
            atime: nfstime(a.mtime_ns),
            mtime: nfstime(a.mtime_ns),
            ctime: nfstime(a.ctime_ns),
        }
    }

    async fn new_entry(&self, path: &str, attr: FileAttr) -> (fileid3, fattr3) {
        let id = self.ids.lock().intern(path);
        self.remember(path, attr.clone());
        self.forget_attr(parent(path));
        (id, self.fattr(id, &attr))
    }
}

#[async_trait]
impl NFSFileSystem for AetherNfs {
    fn capabilities(&self) -> VFSCapabilities {
        if self.read_only {
            VFSCapabilities::ReadOnly
        } else {
            VFSCapabilities::ReadWrite
        }
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_ID
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let dir = self.path_of(dirid)?;
        let name = name_str(filename)?;
        let path = match name {
            "" | "." => return Ok(dirid),
            ".." => parent(&dir).to_string(),
            n => {
                validate_name(n).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
                join(&dir, n)
            }
        };
        self.stat_path(&path).await?;
        Ok(self.ids.lock().intern(&path))
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let path = self.path_of(id)?;
        let a = self.stat_path(&path).await?;
        Ok(self.fattr(id, &a))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let path = self.path_of(id)?;
        if let set_size3::size(size) = setattr.size {
            self.rw()?;
            let a = self
                .client
                .truncate(self.share, &path, size)
                .await
                .map_err(to_nfs)?;
            self.remember(&path, a.clone());
            return Ok(self.fattr(id, &a));
        }
        // Mode/owner/time changes are not part of the VFS protocol; accept them as no-ops so
        // tools like `cp -p` keep working, and report the current attributes.
        self.forget_attr(&path);
        self.getattr(id).await
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let path = self.path_of(id)?;
        let attr = self.stat_path(&path).await?;
        if self.client.cache().is_some() && attr.kind == FileKind::File {
            let data = self
                .client
                .read_cached(self.share, &path, offset, count)
                .await
                .map_err(to_nfs)?;
            let eof = offset + data.len() as u64 >= attr.size;
            Ok((data, eof))
        } else {
            self.client
                .read(self.share, &path, offset, count)
                .await
                .map_err(to_nfs)
        }
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        self.rw()?;
        let path = self.path_of(id)?;
        let a = self
            .client
            .write(self.share, &path, offset, data.to_vec())
            .await
            .map_err(to_nfs)?;
        self.remember(&path, a.clone());
        Ok(self.fattr(id, &a))
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.rw()?;
        let path = join(&self.path_of(dirid)?, checked_name(filename)?);
        let mut a = self
            .client
            .create(self.share, &path, false)
            .await
            .map_err(to_nfs)?;
        if let set_size3::size(size) = attr.size {
            a = self
                .client
                .truncate(self.share, &path, size)
                .await
                .map_err(to_nfs)?;
        }
        Ok(self.new_entry(&path, a).await)
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        self.rw()?;
        let path = join(&self.path_of(dirid)?, checked_name(filename)?);
        let a = self
            .client
            .create(self.share, &path, true)
            .await
            .map_err(to_nfs)?;
        Ok(self.new_entry(&path, a).await.0)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.rw()?;
        let path = join(&self.path_of(dirid)?, checked_name(dirname)?);
        let a = self.client.mkdir(self.share, &path).await.map_err(to_nfs)?;
        Ok(self.new_entry(&path, a).await)
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        self.rw()?;
        let dir = self.path_of(dirid)?;
        let path = join(&dir, checked_name(filename)?);
        self.client
            .remove(self.share, &path)
            .await
            .map_err(to_nfs)?;
        self.ids.lock().forget(&path);
        self.forget_attr(&path);
        self.forget_attr(&dir);
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        self.rw()?;
        let fdir = self.path_of(from_dirid)?;
        let tdir = self.path_of(to_dirid)?;
        let from = join(&fdir, checked_name(from_filename)?);
        let to = join(&tdir, checked_name(to_filename)?);
        self.client
            .rename(self.share, &from, &to)
            .await
            .map_err(to_nfs)?;
        self.ids.lock().rename(&from, &to);
        // Cached attributes of the moved subtree are keyed by path: drop them all.
        self.attrs.lock().clear();
        Ok(())
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let dir = self.path_of(dirid)?;
        let entries = self
            .client
            .read_dir(self.share, &dir)
            .await
            .map_err(to_nfs)?;
        let mut items = Vec::with_capacity(entries.len());
        {
            let mut ids = self.ids.lock();
            for e in entries {
                let path = join(&dir, &e.name);
                let id = ids.intern(&path);
                items.push((id, path, e));
            }
        }
        let start = if start_after == 0 {
            0
        } else {
            match items.iter().position(|(id, _, _)| *id == start_after) {
                Some(p) => p + 1,
                None => return Err(nfsstat3::NFS3ERR_BAD_COOKIE),
            }
        };
        let mut out = ReadDirResult {
            entries: Vec::new(),
            end: false,
        };
        let total = items.len();
        for (i, (id, path, e)) in items.into_iter().enumerate().skip(start) {
            if out.entries.len() >= max_entries {
                break;
            }
            let attr = self.fattr(id, &e.attr);
            self.remember(&path, e.attr);
            out.entries.push(DirEntry {
                fileid: id,
                name: nfsstring(e.name.into_bytes()),
                attr,
            });
            if i + 1 == total {
                out.end = true;
            }
        }
        if start >= total {
            out.end = true;
        }
        Ok(out)
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.rw()?;
        let path = join(&self.path_of(dirid)?, checked_name(linkname)?);
        let target = std::str::from_utf8(&symlink.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let a = self
            .client
            .symlink(self.share, &path, target)
            .await
            .map_err(to_nfs)?;
        Ok(self.new_entry(&path, a).await)
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let path = self.path_of(id)?;
        let t = self
            .client
            .read_link(self.share, &path)
            .await
            .map_err(to_nfs)?;
        Ok(nfsstring(t.into_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_map_rename_moves_subtree() {
        let mut m = IdMap::new();
        let a = m.intern("a");
        let ab = m.intern("a/b");
        let ax = m.intern("ax");
        m.rename("a", "z/a");
        assert_eq!(m.by_id[&a], "z/a");
        assert_eq!(m.by_id[&ab], "z/a/b");
        assert_eq!(m.by_id[&ax], "ax");
        assert_eq!(m.intern("z/a/b"), ab);
        assert_eq!(m.by_id[&ROOT_ID], "");
    }
}
