//! Pipelined host request loop.

use std::sync::Arc;

use aether_protocol::framing::{read_frame, write_frame};
use aether_protocol::{VfsEnvelope, VfsErrorKind, VfsRequest, VfsResponse};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc};

use crate::error::Result;
use crate::host::VfsHost;

/// Maximum number of requests handled concurrently per stream (back-pressure: the
/// reader stops pulling frames while this many are in flight).
pub const MAX_IN_FLIGHT: usize = 64;

/// Serve VFS requests read from `r`, writing responses to `w`, until the peer closes its
/// sending side. Requests are handled concurrently on the blocking thread pool; responses
/// are written by a single ordered writer as they complete, tagged with their `req_id`.
pub async fn serve<R, W>(host: Arc<VfsHost>, mut r: R, mut w: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (tx, mut rx) = mpsc::channel::<VfsEnvelope<VfsResponse>>(MAX_IN_FLIGHT);
    let sem = Arc::new(Semaphore::new(MAX_IN_FLIGHT));

    let reader = async move {
        loop {
            let permit = sem
                .clone()
                .acquire_owned()
                .await
                .expect("semaphore never closed");
            let Some(env) = read_frame::<_, VfsEnvelope<VfsRequest>>(&mut r).await? else {
                break;
            };
            let host = host.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let req_id = env.req_id;
                let body = match tokio::task::spawn_blocking(move || host.handle(env.body)).await {
                    Ok(resp) => resp,
                    Err(e) => VfsResponse::Error {
                        kind: VfsErrorKind::Io,
                        message: format!("handler failed: {e}"),
                    },
                };
                let _ = tx.send(VfsEnvelope { req_id, body }).await;
                drop(permit);
            });
        }
        // `tx` is dropped here; the writer finishes once every handler has replied.
        Ok::<(), crate::VfsError>(())
    };

    let writer = async move {
        while let Some(resp) = rx.recv().await {
            write_frame(&mut w, &resp).await?;
            if rx.is_empty() {
                w.flush().await?;
            }
        }
        w.flush().await?;
        let _ = w.shutdown().await;
        Ok::<(), crate::VfsError>(())
    };

    tokio::try_join!(reader, writer)?;
    Ok(())
}
