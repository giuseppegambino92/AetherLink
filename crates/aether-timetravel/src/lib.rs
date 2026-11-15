//! # aether-timetravel
//!
//! The "blackbox" behind AetherLink's time-travel DVR: an in-memory ring of the most
//! recent encoded video access units (default 180 s), timeline marks and telemetry.
//!
//! * Every stored payload is encrypted with ChaCha20-Poly1305 under a random 32-byte
//!   session key that is generated in memory, never persisted, and zeroized on drop.
//!   Nonces are `salt(4) ‖ counter(8)`, unique per entry; the entry's metadata
//!   (kind, keyframe flag, pts, counter) is bound as associated data, so flipping a
//!   ciphertext byte *or* a metadata bit is detected on decryption.
//! * Eviction is by time window **and** byte budget, always in whole GOPs from the
//!   front, so the oldest retained frame is always a keyframe and every retained
//!   timestamp is seekable. A single GOP larger than twice the budget is dropped and the
//!   recorder waits for the next keyframe ([`Blackbox::needs_keyframe`] tells the caller to
//!   request one), which keeps memory hard-bounded even for a stream without keyframes.
//! * [`Blackbox::seek`] returns the decrypted keyframe plus every following frame up to
//!   the requested timestamp — exactly what a decoder needs to reconstruct that instant.

use std::collections::VecDeque;

use aether_protocol::{TelemetrySample, TimeTravelIndex, TimelineEventKind, TimelineMark};
use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

pub mod spike;
pub use spike::{SpikeConfig, detect_spike};

/// Errors of this crate.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TimeTravelError {
    #[error("the blackbox holds no seekable frames")]
    Empty,
    #[error("timestamp {requested} µs is before the oldest seekable keyframe at {start} µs")]
    OutOfRange {
        requested: u64,
        start: u64,
        end: u64,
    },
    #[error("authentication failed: recorded data was modified or corrupted")]
    Tampered,
    #[error("bundle error: {0}")]
    Bundle(String),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

pub type Result<T> = std::result::Result<T, TimeTravelError>;

/// Accounting overhead charged per stored entry on top of its ciphertext.
pub const ENTRY_OVERHEAD: usize = 64;

/// Blackbox configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct BlackboxConfig {
    /// Retention window in µs (default 180 s).
    pub window_us: u64,
    /// Byte budget for stored frames + mark payloads (ciphertext + [`ENTRY_OVERHEAD`] each).
    pub max_bytes: usize,
    /// Maximum number of timeline marks kept.
    pub max_marks: usize,
    /// Maximum number of telemetry samples kept.
    pub max_telemetry: usize,
    /// Automatic `ResourceSpike` marks from telemetry (`None` disables).
    pub spike: Option<SpikeConfig>,
}

impl Default for BlackboxConfig {
    fn default() -> Self {
        Self {
            window_us: 180_000_000,
            max_bytes: 512 * 1024 * 1024,
            max_marks: 10_000,
            max_telemetry: 180 * 10,
            spike: Some(SpikeConfig::default()),
        }
    }
}

/// A decrypted frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedFrame {
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// Everything needed to display the instant `target_pts_us`: decode `frames` in order
/// (the first one is the keyframe at `keyframe_pts_us`) and present the last one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeekPlan {
    pub keyframe_pts_us: u64,
    pub target_pts_us: u64,
    pub frames: Vec<RecordedFrame>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum EntryKind {
    Frame = 1,
    MarkPayload = 2,
}

struct Entry {
    seq: u64,
    pts_us: u64,
    kind: EntryKind,
    keyframe: bool,
    counter: u64,
    ciphertext: Vec<u8>,
}

impl Entry {
    fn cost(&self) -> usize {
        self.ciphertext.len() + ENTRY_OVERHEAD
    }
}

struct StoredMark {
    mark: TimelineMark,
    payload: Option<Entry>,
}

struct Sealer {
    cipher: ChaCha20Poly1305,
    salt: [u8; 4],
    counter: u64,
}

impl Sealer {
    fn new() -> Self {
        let mut key = ChaCha20Poly1305::generate_key(&mut OsRng);
        let cipher = ChaCha20Poly1305::new(&key);
        key.as_mut_slice().zeroize();
        Self {
            cipher,
            salt: rand::random(),
            counter: 0,
        }
    }

    fn nonce(&self, counter: u64) -> Nonce {
        let mut n = [0u8; 12];
        n[..4].copy_from_slice(&self.salt);
        n[4..].copy_from_slice(&counter.to_le_bytes());
        *Nonce::from_slice(&n)
    }

    fn aad(kind: EntryKind, keyframe: bool, pts_us: u64, counter: u64) -> [u8; 18] {
        let mut a = [0u8; 18];
        a[0] = kind as u8;
        a[1] = keyframe as u8;
        a[2..10].copy_from_slice(&pts_us.to_le_bytes());
        a[10..].copy_from_slice(&counter.to_le_bytes());
        a
    }

    fn seal(
        &mut self,
        seq: u64,
        kind: EntryKind,
        keyframe: bool,
        pts_us: u64,
        plain: &[u8],
    ) -> Entry {
        let counter = self.counter;
        self.counter += 1;
        let aad = Self::aad(kind, keyframe, pts_us, counter);
        let ciphertext = self
            .cipher
            .encrypt(
                &self.nonce(counter),
                Payload {
                    msg: plain,
                    aad: &aad,
                },
            )
            .expect("ChaCha20-Poly1305 encryption of an in-memory buffer cannot fail");
        Entry {
            seq,
            pts_us,
            kind,
            keyframe,
            counter,
            ciphertext,
        }
    }

    fn open(&self, e: &Entry) -> Result<Vec<u8>> {
        let aad = Self::aad(e.kind, e.keyframe, e.pts_us, e.counter);
        self.cipher
            .decrypt(
                &self.nonce(e.counter),
                Payload {
                    msg: &e.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| TimeTravelError::Tampered)
    }
}

/// Encrypted in-memory DVR ring.
pub struct Blackbox {
    cfg: BlackboxConfig,
    sealer: Sealer,
    frames: VecDeque<Entry>,
    /// `(seq, pts)` of every retained keyframe, oldest first.
    keyframes: VecDeque<(u64, u64)>,
    next_seq: u64,
    frame_bytes: usize,
    marks: VecDeque<StoredMark>,
    mark_bytes: usize,
    telemetry: VecDeque<TelemetrySample>,
    waiting_for_keyframe: bool,
    last_spike_us: Option<u64>,
    dropped_frames: u64,
}

impl std::fmt::Debug for Blackbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Blackbox")
            .field("frames", &self.frames.len())
            .field("keyframes", &self.keyframes.len())
            .field("bytes", &self.bytes_used())
            .field("marks", &self.marks.len())
            .finish_non_exhaustive()
    }
}

impl Default for Blackbox {
    fn default() -> Self {
        Self::new(BlackboxConfig::default())
    }
}

impl Blackbox {
    pub fn new(cfg: BlackboxConfig) -> Self {
        Self {
            cfg,
            sealer: Sealer::new(),
            frames: VecDeque::new(),
            keyframes: VecDeque::new(),
            next_seq: 0,
            frame_bytes: 0,
            marks: VecDeque::new(),
            mark_bytes: 0,
            telemetry: VecDeque::new(),
            waiting_for_keyframe: true,
            last_spike_us: None,
            dropped_frames: 0,
        }
    }

    pub fn config(&self) -> &BlackboxConfig {
        &self.cfg
    }

    /// Record one encoded access unit. Frames must arrive in non-decreasing `pts_us`
    /// order; a timestamp going backwards is treated as a stream restart and clears the
    /// recorded frames. Non-keyframes are ignored until the first keyframe arrives.
    pub fn push_frame(&mut self, pts_us: u64, keyframe: bool, payload: &[u8]) {
        if let Some(last) = self.frames.back()
            && pts_us < last.pts_us
        {
            self.clear_frames();
        }
        if self.waiting_for_keyframe && !keyframe {
            self.dropped_frames += 1;
            return;
        }
        self.waiting_for_keyframe = false;
        let seq = self.next_seq;
        self.next_seq += 1;
        let e = self
            .sealer
            .seal(seq, EntryKind::Frame, keyframe, pts_us, payload);
        self.frame_bytes += e.cost();
        if keyframe {
            self.keyframes.push_back((seq, pts_us));
        }
        self.frames.push_back(e);
        self.evict();
    }

    /// Add a timeline mark, optionally with an (encrypted) payload such as clipboard text.
    pub fn push_mark(&mut self, mark: TimelineMark, payload: Option<&[u8]>) {
        let payload = payload.map(|p| {
            let seq = self.next_seq;
            self.next_seq += 1;
            let e = self
                .sealer
                .seal(seq, EntryKind::MarkPayload, false, mark.pts_us, p);
            self.mark_bytes += e.cost();
            e
        });
        // Keep marks sorted by time (they may arrive slightly out of order).
        let pos = self.marks.partition_point(|m| m.mark.pts_us <= mark.pts_us);
        self.marks.insert(pos, StoredMark { mark, payload });
        self.evict();
    }

    /// Record a telemetry sample; adds a `ResourceSpike` mark when [`detect_spike`] fires
    /// (rate-limited by [`SpikeConfig::cooldown_us`]).
    pub fn push_telemetry(&mut self, s: TelemetrySample) {
        if let Some(cfg) = self.cfg.spike.clone() {
            let hist: Vec<TelemetrySample> = self
                .telemetry
                .iter()
                .rev()
                .take(cfg.history)
                .rev()
                .copied()
                .collect();
            let cooled = self
                .last_spike_us
                .is_none_or(|t| s.pts_us.saturating_sub(t) >= cfg.cooldown_us);
            if cooled && let Some(label) = detect_spike(&hist, &s, &cfg) {
                self.last_spike_us = Some(s.pts_us);
                self.push_mark(
                    TimelineMark {
                        pts_us: s.pts_us,
                        kind: TimelineEventKind::ResourceSpike,
                        label: Some(label),
                    },
                    None,
                );
            }
        }
        self.telemetry.push_back(s);
        while self.telemetry.len() > self.cfg.max_telemetry {
            self.telemetry.pop_front();
        }
        self.evict();
    }

    /// Timeline summary for the UI.
    pub fn index(&self) -> TimeTravelIndex {
        TimeTravelIndex {
            start_us: self.start_us().unwrap_or(0),
            end_us: self.end_us().unwrap_or(0),
            keyframes: self.keyframes.iter().map(|(_, p)| *p).collect(),
            marks: self.marks.iter().map(|m| m.mark.clone()).collect(),
        }
    }

    /// Oldest seekable timestamp (always a keyframe).
    pub fn start_us(&self) -> Option<u64> {
        self.frames.front().map(|e| e.pts_us)
    }

    /// Newest recorded timestamp.
    pub fn end_us(&self) -> Option<u64> {
        self.frames.back().map(|e| e.pts_us)
    }

    pub fn duration_us(&self) -> u64 {
        match (self.start_us(), self.end_us()) {
            (Some(a), Some(b)) => b - a,
            _ => 0,
        }
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Accounted bytes (frames + mark payloads, ciphertext + per-entry overhead).
    pub fn bytes_used(&self) -> usize {
        self.frame_bytes + self.mark_bytes
    }

    /// Frames ignored because no keyframe was available to anchor them.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    /// True while the recorder has no keyframe to anchor incoming frames
    /// (the caller should send `ControlMessage::RequestKeyframe`).
    pub fn needs_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }

    fn position_of(&self, seq: u64) -> usize {
        let front = self.frames.front().map(|e| e.seq).unwrap_or(0);
        (seq - front) as usize
    }

    /// Decrypted keyframe at or before `pts_us` and every frame after it up to `pts_us`.
    /// Timestamps past the newest frame are clamped to it.
    pub fn seek(&self, pts_us: u64) -> Result<SeekPlan> {
        let (start, end) = match (self.start_us(), self.end_us()) {
            (Some(s), Some(e)) => (s, e),
            _ => return Err(TimeTravelError::Empty),
        };
        if pts_us < start {
            return Err(TimeTravelError::OutOfRange {
                requested: pts_us,
                start,
                end,
            });
        }
        let target = pts_us.min(end);
        let k = self.keyframes.partition_point(|(_, p)| *p <= target);
        let (kseq, kpts) = self.keyframes[k.checked_sub(1).expect("front frame is a keyframe")];
        let mut frames = Vec::new();
        for e in self.frames.range(self.position_of(kseq)..) {
            if e.pts_us > target {
                break;
            }
            frames.push(RecordedFrame {
                pts_us: e.pts_us,
                keyframe: e.keyframe,
                data: self.sealer.open(e)?,
            });
        }
        Ok(SeekPlan {
            keyframe_pts_us: kpts,
            target_pts_us: target,
            frames,
        })
    }

    /// Decrypted frames with `from_us <= pts <= to_us` (for scrubbing/thumbnails;
    /// decode from a [`Blackbox::seek`] anchor first).
    pub fn frames_in_range(&self, from_us: u64, to_us: u64) -> Result<Vec<RecordedFrame>> {
        let first = self.frames.partition_point(|e| e.pts_us < from_us);
        let mut out = Vec::new();
        for e in self.frames.range(first..) {
            if e.pts_us > to_us {
                break;
            }
            out.push(RecordedFrame {
                pts_us: e.pts_us,
                keyframe: e.keyframe,
                data: self.sealer.open(e)?,
            });
        }
        Ok(out)
    }

    /// Timeline marks, oldest first.
    pub fn marks(&self) -> Vec<TimelineMark> {
        self.marks.iter().map(|m| m.mark.clone()).collect()
    }

    /// Decrypted payload of the first mark equal to `mark`.
    pub fn mark_payload(&self, mark: &TimelineMark) -> Result<Option<Vec<u8>>> {
        match self.marks.iter().find(|m| &m.mark == mark) {
            Some(StoredMark {
                payload: Some(e), ..
            }) => self.sealer.open(e).map(Some),
            _ => Ok(None),
        }
    }

    /// Retained telemetry samples, oldest first.
    pub fn telemetry(&self) -> impl Iterator<Item = &TelemetrySample> {
        self.telemetry.iter()
    }

    /// Drop all recorded frames (marks and telemetry are kept).
    pub fn clear_frames(&mut self) {
        self.frames.clear();
        self.keyframes.clear();
        self.frame_bytes = 0;
        self.waiting_for_keyframe = true;
    }

    fn drop_front_gop(&mut self) {
        let next = self.keyframes.get(1).map(|(s, _)| *s);
        match next {
            Some(next_seq) => {
                while let Some(e) = self.frames.front() {
                    if e.seq >= next_seq {
                        break;
                    }
                    self.frame_bytes -= e.cost();
                    self.frames.pop_front();
                }
                self.keyframes.pop_front();
            }
            None => self.clear_frames(),
        }
    }

    fn evict(&mut self) {
        // 1. Time window: drop front GOPs that end entirely before the cutoff.
        if let Some(newest) = self.end_us() {
            let cutoff = newest.saturating_sub(self.cfg.window_us);
            while self.keyframes.len() >= 2 && self.keyframes[1].1 <= cutoff {
                self.drop_front_gop();
            }
        }
        // 2. Byte budget: drop whole GOPs from the front, but keep the current GOP unless
        //    it alone exceeds twice the budget (then wait for the next keyframe).
        while self.bytes_used() > self.cfg.max_bytes && self.keyframes.len() >= 2 {
            self.drop_front_gop();
        }
        if self.frame_bytes > self.cfg.max_bytes.saturating_mul(2) {
            self.clear_frames();
        }
        // Mark payloads count against the budget too: drop the oldest marks' payloads first.
        while self.bytes_used() > self.cfg.max_bytes && self.mark_bytes > 0 {
            if let Some(m) = self.marks.iter_mut().find(|m| m.payload.is_some())
                && let Some(p) = m.payload.take()
            {
                self.mark_bytes -= p.cost();
            }
        }
        // 3. Marks and telemetry older than the oldest seekable frame (or beyond the
        //    window when nothing is recorded) are no longer reachable.
        let floor = match (self.start_us(), self.end_us()) {
            (Some(s), _) => Some(s),
            _ => self
                .telemetry
                .back()
                .map(|t| t.pts_us.saturating_sub(self.cfg.window_us)),
        };
        if let Some(floor) = floor {
            while self.marks.front().is_some_and(|m| m.mark.pts_us < floor) {
                self.pop_mark();
            }
            while self.telemetry.front().is_some_and(|t| t.pts_us < floor) {
                self.telemetry.pop_front();
            }
        }
        while self.marks.len() > self.cfg.max_marks {
            self.pop_mark();
        }
    }

    fn pop_mark(&mut self) {
        if let Some(m) = self.marks.pop_front()
            && let Some(p) = m.payload
        {
            self.mark_bytes -= p.cost();
        }
    }

    // ---- export / import -------------------------------------------------------------

    /// Export everything currently recorded as a self-contained bundle encrypted with
    /// `export_key` (ChaCha20-Poly1305, random nonce). The session key is never exported.
    pub fn export_bundle(&self, export_key: &[u8; 32]) -> Result<Vec<u8>> {
        let mut plain = BundlePlain {
            frames: Vec::new(),
            marks: Vec::new(),
            telemetry: Vec::new(),
        };
        for e in &self.frames {
            plain
                .frames
                .push((e.pts_us, e.keyframe, self.sealer.open(e)?));
        }
        for m in &self.marks {
            let p = m
                .payload
                .as_ref()
                .map(|e| self.sealer.open(e))
                .transpose()?;
            plain.marks.push((m.mark.clone(), p));
        }
        plain.telemetry = self.telemetry.iter().copied().collect();
        let mut bytes =
            bincode::serialize(&plain).map_err(|e| TimeTravelError::Bundle(e.to_string()))?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(export_key));
        let nonce: [u8; 12] = rand::random();
        let ct = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &bytes,
                    aad: BUNDLE_MAGIC,
                },
            )
            .map_err(|_| TimeTravelError::Bundle("encryption failed".into()))?;
        bytes.zeroize();
        let mut out = Vec::with_capacity(BUNDLE_MAGIC.len() + 12 + ct.len());
        out.extend_from_slice(BUNDLE_MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Load a bundle produced by [`Blackbox::export_bundle`] into a new blackbox (with a
    /// fresh session key). Window/budget of `cfg` apply while re-inserting.
    pub fn import_bundle(
        bundle: &[u8],
        export_key: &[u8; 32],
        cfg: BlackboxConfig,
    ) -> Result<Blackbox> {
        let hdr = BUNDLE_MAGIC.len() + 12;
        if bundle.len() < hdr || &bundle[..BUNDLE_MAGIC.len()] != BUNDLE_MAGIC {
            return Err(TimeTravelError::Bundle(
                "not an AetherLink time-travel bundle".into(),
            ));
        }
        let nonce: [u8; 12] = bundle[BUNDLE_MAGIC.len()..hdr]
            .try_into()
            .expect("12 bytes");
        let cipher = ChaCha20Poly1305::new(Key::from_slice(export_key));
        let mut plain = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &bundle[hdr..],
                    aad: BUNDLE_MAGIC,
                },
            )
            .map_err(|_| TimeTravelError::Tampered)?;
        let b: BundlePlain =
            bincode::deserialize(&plain).map_err(|e| TimeTravelError::Bundle(e.to_string()))?;
        plain.zeroize();
        let mut bb = Blackbox::new(BlackboxConfig {
            spike: None,
            ..cfg.clone()
        });
        for (pts, kf, data) in &b.frames {
            bb.push_frame(*pts, *kf, data);
        }
        for t in b.telemetry {
            bb.push_telemetry(t);
        }
        for (m, p) in b.marks {
            bb.push_mark(m, p.as_deref());
        }
        bb.cfg = cfg;
        Ok(bb)
    }
}

const BUNDLE_MAGIC: &[u8] = b"AETHERTT\x01";

#[derive(Serialize, Deserialize)]
struct BundlePlain {
    frames: Vec<(u64, bool, Vec<u8>)>,
    marks: Vec<(TimelineMark, Option<Vec<u8>>)>,
    telemetry: Vec<TelemetrySample>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tamper_detection() {
        let mut bb = Blackbox::default();
        bb.push_frame(0, true, b"keyframe");
        bb.push_frame(16_666, false, b"delta");
        assert_eq!(bb.seek(16_666).unwrap().frames.len(), 2);

        // Flip one ciphertext byte.
        bb.frames[1].ciphertext[0] ^= 0x01;
        assert_eq!(bb.seek(16_666), Err(TimeTravelError::Tampered));
        assert_eq!(
            bb.seek(0).unwrap().frames[0].data,
            b"keyframe",
            "untouched frames still decrypt"
        );
        bb.frames[1].ciphertext[0] ^= 0x01;
        assert!(bb.seek(16_666).is_ok());

        // Metadata is authenticated too (keyframe flag / timestamp are AAD).
        bb.frames[1].keyframe = true;
        assert_eq!(
            bb.frames_in_range(0, 20_000),
            Err(TimeTravelError::Tampered)
        );
        bb.frames[1].keyframe = false;
        bb.frames[1].pts_us += 1;
        assert_eq!(
            bb.frames_in_range(0, 20_000),
            Err(TimeTravelError::Tampered)
        );
    }

    #[test]
    fn nonces_are_unique_and_payloads_not_plaintext() {
        let mut bb = Blackbox::default();
        let payload = vec![0x41u8; 256];
        bb.push_frame(0, true, &payload);
        bb.push_frame(1, false, &payload);
        assert_ne!(bb.frames[0].counter, bb.frames[1].counter);
        assert_ne!(bb.frames[0].ciphertext, bb.frames[1].ciphertext);
        assert!(
            !bb.frames[0]
                .ciphertext
                .windows(16)
                .any(|w| w == &payload[..16])
        );
        assert_eq!(bb.frames[0].ciphertext.len(), payload.len() + 16);
    }

    #[test]
    fn separate_blackboxes_use_different_keys() {
        let mut a = Blackbox::default();
        let mut b = Blackbox::default();
        a.push_frame(0, true, b"same");
        b.push_frame(0, true, b"same");
        assert_ne!(a.frames[0].ciphertext, b.frames[0].ciphertext);
    }
}
