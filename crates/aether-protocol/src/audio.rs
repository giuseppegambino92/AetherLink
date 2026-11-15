//! Audio packets.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Opus,
    /// Interleaved signed 16-bit little-endian PCM.
    PcmS16Le,
    /// Interleaved 32-bit float little-endian PCM.
    PcmF32Le,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioPacket {
    pub seq: u64,
    pub pts_us: u64,
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u8,
    pub data: Vec<u8>,
}
