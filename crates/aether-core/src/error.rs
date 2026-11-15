/// Engine errors. `code()` returns a stable i18n key that the UI localizes
/// (keys live under `errors.*` in the locale files).
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("clipboard: {0}")]
    Clipboard(String),
    #[error("image transcoding: {0}")]
    Image(String),
    #[error("terminal: {0}")]
    Terminal(String),
    #[error("network: {0}")]
    Net(String),
    #[error("session not found")]
    SessionNotFound,
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("unsupported on this platform: {0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    Other(String),
}

impl CoreError {
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::Io(_) => "errors.io",
            CoreError::Json(_) => "errors.io",
            CoreError::Clipboard(_) => "errors.clipboard",
            CoreError::Image(_) => "errors.clipboard",
            CoreError::Terminal(_) => "errors.terminal",
            CoreError::Net(_) => "errors.network",
            CoreError::SessionNotFound => "errors.sessionNotFound",
            CoreError::Invalid(_) => "errors.invalid",
            CoreError::Unsupported(_) => "errors.unsupported",
            CoreError::Other(_) => "errors.generic",
        }
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;
