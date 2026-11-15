//! # aether-vfs
//!
//! Remote virtual filesystem for AetherLink:
//!
//! * [`VfsHost`] — path-sandboxed handler for [`VfsRequest`]s against a [`ShareRegistry`]
//!   of local directories, plus [`serve`], the pipelined request loop over any
//!   `AsyncRead`/`AsyncWrite` pair (a QUIC `StreamKind::Vfs` bi-stream in production).
//! * [`VfsClient`] — pipelined async RPC client with typed helpers and an optional
//!   on-disk, BLAKE3-verified, content-addressed LRU [`ChunkCache`].
//! * [`mount()`] — exposes a remote share as a native folder: a FUSE filesystem on Linux
//!   (`fuser` + `fusermount3`, no root; NFS fallback when root), a loopback NFSv3 server
//!   (`nfsserve`) mounted with the OS NFS client on macOS, or as a drive
//!   letter through the [`webdav`] server and the WebClient redirector (Windows).
//! * [`webdav`] — a loopback WebDAV server for one remote share (any OS; on Windows it is
//!   what [`mount()`] maps as a drive through the built-in WebClient redirector).
//! * [`transfer`] — drag-and-drop file transfer (`TransferMessage` streams), BLAKE3-verified,
//!   with atomic renames and name-collision handling.
//!
//! ## Security model
//!
//! The host treats every request as hostile. Paths must be relative, `/`-separated,
//! must not contain `..`, NUL bytes or Windows drive/UNC prefixes. Every path is
//! resolved with symlinks followed (`canonicalize`) and refused if it ends up outside
//! the share root; for operations that create or rename entries the *parent* directory
//! is canonicalized and checked. Namespace-changing operations (create, mkdir, remove,
//! rename, symlink) are serialized per share so a peer cannot race its own requests
//! (e.g. swap a directory for a symlink between check and use); final opens use
//! `O_NOFOLLOW` on Unix. Symlinks created through the VFS must have relative targets
//! that stay inside the share.

mod cache;
mod client;
mod error;
#[cfg(target_os = "linux")]
mod fuse;
mod host;
pub mod mount;
pub mod mount_windows;
// Only the macOS/Linux mount uses the NFS adapter (Windows mounts over WebDAV).
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
mod nfs;
mod path;
mod serve;
pub mod transfer;
pub mod webdav;

pub use aether_protocol::{
    DirEntry, FileAttr, FileKind, ShareInfo, VFS_CHUNK_SIZE, VfsEnvelope, VfsErrorKind, VfsRequest,
    VfsResponse,
};
pub use cache::{CacheStats, ChunkCache};
pub use client::{PendingResponse, VfsClient};
pub use error::{Result, VfsError};
pub use host::{MAX_READ_LEN, Share, ShareRegistry, VfsHost};
pub use mount::{MountBackend, MountHandle, default_mount_point, mount};
pub use path::{split_rel_path, validate_name};
pub use serve::{MAX_IN_FLIGHT, serve};
pub use webdav::WebDavHandle;
