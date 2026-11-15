//! Pipelined VFS RPC client.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aether_protocol::framing::{read_frame, write_frame};
use aether_protocol::{
    DirEntry, FileAttr, FileKind, ShareInfo, VFS_CHUNK_SIZE, VfsEnvelope, VfsErrorKind, VfsRequest,
    VfsResponse,
};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::cache::ChunkCache;
use crate::error::{Result, VfsError};

type Pending = Mutex<Option<HashMap<u64, oneshot::Sender<VfsResponse>>>>;

/// Upper bound on remembered `(share, path, mtime) → chunk hashes` lists.
const HASH_MEMO_CAP: usize = 4096;

struct HashMemo {
    mtime_ns: i128,
    size: u64,
    hashes: Arc<Vec<[u8; 32]>>,
}

/// A request that has been sent; await [`PendingResponse::wait`] for its response.
/// Sending several requests before awaiting any of them pipelines them on the stream.
pub struct PendingResponse {
    rx: oneshot::Receiver<VfsResponse>,
}

impl PendingResponse {
    pub async fn wait(self) -> Result<VfsResponse> {
        self.rx.await.map_err(|_| VfsError::Closed)
    }
}

/// Pipelined VFS RPC client over any `(AsyncRead, AsyncWrite)` pair.
///
/// Must be created inside a Tokio runtime (it spawns a reader and a writer task).
/// Dropping the client closes the sending side and stops both tasks.
pub struct VfsClient {
    tx: mpsc::Sender<VfsEnvelope<VfsRequest>>,
    pending: Arc<Pending>,
    next_id: AtomicU64,
    cache: Option<ChunkCache>,
    memo: Mutex<HashMap<(u32, String), HashMemo>>,
    reader_task: JoinHandle<()>,
}

impl std::fmt::Debug for VfsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfsClient")
            .field("closed", &self.is_closed())
            .field("cache", &self.cache)
            .finish()
    }
}

fn close(pending: &Pending) {
    // Dropping the senders wakes every waiter with `Closed`.
    pending.lock().take();
}

impl VfsClient {
    /// Client without a chunk cache.
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::with_cache(reader, writer, None)
    }

    /// Client with an optional on-disk chunk cache used by [`VfsClient::read_cached`].
    pub fn with_cache<R, W>(mut reader: R, mut writer: W, cache: Option<ChunkCache>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Arc<Pending> = Arc::new(Mutex::new(Some(HashMap::new())));
        let (tx, mut rx) = mpsc::channel::<VfsEnvelope<VfsRequest>>(256);

        let p = pending.clone();
        // Detached: exits once `tx` is dropped (after flushing queued requests) or on error.
        tokio::spawn(async move {
            while let Some(env) = rx.recv().await {
                let res = async {
                    write_frame(&mut writer, &env).await?;
                    if rx.is_empty() {
                        writer.flush().await?;
                    }
                    Ok::<(), VfsError>(())
                }
                .await;
                if let Err(e) = res {
                    tracing::debug!("vfs client writer stopped: {e}");
                    close(&p);
                    return;
                }
            }
            let _ = writer.shutdown().await;
        });

        let p = pending.clone();
        let reader_task = tokio::spawn(async move {
            loop {
                match read_frame::<_, VfsEnvelope<VfsResponse>>(&mut reader).await {
                    Ok(Some(env)) => {
                        let waiter = p.lock().as_mut().and_then(|m| m.remove(&env.req_id));
                        match waiter {
                            Some(w) => {
                                let _ = w.send(env.body);
                            }
                            None => {
                                tracing::debug!("vfs response for unknown req_id {}", env.req_id)
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!("vfs client reader stopped: {e}");
                        break;
                    }
                }
            }
            close(&p);
        });

        Self {
            tx,
            pending,
            next_id: AtomicU64::new(1),
            cache,
            memo: Mutex::new(HashMap::new()),
            reader_task,
        }
    }

    pub fn cache(&self) -> Option<&ChunkCache> {
        self.cache.as_ref()
    }

    /// True once the underlying stream has failed or been closed by the peer.
    pub fn is_closed(&self) -> bool {
        self.pending.lock().is_none()
    }

    /// Send a request without waiting for its response.
    pub async fn send(&self, body: VfsRequest) -> Result<PendingResponse> {
        let req_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (otx, rx) = oneshot::channel();
        match self.pending.lock().as_mut() {
            Some(m) => {
                m.insert(req_id, otx);
            }
            None => return Err(VfsError::Closed),
        }
        if self.tx.send(VfsEnvelope { req_id, body }).await.is_err() {
            if let Some(m) = self.pending.lock().as_mut() {
                m.remove(&req_id);
            }
            return Err(VfsError::Closed);
        }
        Ok(PendingResponse { rx })
    }

    /// Send a request and wait for its response (raw; remote errors are *not* converted).
    pub async fn request(&self, body: VfsRequest) -> Result<VfsResponse> {
        self.send(body).await?.wait().await
    }

    async fn call(&self, body: VfsRequest) -> Result<VfsResponse> {
        match self.request(body).await? {
            VfsResponse::Error { kind, message } => Err(VfsError::Remote { kind, message }),
            r => Ok(r),
        }
    }

    pub async fn list_shares(&self) -> Result<Vec<ShareInfo>> {
        match self.call(VfsRequest::ListShares).await? {
            VfsResponse::Shares(s) => Ok(s),
            r => Err(unexpected(r)),
        }
    }

    pub async fn stat(&self, share: u32, path: &str) -> Result<FileAttr> {
        expect_attr(
            self.call(VfsRequest::Stat {
                share,
                path: path.into(),
            })
            .await?,
        )
    }

    pub async fn read_dir(&self, share: u32, path: &str) -> Result<Vec<DirEntry>> {
        match self
            .call(VfsRequest::ReadDir {
                share,
                path: path.into(),
            })
            .await?
        {
            VfsResponse::Entries(e) => Ok(e),
            r => Err(unexpected(r)),
        }
    }

    /// One `Read` request (the host caps `len` at [`crate::MAX_READ_LEN`]). Returns `(data, eof)`.
    pub async fn read(
        &self,
        share: u32,
        path: &str,
        offset: u64,
        len: u32,
    ) -> Result<(Vec<u8>, bool)> {
        expect_data(
            self.call(VfsRequest::Read {
                share,
                path: path.into(),
                offset,
                len,
            })
            .await?,
        )
    }

    /// Read a whole file with pipelined 4 MiB requests (no cache).
    pub async fn read_all(&self, share: u32, path: &str) -> Result<Vec<u8>> {
        let attr = self.stat(share, path).await?;
        let step = crate::MAX_READ_LEN as u64;
        let mut pend = Vec::new();
        let mut off = 0;
        while off < attr.size {
            let len = step.min(attr.size - off) as u32;
            pend.push(
                self.send(VfsRequest::Read {
                    share,
                    path: path.into(),
                    offset: off,
                    len,
                })
                .await?,
            );
            off += step;
        }
        let mut out = Vec::with_capacity(attr.size as usize);
        for p in pend {
            let (d, _) = expect_data(remote(p.wait().await?)?)?;
            out.extend_from_slice(&d);
        }
        Ok(out)
    }

    /// BLAKE3 hashes of each [`VFS_CHUNK_SIZE`] chunk of a file.
    pub async fn chunk_hashes(&self, share: u32, path: &str) -> Result<Vec<[u8; 32]>> {
        match self
            .call(VfsRequest::ChunkHashes {
                share,
                path: path.into(),
            })
            .await?
        {
            VfsResponse::Hashes { chunk_size, hashes } if chunk_size == VFS_CHUNK_SIZE => {
                Ok(hashes)
            }
            VfsResponse::Hashes { chunk_size, .. } => Err(VfsError::Protocol(format!(
                "unexpected chunk size {chunk_size}"
            ))),
            r => Err(unexpected(r)),
        }
    }

    pub async fn write(
        &self,
        share: u32,
        path: &str,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<FileAttr> {
        self.invalidate(share, path);
        match self
            .call(VfsRequest::Write {
                share,
                path: path.into(),
                offset,
                data,
            })
            .await?
        {
            VfsResponse::Written { attr, .. } => Ok(attr),
            r => Err(unexpected(r)),
        }
    }

    pub async fn create(&self, share: u32, path: &str, exclusive: bool) -> Result<FileAttr> {
        self.invalidate(share, path);
        expect_attr(
            self.call(VfsRequest::Create {
                share,
                path: path.into(),
                exclusive,
            })
            .await?,
        )
    }

    pub async fn mkdir(&self, share: u32, path: &str) -> Result<FileAttr> {
        expect_attr(
            self.call(VfsRequest::Mkdir {
                share,
                path: path.into(),
            })
            .await?,
        )
    }

    pub async fn remove(&self, share: u32, path: &str) -> Result<()> {
        self.invalidate(share, path);
        expect_ok(
            self.call(VfsRequest::Remove {
                share,
                path: path.into(),
            })
            .await?,
        )
    }

    pub async fn rename(&self, share: u32, from: &str, to: &str) -> Result<()> {
        self.invalidate(share, from);
        self.invalidate(share, to);
        expect_ok(
            self.call(VfsRequest::Rename {
                share,
                from: from.into(),
                to: to.into(),
            })
            .await?,
        )
    }

    pub async fn truncate(&self, share: u32, path: &str, size: u64) -> Result<FileAttr> {
        self.invalidate(share, path);
        expect_attr(
            self.call(VfsRequest::Truncate {
                share,
                path: path.into(),
                size,
            })
            .await?,
        )
    }

    pub async fn symlink(&self, share: u32, path: &str, target: &str) -> Result<FileAttr> {
        expect_attr(
            self.call(VfsRequest::Symlink {
                share,
                path: path.into(),
                target: target.into(),
            })
            .await?,
        )
    }

    pub async fn read_link(&self, share: u32, path: &str) -> Result<String> {
        match self
            .call(VfsRequest::ReadLink {
                share,
                path: path.into(),
            })
            .await?
        {
            VfsResponse::Link { target } => Ok(target),
            r => Err(unexpected(r)),
        }
    }

    fn invalidate(&self, share: u32, path: &str) {
        self.memo.lock().remove(&(share, path.to_string()));
    }

    async fn hashes_for(
        &self,
        share: u32,
        path: &str,
        attr: &FileAttr,
        force: bool,
    ) -> Result<Arc<Vec<[u8; 32]>>> {
        let key = (share, path.to_string());
        if !force
            && let Some(m) = self.memo.lock().get(&key)
            && m.mtime_ns == attr.mtime_ns
            && m.size == attr.size
        {
            return Ok(m.hashes.clone());
        }
        let hashes = Arc::new(self.chunk_hashes(share, path).await?);
        let mut memo = self.memo.lock();
        if memo.len() >= HASH_MEMO_CAP {
            memo.clear();
        }
        memo.insert(
            key,
            HashMemo {
                mtime_ns: attr.mtime_ns,
                size: attr.size,
                hashes: hashes.clone(),
            },
        );
        Ok(hashes)
    }

    /// Read `len` bytes at `offset` through the chunk cache: chunks are identified by
    /// `(share, path, chunk index, mtime)` → BLAKE3 hash (from `ChunkHashes`), served from
    /// the on-disk cache when present and fetched (pipelined) otherwise. Every fetched chunk
    /// is verified against its hash before being cached or returned; if the file changed
    /// underneath, the hash list is refreshed once and the read retried.
    pub async fn read_cached(
        &self,
        share: u32,
        path: &str,
        offset: u64,
        len: u32,
    ) -> Result<Vec<u8>> {
        let cache = self.cache.clone().ok_or(VfsError::Unsupported(
            "VfsClient has no chunk cache configured",
        ))?;
        let cs = VFS_CHUNK_SIZE as u64;
        for attempt in 0..2 {
            let attr = self.stat(share, path).await?;
            match attr.kind {
                FileKind::File => {}
                FileKind::Directory => {
                    return Err(VfsError::Remote {
                        kind: VfsErrorKind::IsADirectory,
                        message: path.into(),
                    });
                }
                // Stat does not follow symlinks; read the target directly (uncached).
                FileKind::Symlink => return Ok(self.read(share, path, offset, len).await?.0),
            }
            if offset >= attr.size || len == 0 {
                return Ok(Vec::new());
            }
            let end = attr.size.min(offset + len as u64);
            let hashes = self.hashes_for(share, path, &attr, attempt > 0).await?;
            if hashes.len() as u64 != attr.size.div_ceil(cs) {
                self.invalidate(share, path);
                continue;
            }
            let first = offset / cs;
            let last = (end - 1) / cs;
            let wanted: Vec<(u64, [u8; 32])> =
                (first..=last).map(|i| (i, hashes[i as usize])).collect();

            let c = cache.clone();
            let lookup = wanted.clone();
            let mut chunks: Vec<Option<Vec<u8>>> =
                tokio::task::spawn_blocking(move || lookup.iter().map(|(_, h)| c.get(h)).collect())
                    .await
                    .map_err(|e| VfsError::Io(std::io::Error::other(e)))?;

            // Pipeline every missing chunk.
            let mut pend = Vec::new();
            for (slot, (idx, _)) in wanted.iter().enumerate() {
                if chunks[slot].is_none() {
                    let off = idx * cs;
                    let clen = cs.min(attr.size - off) as u32;
                    pend.push((
                        slot,
                        self.send(VfsRequest::Read {
                            share,
                            path: path.into(),
                            offset: off,
                            len: clen,
                        })
                        .await?,
                    ));
                }
            }
            let mut stale = false;
            let mut fresh = Vec::new();
            for (slot, p) in pend {
                let (data, _) = expect_data(remote(p.wait().await?)?)?;
                let h = wanted[slot].1;
                if blake3::hash(&data).as_bytes() != &h {
                    stale = true;
                    continue;
                }
                fresh.push((h, data.clone()));
                chunks[slot] = Some(data);
            }
            if !fresh.is_empty() {
                let c = cache.clone();
                tokio::task::spawn_blocking(move || {
                    for (h, d) in fresh {
                        if let Err(e) = c.insert(h, &d) {
                            tracing::warn!("chunk cache insert failed: {e}");
                        }
                    }
                })
                .await
                .map_err(|e| VfsError::Io(std::io::Error::other(e)))?;
            }
            if stale {
                self.invalidate(share, path);
                if attempt == 0 {
                    continue;
                }
                return Err(VfsError::Integrity(format!(
                    "chunk of {path:?} does not match its BLAKE3 hash"
                )));
            }

            let mut out = Vec::with_capacity((end - offset) as usize);
            for (slot, (idx, _)) in wanted.iter().enumerate() {
                let data = chunks[slot].as_ref().expect("all chunks present");
                let base = idx * cs;
                let from = offset.max(base) - base;
                let to = end.min(base + data.len() as u64) - base;
                if from < to {
                    out.extend_from_slice(&data[from as usize..to as usize]);
                }
            }
            return Ok(out);
        }
        Err(VfsError::Integrity(format!(
            "{path:?} changed while reading (chunk list mismatch)"
        )))
    }
}

impl Drop for VfsClient {
    fn drop(&mut self) {
        self.reader_task.abort();
        close(&self.pending);
    }
}

fn remote(r: VfsResponse) -> Result<VfsResponse> {
    match r {
        VfsResponse::Error { kind, message } => Err(VfsError::Remote { kind, message }),
        r => Ok(r),
    }
}

fn unexpected(r: VfsResponse) -> VfsError {
    VfsError::Protocol(
        format!("unexpected response: {r:?}")
            .chars()
            .take(200)
            .collect(),
    )
}

fn expect_attr(r: VfsResponse) -> Result<FileAttr> {
    match r {
        VfsResponse::Attr(a) => Ok(a),
        VfsResponse::Written { attr, .. } => Ok(attr),
        r => Err(unexpected(r)),
    }
}

fn expect_data(r: VfsResponse) -> Result<(Vec<u8>, bool)> {
    match r {
        VfsResponse::Data { data, eof } => Ok((data, eof)),
        r => Err(unexpected(r)),
    }
}

fn expect_ok(r: VfsResponse) -> Result<()> {
    match r {
        VfsResponse::Ok => Ok(()),
        r => Err(unexpected(r)),
    }
}
