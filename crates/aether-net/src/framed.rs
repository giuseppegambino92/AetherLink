//! Typed framing over QUIC streams with raw `AsyncRead`/`AsyncWrite` passthrough.

use crate::{NetError, Result};
use aether_protocol::framing::{read_frame, write_frame};
use serde::{Serialize, de::DeserializeOwned};
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Sending half of a QUIC stream: `send()` writes one length-prefixed bincode frame;
/// the `AsyncWrite` impl writes raw bytes (port tunnels, VFS, …).
#[derive(Debug)]
pub struct FramedSend {
    inner: quinn::SendStream,
}

impl FramedSend {
    pub(crate) fn new(inner: quinn::SendStream) -> Self {
        Self { inner }
    }

    /// Write one frame.
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        write_frame(&mut self.inner, msg).await.map_err(map_framing)
    }

    /// Gracefully finish the stream (peer sees EOF after the buffered data).
    pub fn finish(&mut self) -> Result<()> {
        self.inner.finish().map_err(NetError::from)
    }

    /// Abruptly abort the stream with an application error code.
    pub fn reset(&mut self, code: u32) {
        let _ = self.inner.reset(code.into());
    }

    /// Prioritise this stream relative to others on the connection (higher = sooner).
    pub fn set_priority(&self, priority: i32) {
        let _ = self.inner.set_priority(priority);
    }

    /// Wait until the peer has acknowledged all data (or the stream was stopped).
    pub async fn stopped(&mut self) -> Result<()> {
        self.inner.stopped().await.map(|_| ()).map_err(|e| NetError::Io(io::Error::other(e)))
    }

    pub fn id(&self) -> u64 {
        self.inner.id().index()
    }
}

/// Receiving half of a QUIC stream: `recv()` reads one frame (`Ok(None)` on clean EOF);
/// the `AsyncRead` impl reads raw bytes. Frames are read exactly, so mixing typed and raw
/// reads is safe (no read-ahead buffering).
#[derive(Debug)]
pub struct FramedRecv {
    inner: quinn::RecvStream,
}

impl FramedRecv {
    pub(crate) fn new(inner: quinn::RecvStream) -> Self {
        Self { inner }
    }

    /// Read one frame; `Ok(None)` when the peer finished the stream at a frame boundary.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        read_frame(&mut self.inner).await.map_err(map_framing)
    }

    /// Read one frame, treating EOF as an error.
    pub async fn recv_required<T: DeserializeOwned>(&mut self) -> Result<T> {
        self.recv().await?.ok_or(NetError::StreamClosed)
    }

    /// Tell the peer to stop sending.
    pub fn stop(&mut self, code: u32) {
        let _ = self.inner.stop(code.into());
    }

    pub fn id(&self) -> u64 {
        self.inner.id().index()
    }
}

fn map_framing(e: aether_protocol::FramingError) -> NetError {
    match e {
        aether_protocol::FramingError::Truncated => NetError::StreamClosed,
        other => NetError::Framing(other),
    }
}

impl AsyncWrite for FramedSend {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.inner), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.inner), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.inner), cx)
    }
}

impl AsyncRead for FramedRecv {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        AsyncRead::poll_read(Pin::new(&mut self.inner), cx, buf)
    }
}
