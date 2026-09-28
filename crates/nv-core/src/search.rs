#![allow(dead_code, unused_variables, clippy::new_without_default)]

use crate::model::{NoteId, SearchFields};

pub struct SearchState;

impl Default for SearchState {
    fn default() -> Self {
        todo!()
    }
}

impl SearchState {
    pub fn new() -> Self {
        todo!()
    }

    /// Filter in input order, optionally bypassing the cached narrowing state.
    pub fn filter(
        &mut self,
        query: &str,
        notes: &[(NoteId, &SearchFields)],
        force: bool,
    ) -> Vec<NoteId> {
        todo!()
    }

    pub fn autocomplete(
        &self,
        query: &str,
        filtered: &[(NoteId, &SearchFields)],
    ) -> Option<NoteId> {
        todo!()
    }
}
