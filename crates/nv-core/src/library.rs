#![allow(dead_code, unused_variables, clippy::new_without_default)]

use crate::config::SortOrder;
use crate::model::{Note, NoteId};
use crate::storage::NoteStore;
use crate::watcher::StoreEvent;
use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    Conflict { copy_id: NoteId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Updated(NoteId),
    Removed(NoteId),
    Renamed(NoteId),
    Added(NoteId),
    None,
}

pub struct Library;

impl Library {
    /// Combine an opened store with the configured display order.
    pub fn new(store: NoteStore, sort: SortOrder) -> Self {
        todo!()
    }

    pub fn search(&mut self, query: &str) -> Vec<NoteId> {
        todo!()
    }

    /// Choose a completion from filtered identifiers in display order.
    pub fn autocomplete(&self, query: &str, filtered: &[NoteId]) -> Option<NoteId> {
        todo!()
    }

    pub fn create(&mut self, title: &str, body: &str) -> Result<NoteId> {
        todo!()
    }

    pub fn save(&mut self, id: NoteId, body: &str) -> Result<SaveOutcome> {
        todo!()
    }

    pub fn rename(&mut self, id: NoteId, new_title: &str) -> Result<()> {
        todo!()
    }

    pub fn trash(&mut self, id: NoteId) -> Result<()> {
        todo!()
    }

    /// Re-create the retained note after external deletion, preserving its identifier.
    pub fn restore(&mut self, id: NoteId) -> Result<()> {
        todo!()
    }

    /// Apply a filesystem event, logging failures and returning the resulting change.
    pub fn apply_external(&mut self, event: StoreEvent) -> Change {
        todo!()
    }

    pub fn get(&self, id: NoteId) -> Option<&Note> {
        todo!()
    }

    pub fn all(&self) -> Vec<&Note> {
        todo!()
    }

    /// Report whether autosave must stop because the note was removed externally.
    pub fn deleted_on_disk(&self, id: NoteId) -> bool {
        todo!()
    }
}
