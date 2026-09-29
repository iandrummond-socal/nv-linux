use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteFile {
    /// Dropbox file ID ("id:…"), used for download and delete.
    pub id: String,
    /// Display-cased filename.
    pub name: String,
    /// The API's own path_lower, never computed locally.
    pub key: String,
    pub rev: String,
    pub content_hash: String,
    pub size: u64,
}

/// A change to a remote note.
///
/// Entries that are folders or fail `nv_core::watcher::is_note_path` are ignored
/// by the remote layer and never produced as changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteChange {
    Upsert(RemoteFile),
    Deleted { key: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor(pub String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemoteError {
    #[error("Remote authentication is required")]
    Unauthorized,
    #[error("Remote rate limit reached; retry after {0:?}")]
    RateLimited(Duration),
    #[error("Remote cursor must be reset")]
    ResetCursor,
    #[error("Remote revision conflict")]
    Conflict,
    #[error("File is too large")]
    TooLarge,
    #[error("Invalid remote filename")]
    BadName,
    #[error("Remote service is offline")]
    Offline,
    #[error("Remote error: {0}")]
    Other(String),
}

pub type RemoteResult<T> = std::result::Result<T, RemoteError>;

pub trait Remote: Send {
    /// List all remote notes and return a cursor for subsequent changes.
    fn list(&mut self) -> RemoteResult<(Vec<RemoteFile>, Cursor)>;

    /// Return note changes since the supplied cursor and an updated cursor.
    fn changes(&mut self, since: &Cursor) -> RemoteResult<(Vec<RemoteChange>, Cursor)>;

    /// Download a note by its remote file ID.
    fn download(&mut self, id: &str) -> RemoteResult<Vec<u8>>;

    /// Upload a note, checking the expected revision when supplied.
    fn upload(
        &mut self,
        name: &str,
        bytes: &[u8],
        expect_rev: Option<&str>,
    ) -> RemoteResult<RemoteFile>;

    /// Delete a note by its remote file ID, checking its parent revision.
    fn delete(&mut self, id: &str, parent_rev: &str) -> RemoteResult<()>;
}

/// Runs separately from `Remote` because longpoll can block for up to 90 seconds
/// and operates on its own thread.
pub trait Notifier: Send {
    /// Wait for changes after the cursor, returning whether changes are available.
    fn wait(&mut self, since: &Cursor, timeout: Duration) -> RemoteResult<bool>;
}

/// Classify a remote filename using the core note-path rules.
pub fn is_note_name(name: &str) -> bool {
    nv_core::watcher::is_note_path(Path::new(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_change_serde_round_trip() {
        let changes = vec![
            RemoteChange::Upsert(RemoteFile {
                id: "id:example".into(),
                name: "Note.txt".into(),
                key: "/note.txt".into(),
                rev: "revision".into(),
                content_hash: "hash".into(),
                size: 42,
            }),
            RemoteChange::Deleted {
                key: "/deleted.txt".into(),
            },
        ];
        let json = serde_json::to_string(&changes).unwrap();
        let decoded: Vec<RemoteChange> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, changes);
    }

    #[test]
    fn note_names() {
        for name in ["a.txt", "a.TEXT", "a.UtF8", "a.taskpaper", "a.MD"] {
            assert!(is_note_name(name), "{name}");
        }
        for name in [".hidden.txt", "x.jpg", "a.txt~", "4913", "a"] {
            assert!(!is_note_name(name), "{name}");
        }
    }
}
