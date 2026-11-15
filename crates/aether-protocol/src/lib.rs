//! # aether-protocol
//!
//! The AetherLink wire protocol. Every QUIC bidirectional/unidirectional stream
//! starts with a single [`StreamHeader`] frame announcing its [`StreamKind`];
//! all following frames on that stream carry the message enum associated with
//! the kind (see the table on [`StreamKind`]). Latency-critical, loss-tolerant
//! traffic (pointer motion, cursor position, pings) travels as QUIC datagrams
//! encoded as [`Datagram`].
//!
//! Frames are `u32` little-endian length prefixed `bincode` payloads; see
//! [`framing`].

pub mod audio;
pub mod clipboard;
pub mod control;
pub mod dom;
pub mod framing;
pub mod handshake;
pub mod ids;
pub mod input;
pub mod rendezvous;
pub mod stream;
pub mod terminal;
pub mod timetravel;
pub mod transfer;
pub mod tunnel;
pub mod usb;
pub mod vfs;
pub mod video;

pub use audio::*;
pub use clipboard::*;
pub use control::*;
pub use dom::*;
pub use framing::{FrameDecoder, FramingError, MAX_FRAME_LEN, decode_message, encode_frame, encode_message};
pub use handshake::*;
pub use ids::*;
pub use input::*;
pub use stream::*;
pub use terminal::*;
pub use timetravel::*;
pub use transfer::*;
pub use tunnel::*;
pub use usb::*;
pub use vfs::*;
pub use video::*;

/// Wire protocol version. Bumped on any incompatible change.
pub const PROTOCOL_VERSION: u16 = 1;

/// ALPN identifier negotiated during the QUIC/TLS handshake.
pub const ALPN: &[u8] = b"aetherlink/1";

/// mDNS / DNS-SD service type used for LAN discovery.
pub const MDNS_SERVICE_TYPE: &str = "_aetherlink._udp.local.";

/// Default UDP port for the QUIC endpoint (0 = ephemeral is also allowed).
pub const DEFAULT_PORT: u16 = 21118;

/// Default UDP port of the rendezvous / relay server.
pub const DEFAULT_RELAY_PORT: u16 = 21116;

/// Pixel rectangle in host display coordinates (physical pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x
            && py >= self.y
            && (px as i64) < self.x as i64 + self.width as i64
            && (py as i64) < self.y as i64 + self.height as i64
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        (self.x as i64) < other.x as i64 + other.width as i64
            && (other.x as i64) < self.x as i64 + self.width as i64
            && (self.y as i64) < other.y as i64 + other.height as i64
            && (other.y as i64) < self.y as i64 + self.height as i64
    }

    /// Smallest rectangle containing both.
    pub fn union(&self, other: &Rect) -> Rect {
        if self.area() == 0 {
            return *other;
        }
        if other.area() == 0 {
            return *self;
        }
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = (self.x as i64 + self.width as i64).max(other.x as i64 + other.width as i64);
        let y1 = (self.y as i64 + self.height as i64).max(other.y as i64 + other.height as i64);
        Rect::new(x0, y0, (x1 - x0 as i64) as u32, (y1 - y0 as i64) as u32)
    }
}

/// Operating system family of a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum OsKind {
    MacOs,
    Windows,
    Linux,
    Android,
    Ios,
    Unknown,
}

impl OsKind {
    /// The OS this binary was compiled for.
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            OsKind::MacOs
        } else if cfg!(target_os = "windows") {
            OsKind::Windows
        } else if cfg!(target_os = "android") {
            OsKind::Android
        } else if cfg!(target_os = "ios") {
            OsKind::Ios
        } else if cfg!(target_os = "linux") {
            OsKind::Linux
        } else {
            OsKind::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_math() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        let c = Rect::new(20, 20, 1, 1);
        assert!(a.intersects(&b));
        assert!(!a.intersects(&c));
        assert_eq!(a.union(&b), Rect::new(0, 0, 15, 15));
        assert!(a.contains(9, 9));
        assert!(!a.contains(10, 10));
        assert_eq!(Rect::default().union(&c), c);
    }
}
