//! Remote virtual filesystem RPC (backs the native NFS/FUSE/WinFsp mount).
//!
//! Paths are always `/`-separated and relative to a share root; the host
//! must reject any path that escapes the share.

use serde::{Deserialize, Serialize};

/// Size of content-addressed chunks used for streaming and cache verification.
pub const VFS_CHUNK_SIZE: u32 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    pub id: u32,
    pub name: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAttr {
    /// Stable per-share inode-like id.
    pub file_id: u64,
    pub kind: FileKind,
    pub size: u64,
    /// Unix permission bits (best-effort on Windows).
    pub mode: u32,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub attr: FileAttr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VfsRequest {
    ListShares,
    Stat { share: u32, path: String },
    ReadDir { share: u32, path: String },
    Read { share: u32, path: String, offset: u64, len: u32 },
    /// BLAKE3 hashes of each `VFS_CHUNK_SIZE` chunk, for cache validation.
    ChunkHashes { share: u32, path: String },
    Write { share: u32, path: String, offset: u64, data: Vec<u8> },
    Create { share: u32, path: String, exclusive: bool },
    Mkdir { share: u32, path: String },
    Remove { share: u32, path: String },
    Rename { share: u32, from: String, to: String },
    Truncate { share: u32, path: String, size: u64 },
    Symlink { share: u32, path: String, target: String },
    ReadLink { share: u32, path: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VfsErrorKind {
    NotFound,
    PermissionDenied,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    NotEmpty,
    ReadOnly,
    InvalidPath,
    NoSuchShare,
    Io,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VfsResponse {
    Shares(Vec<ShareInfo>),
    Attr(FileAttr),
    Entries(Vec<DirEntry>),
    Data { data: Vec<u8>, eof: bool },
    Hashes { chunk_size: u32, hashes: Vec<[u8; 32]> },
    Written { bytes: u32, attr: FileAttr },
    Link { target: String },
    Ok,
    Error { kind: VfsErrorKind, message: String },
    /// Like `Hashes`, plus whether the file's mtime is *settled*: old enough, by the host's
    /// own clock, that any later write must change it even on file systems with coarse
    /// timestamps (FAT: 2 s). Only settled `(mtime, size) → hashes` pairs may be remembered;
    /// otherwise a same-size rewrite within the same timestamp tick would look unchanged.
    HashesV2 {
        chunk_size: u32,
        hashes: Vec<[u8; 32]>,
        settled: bool,
    },
}

/// Request/response correlation wrapper (requests are pipelined on one stream).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VfsEnvelope<T> {
    pub req_id: u64,
    pub body: T,
}
