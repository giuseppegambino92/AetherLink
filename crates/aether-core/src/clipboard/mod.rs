//! Universal smart clipboard: policy, echo suppression, and OS integration.

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod system;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[path = "../mobile_clipboard.rs"]
pub mod system;
pub mod transcode;

use aether_protocol::{ClipboardFormat, ClipboardItem, ClipboardMode, ClipboardPayload, OsKind};

/// Upper bound for a single clipboard payload pushed automatically.
pub const MAX_AUTO_SYNC_BYTES: usize = 32 * 1024 * 1024;

/// Content digest independent of item order.
pub fn digest(items: &[ClipboardItem]) -> [u8; 32] {
    let mut parts: Vec<[u8; 32]> = items
        .iter()
        .map(|i| {
            let mut h = blake3::Hasher::new();
            h.update(format!("{:?}", i.format).as_bytes());
            h.update(&i.data);
            *h.finalize().as_bytes()
        })
        .collect();
    parts.sort_unstable();
    let mut h = blake3::Hasher::new();
    for p in parts {
        h.update(&p);
    }
    *h.finalize().as_bytes()
}

/// Decides what crosses the wire. One instance per session.
#[derive(Debug)]
pub struct ClipboardSync {
    mode: ClipboardMode,
    next_seq: u64,
    last_remote_seq: Option<u64>,
    /// Digest of what is currently on the local clipboard because *we* put it there.
    applied_remote: Option<[u8; 32]>,
    last_sent: Option<[u8; 32]>,
}

impl ClipboardSync {
    pub fn new(mode: ClipboardMode) -> Self {
        Self { mode, next_seq: 1, last_remote_seq: None, applied_remote: None, last_sent: None }
    }

    pub fn mode(&self) -> ClipboardMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: ClipboardMode) {
        self.mode = mode;
    }

    /// The local clipboard changed. Returns the payload to send, if any.
    pub fn on_local_change(&mut self, items: Vec<ClipboardItem>) -> Option<ClipboardPayload> {
        if self.mode == ClipboardMode::Disabled || items.is_empty() {
            return None;
        }
        let items = transcode::canonicalize(items);
        let d = digest(&items);
        // Echo of a remote update we just applied, or a duplicate notification.
        if Some(d) == self.applied_remote || Some(d) == self.last_sent {
            return None;
        }
        if items.iter().map(|i| i.data.len()).sum::<usize>() > MAX_AUTO_SYNC_BYTES {
            return None;
        }
        self.last_sent = Some(d);
        let seq = self.next_seq;
        self.next_seq += 1;
        Some(ClipboardPayload { seq, origin_os: OsKind::current(), items, files: vec![] })
    }

    /// A peer update arrived. Returns the items to write locally, if any.
    pub fn on_remote_update(&mut self, payload: &ClipboardPayload) -> Option<Vec<ClipboardItem>> {
        if self.mode != ClipboardMode::Bidirectional {
            return None;
        }
        if self.last_remote_seq.is_some_and(|s| payload.seq <= s) {
            return None; // stale / replayed
        }
        self.last_remote_seq = Some(payload.seq);
        let d = digest(&payload.items);
        if Some(d) == self.last_sent {
            return None; // our own content bounced back
        }
        self.applied_remote = Some(d);
        Some(payload.items.clone())
    }
}

/// Pick the best representation for the local OS from a wire payload.
pub fn best_text(items: &[ClipboardItem]) -> Option<String> {
    items
        .iter()
        .find(|i| i.format == ClipboardFormat::PlainText)
        .and_then(|i| String::from_utf8(i.data.clone()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Vec<ClipboardItem> {
        vec![ClipboardItem { format: ClipboardFormat::PlainText, data: s.as_bytes().to_vec() }]
    }

    #[test]
    fn modes() {
        let mut off = ClipboardSync::new(ClipboardMode::Disabled);
        assert!(off.on_local_change(text("a")).is_none());

        let mut push = ClipboardSync::new(ClipboardMode::PushOnly);
        let p = push.on_local_change(text("a")).unwrap();
        assert_eq!(p.seq, 1);
        assert!(push.on_remote_update(&p).is_none());
    }

    #[test]
    fn no_echo_loops_between_two_peers() {
        let mut a = ClipboardSync::new(ClipboardMode::Bidirectional);
        let mut b = ClipboardSync::new(ClipboardMode::Bidirectional);

        let pa = a.on_local_change(text("ciao")).unwrap();
        let applied = b.on_remote_update(&pa).unwrap();
        // B's clipboard watcher now fires with the content we just wrote: must not bounce.
        assert!(b.on_local_change(applied).is_none());
        // Duplicate notification on A as well.
        assert!(a.on_local_change(text("ciao")).is_none());

        // A real new copy on B goes through and is applied on A.
        let pb = b.on_local_change(text("mondo")).unwrap();
        assert_eq!(best_text(&a.on_remote_update(&pb).unwrap()).as_deref(), Some("mondo"));
        // Replayed packet is ignored.
        assert!(a.on_remote_update(&pb).is_none());
    }

    #[test]
    fn digest_is_order_independent() {
        let x = ClipboardItem { format: ClipboardFormat::Html, data: b"<b>x</b>".to_vec() };
        let y = ClipboardItem { format: ClipboardFormat::PlainText, data: b"x".to_vec() };
        assert_eq!(digest(&[x.clone(), y.clone()]), digest(&[y, x]));
    }
}
