#![allow(dead_code, unused_variables, clippy::new_without_default)]

use nv_core::model::NoteId;

pub struct Editor;

impl Editor {
    /// Construct an editor with the configured autosave debounce interval in milliseconds.
    pub fn new(autosave_ms: u64) -> Self {
        todo!()
    }

    /// Borrow the container holding the banner and scrolling text view.
    pub fn widget(&self) -> &gtk::Box {
        todo!()
    }

    pub fn show(&self, id: NoteId, body: &str, read_only: bool) {
        todo!()
    }

    pub fn clear(&self) {
        todo!()
    }

    pub fn focus(&self) {
        todo!()
    }

    pub fn on_changed_debounced(&self, callback: impl Fn(NoteId, String) + 'static) {
        todo!()
    }

    pub fn flush(&self) -> Option<(NoteId, String)> {
        todo!()
    }

    pub fn set_banner(&self, banner: Option<&str>) {
        todo!()
    }
}
