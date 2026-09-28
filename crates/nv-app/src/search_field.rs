#![allow(dead_code, unused_variables, clippy::new_without_default)]

pub struct SearchField;

impl SearchField {
    pub fn new() -> Self {
        todo!()
    }

    /// Borrow the entry for window layout and focus management.
    pub fn widget(&self) -> &gtk::Entry {
        todo!()
    }

    pub fn on_query_changed(&self, callback: impl Fn(String, bool) + 'static) {
        todo!()
    }

    pub fn on_activate(&self, callback: impl Fn() + 'static) {
        todo!()
    }

    pub fn on_move(&self, callback: impl Fn(i32, bool) + 'static) {
        todo!()
    }

    pub fn on_tab(&self, callback: impl Fn() + 'static) {
        todo!()
    }

    pub fn on_nav(&self, callback: impl Fn(bool) + 'static) {
        todo!()
    }

    pub fn set_completion(&self, full_title: &str, caret_chars: usize) {
        todo!()
    }

    pub fn show_title(&self, title: &str) {
        todo!()
    }

    pub fn restore_typed(&self) {
        todo!()
    }

    pub fn typed(&self) -> String {
        todo!()
    }

    pub fn focus_select_all(&self) {
        todo!()
    }

    pub fn set_rename_mode(&self, enabled: bool) {
        todo!()
    }
}
