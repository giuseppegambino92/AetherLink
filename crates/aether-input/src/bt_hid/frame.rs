//! Binary frame protocol between AetherLink and the ESP32-S3 HID dongle
//! (`firmware/esp32-s3-hid-dongle`). The firmware parses exactly this format;
//! keep both sides in sync and bump [`PROTOCOL_VERSION`] on any change.
//!
//! ```text
//! +------+------+---------+------+-----+-----+-----------------+-----------+
//! | 0xA5 | 0x5A | version | type | seq | len | payload[len]    | crc16 LE  |
//! +------+------+---------+------+-----+-----+-----------------+-----------+
//!   sync (2 B)    1 B       1 B    1 B   1 B   0..=MAX_PAYLOAD   2 B
//! ```
//!
//! * `crc16` is CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF, no reflection,
//!   no final xor) over `version..=payload`, transmitted little-endian.
//! * All multi-byte payload fields are little-endian.
//! * Every host → dongle frame is answered by an [`MsgType::Ack`] carrying the
//!   acknowledged `seq` and an [`AckStatus`]. Dongle → host frames are not
//!   acknowledged.
//! * The receiver resynchronises after garbage or a CRC error by discarding one
//!   byte and scanning for the next sync pair.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const SYNC: [u8; 2] = [0xA5, 0x5A];
pub const PROTOCOL_VERSION: u8 = 1;
/// Largest payload either side accepts (bounds firmware buffers).
pub const MAX_PAYLOAD: usize = 64;
/// sync + version + type + seq + len.
pub const HEADER_LEN: usize = 6;
pub const CRC_LEN: usize = 2;
pub const MAX_FRAME_LEN: usize = HEADER_LEN + MAX_PAYLOAD + CRC_LEN;

/// CRC-16/CCITT-FALSE. Check value: `crc16_ccitt(b"123456789") == 0x29B1`.
pub fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// Message types. Host → dongle unless noted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MsgType {
    /// `[host_protocol_version]`. Answered by Ack + [`MsgType::Info`].
    Hello = 0x01,
    /// Dongle → host: [`DongleInfo`] (8 B).
    Info = 0x02,
    /// Report 1 payload (8 B: modifiers, reserved, 6 keys).
    Keyboard = 0x10,
    /// Report 2 payload (7 B: buttons, dx i16, dy i16, wheel i8, pan i8).
    Mouse = 0x11,
    /// Report 3 payload (5 B: buttons, x u16, y u16; 0..=32767).
    AbsPointer = 0x12,
    /// Report 4 payload (2 B: consumer usage u16).
    Consumer = 0x13,
    /// `[Target]` — which link(s) reports are sent on.
    SetTarget = 0x20,
    /// `[PairingOp, arg]`.
    Pairing = 0x21,
    /// Empty; answered by Ack + [`MsgType::Status`].
    GetStatus = 0x30,
    /// Dongle → host: [`DongleStatus`] (6 B). Also sent unsolicited on change.
    Status = 0x31,
    /// Dongle → host: keyboard LED output report from the target `[leds]`.
    Leds = 0x32,
    /// Dongle → host: `[acked_seq, AckStatus]`.
    Ack = 0x7E,
}

impl MsgType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x01 => Self::Hello,
            0x02 => Self::Info,
            0x10 => Self::Keyboard,
            0x11 => Self::Mouse,
            0x12 => Self::AbsPointer,
            0x13 => Self::Consumer,
            0x20 => Self::SetTarget,
            0x21 => Self::Pairing,
            0x30 => Self::GetStatus,
            0x31 => Self::Status,
            0x32 => Self::Leds,
            0x7E => Self::Ack,
            _ => return None,
        })
    }

    /// Fixed payload length for messages that have one.
    pub fn fixed_len(self) -> Option<usize> {
        Some(match self {
            Self::Hello => 1,
            Self::Info => DongleInfo::LEN,
            Self::Keyboard => crate::hid_report::KEYBOARD_REPORT_LEN,
            Self::Mouse => crate::hid_report::MOUSE_REPORT_LEN,
            Self::AbsPointer => crate::hid_report::ABS_POINTER_REPORT_LEN,
            Self::Consumer => crate::hid_report::CONSUMER_REPORT_LEN,
            Self::SetTarget => 1,
            Self::Pairing => 2,
            Self::GetStatus => 0,
            Self::Status => DongleStatus::LEN,
            Self::Leds => 1,
            Self::Ack => 2,
        })
    }

    /// Whether re-sending the frame after a lost ack is harmless. Relative
    /// mouse motion is not (it would be applied twice); every other report
    /// carries complete device state.
    pub fn idempotent(self) -> bool {
        !matches!(self, Self::Mouse)
    }

    /// Message type carrying the given HID report ID.
    pub fn for_report_id(id: u8) -> Option<Self> {
        use crate::hid_report::*;
        Some(match id {
            REPORT_ID_KEYBOARD => Self::Keyboard,
            REPORT_ID_MOUSE => Self::Mouse,
            REPORT_ID_ABS_POINTER => Self::AbsPointer,
            REPORT_ID_CONSUMER => Self::Consumer,
            _ => return None,
        })
    }
}

/// Link(s) the dongle forwards reports to. Bitmask on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Target {
    /// USB HID device on the S3's native USB port.
    Usb = 1,
    /// BLE HID-over-GATT (NimBLE).
    #[default]
    Ble = 2,
    Both = 3,
}

impl Target {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Usb),
            2 => Some(Self::Ble),
            3 => Some(Self::Both),
            _ => None,
        }
    }
}

/// `Pairing` sub-operations (`arg` is the advertising timeout in seconds for
/// `StartAdvertising`, 0 = until stopped; ignored otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingOp {
    StopAdvertising = 0,
    StartAdvertising = 1,
    /// Forget all BLE bonds.
    ClearBonds = 2,
    /// Drop the current BLE connection.
    Disconnect = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckStatus {
    Ok = 0,
    UnknownType = 1,
    BadLength = 2,
    /// Accepted but no target link is connected (report dropped).
    NotConnected = 3,
    BadArgument = 4,
    /// Incompatible protocol version (Hello).
    VersionMismatch = 5,
}

impl AckStatus {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Ok,
            1 => Self::UnknownType,
            2 => Self::BadLength,
            3 => Self::NotConnected,
            4 => Self::BadArgument,
            5 => Self::VersionMismatch,
            _ => return None,
        })
    }
}

/// Capability bits in [`DongleInfo::capabilities`].
pub mod caps {
    pub const USB_HID: u8 = 0x01;
    pub const BLE_HID: u8 = 0x02;
    pub const ABS_POINTER: u8 = 0x04;
    pub const CONSUMER: u8 = 0x08;
}

/// `Info` payload: `[proto, fw_major, fw_minor, fw_patch, caps, target, max_payload, reserved]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DongleInfo {
    pub protocol_version: u8,
    pub firmware: (u8, u8, u8),
    pub capabilities: u8,
    pub target: Option<Target>,
    pub max_payload: u8,
}

impl DongleInfo {
    pub const LEN: usize = 8;
    pub fn encode(&self) -> [u8; Self::LEN] {
        [
            self.protocol_version,
            self.firmware.0,
            self.firmware.1,
            self.firmware.2,
            self.capabilities,
            self.target.map(|t| t as u8).unwrap_or(0),
            self.max_payload,
            0,
        ]
    }
    pub fn decode(p: &[u8]) -> Option<Self> {
        (p.len() == Self::LEN).then(|| Self {
            protocol_version: p[0],
            firmware: (p[1], p[2], p[3]),
            capabilities: p[4],
            target: Target::from_u8(p[5]),
            max_payload: p[6],
        })
    }
}

/// `Status` payload: `[target, usb_mounted, ble_connected, ble_advertising, leds, bond_count]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DongleStatus {
    pub target: Option<Target>,
    pub usb_mounted: bool,
    pub ble_connected: bool,
    pub ble_advertising: bool,
    pub leds: u8,
    pub bond_count: u8,
}

impl DongleStatus {
    pub const LEN: usize = 6;
    pub fn encode(&self) -> [u8; Self::LEN] {
        [
            self.target.map(|t| t as u8).unwrap_or(0),
            self.usb_mounted as u8,
            self.ble_connected as u8,
            self.ble_advertising as u8,
            self.leds,
            self.bond_count,
        ]
    }
    pub fn decode(p: &[u8]) -> Option<Self> {
        (p.len() == Self::LEN).then(|| Self {
            target: Target::from_u8(p[0]),
            usb_mounted: p[1] != 0,
            ble_connected: p[2] != 0,
            ble_advertising: p[3] != 0,
            leds: p[4],
            bond_count: p[5],
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub version: u8,
    pub msg_type: u8,
    pub seq: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(msg_type: MsgType, seq: u8, payload: &[u8]) -> Self {
        Self { version: PROTOCOL_VERSION, msg_type: msg_type as u8, seq, payload: payload.to_vec() }
    }

    pub fn kind(&self) -> Option<MsgType> {
        MsgType::from_u8(self.msg_type)
    }

    /// Serialise. Panics if the payload exceeds [`MAX_PAYLOAD`] (a programming
    /// error: every message type has a fixed, small payload).
    pub fn encode(&self) -> Vec<u8> {
        assert!(self.payload.len() <= MAX_PAYLOAD, "frame payload too large");
        let mut v = Vec::with_capacity(HEADER_LEN + self.payload.len() + CRC_LEN);
        v.extend_from_slice(&SYNC);
        v.extend_from_slice(&[self.version, self.msg_type, self.seq, self.payload.len() as u8]);
        v.extend_from_slice(&self.payload);
        let crc = crc16_ccitt(&v[2..]);
        v.extend_from_slice(&crc.to_le_bytes());
        v
    }
}

/// Counters kept by [`FrameDecoder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecoderStats {
    pub frames: u64,
    pub crc_errors: u64,
    /// Bytes skipped while hunting for a sync pair (garbage, boot log, …).
    pub skipped_bytes: u64,
    pub bad_headers: u64,
}

/// Streaming decoder: feed arbitrary chunks with [`push`](Self::push), pull
/// frames with [`next_frame`](Self::next_frame).
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: VecDeque<u8>,
    pub stats: DecoderStats,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
        // Bound memory if the peer streams garbage without sync bytes.
        let limit = 4 * MAX_FRAME_LEN;
        if self.buf.len() > limit {
            let drop = self.buf.len() - limit;
            self.buf.drain(..drop);
            self.stats.skipped_bytes += drop as u64;
        }
    }

    fn skip(&mut self, n: usize) {
        self.buf.drain(..n);
        self.stats.skipped_bytes += n as u64;
    }

    /// Next complete, CRC-valid frame, or `None` when more bytes are needed.
    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            // Hunt for the sync pair.
            let start = (0..self.buf.len().saturating_sub(1)).find(|&i| self.buf[i] == SYNC[0] && self.buf[i + 1] == SYNC[1]);
            match start {
                Some(i) if i > 0 => self.skip(i),
                Some(_) => {}
                None => {
                    // Keep a trailing first sync byte, drop everything else.
                    let keep = usize::from(self.buf.back() == Some(&SYNC[0]));
                    let n = self.buf.len() - keep;
                    if n > 0 {
                        self.skip(n);
                    }
                    return None;
                }
            }
            if self.buf.len() < HEADER_LEN {
                return None;
            }
            let version = self.buf[2];
            let len = self.buf[5] as usize;
            if version != PROTOCOL_VERSION || len > MAX_PAYLOAD {
                self.stats.bad_headers += 1;
                self.skip(1);
                continue;
            }
            let total = HEADER_LEN + len + CRC_LEN;
            if self.buf.len() < total {
                return None;
            }
            let bytes: Vec<u8> = self.buf.range(..total).copied().collect();
            let crc = u16::from_le_bytes([bytes[total - 2], bytes[total - 1]]);
            if crc16_ccitt(&bytes[2..total - 2]) != crc {
                self.stats.crc_errors += 1;
                self.skip(1);
                continue;
            }
            self.buf.drain(..total);
            self.stats.frames += 1;
            return Some(Frame { version, msg_type: bytes[3], seq: bytes[4], payload: bytes[HEADER_LEN..total - 2].to_vec() });
        }
    }
}

/// A frame awaiting its ack.
#[derive(Debug, Clone)]
pub struct InFlight {
    pub seq: u8,
    pub msg_type: MsgType,
    pub bytes: Vec<u8>,
    pub sent_at: Instant,
    pub attempts: u32,
}

/// Outcome of feeding an ack to the [`AckTracker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    Acked { msg_type: MsgType, status: AckStatus },
    /// Ack for a seq that is not in flight (duplicate or late ack).
    Unexpected,
}

/// Sequence numbering, in-flight window and retransmission bookkeeping on
/// the host side.
#[derive(Debug)]
pub struct AckTracker {
    next_seq: u8,
    window: usize,
    timeout: Duration,
    max_attempts: u32,
    in_flight: VecDeque<InFlight>,
    pub acked: u64,
    pub nacked: u64,
    pub retransmits: u64,
    pub dropped: u64,
}

impl AckTracker {
    pub fn new(window: usize, timeout: Duration, max_attempts: u32) -> Self {
        Self {
            next_seq: 0,
            window: window.clamp(1, 128),
            timeout,
            max_attempts: max_attempts.max(1),
            in_flight: VecDeque::new(),
            acked: 0,
            nacked: 0,
            retransmits: 0,
            dropped: 0,
        }
    }

    /// Whether another frame may be sent without exceeding the window.
    pub fn can_send(&self) -> bool {
        self.in_flight.len() < self.window
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Assign the next sequence number, record the frame as in flight and
    /// return its wire bytes. If the window is full the oldest entry is
    /// evicted (counted in `dropped`) — input must never block on a stuck
    /// dongle.
    pub fn prepare(&mut self, msg_type: MsgType, payload: &[u8], now: Instant) -> (u8, Vec<u8>) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let bytes = Frame::new(msg_type, seq, payload).encode();
        if !self.can_send() {
            self.in_flight.pop_front();
            self.dropped += 1;
        }
        self.in_flight.push_back(InFlight { seq, msg_type, bytes: bytes.clone(), sent_at: now, attempts: 1 });
        (seq, bytes)
    }

    pub fn on_ack(&mut self, seq: u8, status: u8) -> AckOutcome {
        let Some(pos) = self.in_flight.iter().position(|f| f.seq == seq) else {
            return AckOutcome::Unexpected;
        };
        let f = self.in_flight.remove(pos).expect("position is valid");
        let status = AckStatus::from_u8(status).unwrap_or(AckStatus::BadArgument);
        if status == AckStatus::Ok {
            self.acked += 1;
        } else {
            self.nacked += 1;
        }
        AckOutcome::Acked { msg_type: f.msg_type, status }
    }

    /// Frames whose ack timed out: idempotent ones are returned for
    /// re-sending (same seq, so the dongle can recognise a duplicate);
    /// non-idempotent ones and those out of attempts are dropped.
    pub fn poll_timeouts(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut resend = Vec::new();
        let (timeout, max) = (self.timeout, self.max_attempts);
        let mut dropped = 0;
        self.in_flight.retain_mut(|f| {
            if now.duration_since(f.sent_at) < timeout {
                return true;
            }
            if f.msg_type.idempotent() && f.attempts < max {
                f.attempts += 1;
                f.sent_at = now;
                resend.push(f.bytes.clone());
                true
            } else {
                dropped += 1;
                false
            }
        });
        self.retransmits += resend.len() as u64;
        self.dropped += dropped;
        resend
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_vectors() {
        assert_eq!(crc16_ccitt(b"123456789"), 0x29B1);
        assert_eq!(crc16_ccitt(b""), 0xFFFF);
        assert_eq!(crc16_ccitt(&[0x00]), 0xE1F0);
        assert_eq!(crc16_ccitt(b"A"), 0xB915);
    }

    #[test]
    fn encode_layout_and_round_trip() {
        let f = Frame::new(MsgType::Keyboard, 7, &[0x02, 0, 0x04, 0, 0, 0, 0, 0]);
        let b = f.encode();
        assert_eq!(&b[..6], &[0xA5, 0x5A, 1, 0x10, 7, 8]);
        assert_eq!(b.len(), 6 + 8 + 2);
        let crc = crc16_ccitt(&b[2..14]);
        assert_eq!(&b[14..], &crc.to_le_bytes());
        let mut d = FrameDecoder::new();
        d.push(&b);
        assert_eq!(d.next_frame(), Some(f));
        assert_eq!(d.next_frame(), None);
        // Known-answer vector shared with the firmware README.
        let hello = Frame::new(MsgType::Hello, 0, &[1]).encode();
        assert_eq!(hello[..7], [0xA5, 0x5A, 0x01, 0x01, 0x00, 0x01, 0x01]);
        assert_eq!(u16::from_le_bytes([hello[7], hello[8]]), crc16_ccitt(&[1, 1, 0, 1, 1]));
    }

    #[test]
    fn byte_at_a_time_and_empty_payload() {
        let frames =
            [Frame::new(MsgType::GetStatus, 255, &[]), Frame::new(MsgType::Mouse, 0, &[1, 2, 3, 4, 5, 6, 7])];
        let wire: Vec<u8> = frames.iter().flat_map(|f| f.encode()).collect();
        let mut d = FrameDecoder::new();
        let mut got = Vec::new();
        for b in wire {
            d.push(&[b]);
            while let Some(f) = d.next_frame() {
                got.push(f);
            }
        }
        assert_eq!(got, frames);
        assert_eq!(d.stats.frames, 2);
        assert_eq!(d.stats.skipped_bytes, 0);
    }

    #[test]
    fn resync_after_garbage_and_corruption() {
        let good = Frame::new(MsgType::Consumer, 3, &[0x24, 0x02]);
        let mut corrupt = Frame::new(MsgType::Keyboard, 4, &[0; 8]).encode();
        corrupt[8] ^= 0xFF; // payload bit flip -> CRC mismatch
        let mut wire = b"ESP-ROM:esp32s3 boot log \xA5\xA5\x5A\xFF".to_vec(); // garbage incl. fake sync
        wire.extend_from_slice(&corrupt);
        wire.extend_from_slice(&[0xA5]); // stray sync byte
        wire.extend_from_slice(&good.encode());
        let mut d = FrameDecoder::new();
        d.push(&wire);
        assert_eq!(d.next_frame(), Some(good));
        assert_eq!(d.next_frame(), None);
        assert!(d.stats.crc_errors >= 1);
        assert!(d.stats.bad_headers >= 1);
        assert!(d.stats.skipped_bytes > 20);
    }

    #[test]
    fn oversized_length_is_rejected() {
        let mut d = FrameDecoder::new();
        d.push(&[0xA5, 0x5A, 1, 0x10, 0, 200]);
        let good = Frame::new(MsgType::Leds, 1, &[0x02]);
        d.push(&good.encode());
        assert_eq!(d.next_frame(), Some(good));
        assert_eq!(d.stats.bad_headers, 1);
    }

    #[test]
    fn garbage_is_bounded() {
        let mut d = FrameDecoder::new();
        d.push(&vec![0x11; 10_000]);
        assert!(d.next_frame().is_none());
        d.push(&vec![0x11; 10_000]);
        assert!(d.buf.len() <= 4 * MAX_FRAME_LEN);
    }

    #[test]
    fn info_and_status_round_trip() {
        let i = DongleInfo {
            protocol_version: 1,
            firmware: (0, 1, 2),
            capabilities: caps::USB_HID | caps::BLE_HID,
            target: Some(Target::Both),
            max_payload: 64,
        };
        assert_eq!(DongleInfo::decode(&i.encode()), Some(i));
        let s = DongleStatus {
            target: Some(Target::Ble),
            usb_mounted: false,
            ble_connected: true,
            ble_advertising: false,
            leds: 2,
            bond_count: 1,
        };
        assert_eq!(DongleStatus::decode(&s.encode()), Some(s));
        assert_eq!(DongleStatus::decode(&[0; 5]), None);
        for t in [MsgType::Hello, MsgType::Keyboard, MsgType::Ack, MsgType::Status] {
            assert_eq!(MsgType::from_u8(t as u8), Some(t));
        }
        assert_eq!(MsgType::for_report_id(3), Some(MsgType::AbsPointer));
    }

    #[test]
    fn ack_tracking_seq_wrap_and_retransmit() {
        let t0 = Instant::now();
        let mut t = AckTracker::new(4, Duration::from_millis(100), 3);
        // Sequence numbers wrap.
        for i in 0..=255u16 {
            let (seq, _) = t.prepare(MsgType::Keyboard, &[0; 8], t0);
            assert_eq!(seq as u16, i);
            assert_eq!(t.on_ack(seq, 0), AckOutcome::Acked { msg_type: MsgType::Keyboard, status: AckStatus::Ok });
        }
        let (seq, _) = t.prepare(MsgType::Keyboard, &[0; 8], t0);
        assert_eq!(seq, 0);
        assert_eq!(t.on_ack(seq, 0), AckOutcome::Acked { msg_type: MsgType::Keyboard, status: AckStatus::Ok });
        assert_eq!(t.on_ack(seq, 0), AckOutcome::Unexpected); // duplicate ack

        // Timeout: keyboard (idempotent) is re-sent with the same bytes, mouse is dropped.
        let (ks, kb) = t.prepare(MsgType::Keyboard, &[1; 8], t0);
        let (_ms, _) = t.prepare(MsgType::Mouse, &[0; 7], t0);
        assert!(t.poll_timeouts(t0 + Duration::from_millis(50)).is_empty());
        let resent = t.poll_timeouts(t0 + Duration::from_millis(150));
        assert_eq!(resent, vec![kb.clone()]);
        assert_eq!(t.in_flight(), 1);
        // Out of attempts after max_attempts sends.
        let resent = t.poll_timeouts(t0 + Duration::from_millis(300));
        assert_eq!(resent.len(), 1);
        assert!(t.poll_timeouts(t0 + Duration::from_millis(450)).is_empty());
        assert_eq!(t.in_flight(), 0);
        assert_eq!(t.on_ack(ks, 0), AckOutcome::Unexpected);
        assert_eq!(t.dropped, 2);
        assert_eq!(t.retransmits, 2);

        // NACK status is reported.
        let (s, _) = t.prepare(MsgType::Consumer, &[0, 0], t0);
        assert_eq!(
            t.on_ack(s, AckStatus::NotConnected as u8),
            AckOutcome::Acked { msg_type: MsgType::Consumer, status: AckStatus::NotConnected }
        );
        assert_eq!(t.nacked, 1);

        // A full window evicts the oldest entry instead of blocking.
        for _ in 0..6 {
            t.prepare(MsgType::Keyboard, &[0; 8], t0);
        }
        assert_eq!(t.in_flight(), 4);
        assert!(!t.can_send());
    }
}
