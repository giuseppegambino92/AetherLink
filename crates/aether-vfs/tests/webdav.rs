//! WebDAV loopback server driven by a real HTTP client (reqwest) against a
//! `VfsHost`-backed `VfsClient` (in-memory duplex stream, real files in a temp dir).

use std::sync::Arc;

use aether_vfs::{ChunkCache, ShareRegistry, VfsClient, VfsHost, serve, webdav};
use reqwest::{Method, StatusCode};

struct Fx {
    _dirs: Vec<tempfile::TempDir>,
    root: std::path::PathBuf,
    client: Arc<VfsClient>,
    rw: u32,
    ro: u32,
    http: reqwest::Client,
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

async fn setup(with_cache: bool) -> Fx {
    let share = tempfile::tempdir().unwrap();
    let ro = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = share.path().to_path_buf();
    std::fs::write(root.join("hello.txt"), b"hello world\n").unwrap();
    std::fs::create_dir(root.join("sub dir")).unwrap();
    std::fs::write(root.join("sub dir/data.bin"), pattern(3 * 1024 * 1024 + 17)).unwrap();
    std::fs::write(ro.path().join("readme.md"), b"read only").unwrap();
    let mut reg = ShareRegistry::new();
    let rw_id = reg.add("Work", &root, false).unwrap();
    let ro_id = reg.add("Docs", ro.path(), true).unwrap();
    let host = Arc::new(VfsHost::new(reg));
    let (client_io, host_io) = tokio::io::duplex(1024 * 1024);
    let (hr, hw) = tokio::io::split(host_io);
    tokio::spawn(serve(host, hr, hw));
    let (cr, cw) = tokio::io::split(client_io);
    let cc = with_cache.then(|| ChunkCache::open(cache.path(), 64 * 1024 * 1024).unwrap());
    let client = Arc::new(VfsClient::with_cache(cr, cw, cc));
    Fx {
        _dirs: vec![share, ro, cache],
        root,
        client,
        rw: rw_id,
        ro: ro_id,
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
    }
}

fn m(name: &str) -> Method {
    Method::from_bytes(name.as_bytes()).unwrap()
}

async fn start(f: &Fx, share: u32) -> webdav::WebDavHandle {
    webdav::serve(f.client.clone(), share, "127.0.0.1:0".parse().unwrap()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn propfind_get_range_put_mkcol_move_delete() {
    for with_cache in [false, true] {
        let f = setup(with_cache).await;
        let dav = start(&f, f.rw).await;
        let base = dav.url();
        assert!(base.starts_with("http://127.0.0.1:") && base.ends_with('/'));
        assert!(dav.unc_path().starts_with(r"\\localhost@"));

        // PROPFIND depth 1 on the share root lists both entries with sizes.
        let r = f
            .http
            .request(m("PROPFIND"), &base)
            .header("Depth", "1")
            .header("Content-Type", "application/xml")
            .body(r#"<?xml version="1.0"?><D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>"#)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::MULTI_STATUS);
        let xml = r.text().await.unwrap();
        assert!(xml.contains("hello.txt"), "{xml}");
        assert!(xml.contains("sub%20dir/"), "{xml}");
        assert!(xml.contains("<D:getcontentlength>12</D:getcontentlength>"), "{xml}");
        assert!(xml.contains("<D:collection"), "{xml}");

        // Windows-style PROPFIND (empty body, Microsoft UA) returns the Win32 properties.
        let r = f
            .http
            .request(m("PROPFIND"), format!("{base}hello.txt"))
            .header("Depth", "0")
            .header("User-Agent", "Microsoft-WebDAV-MiniRedir/10.0.19045")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::MULTI_STATUS);
        let xml = r.text().await.unwrap();
        assert!(xml.contains("Win32FileAttributes"), "{xml}");

        // GET whole + Range.
        let r = f.http.get(format!("{base}hello.txt")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.bytes().await.unwrap().as_ref(), b"hello world\n");
        let r = f.http.get(format!("{base}hello.txt")).header("Range", "bytes=6-10").send().await.unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(r.bytes().await.unwrap().as_ref(), b"world");
        let big = pattern(3 * 1024 * 1024 + 17);
        let r = f.http.get(format!("{base}sub%20dir/data.bin")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.bytes().await.unwrap().as_ref(), &big[..]);
        let r = f
            .http
            .get(format!("{base}sub%20dir/data.bin"))
            .header("Range", "bytes=1048570-1048580")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(r.bytes().await.unwrap().as_ref(), &big[1048570..=1048580]);

        // PUT a new multi-MiB file (exercises write buffering), read it back from disk + HTTP.
        let up: Vec<u8> = (0..2_500_000u32).map(|i| (i % 253) as u8).collect();
        let r = f.http.put(format!("{base}upload.bin")).body(up.clone()).send().await.unwrap();
        assert!(r.status() == StatusCode::CREATED || r.status() == StatusCode::NO_CONTENT, "{}", r.status());
        assert_eq!(std::fs::read(f.root.join("upload.bin")).unwrap(), up);
        // Overwrite with a shorter body truncates.
        let r = f.http.put(format!("{base}upload.bin")).body("short").send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.status());
        assert_eq!(std::fs::read(f.root.join("upload.bin")).unwrap(), b"short");

        // MKCOL, MOVE, COPY, DELETE.
        let r = f.http.request(m("MKCOL"), format!("{base}newdir")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        assert!(f.root.join("newdir").is_dir());
        let r = f
            .http
            .request(m("MOVE"), format!("{base}upload.bin"))
            .header("Destination", format!("{base}newdir/moved.bin"))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.status());
        assert!(!f.root.join("upload.bin").exists());
        assert_eq!(std::fs::read(f.root.join("newdir/moved.bin")).unwrap(), b"short");
        let r = f
            .http
            .request(m("COPY"), format!("{base}hello.txt"))
            .header("Destination", format!("{base}newdir/copy.txt"))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.status());
        assert_eq!(std::fs::read(f.root.join("newdir/copy.txt")).unwrap(), b"hello world\n");
        let r = f.http.delete(format!("{base}newdir/moved.bin")).send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.status());
        assert!(!f.root.join("newdir/moved.bin").exists());
        let r = f.http.get(format!("{base}newdir/moved.bin")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);

        dav.shutdown();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lock_unlock_and_windows_probes() {
    let f = setup(false).await;
    let dav = start(&f, f.rw).await;
    let base = dav.url();

    let lock_body = r#"<?xml version="1.0" encoding="utf-8"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner><D:href>tester</D:href></D:owner></D:lockinfo>"#;
    let r = f
        .http
        .request(m("LOCK"), format!("{base}hello.txt"))
        .header("Timeout", "Second-3600")
        .header("Depth", "0")
        .body(lock_body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let token = r.headers().get("Lock-Token").expect("Lock-Token header").to_str().unwrap().to_string();
    assert!(token.starts_with('<') && token.ends_with('>') && token.len() > 10, "{token}");
    // A PUT without the token is refused while locked; with the token it succeeds.
    let r = f.http.put(format!("{base}hello.txt")).body("x").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::LOCKED);
    let r = f
        .http
        .put(format!("{base}hello.txt"))
        .header("If", format!("({token})"))
        .body("locked write")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let r = f.http.request(m("UNLOCK"), format!("{base}hello.txt")).header("Lock-Token", &token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(std::fs::read(f.root.join("hello.txt")).unwrap(), b"locked write");

    // OPTIONS advertises class 2 on the share and on the anonymous root probe.
    let r = f.http.request(Method::OPTIONS, &base).send().await.unwrap();
    assert!(r.status().is_success());
    assert!(r.headers().get("DAV").unwrap().to_str().unwrap().contains('2'));
    let root = format!("http://{}/", dav.local_addr());
    let r = f.http.request(Method::OPTIONS, &root).send().await.unwrap();
    assert!(r.status().is_success());
    assert!(r.headers().get("DAV").unwrap().to_str().unwrap().contains('2'));
    let r = f.http.request(m("PROPFIND"), &root).header("Depth", "1").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::MULTI_STATUS);
    let xml = r.text().await.unwrap();
    assert!(!xml.contains(dav.token()), "root listing must not reveal the share token");

    // Anything outside the capability prefix is invisible.
    let r = f.http.get(format!("{root}hello.txt")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let r = f.http.get(format!("{root}{}x/hello.txt", dav.token())).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    // DNS-rebinding style Host headers are refused.
    let r = f.http.get(format!("{base}hello.txt")).header("Host", "attacker.example").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_share_and_loopback_only() {
    let f = setup(false).await;
    let dav = start(&f, f.ro).await;
    assert!(dav.read_only());
    let base = dav.url();
    let r = f.http.get(format!("{base}readme.md")).send().await.unwrap();
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"read only");
    for (method, url) in [
        (Method::PUT, format!("{base}new.txt")),
        (m("MKCOL"), format!("{base}d")),
        (Method::DELETE, format!("{base}readme.md")),
    ] {
        let r = f.http.request(method.clone(), url).body("x").send().await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{method}");
    }

    let err = webdav::serve(f.client.clone(), f.rw, "0.0.0.0:0".parse().unwrap()).await.unwrap_err();
    assert!(err.to_string().contains("loopback"), "{err}");
    let err = webdav::serve(f.client.clone(), 999, "127.0.0.1:0".parse().unwrap()).await.unwrap_err();
    assert!(err.to_string().contains("999"), "{err}");

    // Shutting down closes the listener.
    let url = dav.url();
    dav.shutdown();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(f.http.get(url).send().await.is_err());
}
