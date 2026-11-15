//! Drag-and-drop & clipboard file transfers.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferEntry {
    /// `/`-separated path relative to the drop root.
    pub rel_path: String,
    pub size: u64,
    pub is_dir: bool,
    pub mtime_ns: i128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferMessage {
    /// First message: what is coming. `drop_target` is a host-local directory hint
    /// (the window under the drop point) or `None` for the default Downloads folder.
    Offer { transfer_id: u64, entries: Vec<TransferEntry>, total_bytes: u64, drop_target: Option<String> },
    Chunk { entry: u32, offset: u64, data: Vec<u8> },
    /// End of one entry with its BLAKE3 digest for verification.
    EntryDone { entry: u32, blake3: [u8; 32] },
    Complete,
    Cancel { reason: String },
}

/// Recommended chunk size for transfer streams.
pub const TRANSFER_CHUNK_SIZE: usize = 512 * 1024;
