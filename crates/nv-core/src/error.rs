use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Theme error: {0}")]
    Theme(String),
    #[error("Note name collision: {0}")]
    NameCollision(String),
    #[error("Note changed externally: {}", .0.display())]
    ExternalChange(PathBuf),
    #[error("Note is read-only: {}", .0.display())]
    ReadOnly(PathBuf),
    #[error("Trash error: {0}")]
    Trash(String),
}

pub type Result<T> = std::result::Result<T, Error>;
