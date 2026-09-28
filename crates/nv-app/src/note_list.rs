#![allow(dead_code, unused_variables, clippy::new_without_default)]

use std::time::SystemTime;

use nv_core::model::NoteId;

/// Cached display data; preview is the first body line and modified drives relative dates.
#[derive(Debug, Clone)]
pub struct RowData {
    pub id: NoteId,
    pub title: String,
    pub preview: String,
    pub modified: SystemTime,
}

pub struct NoteList;

impl NoteList {
    pub fn new() -> Self {
        todo!()
    }

    /// Borrow the list's scrolling container for window layout.
    pub fn widget(&self) -> &gtk::ScrolledWindow {
        todo!()
    }

    pub fn set_notes(&self, ids: &[NoteId]) {
        todo!()
    }

    pub fn update_row(&self, row: RowData) {
        todo!()
    }

    pub fn remove_row(&self, id: NoteId) {
        todo!()
    }

    pub fn select(&self, id: Option<NoteId>) {
        todo!()
    }

    pub fn selected(&self) -> Option<NoteId> {
        todo!()
    }

    pub fn move_selection(&self, delta: i32) {
        todo!()
    }

    pub fn on_selected(&self, callback: impl Fn(Option<NoteId>, bool) + 'static) {
        todo!()
    }

    pub fn on_activate(&self, callback: impl Fn(NoteId) + 'static) {
        todo!()
    }
}
