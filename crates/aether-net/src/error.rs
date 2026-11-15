use aether_protocol::{AuthRejectReason, FramingError, rendezvous::RendezvousErrorCode};

/// Errors produced by `aether-net`.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("framing error: {0}")]
    Framing(#[from] FramingError),
    #[error("TLS/QUIC configuration error: {0}")]
    Config(String),
    #[error("QUIC connect error: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("QUIC connection error: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("stream write error: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("stream already closed")]
    ClosedStream(#[from] quinn::ClosedStream),
    #[error("datagram error: {0}")]
    Datagram(#[from] quinn::SendDatagramError),
    #[error("peer closed the stream")]
    StreamClosed,
    #[error("authentication rejected by host: {0:?}")]
    AuthRejected(AuthRejectReason),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("timed out: {0}")]
    Timeout(&'static str),
    #[error("keychain error: {0}")]
    Keychain(String),
    #[error("identity/trust store error: {0}")]
    Store(String),
    #[error("discovery error: {0}")]
    Discovery(String),
    #[error("STUN error: {0}")]
    Stun(String),
    #[error("rendezvous server error: {0}")]
    Rendezvous(RendezvousErrorCode),
    #[error("tunnel refused: {0}")]
    TunnelRefused(String),
    #[error("peer unreachable: {0}")]
    Unreachable(String),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

pub type Result<T> = std::result::Result<T, NetError>;

impl From<quinn::ReadError> for NetError {
    fn from(e: quinn::ReadError) -> Self {
        NetError::Io(e.into())
    }
}

impl From<quinn::ReadExactError> for NetError {
    fn from(e: quinn::ReadExactError) -> Self {
        match e {
            quinn::ReadExactError::FinishedEarly(_) => NetError::StreamClosed,
            quinn::ReadExactError::ReadError(e) => e.into(),
        }
    }
}
