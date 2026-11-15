//! OS clipboard access (via `arboard`) and change detection.

use super::transcode;
use crate::{CoreError, Result};
use aether_protocol::{ClipboardFormat, ClipboardItem};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub struct SystemClipboard {
    cb: arboard::Clipboard,
}

fn err(e: arboard::Error) -> CoreError {
    CoreError::Clipboard(e.to_string())
}

impl SystemClipboard {
    pub fn new() -> Result<Self> {
        Ok(Self { cb: arboard::Clipboard::new().map_err(err)? })
    }

    /// Snapshot the clipboard in wire canon (HTML, text, PNG).
    pub fn read(&mut self) -> Result<Vec<ClipboardItem>> {
        let mut items = Vec::new();
        if let Ok(html) = self.cb.get().html()
            && !html.is_empty() {
                items.push(ClipboardItem { format: ClipboardFormat::Html, data: html.into_bytes() });
            }
        if let Ok(text) = self.cb.get_text()
            && !text.is_empty() {
                items.push(ClipboardItem { format: ClipboardFormat::PlainText, data: text.into_bytes() });
            }
        if let Ok(img) = self.cb.get_image() {
            let png = transcode::rgba_to_png(img.width as u32, img.height as u32, img.bytes.into_owned())?;
            items.push(ClipboardItem { format: ClipboardFormat::Png, data: png });
        }
        Ok(items)
    }

    /// Write a wire payload, choosing the richest representation available.
    pub fn write(&mut self, items: &[ClipboardItem]) -> Result<()> {
        let find = |f| items.iter().find(|i: &&ClipboardItem| i.format == f).map(|i| i.data.as_slice());
        let text = find(ClipboardFormat::PlainText).and_then(|d| std::str::from_utf8(d).ok());
        if let Some(png) = find(ClipboardFormat::Png) {
            let (w, h, rgba) = transcode::png_to_rgba(png)?;
            return self
                .cb
                .set_image(arboard::ImageData { width: w as usize, height: h as usize, bytes: rgba.into() })
                .map_err(err);
        }
        if let Some(html) = find(ClipboardFormat::Html).and_then(|d| std::str::from_utf8(d).ok()) {
            return self.cb.set_html(html, text).map_err(err);
        }
        if let Some(text) = text {
            return self.cb.set_text(text).map_err(err);
        }
        Ok(())
    }
}

/// Cheap change indicator. On macOS this is `NSPasteboard.changeCount`; elsewhere a
/// digest of the text/image size (arboard has no portable change counter).
pub fn change_token(cb: &mut SystemClipboard) -> u64 {
    #[cfg(target_os = "macos")]
    {
        let _ = cb;
        macos::change_count()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut h = blake3::Hasher::new();
        if let Ok(t) = cb.cb.get_text() {
            h.update(t.as_bytes());
        }
        if let Ok(i) = cb.cb.get_image() {
            h.update(&(i.width as u64).to_le_bytes());
            h.update(&(i.height as u64).to_le_bytes());
            h.update(&i.bytes[..i.bytes.len().min(4096)]);
        }
        u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2_app_kit::NSPasteboard;

    pub fn change_count() -> u64 {
        NSPasteboard::generalPasteboard().changeCount() as u64
    }
}

/// Background watcher that invokes `on_change` with the new clipboard contents.
pub struct ClipboardWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ClipboardWatcher {
    pub fn spawn(interval: Duration, mut on_change: impl FnMut(Vec<ClipboardItem>) + Send + 'static) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new().name("aether-clipboard".into()).spawn(move || {
            let Ok(mut cb) = SystemClipboard::new() else { return };
            let mut last = change_token(&mut cb);
            while !stop2.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                let now = change_token(&mut cb);
                if now != last {
                    last = now;
                    if let Ok(items) = cb.read()
                        && !items.is_empty() {
                            on_change(items);
                        }
                }
            }
        })?;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for ClipboardWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
