use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib::{self, Propagation};
use gtk::prelude::*;

type Callback<T> = RefCell<Option<Rc<T>>>;

#[derive(Clone)]
pub struct SearchField {
    inner: Rc<Inner>,
}

struct Inner {
    entry: gtk::Entry,
    guard: Cell<bool>,
    in_changed: Cell<bool>,
    last_edit_inserted: Cell<bool>,
    typed: RefCell<String>,
    shown_title: RefCell<Option<String>>,
    rename: RefCell<Option<SavedSearch>>,
    pending: RefCell<Option<View>>,
    idle_scheduled: Cell<bool>,
    query_changed: Callback<dyn Fn(String, bool)>,
    activate: Callback<dyn Fn()>,
    move_selection: Callback<dyn Fn(i32, bool)>,
    tab: Callback<dyn Fn()>,
    nav: Callback<dyn Fn(bool)>,
}

#[derive(Clone)]
struct View {
    text: String,
    start: i32,
    end: i32,
}

struct SavedSearch {
    typed: String,
    shown_title: Option<String>,
    view: View,
}

impl Default for SearchField {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchField {
    #[allow(dead_code)]
    pub fn new() -> Self {
        let entry = gtk::Entry::builder()
            .placeholder_text("Search or Create")
            .hexpand(true)
            .build();
        entry.add_css_class("nv-search");
        let field = Self {
            inner: Rc::new(Inner {
                entry,
                guard: Cell::new(false),
                in_changed: Cell::new(false),
                last_edit_inserted: Cell::new(false),
                typed: RefCell::new(String::new()),
                shown_title: RefCell::new(None),
                rename: RefCell::new(None),
                pending: RefCell::new(None),
                idle_scheduled: Cell::new(false),
                query_changed: RefCell::new(None),
                activate: RefCell::new(None),
                move_selection: RefCell::new(None),
                tab: RefCell::new(None),
                nav: RefCell::new(None),
            }),
        };
        // Weak captures avoid an Entry -> signal -> Inner -> Entry cycle.
        let delegate = field
            .inner
            .entry
            .delegate()
            .unwrap_or_else(|| field.inner.entry.clone().upcast::<gtk::Editable>());
        let weak = Rc::downgrade(&field.inner);
        delegate.connect_insert_text(move |_, _, _| {
            if let Some(inner) = weak.upgrade() {
                if !inner.guard.get() {
                    inner.last_edit_inserted.set(true);
                }
            }
        });
        let weak = Rc::downgrade(&field.inner);
        delegate.connect_delete_text(move |_, _, _| {
            if let Some(inner) = weak.upgrade() {
                if !inner.guard.get() {
                    inner.last_edit_inserted.set(false);
                }
            }
        });
        let weak = Rc::downgrade(&field.inner);
        field.inner.entry.connect_changed(move |entry| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if inner.guard.get() {
                return;
            }
            // A real edit supersedes any completion waiting for the idle loop.
            inner.pending.borrow_mut().take();
            if inner.rename.borrow().is_some() {
                return;
            }
            let text = entry.text().to_string();
            inner.typed.replace(text.clone());
            inner.shown_title.borrow_mut().take();
            let inserted = inner.last_edit_inserted.replace(false);
            let callback = inner.query_changed.borrow().clone();
            let previous = inner.in_changed.replace(true);
            if let Some(callback) = callback {
                callback(text, inserted);
            }
            inner.in_changed.set(previous);
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&field.inner);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(inner) = weak.upgrade() else {
                return Propagation::Proceed;
            };
            Self { inner }.key_pressed(key, state)
        });
        field.inner.entry.add_controller(keys);
        field
    }

    /// Borrow the entry for window layout and focus management.
    #[allow(dead_code)]
    pub fn widget(&self) -> &gtk::Entry {
        &self.inner.entry
    }

    #[allow(dead_code)]
    pub fn on_query_changed(&self, callback: impl Fn(String, bool) + 'static) {
        self.inner.query_changed.replace(Some(Rc::new(callback)));
    }

    #[allow(dead_code)]
    pub fn on_activate(&self, callback: impl Fn() + 'static) {
        self.inner.activate.replace(Some(Rc::new(callback)));
    }

    #[allow(dead_code)]
    pub fn on_move(&self, callback: impl Fn(i32, bool) + 'static) {
        self.inner.move_selection.replace(Some(Rc::new(callback)));
    }

    #[allow(dead_code)]
    pub fn on_tab(&self, callback: impl Fn() + 'static) {
        self.inner.tab.replace(Some(Rc::new(callback)));
    }

    #[allow(dead_code)]
    pub fn on_nav(&self, callback: impl Fn(bool) + 'static) {
        self.inner.nav.replace(Some(Rc::new(callback)));
    }

    #[allow(dead_code)]
    pub fn set_completion(&self, full_title: &str, caret_chars: usize) {
        if self.is_rename_mode() {
            return;
        }
        let typed = self.typed();
        let text = completion_text(&typed, full_title, caret_chars);
        let caret = char_position(caret_chars.min(typed.chars().count()));
        self.present(View {
            text,
            start: caret,
            end: -1,
        });
    }

    #[allow(dead_code)]
    pub fn show_title(&self, title: &str) {
        if self.is_rename_mode() {
            return;
        }
        if self.inner.shown_title.borrow().is_none() {
            let typed = self.inner.typed.borrow().clone();
            self.inner.shown_title.replace(Some(typed));
        }
        self.present(View {
            text: title.to_owned(),
            start: 0,
            end: -1,
        });
    }

    #[allow(dead_code)]
    pub fn restore_typed(&self) {
        if self.is_rename_mode() {
            return;
        }
        let cached = self.inner.shown_title.borrow_mut().take();
        if let Some(text) = cached {
            self.inner.typed.replace(text.clone());
            self.present(View {
                text,
                start: -1,
                end: -1,
            });
        }
    }

    #[allow(dead_code)]
    pub fn typed(&self) -> String {
        self.inner
            .shown_title
            .borrow()
            .clone()
            .unwrap_or_else(|| self.inner.typed.borrow().clone())
    }

    /// Current displayed text, including any selected completion or rename edit.
    #[allow(dead_code)]
    pub fn text(&self) -> String {
        self.inner.entry.text().to_string()
    }

    #[allow(dead_code)]
    pub fn focus_select_all(&self) {
        let previous = self.inner.guard.replace(true);
        self.inner.entry.grab_focus();
        self.inner.guard.set(previous);
        self.select_region(0, -1);
    }

    /// Set the query silently, or the rename text while search state is suspended.
    #[allow(dead_code)]
    pub fn set_text_silently(&self, text: &str) {
        if !self.is_rename_mode() {
            self.inner.typed.replace(text.to_owned());
            self.inner.shown_title.borrow_mut().take();
        }
        self.present(View {
            text: text.to_owned(),
            start: 0,
            end: -1,
        });
    }

    #[allow(dead_code)]
    pub fn set_rename_mode(&self, enabled: bool) {
        if enabled == self.is_rename_mode() {
            return;
        }
        if enabled {
            self.inner.rename.replace(Some(SavedSearch {
                typed: self.inner.typed.borrow().clone(),
                shown_title: self.inner.shown_title.borrow().clone(),
                view: self.current_view(),
            }));
        } else {
            let saved = self.inner.rename.borrow_mut().take();
            if let Some(saved) = saved {
                self.inner.typed.replace(saved.typed);
                self.inner.shown_title.replace(saved.shown_title);
                self.present(saved.view);
            }
        }
    }

    #[allow(dead_code)]
    pub fn is_rename_mode(&self) -> bool {
        self.inner.rename.borrow().is_some()
    }

    fn current_view(&self) -> View {
        if let Some(view) = self.inner.pending.borrow().clone() {
            return view;
        }
        let entry = &self.inner.entry;
        let (start, end) = entry
            .selection_bounds()
            .unwrap_or_else(|| (entry.position(), entry.position()));
        View {
            text: entry.text().to_string(),
            start,
            end,
        }
    }

    fn select_region(&self, start: i32, end: i32) {
        let mut view = self.current_view();
        view.start = start;
        view.end = end;
        self.present(view);
    }

    fn present(&self, view: View) {
        if self.inner.in_changed.get() {
            self.inner.pending.replace(Some(view));
            if !self.inner.idle_scheduled.replace(true) {
                let weak = Rc::downgrade(&self.inner);
                glib::idle_add_local_once(move || {
                    if let Some(inner) = weak.upgrade() {
                        inner.idle_scheduled.set(false);
                        let pending = inner.pending.borrow_mut().take();
                        if let Some(view) = pending {
                            Self { inner }.present(view);
                        }
                    }
                });
            }
            return;
        }
        self.inner.pending.borrow_mut().take();
        let previous = self.inner.guard.replace(true);
        let entry = &self.inner.entry;
        if entry.text().as_str() != view.text {
            entry.set_text(&view.text);
        }
        entry.set_position(view.start);
        entry.select_region(view.start, view.end);
        self.inner.guard.set(previous);
    }

    fn key_pressed(&self, key: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> Propagation {
        use gtk::gdk::{Key, ModifierType};

        let control = state.contains(ModifierType::CONTROL_MASK);
        let shift = state.contains(ModifierType::SHIFT_MASK);
        match key {
            Key::Return | Key::KP_Enter => {
                let callback = self.inner.activate.borrow().clone();
                if let Some(callback) = callback {
                    callback();
                }
            }
            Key::Up | Key::Down if !self.is_rename_mode() => {
                let callback = self.inner.move_selection.borrow().clone();
                if let Some(callback) = callback {
                    callback(if key == Key::Up { -1 } else { 1 }, shift);
                }
                self.select_region(0, -1);
            }
            Key::Tab if state.is_empty() && !self.is_rename_mode() => {
                if !self.inner.entry.text().is_empty() {
                    let callback = self.inner.tab.borrow().clone();
                    if let Some(callback) = callback {
                        callback();
                    }
                }
            }
            Key::Home | Key::End if control && !self.is_rename_mode() => {
                let callback = self.inner.nav.borrow().clone();
                if let Some(callback) = callback {
                    callback(key == Key::Home);
                }
            }
            Key::Home | Key::End if !control => {
                let position = if key == Key::Home { 0 } else { -1 };
                self.select_region(position, position);
            }
            Key::u | Key::U if control => {
                // Deliberately unguarded: this command is a user deletion.
                self.inner.last_edit_inserted.set(false);
                self.inner.entry.delete_text(0, self.inner.entry.position());
            }
            _ => return Propagation::Proceed,
        }
        Propagation::Stop
    }
}

fn completion_text(typed: &str, full_title: &str, caret_chars: usize) -> String {
    typed
        .chars()
        .take(caret_chars)
        .chain(full_title.chars().skip(caret_chars))
        .collect()
}

fn char_position(chars: usize) -> i32 {
    i32::try_from(chars).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::{char_position, completion_text};

    #[test]
    fn completion_preserves_typed_case_and_title_suffix() {
        assert_eq!(completion_text("do", "Dog Food", 2), "dog Food");
        assert_eq!(completion_text("DOignored", "Dog Food", 2), "DOg Food");
    }

    #[test]
    fn completion_offsets_are_unicode_chars() {
        assert_eq!(completion_text("ä🍎", "Ä🍎Tree", 2), "ä🍎Tree");
        assert_eq!(completion_text("i", "İstanbul", 1), "istanbul");
        assert_eq!(
            completion_text("e\u{301}", "E\u{301}clair", 2),
            "e\u{301}clair"
        );
    }

    #[test]
    fn completion_handles_empty_and_out_of_range_offsets() {
        assert_eq!(completion_text("", "Title", 0), "Title");
        assert_eq!(completion_text("dog", "Dog", 3), "dog");
        assert_eq!(completion_text("dog", "Dog", usize::MAX), "dog");
        assert_eq!(completion_text("dog", "", 2), "do");
        assert_eq!(completion_text("", "", 0), "");
    }

    #[test]
    fn gtk_positions_never_wrap_negative() {
        assert_eq!(char_position(0), 0);
        assert_eq!(char_position(42), 42);
        assert_eq!(char_position(usize::MAX), i32::MAX);
    }
}
