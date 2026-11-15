use aether_protocol::FramingError;

#[derive(Debug, thiserror::Error)]
pub enum UsbError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("framing error: {0}")]
    Framing(#[from] FramingError),
    #[error("malformed descriptor: {0}")]
    Descriptor(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("no such device: {0}")]
    NoDevice(u32),
    #[error("device {0} is not attached")]
    NotAttached(u32),
    #[error("usb backend error: {0}")]
    Backend(String),
    #[error("connection closed")]
    Closed,
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

#[cfg(all(feature = "libusb", not(target_os = "ios")))]
impl From<rusb::Error> for UsbError {
    fn from(e: rusb::Error) -> Self {
        match e {
            rusb::Error::NotSupported => {
                UsbError::Unsupported("operation not supported by libusb on this platform")
            }
            e => UsbError::Backend(e.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, UsbError>;
