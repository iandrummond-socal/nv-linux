#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Remote error: {0}")]
    Remote(#[from] crate::remote::RemoteError),
    #[error("Authentication error: {0}")]
    Auth(String),
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Sync is locked")]
    Locked,
    #[error("Sync conflict: {0}")]
    Conflict(String),
    #[error("Core error: {0}")]
    Core(#[from] nv_core::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
