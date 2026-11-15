//! Stream plumbing shared by host and client sessions.

use crate::clipboard::{ClipboardSync, system::ClipboardWatcher, system::SystemClipboard};
use aether_net::{FramedRecv, FramedSend};
use aether_protocol::{ClipboardMessage, ClipboardMode};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// `FramedRecv::recv` is not cancel-safe, so every stream that is also driven by
/// timers gets a dedicated reader task feeding a channel (which is cancel-safe).
pub fn spawn_reader<T>(mut recv: FramedRecv, capacity: usize) -> mpsc::Receiver<T>
where
    T: DeserializeOwned + Send + 'static,
{
    let (tx, rx) = mpsc::channel(capacity);
    tokio::spawn(async move {
        loop {
            match recv.recv::<T>().await {
                Ok(Some(m)) => {
                    if tx.send(m).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!("stream reader ended: {e}");
                    break;
                }
            }
        }
    });
    rx
}

/// Shared, serialized writer for a stream used by several tasks.
pub type SharedSend = Arc<tokio::sync::Mutex<FramedSend>>;

pub fn shared(send: FramedSend) -> SharedSend {
    Arc::new(tokio::sync::Mutex::new(send))
}

/// Bidirectional clipboard sync over one `Clipboard` stream; identical on both peers.
/// `mode_rx` lets the UI change the mode live. Runs until the stream closes.
pub async fn clipboard_link(
    mode: ClipboardMode,
    mut mode_rx: tokio::sync::watch::Receiver<ClipboardMode>,
    mut send: FramedSend,
    recv: FramedRecv,
    on_applied: impl Fn() + Send + 'static,
) {
    let sync = Arc::new(parking_lot::Mutex::new(ClipboardSync::new(mode)));
    let (local_tx, mut local_rx) = mpsc::channel(4);
    let _watcher = match ClipboardWatcher::spawn(Duration::from_millis(300), move |items| {
        let _ = local_tx.try_send(items);
    }) {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!("clipboard watcher unavailable: {e}");
            None
        }
    };
    let mut remote = spawn_reader::<ClipboardMessage>(recv, 4);
    loop {
        tokio::select! {
            Some(items) = local_rx.recv() => {
                let payload = sync.lock().on_local_change(items);
                if let Some(p) = payload
                    && send.send(&ClipboardMessage::Update(p)).await.is_err() { break; }
            }
            msg = remote.recv() => {
                let Some(ClipboardMessage::Update(p)) = msg else {
                    if msg.is_none() { break; } else { continue; }
                };
                let apply = sync.lock().on_remote_update(&p);
                if let Some(items) = apply {
                    let res = tokio::task::spawn_blocking(move || SystemClipboard::new().and_then(|mut c| c.write(&items))).await;
                    match res {
                        Ok(Ok(())) => on_applied(),
                        Ok(Err(e)) => tracing::warn!("clipboard write failed: {e}"),
                        Err(e) => tracing::warn!("clipboard task failed: {e}"),
                    }
                }
            }
            changed = mode_rx.changed() => {
                if changed.is_err() { break; }
                let m = *mode_rx.borrow();
                sync.lock().set_mode(m);
            }
        }
    }
}
