use aether_protocol::{FramingError, VfsErrorKind};

/// Errors produced by this crate.
#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("framing error: {0}")]
    Framing(#[from] FramingError),
    /// The remote host answered with a [`aether_protocol::VfsResponse::Error`].
    #[error("remote error ({kind:?}): {message}")]
    Remote { kind: VfsErrorKind, message: String },
    /// The peer sent a response that does not match the request.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// BLAKE3 verification failed.
    #[error("integrity check failed: {0}")]
    Integrity(String),
    /// The RPC connection is gone.
    #[error("connection closed")]
    Closed,
    #[error("cancelled: {0}")]
    Cancelled(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("mount failed: {0}")]
    Mount(String),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

impl VfsError {
    /// The VFS error kind, for remote errors.
    pub fn kind(&self) -> Option<VfsErrorKind> {
        match self {
            VfsError::Remote { kind, .. } => Some(*kind),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, VfsError>;
