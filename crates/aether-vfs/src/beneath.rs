//! Race-free path resolution beneath a share root (Unix).
//!
//! Every operation starts from a directory file descriptor of the share root and walks the
//! (already lexically validated) components with `openat(O_NOFOLLOW | O_DIRECTORY)`, never by
//! re-resolving a path string. The final operation is then performed *relative to the
//! resolved parent descriptor* (`openat`, `fstatat`, `mkdirat`, `unlinkat`, `renameat`,
//! `symlinkat`, `readlinkat`). Replacing a directory with a symlink after resolution
//! therefore cannot redirect the operation: the descriptor still refers to the original
//! directory, and a symlink appearing in the final component makes `O_NOFOLLOW` fail
//! (`ELOOP`, reported as permission denied).
//!
//! Symlinks found while walking are followed only if they stay beneath the root, with the
//! same rules as Linux `openat2(RESOLVE_BENEATH)`: absolute targets are refused and `..`
//! never climbs above the root (it is applied to the walk's own descriptor stack, not via
//! the filesystem's `..`). On Linux, `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)` is
//! used for direct opens when the kernel supports it (5.6+); the portable walk is the
//! fallback (`ENOSYS`, or seccomp `EPERM`).
//!
//! Residual risk: a local process that can *move a directory out of the share* while a
//! request holds a descriptor to it can make that one request operate in the moved
//! directory (true for `openat2` as well). It cannot make the walk follow a symlink out.

use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::path::Path;

/// Maximum number of symlinks expanded while resolving one path (as `MAXSYMLINKS`).
const MAX_SYMLINKS: u32 = 40;

fn cstr(s: &[u8]) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL byte in path"))
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Retry `f` while it fails with `EINTR`.
fn retry<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match f() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => return r,
        }
    }
}

/// The error returned when a path would leave the share.
pub(crate) fn outside() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "path resolves outside of the share",
    )
}

pub(crate) fn is_symlink(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFLNK
}

pub(crate) fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

/// Open the share root itself (absolute, canonical path).
pub(crate) fn open_root(root: &Path) -> io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let c = cstr(root.as_os_str().as_bytes())?;
    retry(|| {
        let fd = cvt(unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        })?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    })
}

/// `openat(dir, name, flags | O_CLOEXEC, mode)`.
pub(crate) fn openat(
    dir: BorrowedFd<'_>,
    name: &[u8],
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<OwnedFd> {
    let c = cstr(name)?;
    retry(|| {
        let fd = cvt(unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c.as_ptr(),
                flags | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        })?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    })
}

pub(crate) fn fstat(fd: BorrowedFd<'_>) -> io::Result<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    cvt(unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) })?;
    Ok(unsafe { st.assume_init() })
}

/// `fstatat(dir, name, AT_SYMLINK_NOFOLLOW)`.
pub(crate) fn lstatat(dir: BorrowedFd<'_>, name: &[u8]) -> io::Result<libc::stat> {
    let c = cstr(name)?;
    let mut st = MaybeUninit::<libc::stat>::uninit();
    cvt(unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    Ok(unsafe { st.assume_init() })
}

pub(crate) fn readlinkat(dir: BorrowedFd<'_>, name: &[u8]) -> io::Result<Vec<u8>> {
    let c = cstr(name)?;
    let mut buf = vec![0u8; 256];
    loop {
        let n = unsafe {
            libc::readlinkat(
                dir.as_raw_fd(),
                c.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = n as usize;
        if n < buf.len() {
            buf.truncate(n);
            return Ok(buf);
        }
        if buf.len() >= 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "symlink target too long",
            ));
        }
        buf.resize(buf.len() * 2, 0);
    }
}

pub(crate) fn mkdirat(dir: BorrowedFd<'_>, name: &[u8], mode: libc::mode_t) -> io::Result<()> {
    let c = cstr(name)?;
    cvt(unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), mode) }).map(drop)
}

pub(crate) fn unlinkat(dir: BorrowedFd<'_>, name: &[u8], is_dir: bool) -> io::Result<()> {
    let c = cstr(name)?;
    let flags = if is_dir { libc::AT_REMOVEDIR } else { 0 };
    cvt(unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), flags) }).map(drop)
}

pub(crate) fn renameat(
    from_dir: BorrowedFd<'_>,
    from: &[u8],
    to_dir: BorrowedFd<'_>,
    to: &[u8],
) -> io::Result<()> {
    let (f, t) = (cstr(from)?, cstr(to)?);
    cvt(unsafe {
        libc::renameat(
            from_dir.as_raw_fd(),
            f.as_ptr(),
            to_dir.as_raw_fd(),
            t.as_ptr(),
        )
    })
    .map(drop)
}

pub(crate) fn symlinkat(target: &[u8], dir: BorrowedFd<'_>, name: &[u8]) -> io::Result<()> {
    let (t, n) = (cstr(target)?, cstr(name)?);
    cvt(unsafe { libc::symlinkat(t.as_ptr(), dir.as_raw_fd(), n.as_ptr()) }).map(drop)
}

/// Names in the directory `dir` (without `.` and `..`).
pub(crate) fn read_dir_names(dir: BorrowedFd<'_>) -> io::Result<Vec<Vec<u8>>> {
    // fdopendir takes ownership of the descriptor: hand it a fresh one positioned at the start.
    let own = openat(dir, b".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let raw = own.into_raw_fd();
    let dp = unsafe { libc::fdopendir(raw) };
    if dp.is_null() {
        let e = io::Error::last_os_error();
        drop(unsafe { OwnedFd::from_raw_fd(raw) });
        return Err(e);
    }
    let mut out = Vec::new();
    loop {
        let ent = unsafe { libc::readdir(dp) };
        if ent.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            out.push(name.to_vec());
        }
    }
    unsafe { libc::closedir(dp) };
    Ok(out)
}

/// Resolve `comps` beneath `root`, following (contained) symlinks in every component,
/// including the last. Returns the directory holding the final component and its name, or
/// `(dir, None)` when the path designates a directory reached through the walk (the root,
/// or a symlink target ending in `..`/`/`). The final component is *not* opened.
fn resolve(root: BorrowedFd<'_>, comps: &[&str]) -> io::Result<(OwnedFd, Option<Vec<u8>>)> {
    let mut stack: Vec<OwnedFd> = vec![root.try_clone_to_owned()?];
    let mut queue: VecDeque<Vec<u8>> = comps.iter().map(|c| c.as_bytes().to_vec()).collect();
    let mut links = 0u32;
    while let Some(c) = queue.pop_front() {
        match c.as_slice() {
            b"" | b"." => continue,
            b".." => {
                if stack.len() == 1 {
                    return Err(outside());
                }
                stack.pop();
                continue;
            }
            _ => {}
        }
        let top = stack.last().expect("stack never empty").as_fd();
        if queue.iter().all(|q| q.is_empty() || q == b".") {
            // Final component: follow it only if it is a symlink.
            match lstatat(top, &c) {
                Ok(st) if is_symlink(&st) => {}
                _ => return Ok((stack.pop().expect("non-empty"), Some(c))),
            }
        } else {
            match openat(
                top,
                &c,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            ) {
                Ok(fd) => {
                    stack.push(fd);
                    continue;
                }
                Err(e) => match lstatat(top, &c) {
                    Ok(st) if is_symlink(&st) => {}
                    _ => return Err(e),
                },
            }
        }
        // `c` is a symlink: splice its target into the remaining components.
        links += 1;
        if links > MAX_SYMLINKS {
            return Err(io::Error::from_raw_os_error(libc::ELOOP));
        }
        let target = readlinkat(top, &c)?;
        if target.is_empty() || target[0] == b'/' {
            return Err(outside());
        }
        for part in target.split(|b| *b == b'/').rev() {
            queue.push_front(part.to_vec());
        }
    }
    Ok((stack.pop().expect("non-empty"), None))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    static UNSUPPORTED: AtomicBool = AtomicBool::new(false);

    #[cfg(test)]
    thread_local! {
        /// Tests: force the portable walk on this thread.
        pub(crate) static FORCE_WALK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// `openat2(root, comps, flags, RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`; `None` when
    /// the kernel (or a seccomp filter) does not offer `openat2`.
    pub(super) fn open_beneath(
        root: BorrowedFd<'_>,
        comps: &[&str],
        flags: libc::c_int,
    ) -> Option<io::Result<OwnedFd>> {
        if UNSUPPORTED.load(Ordering::Relaxed) {
            return None;
        }
        #[cfg(test)]
        if FORCE_WALK.with(|f| f.get()) {
            return None;
        }
        let path = if comps.is_empty() {
            ".".to_string()
        } else {
            comps.join("/")
        };
        let c = match cstr(path.as_bytes()) {
            Ok(c) => c,
            Err(e) => return Some(Err(e)),
        };
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (flags | libc::O_CLOEXEC) as u64;
        how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS;
        for _ in 0..16 {
            let r = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    root.as_raw_fd(),
                    c.as_ptr(),
                    &how as *const libc::open_how,
                    std::mem::size_of::<libc::open_how>(),
                )
            };
            if r >= 0 {
                return Some(Ok(unsafe { OwnedFd::from_raw_fd(r as libc::c_int) }));
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ENOSYS) | Some(libc::EPERM) => {
                    UNSUPPORTED.store(true, Ordering::Relaxed);
                    return None;
                }
                Some(libc::EXDEV) => return Some(Err(outside())),
                // EAGAIN: a concurrent rename raced a `..` lookup; retry.
                Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
                _ => return Some(Err(e)),
            }
        }
        None
    }
}

/// Open the directory at `comps` (symlinks followed beneath the root).
pub(crate) fn open_dir(root: BorrowedFd<'_>, comps: &[&str]) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY;
    #[cfg(target_os = "linux")]
    if let Some(r) = linux::open_beneath(root, comps, flags) {
        return r;
    }
    match resolve(root, comps)? {
        (dir, None) => Ok(dir),
        (dir, Some(name)) => openat(dir.as_fd(), &name, flags | libc::O_NOFOLLOW, 0),
    }
}

/// Open the entry at `comps` with `flags` (symlinks followed beneath the root; a symlink
/// that appears in the final component after resolution fails with `ELOOP`).
pub(crate) fn open_follow(
    root: BorrowedFd<'_>,
    comps: &[&str],
    flags: libc::c_int,
) -> io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    if let Some(r) = linux::open_beneath(root, comps, flags) {
        return r;
    }
    match resolve(root, comps)? {
        (dir, None) => openat(dir.as_fd(), b".", flags, 0),
        (dir, Some(name)) => openat(dir.as_fd(), &name, flags | libc::O_NOFOLLOW, 0),
    }
}

/// `stat` of the entry at `comps`, following (contained) symlinks.
pub(crate) fn stat_follow(root: BorrowedFd<'_>, comps: &[&str]) -> io::Result<libc::stat> {
    match resolve(root, comps)? {
        (dir, None) => fstat(dir.as_fd()),
        (dir, Some(name)) => lstatat(dir.as_fd(), &name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn walk_stays_beneath_the_root() {
        check_walk_stays_beneath_the_root();
    }

    /// Linux: the same guarantees without `openat2` (old kernels, seccomp).
    #[cfg(target_os = "linux")]
    #[test]
    fn portable_walk_stays_beneath_the_root() {
        linux::FORCE_WALK.with(|f| f.set(true));
        check_walk_stays_beneath_the_root();
        linux::FORCE_WALK.with(|f| f.set(false));
    }

    fn check_walk_stays_beneath_the_root() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("share");
        let outside_dir = t.path().join("outside");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();
        std::fs::write(outside_dir.join("secret"), b"x").unwrap();
        std::fs::write(root.join("a/b/f"), b"inside").unwrap();
        symlink("b", root.join("a/rel")).unwrap();
        symlink("../a/b/f", root.join("a/to_file")).unwrap();
        symlink("../../outside", root.join("a/up")).unwrap();
        symlink(&outside_dir, root.join("abs")).unwrap();
        symlink("loop2", root.join("loop1")).unwrap();
        symlink("loop1", root.join("loop2")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let rfd = open_root(&root).unwrap();
        let r = rfd.as_fd();

        // Contained symlinks are followed.
        assert!(open_dir(r, &["a", "rel"]).is_ok());
        let f = open_follow(r, &["a", "to_file"], libc::O_RDONLY).unwrap();
        assert_eq!(fstat(f.as_fd()).unwrap().st_size, 6);
        assert!(stat_follow(r, &["a", "rel", "f"]).is_ok());
        // Escapes are refused, whichever way they are attempted.
        for p in [&["a", "up", "secret"][..], &["abs", "secret"], &["abs"]] {
            let e = open_follow(r, p, libc::O_RDONLY).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{p:?}: {e}");
        }
        assert_eq!(
            open_dir(r, &["a", "up"]).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(open_follow(r, &["loop1"], libc::O_RDONLY).is_err());
    }

    #[test]
    fn swapped_directory_cannot_redirect_an_operation() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("share");
        let outside_dir = t.path().join("outside");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let rfd = open_root(&root).unwrap();

        // Time of check: resolve the parent directory.
        let parent = open_dir(rfd.as_fd(), &["sub"]).unwrap();
        // Another local process swaps the directory for a symlink to the outside…
        std::fs::rename(root.join("sub"), root.join("sub_moved")).unwrap();
        symlink(&outside_dir, root.join("sub")).unwrap();
        // …time of use: the operation still lands in the directory that was checked.
        let f = openat(
            parent.as_fd(),
            b"new.txt",
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o644,
        )
        .unwrap();
        drop(f);
        assert!(root.join("sub_moved/new.txt").exists());
        assert!(!outside_dir.join("new.txt").exists());
        // A fresh resolution now refuses the swapped-in symlink.
        assert_eq!(
            open_dir(rfd.as_fd(), &["sub"]).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
