//! Real FUSE mount through `aether_vfs::mount()` on Linux (no root needed: `fusermount3`).
//!
//! Run with `cargo test -p aether-vfs --test fuse_linux -- --ignored`; in Docker pass
//! `--device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined`.
#![cfg(target_os = "linux")]

use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aether_vfs::{
    ChunkCache, MountBackend, ShareRegistry, VFS_CHUNK_SIZE, VfsClient, VfsHost, serve,
};

fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect()
}

struct Fixture {
    _share_dir: tempfile::TempDir,
    _ro_dir: tempfile::TempDir,
    _cache_dir: tempfile::TempDir,
    root: PathBuf,
    client: Arc<VfsClient>,
    rw: u32,
    ro: u32,
}

async fn setup() -> Fixture {
    let share_dir = tempfile::tempdir().unwrap();
    let ro_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = share_dir.path().to_path_buf();
    std::fs::write(root.join("hello.txt"), b"hello world\n").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(
        root.join("sub/big.bin"),
        pattern(VFS_CHUNK_SIZE as usize * 4 + 12_345, 1),
    )
    .unwrap();
    std::fs::write(ro_dir.path().join("readme.md"), b"read only").unwrap();

    let mut reg = ShareRegistry::new();
    let rw = reg.add("Work", &root, false).unwrap();
    let ro = reg.add("Docs", ro_dir.path(), true).unwrap();
    let host = Arc::new(VfsHost::new(reg));

    let (client_io, host_io) = tokio::io::duplex(256 * 1024);
    let (hr, hw) = tokio::io::split(host_io);
    tokio::spawn(serve(host, hr, hw));
    let (cr, cw) = tokio::io::split(client_io);
    let cache = ChunkCache::open(cache_dir.path(), 64 * 1024 * 1024).unwrap();
    let client = Arc::new(VfsClient::with_cache(cr, cw, Some(cache)));
    Fixture {
        root,
        _share_dir: share_dir,
        _ro_dir: ro_dir,
        _cache_dir: cache_dir,
        client,
        rw,
        ro,
    }
}

/// Whether `mp` is a mount point of this process' mount namespace.
fn is_mounted(mp: &Path) -> bool {
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    let mp = mp.to_str().unwrap();
    // Field 5 is the mount point (octal-escaped; our temp paths need no escaping).
    info.lines().any(|l| l.split(' ').nth(4) == Some(mp))
}

fn mount_line(mp: &Path) -> Option<String> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    let mp = mp.to_str().unwrap();
    info.lines()
        .find(|l| l.split(' ').nth(4) == Some(mp))
        .map(str::to_string)
}

fn rw_checks(mp: &Path, root: &Path) -> std::io::Result<()> {
    let mut names: Vec<String> = std::fs::read_dir(mp)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>()?;
    names.sort();
    assert_eq!(names, ["hello.txt", "sub"], "{names:?}");

    let md = std::fs::metadata(mp.join("hello.txt"))?;
    assert_eq!(md.len(), 12);
    // SAFETY: getuid has no preconditions.
    assert_eq!(std::os::unix::fs::MetadataExt::uid(&md), unsafe {
        libc::getuid()
    });
    assert_eq!(std::fs::read(mp.join("hello.txt"))?, b"hello world\n");
    let big = std::fs::read(mp.join("sub/big.bin"))?;
    assert_eq!(
        big,
        std::fs::read(root.join("sub/big.bin"))?,
        "multi-chunk read"
    );

    // Write a new file, read it back both through the mount and on the host.
    std::fs::write(mp.join("from_mount.txt"), b"written through FUSE")?;
    assert_eq!(
        std::fs::read(root.join("from_mount.txt"))?,
        b"written through FUSE"
    );
    assert_eq!(
        std::fs::read(mp.join("from_mount.txt"))?,
        b"written through FUSE"
    );
    // Overwrite with O_TRUNC (shorter content).
    std::fs::write(mp.join("from_mount.txt"), b"short")?;
    assert_eq!(std::fs::read(root.join("from_mount.txt"))?, b"short");
    // Exclusive create of an existing file.
    let e = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(mp.join("from_mount.txt"))
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::AlreadyExists);

    // mkdir + rename into it.
    std::fs::create_dir(mp.join("newdir"))?;
    assert!(root.join("newdir").is_dir());
    std::fs::rename(mp.join("from_mount.txt"), mp.join("newdir/renamed.txt"))?;
    assert!(root.join("newdir/renamed.txt").exists());
    assert!(!mp.join("from_mount.txt").exists());
    assert_eq!(std::fs::read(mp.join("newdir/renamed.txt"))?, b"short");

    // Truncate (grow and shrink).
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(mp.join("newdir/renamed.txt"))?;
    f.set_len(10)?;
    assert_eq!(
        std::fs::metadata(root.join("newdir/renamed.txt"))?.len(),
        10
    );
    f.set_len(2)?;
    drop(f);
    assert_eq!(std::fs::read(mp.join("newdir/renamed.txt"))?, b"sh");

    // chmod is accepted as a no-op.
    std::fs::set_permissions(
        mp.join("newdir/renamed.txt"),
        std::fs::Permissions::from_mode(0o600),
    )?;

    // symlink + readlink.
    std::os::unix::fs::symlink("../hello.txt", mp.join("newdir/link"))?;
    assert_eq!(
        std::fs::read_link(mp.join("newdir/link"))?,
        PathBuf::from("../hello.txt")
    );
    assert_eq!(
        std::fs::read_link(root.join("newdir/link"))?,
        PathBuf::from("../hello.txt")
    );
    assert_eq!(std::fs::read(mp.join("newdir/link"))?, b"hello world\n");

    // rmdir of a non-empty directory fails; unlink + rmdir succeed.
    let e = std::fs::remove_dir(mp.join("newdir")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOTEMPTY), "{e:?}");
    std::fs::remove_file(mp.join("newdir/link"))?;
    std::fs::remove_file(mp.join("newdir/renamed.txt"))?;
    assert!(!root.join("newdir/renamed.txt").exists());
    std::fs::remove_dir(mp.join("newdir"))?;
    assert!(!root.join("newdir").exists());
    let e = std::fs::metadata(mp.join("newdir")).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound);
    Ok(())
}

fn ro_checks(mp: &Path) -> std::io::Result<()> {
    assert_eq!(std::fs::read(mp.join("readme.md"))?, b"read only");
    let mode = std::fs::metadata(mp.join("readme.md"))?
        .permissions()
        .mode();
    assert_eq!(mode & 0o222, 0, "write bits cleared: {mode:o}");
    let erofs = |r: std::io::Result<()>| {
        let e = r.unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EROFS), "{e:?}");
    };
    erofs(std::fs::write(mp.join("new.txt"), b"x"));
    erofs(std::fs::write(mp.join("readme.md"), b"x"));
    erofs(std::fs::create_dir(mp.join("d")));
    erofs(std::fs::remove_file(mp.join("readme.md")));
    erofs(std::fs::rename(mp.join("readme.md"), mp.join("r.md")));
    Ok(())
}

async fn mount_tmp(
    f: &Fixture,
    share: u32,
) -> (tempfile::TempDir, PathBuf, aether_vfs::MountHandle) {
    let parent = tempfile::Builder::new()
        .prefix("aetherlink-fuse-test-")
        .tempdir()
        .unwrap();
    let mp = parent.path().join("Peer");
    let handle = aether_vfs::mount(f.client.clone(), share, &mp)
        .await
        .expect("mount");
    let mp = handle.mount_point().to_path_buf();
    (parent, mp, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs /dev/fuse + fusermount3 (Docker: --device /dev/fuse --cap-add SYS_ADMIN)"]
async fn real_fuse_mount_roundtrip() {
    let f = setup().await;

    // Read-write share.
    let (_p, mp, handle) = mount_tmp(&f, f.rw).await;
    assert_eq!(handle.backend(), MountBackend::Fuse);
    assert_eq!(handle.port(), 0);
    let line = mount_line(&mp).expect("mounted");
    println!("mountinfo: {line}");
    // `fuse.aetherlink aetherlink` via fusermount3; `fuse aetherlink` via the direct (root) mount.
    let fs_fields = line.split(" - ").nth(1).unwrap_or_default();
    assert!(
        fs_fields.starts_with("fuse") && fs_fields.contains(" aetherlink "),
        "{line}"
    );
    let (mpc, root) = (mp.clone(), f.root.clone());
    let res = tokio::task::spawn_blocking(move || rw_checks(&mpc, &root)).await;
    handle.unmount().await.expect("unmount");
    assert!(!is_mounted(&mp), "still mounted after unmount");
    res.expect("check task panicked")
        .expect("filesystem checks through the mount");

    // Read-only share.
    let (_p, mp, handle) = mount_tmp(&f, f.ro).await;
    assert_eq!(handle.backend(), MountBackend::Fuse);
    let mpc = mp.clone();
    let res = tokio::task::spawn_blocking(move || ro_checks(&mpc)).await;
    handle.unmount().await.expect("unmount ro");
    assert!(!is_mounted(&mp), "ro share still mounted after unmount");
    res.expect("ro check task panicked")
        .expect("read-only checks through the mount");
}

/// Dropping the handle unmounts in the background.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs /dev/fuse + fusermount3 (Docker: --device /dev/fuse --cap-add SYS_ADMIN)"]
async fn fuse_mount_unmounts_on_drop() {
    let f = setup().await;
    let (_p, mp, handle) = mount_tmp(&f, f.rw).await;
    assert!(is_mounted(&mp));
    drop(handle);
    for _ in 0..100 {
        if !is_mounted(&mp) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{} still mounted 5 s after drop", mp.display());
}
