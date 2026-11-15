//! Mobile clipboard backend (Android / iOS), mounted as `clipboard::system`.
//!
//! `arboard` has no mobile backend, so the app registers a [`PlatformClipboard`]
//! (Android `ClipboardManager` / iOS `UIPasteboard`, usually through the Tauri
//! clipboard plugin). Without a registration every call returns
//! `CoreError::Clipboard` — never fake data.
//!
//! Only plain text and HTML cross this boundary; images stay on the device.

use crate::{CoreError, Result};
use aether_protocol::{ClipboardFormat, ClipboardItem};
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Platform clipboard provided by the mobile app.
pub trait PlatformClipboard: Send + Sync + 'static {
    fn read_text(&self) -> Option<String>;
    fn write_text(&self, text: &str) -> std::result::Result<(), String>;
}

static PLATFORM: RwLock<Option<Arc<dyn PlatformClipboard>>> = RwLock::new(None);

/// Register (or clear) the platform clipboard.
pub fn set_platform_clipboard(cb: Option<Arc<dyn PlatformClipboard>>) {
    *PLATFORM.write() = cb;
}

fn platform() -> Result<Arc<dyn PlatformClipboard>> {
    PLATFORM.read().clone().ok_or_else(|| CoreError::Clipboard("no platform clipboard registered".into()))
}

pub struct SystemClipboard {
    cb: Arc<dyn PlatformClipboard>,
}

impl SystemClipboard {
    pub fn new() -> Result<Self> {
        Ok(Self { cb: platform()? })
    }

    pub fn read(&mut self) -> Result<Vec<ClipboardItem>> {
        Ok(self
            .cb
            .read_text()
            .filter(|t| !t.is_empty())
            .map(|t| vec![ClipboardItem { format: ClipboardFormat::PlainText, data: t.into_bytes() }])
            .unwrap_or_default())
    }

    pub fn write(&mut self, items: &[ClipboardItem]) -> Result<()> {
        let text = items
            .iter()
            .find(|i| i.format == ClipboardFormat::PlainText)
            .or_else(|| items.iter().find(|i| i.format == ClipboardFormat::Html))
            .and_then(|i| std::str::from_utf8(&i.data).ok());
        match text {
            Some(t) => self.cb.write_text(t).map_err(CoreError::Clipboard),
            None => Ok(()),
        }
    }
}

/// Digest of the current text (mobile OSes expose no portable change counter to apps).
pub fn change_token(cb: &mut SystemClipboard) -> u64 {
    let t = cb.cb.read_text().unwrap_or_default();
    let h = blake3::hash(t.as_bytes());
    u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap_or([0; 8]))
}

/// Background watcher that invokes `on_change` with the new clipboard contents.
pub struct ClipboardWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ClipboardWatcher {
    pub fn spawn(interval: Duration, mut on_change: impl FnMut(Vec<ClipboardItem>) + Send + 'static) -> Result<Self> {
        let mut cb = SystemClipboard::new()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new().name("aether-clipboard".into()).spawn(move || {
            let mut last = change_token(&mut cb);
            while !stop2.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                let now = change_token(&mut cb);
                if now != last {
                    last = now;
                    if let Ok(items) = cb.read()
                        && !items.is_empty()
                    {
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
