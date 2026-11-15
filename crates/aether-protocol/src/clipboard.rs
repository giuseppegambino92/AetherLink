//! Universal clipboard.

use crate::OsKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClipboardMode {
    Disabled,
    /// Local changes are pushed to the peer; remote changes are ignored.
    PushOnly,
    Bidirectional,
}

/// Canonical, OS-neutral clipboard formats. Platform formats are transcoded at
/// the edges (e.g. `public.heic`/TIFF → PNG, `CF_DIBV5` → PNG, `CF_HTML` → HTML).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClipboardFormat {
    PlainText,
    Html,
    Rtf,
    Png,
    Tiff,
    Heic,
    /// Windows `CF_DIBV5` blob (BITMAPV5HEADER + pixels).
    DibV5,
    /// List of files — payload is in [`ClipboardPayload::files`].
    FileList,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardItem {
    pub format: ClipboardFormat,
    pub data: Vec<u8>,
}

/// A file advertised on the remote clipboard. Contents are pulled lazily over a
/// `FileTransfer` stream when the user pastes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteFileRef {
    pub token: u64,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardPayload {
    /// Monotonic per-sender counter; used to suppress echo loops.
    pub seq: u64,
    pub origin_os: OsKind,
    /// Same content in several representations, richest first.
    pub items: Vec<ClipboardItem>,
    pub files: Vec<RemoteFileRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardMessage {
    Update(ClipboardPayload),
    /// Ask the peer to stream a clipboard file (answered on a FileTransfer stream).
    FetchFile { token: u64, transfer_id: u64 },
}
