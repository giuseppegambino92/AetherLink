//! End-to-end tests: VfsClient ↔ serve(VfsHost) over an in-memory duplex stream.

#[cfg(unix)]
use std::path::Path;
use std::sync::Arc;

use aether_vfs::{
    ChunkCache, FileKind, ShareRegistry, VFS_CHUNK_SIZE, VfsClient, VfsError, VfsErrorKind,
    VfsHost, VfsRequest, VfsResponse, serve,
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
    _outside: tempfile::TempDir,
    _cache_dir: tempfile::TempDir,
    root: std::path::PathBuf,
    outside: std::path::PathBuf,
    client: Arc<VfsClient>,
    rw: u32,
    ro: u32,
    server: tokio::task::JoinHandle<aether_vfs::Result<()>>,
}

async fn setup() -> Fixture {
    let share_dir = tempfile::tempdir().unwrap();
    let ro_dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = share_dir.path().to_path_buf();
    std::fs::write(root.join("hello.txt"), b"hello world\n").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(
        root.join("sub/big.bin"),
        pattern(VFS_CHUNK_SIZE as usize * 4 + 12_345, 1),
    )
    .unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"top secret").unwrap();
    std::fs::write(ro_dir.path().join("readme.md"), b"read only").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("escape_file"))
            .unwrap();
        std::os::unix::fs::symlink("hello.txt", root.join("inner_link")).unwrap();
    }

    let mut reg = ShareRegistry::new();
    let rw = reg.add("Work", &root, false).unwrap();
    let ro = reg.add("Docs", ro_dir.path(), true).unwrap();
    let host = Arc::new(VfsHost::new(reg));

    let (client_io, host_io) = tokio::io::duplex(256 * 1024);
    let (hr, hw) = tokio::io::split(host_io);
    let server = tokio::spawn(serve(host, hr, hw));
    let (cr, cw) = tokio::io::split(client_io);
    let cache = ChunkCache::open(cache_dir.path(), 64 * 1024 * 1024).unwrap();
    let client = Arc::new(VfsClient::with_cache(cr, cw, Some(cache)));
    Fixture {
        root,
        outside: outside.path().to_path_buf(),
        _share_dir: share_dir,
        _ro_dir: ro_dir,
        _outside: outside,
        _cache_dir: cache_dir,
        client,
        rw,
        ro,
        server,
    }
}

fn kind(e: VfsError) -> VfsErrorKind {
    e.kind()
        .unwrap_or_else(|| panic!("expected remote error, got {e:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_readdir_read_with_chunk_cache() {
    let f = setup().await;
    let c = &f.client;

    let shares = c.list_shares().await.unwrap();
    assert_eq!(shares.len(), 2);
    assert!(shares.iter().any(|s| s.name == "Work" && !s.read_only));
    assert!(shares.iter().any(|s| s.name == "Docs" && s.read_only));

    let names: Vec<String> = c
        .read_dir(f.rw, "")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(names.contains(&"hello.txt".to_string()));
    assert!(names.contains(&"sub".to_string()));
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "listing must be deterministic");

    let root_attr = c.stat(f.rw, "").await.unwrap();
    assert_eq!(root_attr.kind, FileKind::Directory);
    let a = c.stat(f.rw, "hello.txt").await.unwrap();
    assert_eq!((a.kind, a.size), (FileKind::File, 12));
    assert_ne!(a.file_id, 0);
    assert_eq!(
        c.stat(f.rw, "./hello.txt").await.unwrap().file_id,
        a.file_id,
        "file_id is stable"
    );
    #[cfg(unix)]
    assert_eq!(
        c.stat(f.rw, "inner_link").await.unwrap().kind,
        FileKind::Symlink
    );

    let (d, eof) = c.read(f.rw, "hello.txt", 6, 100).await.unwrap();
    assert_eq!((d.as_slice(), eof), (&b"world\n"[..], true));

    // Chunk-cached reads of a multi-chunk file.
    let expected = std::fs::read(f.root.join("sub/big.bin")).unwrap();
    let n_chunks = expected.len().div_ceil(VFS_CHUNK_SIZE as usize);
    let hashes = c.chunk_hashes(f.rw, "sub/big.bin").await.unwrap();
    assert_eq!(hashes.len(), n_chunks);
    assert_eq!(
        hashes[1],
        *blake3::hash(&expected[VFS_CHUNK_SIZE as usize..2 * VFS_CHUNK_SIZE as usize]).as_bytes()
    );

    let off = VFS_CHUNK_SIZE as u64 - 100;
    let got = c
        .read_cached(f.rw, "sub/big.bin", off, 200_000)
        .await
        .unwrap();
    assert_eq!(got, &expected[off as usize..off as usize + 200_000]);
    let cache = c.cache().unwrap().clone();
    assert_eq!(cache.len(), 2, "two chunks touched");

    let all = c
        .read_cached(f.rw, "sub/big.bin", 0, expected.len() as u32 + 10)
        .await
        .unwrap();
    assert_eq!(all, expected);
    assert_eq!(cache.len(), n_chunks);
    let before = cache.stats();
    let again = c
        .read_cached(f.rw, "sub/big.bin", 0, expected.len() as u32)
        .await
        .unwrap();
    assert_eq!(again, expected);
    let after = cache.stats();
    assert_eq!(
        after.hits - before.hits,
        n_chunks as u64,
        "second read served entirely from cache"
    );
    assert_eq!(after.misses, before.misses);
    // Cache files are content-addressed and verifiable.
    for h in &hashes {
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        let data = std::fs::read(cache.dir().join(format!("{hex}.chunk"))).unwrap();
        assert_eq!(blake3::hash(&data).as_bytes(), h);
    }

    // Changing the file on the host invalidates via the (path, mtime) → hashes mapping.
    let mut changed = expected.clone();
    changed[10] ^= 0xFF;
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(f.root.join("sub/big.bin"), &changed).unwrap();
    let got = c.read_cached(f.rw, "sub/big.bin", 0, 64).await.unwrap();
    assert_eq!(got, &changed[..64]);

    // Host caps Read at 4 MiB.
    std::fs::write(f.root.join("huge.bin"), vec![7u8; 5 * 1024 * 1024]).unwrap();
    let (d, eof) = c.read(f.rw, "huge.bin", 0, 10 * 1024 * 1024).await.unwrap();
    assert_eq!(d.len(), aether_vfs::MAX_READ_LEN as usize);
    assert!(!eof);
    assert_eq!(
        c.read_all(f.rw, "huge.bin").await.unwrap().len(),
        5 * 1024 * 1024
    );

    // Pipelining: many requests in flight before awaiting any.
    let mut pend = Vec::new();
    for i in 0..100u64 {
        pend.push(
            c.send(VfsRequest::Read {
                share: f.rw,
                path: "hello.txt".into(),
                offset: i % 12,
                len: 1,
            })
            .await
            .unwrap(),
        );
    }
    for (i, p) in pend.into_iter().enumerate() {
        match p.wait().await.unwrap() {
            VfsResponse::Data { data, .. } => assert_eq!(data[0], b"hello world\n"[i % 12]),
            r => panic!("{r:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_operations_and_read_only() {
    let f = setup().await;
    let c = &f.client;

    let a = c.create(f.rw, "new.txt", true).await.unwrap();
    assert_eq!(a.size, 0);
    assert_eq!(
        kind(c.create(f.rw, "new.txt", true).await.unwrap_err()),
        VfsErrorKind::AlreadyExists
    );
    let a = c
        .write(f.rw, "new.txt", 0, b"abcdef".to_vec())
        .await
        .unwrap();
    assert_eq!(a.size, 6);
    c.write(f.rw, "new.txt", 3, b"XYZW".to_vec()).await.unwrap();
    assert_eq!(std::fs::read(f.root.join("new.txt")).unwrap(), b"abcXYZW");
    assert_eq!(c.truncate(f.rw, "new.txt", 2).await.unwrap().size, 2);

    c.mkdir(f.rw, "d1").await.unwrap();
    c.mkdir(f.rw, "d1/d2").await.unwrap();
    c.rename(f.rw, "new.txt", "d1/d2/moved.txt").await.unwrap();
    assert_eq!(
        std::fs::read(f.root.join("d1/d2/moved.txt")).unwrap(),
        b"ab"
    );
    assert_eq!(
        kind(c.remove(f.rw, "d1").await.unwrap_err()),
        VfsErrorKind::NotEmpty
    );
    c.remove(f.rw, "d1/d2/moved.txt").await.unwrap();
    c.remove(f.rw, "d1/d2").await.unwrap();
    assert_eq!(
        kind(c.stat(f.rw, "d1/d2").await.unwrap_err()),
        VfsErrorKind::NotFound
    );
    assert_eq!(
        kind(c.read_dir(f.rw, "hello.txt").await.unwrap_err()),
        VfsErrorKind::NotADirectory
    );

    #[cfg(unix)]
    {
        c.symlink(f.rw, "d1/link", "../hello.txt").await.unwrap();
        assert_eq!(c.read_link(f.rw, "d1/link").await.unwrap(), "../hello.txt");
        assert_eq!(c.read(f.rw, "d1/link", 0, 5).await.unwrap().0, b"hello");
        // Removing a symlink removes the link, not the target.
        c.remove(f.rw, "d1/link").await.unwrap();
        assert!(f.root.join("hello.txt").exists());
    }

    // Read-only share.
    assert_eq!(
        c.read(f.ro, "readme.md", 0, 100).await.unwrap().0,
        b"read only"
    );
    for e in [
        c.write(f.ro, "readme.md", 0, b"x".to_vec())
            .await
            .unwrap_err(),
        c.create(f.ro, "x", false).await.unwrap_err(),
        c.mkdir(f.ro, "x").await.unwrap_err(),
        c.remove(f.ro, "readme.md").await.unwrap_err(),
        c.rename(f.ro, "readme.md", "y").await.unwrap_err(),
        c.truncate(f.ro, "readme.md", 0).await.unwrap_err(),
    ] {
        assert_eq!(kind(e), VfsErrorKind::ReadOnly);
    }
    assert_eq!(
        kind(c.stat(999, "").await.unwrap_err()),
        VfsErrorKind::NoSuchShare
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn traversal_attacks_are_rejected() {
    let f = setup().await;
    let c = &f.client;
    let outside_file = f.outside.join("secret.txt");
    let secret_abs = outside_file.to_str().unwrap().to_string();

    for p in [
        "../secret.txt",
        "sub/../../secret.txt",
        "..",
        "/etc/passwd",
        &secret_abs,
        "C:/Windows/win.ini",
        "\\\\server\\share\\x",
        "//server/share",
        "a\0b",
    ] {
        let e = c.read(f.rw, p, 0, 10).await.unwrap_err();
        assert_eq!(kind(e), VfsErrorKind::InvalidPath, "read {p:?}");
        let e = c.stat(f.rw, p).await.unwrap_err();
        assert_eq!(kind(e), VfsErrorKind::InvalidPath, "stat {p:?}");
        let e = c.create(f.rw, p, false).await.unwrap_err();
        assert_eq!(kind(e), VfsErrorKind::InvalidPath, "create {p:?}");
    }

    #[cfg(unix)]
    {
        // Symlinks pointing outside the share cannot be followed…
        let denied = |e: VfsError| assert_eq!(kind(e), VfsErrorKind::PermissionDenied);
        denied(c.read(f.rw, "escape/secret.txt", 0, 10).await.unwrap_err());
        denied(c.read(f.rw, "escape_file", 0, 10).await.unwrap_err());
        denied(c.read_dir(f.rw, "escape").await.unwrap_err());
        denied(c.stat(f.rw, "escape/secret.txt").await.unwrap_err());
        denied(c.chunk_hashes(f.rw, "escape_file").await.unwrap_err());
        // …nor used as parents/targets for writes.
        denied(
            c.write(f.rw, "escape_file", 0, b"pwned".to_vec())
                .await
                .unwrap_err(),
        );
        denied(c.truncate(f.rw, "escape_file", 0).await.unwrap_err());
        denied(c.create(f.rw, "escape/new.txt", false).await.unwrap_err());
        denied(c.create(f.rw, "escape_file", false).await.unwrap_err());
        denied(c.mkdir(f.rw, "escape/newdir").await.unwrap_err());
        denied(
            c.rename(f.rw, "hello.txt", "escape/stolen.txt")
                .await
                .unwrap_err(),
        );
        denied(
            c.rename(f.rw, "escape/secret.txt", "got.txt")
                .await
                .unwrap_err(),
        );
        denied(c.remove(f.rw, "escape/secret.txt").await.unwrap_err());
        denied(c.symlink(f.rw, "escape/l", "x").await.unwrap_err());
        // The link itself is visible (as a symlink) but reveals nothing beyond its text.
        assert_eq!(
            c.stat(f.rw, "escape").await.unwrap().kind,
            FileKind::Symlink
        );
        assert_eq!(std::fs::read(&outside_file).unwrap(), b"top secret");
        assert!(!f.outside.join("new.txt").exists());
        assert!(!f.outside.join("stolen.txt").exists());

        // Creating escaping symlinks is refused.
        for t in ["../outside", "/etc/passwd", "sub/../../x", "C:/x"] {
            assert_eq!(
                kind(c.symlink(f.rw, "evil", t).await.unwrap_err()),
                VfsErrorKind::InvalidPath,
                "{t}"
            );
        }
        assert!(Path::new(&f.root.join("evil")).symlink_metadata().is_err());
    }

    // Operations on the share root itself.
    assert_eq!(
        kind(c.remove(f.rw, "").await.unwrap_err()),
        VfsErrorKind::InvalidPath
    );
    assert_eq!(
        kind(c.rename(f.rw, ".", "x").await.unwrap_err()),
        VfsErrorKind::InvalidPath
    );

    // Connection still healthy after all that.
    assert_eq!(c.read(f.rw, "hello.txt", 0, 5).await.unwrap().0, b"hello");
}

#[tokio::test]
async fn client_reports_closed_connection() {
    let f = setup().await;
    f.server.abort();
    let _ = f.server.await;
    // The host half was dropped with the server task: requests fail with Closed.
    let e = f.client.stat(f.rw, "").await.unwrap_err();
    assert!(matches!(e, VfsError::Closed), "{e:?}");
    assert!(f.client.is_closed());
}

/// Real loopback NFS mount. Run with `cargo test -p aether-vfs -- --ignored mount`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "mounts a real NFS filesystem (macOS: user-owned dir; Linux: needs root)"]
async fn real_nfs_mount_roundtrip() {
    let f = setup().await;
    let home = std::env::var("HOME").unwrap();
    let mp_parent = tempfile::Builder::new()
        .prefix("aetherlink-mount-test-")
        .tempdir_in(&home)
        .unwrap();
    let mp = mp_parent.path().join("Peer");
    let handle = aether_vfs::mount(f.client.clone(), f.rw, &mp)
        .await
        .expect("mount");
    let (mpc, root) = (mp.clone(), f.root.clone());
    let res = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mp = mpc;
        let mut names: Vec<String> = std::fs::read_dir(&mp)?
            .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()?;
        names.sort();
        println!("mounted listing: {names:?}");
        assert!(names.contains(&"hello.txt".to_string()), "{names:?}");
        assert_eq!(std::fs::read(mp.join("hello.txt"))?, b"hello world\n");
        let big = std::fs::read(mp.join("sub/big.bin"))?;
        assert_eq!(big, std::fs::read(root.join("sub/big.bin"))?);
        std::fs::write(mp.join("from_mount.txt"), b"written through NFS")?;
        assert_eq!(
            std::fs::read(root.join("from_mount.txt"))?,
            b"written through NFS"
        );
        std::fs::create_dir(mp.join("newdir"))?;
        std::fs::rename(mp.join("from_mount.txt"), mp.join("newdir/renamed.txt"))?;
        assert!(root.join("newdir/renamed.txt").exists());
        std::fs::remove_file(mp.join("newdir/renamed.txt"))?;
        Ok(())
    })
    .await;
    handle.unmount().await.expect("unmount");
    res.expect("check task panicked")
        .expect("filesystem checks through the mount");
}
