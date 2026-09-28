use std::path::PathBuf;
use std::time::SystemTime;

/// Stable session identifier assigned by the store.
pub type NoteId = u64;

#[derive(Debug, Clone)]
pub struct Note {
    pub id: NoteId,
    pub title: String,
    pub body: String,
    pub path: PathBuf,
    pub modified: SystemTime,
    pub labels: String,
    pub read_only: bool,
}

/// Unicode-lowercased fields used by the search engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchFields {
    pub title: String,
    pub body: String,
    pub labels: String,
}

impl Note {
    pub fn search_fields(&self) -> SearchFields {
        SearchFields {
            title: self.title.to_lowercase(),
            body: self.body.to_lowercase(),
            labels: self.labels.to_lowercase(),
        }
    }
}
