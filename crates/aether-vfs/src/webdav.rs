//! WebDAV (RFC 4918) loopback server exposing one remote share.
//!
//! [`serve`] starts an HTTP/1.1 server on a **loopback** address that translates WebDAV
//! requests into [`VfsClient`] calls (via the [`dav_server`] protocol engine):
//!
//! | WebDAV                | VFS                                                         |
//! |-----------------------|-------------------------------------------------------------|
//! | `PROPFIND` (depth 0/1)| `Stat` / `ReadDir` (symlinks resolved, dangling ones hidden) |
//! | `GET` / `HEAD` + `Range` | `Read` (through the BLAKE3-verified chunk cache when the client has one) |
//! | `PUT`                 | `Create` + `Truncate` + buffered 1 MiB `Write`s              |
//! | `MKCOL`               | `Mkdir`                                                      |
//! | `DELETE`              | `Remove`                                                     |
//! | `MOVE` / `COPY`       | `Rename` / streamed read + write                             |
//! | `LOCK` / `UNLOCK`     | in-memory lock table ([`dav_server::memls::MemLs`])          |
//! | `PROPPATCH`           | Win32 times accepted (no-op), as Windows Explorer requires   |
//!
//! The Windows mini-redirector (WebClient service) refuses to write to servers without
//! class-2 (locking) support and probes `OPTIONS` / `PROPFIND` on the server root before
//! touching the share, so both are answered for `/` without revealing anything.
//!
//! ## Security
//!
//! * The listener only binds loopback addresses ([`serve`] rejects anything else).
//! * Every share is served below a random 128-bit path prefix (a capability URL,
//!   [`WebDavHandle::base_path`]); other local users / processes cannot guess it. Requests
//!   outside the prefix get `404` (except the anonymous root probes above).
//! * Requests whose `Host` header is not a loopback name are refused (`421`), which defeats
//!   DNS-rebinding attacks from web pages in a local browser.
//! * Read-only shares refuse every mutating method with `403`; the remote host enforces
//!   the share's sandbox and read-only flag again on its side.
//!
//! ## Windows WebClient limits
//!
//! The WebClient redirector refuses downloads larger than
//! `HKLM\SYSTEM\CurrentControlSet\Services\WebClient\Parameters\FileSizeLimitInBytes`
//! (default 50 000 000 bytes ≈ 47.7 MiB; max `0xFFFFFFFF` ≈ 4 GiB) with "the file size
//! exceeds the limit allowed and cannot be saved". Raise it (admin) and restart the
//! WebClient service for larger files; this server has no such limit.

use std::convert::Infallible;
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_protocol::{FileAttr, FileKind, VfsErrorKind};
use bytes::{Buf, Bytes};
use dav_server::body::Body;
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream, OpenOptions,
    ReadDirMeta,
};
use dav_server::{DavHandler, memls::MemLs};
use futures::FutureExt;
use hyper::body::Incoming;
use hyper::header::{HeaderValue, HOST};
use hyper::{Method, Request, Response, StatusCode};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::client::VfsClient;
use crate::error::{Result, VfsError};

/// Size of the read requests issued for `GET` (and of the response body chunks).
pub const READ_CHUNK: usize = 256 * 1024;
/// `PUT` bodies are buffered up to this size before a VFS `Write` is sent.
pub const WRITE_BUFFER: usize = 1024 * 1024;
/// Default `FileSizeLimitInBytes` of the Windows WebClient service.
pub const WINDOWS_WEBCLIENT_DEFAULT_FILE_SIZE_LIMIT: u64 = 50_000_000;

// ---------------------------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------------------------

/// A running WebDAV loopback server. Dropping it stops the server (and all connections).
#[derive(Debug)]
pub struct WebDavHandle {
    addr: SocketAddr,
    token: String,
    read_only: bool,
    task: Option<JoinHandle<()>>,
}

impl WebDavHandle {
    /// Address the (first) listener is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The secret path segment the share is served under (no slashes).
    pub fn token(&self) -> &str {
        &self.token
    }

    /// `/<token>/` — the collection that is the root of the share.
    pub fn base_path(&self) -> String {
        format!("/{}/", self.token)
    }

    /// `http://127.0.0.1:<port>/<token>/` (IPv6 loopback addresses are bracketed).
    pub fn url(&self) -> String {
        format!("http://{}{}", self.addr, self.base_path())
    }

    /// UNC path understood by the Windows WebDAV redirector: `\\localhost@<port>\<token>`.
    pub fn unc_path(&self) -> String {
        format!(r"\\localhost@{}\{}", self.addr.port(), self.token)
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Stops accepting connections and aborts the open ones.
    pub fn shutdown(mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }

    /// Detaches the server task (the caller becomes responsible for aborting it).
    #[cfg(windows)]
    pub(crate) fn into_task(mut self) -> JoinHandle<()> {
        self.task.take().expect("server task present until shutdown/drop")
    }
}

impl Drop for WebDavHandle {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// Serves `share` of the remote VFS behind `client` over WebDAV on the loopback address
/// `bind` (use port 0 for an ephemeral port). See the module docs.
pub async fn serve(client: Arc<VfsClient>, share: u32, bind: SocketAddr) -> Result<WebDavHandle> {
    if !bind.ip().is_loopback() {
        return Err(VfsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("the WebDAV server only binds loopback addresses, not {}", bind.ip()),
        )));
    }
    let listener = TcpListener::bind(bind).await?;
    serve_listeners(client, share, vec![listener]).await
}

/// Like [`serve`] but on already bound listeners (all must be loopback). The handle reports
/// the first listener's address.
pub(crate) async fn serve_listeners(
    client: Arc<VfsClient>,
    share: u32,
    listeners: Vec<TcpListener>,
) -> Result<WebDavHandle> {
    let info = client
        .list_shares()
        .await?
        .into_iter()
        .find(|s| s.id == share)
        .ok_or_else(|| VfsError::Mount(format!("remote has no share with id {share}")))?;
    let addr = listeners
        .first()
        .ok_or_else(|| VfsError::Mount("no listener".into()))?
        .local_addr()?;
    let token = random_token();
    let fs = VfsDavFs { client, share, read_only: info.read_only };
    let dav = DavHandler::builder()
        .filesystem(Box::new(fs))
        .locksystem(MemLs::new())
        .strip_prefix(format!("/{token}"))
        .read_buf_size(READ_CHUNK)
        .hide_symlinks(true)
        .build_handler();
    let state = Arc::new(ServerState { dav, token: token.clone(), read_only: info.read_only });

    let task = tokio::spawn(async move {
        let mut accepts = tokio::task::JoinSet::new();
        for l in listeners {
            let state = state.clone();
            accepts.spawn(async move {
                // Connections live in their own JoinSet so aborting the server aborts them.
                let mut conns = tokio::task::JoinSet::new();
                loop {
                    match l.accept().await {
                        Ok((stream, _peer)) => {
                            let _ = stream.set_nodelay(true);
                            let state = state.clone();
                            conns.spawn(serve_connection(state, stream));
                        }
                        Err(e) => {
                            tracing::warn!("webdav accept failed: {e}");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                    while conns.try_join_next().is_some() {}
                }
            });
        }
        while accepts.join_next().await.is_some() {}
    });
    tracing::info!("WebDAV server for share {share} on {addr} (read-only: {})", info.read_only);
    Ok(WebDavHandle { addr, token, read_only: info.read_only, task: Some(task) })
}

fn random_token() -> String {
    let b: [u8; 16] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct ServerState {
    dav: DavHandler,
    token: String,
    read_only: bool,
}

async fn serve_connection(state: Arc<ServerState>, stream: tokio::net::TcpStream) {
    let io = hyper_util::rt::TokioIo::new(stream);
    let svc = hyper::service::service_fn(move |req| {
        let state = state.clone();
        async move { Ok::<_, Infallible>(route(&state, req).await) }
    });
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .keep_alive(true)
        .serve_connection(io, svc)
        .await
    {
        tracing::debug!("webdav connection ended: {e}");
    }
}

/// `Host` must be a loopback name (or absent, e.g. HTTP/1.0 tools).
pub(crate) fn host_is_loopback(host: Option<&HeaderValue>) -> bool {
    let Some(h) = host else { return true };
    let Ok(h) = h.to_str() else { return false };
    let h = h.trim();
    let name = if let Some(rest) = h.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inner, _)) => inner,
            None => return false,
        }
    } else {
        h.rsplit_once(':').map_or(h, |(n, port)| if port.chars().all(|c| c.is_ascii_digit()) { n } else { h })
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

const MUTATING: &[&str] = &["PUT", "DELETE", "MKCOL", "MOVE", "COPY", "PROPPATCH"];

fn simple(status: StatusCode, body: &'static str) -> Response<Body> {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = status;
    r
}

/// Root (`/`) answer for the Windows redirector's capability probes.
fn root_response(method: &Method) -> Response<Body> {
    match method.as_str() {
        "OPTIONS" => {
            let mut r = Response::new(Body::empty());
            let h = r.headers_mut();
            h.insert("DAV", HeaderValue::from_static("1, 2"));
            h.insert("MS-Author-Via", HeaderValue::from_static("DAV"));
            h.insert("Allow", HeaderValue::from_static("OPTIONS, PROPFIND"));
            h.insert("Content-Length", HeaderValue::from_static("0"));
            r
        }
        "PROPFIND" => {
            let mut r = Response::new(Body::from(ROOT_PROPFIND));
            *r.status_mut() = StatusCode::MULTI_STATUS;
            r.headers_mut()
                .insert("Content-Type", HeaderValue::from_static("application/xml; charset=utf-8"));
            r
        }
        _ => simple(StatusCode::NOT_FOUND, "not found\n"),
    }
}

/// Depth-0 view of the server root: an empty, read-only collection.
pub(crate) const ROOT_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/</D:href><D:propstat><D:prop>"#,
    r#"<D:displayname></D:displayname><D:resourcetype><D:collection/></D:resourcetype>"#,
    r#"<D:getlastmodified>Thu, 01 Jan 1970 00:00:00 GMT</D:getlastmodified>"#,
    r#"<D:creationdate>1970-01-01T00:00:00Z</D:creationdate>"#,
    r#"</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#,
);

async fn route(state: &ServerState, req: Request<Incoming>) -> Response<Body> {
    if !host_is_loopback(req.headers().get(HOST)) {
        return simple(StatusCode::MISDIRECTED_REQUEST, "this WebDAV server only answers loopback host names\n");
    }
    let path = req.uri().path();
    let in_share = path
        .strip_prefix('/')
        .and_then(|p| p.strip_prefix(state.token.as_str()))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
    if !in_share {
        return if path == "/" { root_response(req.method()) } else { simple(StatusCode::NOT_FOUND, "not found\n") };
    }
    if state.read_only && MUTATING.contains(&req.method().as_str()) {
        return simple(StatusCode::FORBIDDEN, "share is read-only\n");
    }
    state.dav.handle(req).await
}

// ---------------------------------------------------------------------------------------------
// DavFileSystem over VfsClient
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct VfsDavFs {
    client: Arc<VfsClient>,
    share: u32,
    read_only: bool,
}

fn fs_err(e: VfsError) -> FsError {
    match e.kind() {
        Some(VfsErrorKind::NotFound | VfsErrorKind::NoSuchShare) => FsError::NotFound,
        Some(VfsErrorKind::AlreadyExists | VfsErrorKind::NotEmpty) => FsError::Exists,
        Some(
            VfsErrorKind::PermissionDenied
            | VfsErrorKind::ReadOnly
            | VfsErrorKind::InvalidPath
            | VfsErrorKind::NotADirectory
            | VfsErrorKind::IsADirectory,
        ) => FsError::Forbidden,
        Some(VfsErrorKind::Io) | None => {
            tracing::debug!("webdav: vfs error: {e}");
            FsError::GeneralFailure
        }
    }
}

/// DAV path (decoded, `/`-rooted, maybe with a trailing slash) → share-relative VFS path.
pub(crate) fn vfs_path(p: &DavPath) -> FsResult<String> {
    let s = std::str::from_utf8(p.as_bytes()).map_err(|_| FsError::Forbidden)?;
    let rel = s.trim_matches('/').to_string();
    crate::path::split_rel_path(&rel).map_err(|_| FsError::Forbidden)?;
    Ok(rel)
}

fn ns_to_time(ns: i128) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns.min(u64::MAX as i128) as u64)
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_nanos((-ns).min(u64::MAX as i128) as u64)).unwrap_or(UNIX_EPOCH)
    }
}

#[derive(Debug, Clone)]
struct Meta(FileAttr);

impl DavMetaData for Meta {
    fn len(&self) -> u64 {
        if self.0.kind == FileKind::Directory { 0 } else { self.0.size }
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(ns_to_time(self.0.mtime_ns))
    }
    fn is_dir(&self) -> bool {
        self.0.kind == FileKind::Directory
    }
    fn is_symlink(&self) -> bool {
        self.0.kind == FileKind::Symlink
    }
    fn created(&self) -> FsResult<SystemTime> {
        Ok(ns_to_time(self.0.ctime_ns))
    }
    fn status_changed(&self) -> FsResult<SystemTime> {
        Ok(ns_to_time(self.0.ctime_ns))
    }
    fn executable(&self) -> FsResult<bool> {
        Ok(self.0.kind == FileKind::File && self.0.mode & 0o111 != 0)
    }
    fn etag(&self) -> Option<String> {
        // file id + size + mtime: changes whenever the content (or identity) changes.
        Some(format!("{:x}-{:x}-{:x}", self.0.file_id, self.len(), self.0.mtime_ns))
    }
}

struct Entry {
    name: String,
    attr: FileAttr,
}

impl DavDirEntry for Entry {
    fn name(&self) -> Vec<u8> {
        self.name.as_bytes().to_vec()
    }
    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let m: Box<dyn DavMetaData> = Box::new(Meta(self.attr.clone()));
        futures::future::ready(Ok(m)).boxed()
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

impl VfsDavFs {
    fn writable(&self) -> FsResult<()> {
        if self.read_only { Err(FsError::Forbidden) } else { Ok(()) }
    }

    async fn stat(&self, path: &str) -> FsResult<FileAttr> {
        self.client.stat(self.share, path).await.map_err(fs_err)
    }

    async fn copy_file(&self, from: &str, to: &str) -> FsResult<()> {
        let attr = self.stat(from).await?;
        if attr.kind != FileKind::File {
            return Err(FsError::Forbidden);
        }
        self.client.create(self.share, to, false).await.map_err(fs_err)?;
        self.client.truncate(self.share, to, 0).await.map_err(fs_err)?;
        let mut off = 0u64;
        while off < attr.size {
            let len = (attr.size - off).min(crate::MAX_READ_LEN as u64) as u32;
            let (data, eof) = self.client.read(self.share, from, off, len).await.map_err(fs_err)?;
            if data.is_empty() {
                break;
            }
            let n = data.len() as u64;
            self.client.write(self.share, to, off, data).await.map_err(fs_err)?;
            off += n;
            if eof {
                break;
            }
        }
        Ok(())
    }
}

impl DavFileSystem for VfsDavFs {
    fn open<'a>(&'a self, path: &'a DavPath, options: OpenOptions) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let p = vfs_path(path)?;
            if p.is_empty() {
                return Err(FsError::Forbidden);
            }
            let writing = options.write || options.append || options.truncate || options.create || options.create_new;
            let attr = if writing {
                self.writable()?;
                if options.create_new {
                    self.client.create(self.share, &p, true).await.map_err(fs_err)?
                } else {
                    match self.client.stat(self.share, &p).await {
                        Ok(a) if a.kind == FileKind::Directory => return Err(FsError::Forbidden),
                        Ok(a) if options.truncate && a.size > 0 => {
                            self.client.truncate(self.share, &p, 0).await.map_err(fs_err)?
                        }
                        Ok(a) => a,
                        Err(e) if options.create && e.kind() == Some(VfsErrorKind::NotFound) => {
                            self.client.create(self.share, &p, false).await.map_err(fs_err)?
                        }
                        Err(e) => return Err(fs_err(e)),
                    }
                }
            } else {
                let a = self.stat(&p).await?;
                if a.kind == FileKind::Directory {
                    return Err(FsError::Forbidden);
                }
                a
            };
            let pos = if options.append { attr.size } else { 0 };
            let f: Box<dyn DavFile> = Box::new(VfsDavFile {
                fs: self.clone(),
                path: p,
                attr,
                pos,
                pending: Vec::new(),
                pending_off: 0,
            });
            Ok(f)
        }
        .boxed()
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let p = vfs_path(path)?;
            let entries = self.client.read_dir(self.share, &p).await.map_err(fs_err)?;
            let mut out: Vec<FsResult<Box<dyn DavDirEntry>>> = Vec::with_capacity(entries.len());
            for e in entries {
                let attr = if e.attr.kind == FileKind::Symlink {
                    // Present symlinks as their (in-share) target; hide dangling / escaping ones.
                    match self.client.stat(self.share, &join(&p, &e.name)).await {
                        Ok(a) if a.kind != FileKind::Symlink => a,
                        _ => continue,
                    }
                } else {
                    e.attr
                };
                out.push(Ok(Box::new(Entry { name: e.name, attr })));
            }
            let s: FsStream<Box<dyn DavDirEntry>> = Box::pin(futures::stream::iter(out));
            Ok(s)
        }
        .boxed()
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let p = vfs_path(path)?;
            let m: Box<dyn DavMetaData> = Box::new(Meta(self.stat(&p).await?));
            Ok(m)
        }
        .boxed()
    }

    fn symlink_metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        self.metadata(path)
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            self.writable()?;
            let p = vfs_path(path)?;
            self.client.mkdir(self.share, &p).await.map_err(fs_err)?;
            Ok(())
        }
        .boxed()
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        self.remove_file(path)
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            self.writable()?;
            let p = vfs_path(path)?;
            if p.is_empty() {
                return Err(FsError::Forbidden);
            }
            self.client.remove(self.share, &p).await.map_err(fs_err)
        }
        .boxed()
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            self.writable()?;
            let (f, t) = (vfs_path(from)?, vfs_path(to)?);
            if f.is_empty() || t.is_empty() {
                return Err(FsError::Forbidden);
            }
            self.client.rename(self.share, &f, &t).await.map_err(fs_err)
        }
        .boxed()
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            self.writable()?;
            let (f, t) = (vfs_path(from)?, vfs_path(to)?);
            if f.is_empty() || t.is_empty() {
                return Err(FsError::Forbidden);
            }
            self.copy_file(&f, &t).await
        }
        .boxed()
    }

    fn set_modified<'a>(&'a self, _path: &'a DavPath, _tm: SystemTime) -> FsFuture<'a, ()> {
        // Times are not part of the VFS protocol; accepted as a no-op (like the NFS adapter).
        futures::future::ready(Ok(())).boxed()
    }
}

/// An open file: reads at `pos` go straight to the VFS (or its chunk cache); writes are
/// buffered into one contiguous run and flushed as `Write` requests of ≤ [`WRITE_BUFFER`].
struct VfsDavFile {
    fs: VfsDavFs,
    path: String,
    attr: FileAttr,
    pos: u64,
    pending: Vec<u8>,
    pending_off: u64,
}

impl std::fmt::Debug for VfsDavFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfsDavFile")
            .field("share", &self.fs.share)
            .field("path", &self.path)
            .field("pos", &self.pos)
            .field("pending", &self.pending.len())
            .finish()
    }
}

impl VfsDavFile {
    async fn flush_pending(&mut self) -> FsResult<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.pending);
        let attr = self.fs.client.write(self.fs.share, &self.path, self.pending_off, data).await.map_err(fs_err)?;
        self.attr = attr;
        Ok(())
    }

    async fn write_data(&mut self, data: &[u8]) -> FsResult<()> {
        self.fs.writable()?;
        if !self.pending.is_empty() && self.pending_off + self.pending.len() as u64 != self.pos {
            self.flush_pending().await?;
        }
        if self.pending.is_empty() {
            self.pending_off = self.pos;
        }
        self.pending.extend_from_slice(data);
        self.pos += data.len() as u64;
        if self.pending.len() >= WRITE_BUFFER {
            self.flush_pending().await?;
        }
        Ok(())
    }
}

impl DavFile for VfsDavFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            self.flush_pending().await?;
            self.attr = self.fs.stat(&self.path).await?;
            let m: Box<dyn DavMetaData> = Box::new(Meta(self.attr.clone()));
            Ok(m)
        }
        .boxed()
    }

    fn write_buf(&mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        async move {
            while buf.has_remaining() {
                let chunk = buf.chunk().to_vec();
                buf.advance(chunk.len());
                self.write_data(&chunk).await?;
            }
            Ok(())
        }
        .boxed()
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        async move { self.write_data(&buf).await }.boxed()
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        async move {
            self.flush_pending().await?;
            let len = count.min(crate::MAX_READ_LEN as usize) as u32;
            if len == 0 {
                return Ok(Bytes::new());
            }
            let c = &self.fs.client;
            let data = if c.cache().is_some() {
                c.read_cached(self.fs.share, &self.path, self.pos, len).await.map_err(fs_err)?
            } else {
                c.read(self.fs.share, &self.path, self.pos, len).await.map_err(fs_err)?.0
            };
            self.pos += data.len() as u64;
            Ok(Bytes::from(data))
        }
        .boxed()
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let new = match pos {
                SeekFrom::Start(n) => Some(n),
                SeekFrom::Current(d) => self.pos.checked_add_signed(d),
                SeekFrom::End(d) => {
                    self.flush_pending().await?;
                    self.attr = self.fs.stat(&self.path).await?;
                    self.attr.size.checked_add_signed(d)
                }
            };
            self.pos = new.ok_or(FsError::GeneralFailure)?;
            Ok(self.pos)
        }
        .boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        async move { self.flush_pending().await }.boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_host_check() {
        let ok = |s: &'static str| host_is_loopback(Some(&HeaderValue::from_static(s)));
        assert!(host_is_loopback(None));
        assert!(ok("localhost"));
        assert!(ok("localhost:8080"));
        assert!(ok("LOCALHOST."));
        assert!(ok("127.0.0.1:1234"));
        assert!(ok("127.8.9.1"));
        assert!(ok("[::1]:443"));
        assert!(ok("foo.localhost:1"));
        assert!(!ok("evil.example:80"));
        assert!(!ok("localhost.evil.example"));
        assert!(!ok("10.0.0.1:80"));
        assert!(!ok("[::2]:80"));
        assert!(!ok("[::1"));
    }

    #[test]
    fn dav_paths_map_to_share_relative_paths() {
        let p = |s: &str| vfs_path(&DavPath::new(s).unwrap());
        assert_eq!(p("/").unwrap(), "");
        assert_eq!(p("/a/b/").unwrap(), "a/b");
        assert_eq!(p("/a%20b/c.txt").unwrap(), "a b/c.txt");
        // dav-server's normalisation already refuses escaping the root.
        assert!(DavPath::new("/a/../../etc").is_err());
    }

    #[test]
    fn times_and_meta() {
        assert_eq!(ns_to_time(1_500_000_000), UNIX_EPOCH + Duration::from_millis(1500));
        assert_eq!(ns_to_time(-1_000_000_000), UNIX_EPOCH - Duration::from_secs(1));
        let a = FileAttr {
            file_id: 7,
            kind: FileKind::Directory,
            size: 4096,
            mode: 0o755,
            mtime_ns: 0,
            ctime_ns: 0,
            read_only: false,
        };
        let m = Meta(a);
        assert!(m.is_dir());
        assert_eq!(m.len(), 0);
        assert!(!m.executable().unwrap());
    }
}
