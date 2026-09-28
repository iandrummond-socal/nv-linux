use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gio, glib};
use nv_core::library::{Library, SaveOutcome};
use nv_core::model::NoteId;
use nv_core::Error;

use crate::editor::Editor;
use crate::note_list::{NoteList, RowData};
use crate::search_field::SearchField;

pub struct Controller {
    window: gtk::ApplicationWindow,
    search: SearchField,
    list: NoteList,
    editor: Editor,
    library: RefCell<Library>,
    current_ids: RefCell<Vec<NoteId>>,
    toast_revealer: gtk::Revealer,
    toast_label: gtk::Label,
    toast_timer: RefCell<Option<glib::SourceId>>,
}

pub fn build_window(app: &gtk::Application, library: Library, autosave_ms: u64) -> Rc<Controller> {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("nv-linux")
        .default_width(900)
        .default_height(700)
        .build();
    window.add_css_class("nv");
    let search = SearchField::new();
    let list = NoteList::new();
    let editor = Editor::new(autosave_ms);
    let entry = search.widget();
    entry.set_margin_top(8);
    entry.set_margin_bottom(8);
    entry.set_margin_start(8);
    entry.set_margin_end(8);
    let top = gtk::Box::new(gtk::Orientation::Vertical, 0);
    top.append(entry);
    top.append(list.widget());
    let paned = gtk::Paned::new(gtk::Orientation::Vertical);
    paned.add_css_class("nv");
    paned.set_position(260);
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    paned.set_start_child(Some(&top));
    paned.set_end_child(Some(editor.widget()));
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&paned));
    let toast_label = gtk::Label::new(None);
    toast_label.add_css_class("nv-banner");
    toast_label.add_css_class("nv-toast");
    toast_label.set_wrap(true);
    let toast_revealer = gtk::Revealer::new();
    toast_revealer.set_valign(gtk::Align::End);
    toast_revealer.set_halign(gtk::Align::Center);
    toast_revealer.set_margin_bottom(24);
    toast_revealer.set_transition_type(gtk::RevealerTransitionType::Crossfade);
    toast_revealer.set_child(Some(&toast_label));
    overlay.add_overlay(&toast_revealer);
    window.set_child(Some(&overlay));
    let controller = Rc::new(Controller {
        window,
        search,
        list,
        editor,
        library: RefCell::new(library),
        current_ids: RefCell::new(Vec::new()),
        toast_revealer,
        toast_label,
        toast_timer: RefCell::new(None),
    });
    let rows: Vec<_> = controller
        .library
        .borrow()
        .all()
        .into_iter()
        .map(RowData::from_note)
        .collect();
    for row in rows {
        controller.list.update_row(row);
    }
    controller.refresh_list("");
    controller.list.select(None);
    controller.editor.clear();
    controller.connect();
    controller
}

impl Controller {
    pub fn window(&self) -> &gtk::ApplicationWindow {
        &self.window
    }

    fn connect(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.search.on_query_changed(move |query, inserted| {
            if let Some(this) = weak.upgrade() {
                this.query_changed(&query, inserted);
            }
        });
        let weak = Rc::downgrade(self);
        self.list.on_selected(move |id, by_user| {
            if let Some(this) = weak.upgrade() {
                if by_user {
                    this.selected(id);
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.search.on_activate(move || {
            if let Some(this) = weak.upgrade() {
                this.activate_search();
            }
        });
        let weak = Rc::downgrade(self);
        self.search.on_move(move |delta, _| {
            if let Some(this) = weak.upgrade() {
                this.list.move_selection(delta);
            }
        });
        let weak = Rc::downgrade(self);
        self.search.on_tab(move || {
            if let Some(this) = weak.upgrade() {
                let typed = this.search.typed();
                let completion = this.completion(&typed);
                if this.list.selected().is_none() && completion.is_some() {
                    this.query_changed(&typed, true);
                } else {
                    this.editor.focus();
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.search.on_nav(move |first| {
            if let Some(this) = weak.upgrade() {
                let target = {
                    let ids = this.current_ids.borrow();
                    if first {
                        ids.first().copied()
                    } else {
                        ids.last().copied()
                    }
                };
                this.list.select(target);
                this.selected(target);
            }
        });
        let weak = Rc::downgrade(self);
        self.list.on_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.editor.focus();
            }
        });
        let weak = Rc::downgrade(self);
        self.editor.on_changed_debounced(move |id, text| {
            if let Some(this) = weak.upgrade() {
                this.persist(id, &text);
            }
        });
        self.add_action("clear-search", |this| this.clear_search());
        self.add_action("focus-search", |this| this.search.focus_select_all());
        self.add_action("next-note", |this| {
            this.list.move_selection(1);
        });
        self.add_action("prev-note", |this| {
            this.list.move_selection(-1);
        });
        self.add_action("flush", |this| this.save_pending());
        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            if let Some(this) = weak.upgrade() {
                this.save_pending();
            }
            glib::Propagation::Proceed
        });
    }

    fn add_action(self: &Rc<Self>, name: &str, callback: impl Fn(&Rc<Self>) + 'static) {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                callback(&this);
            }
        });
        self.window.add_action(&action);
    }

    fn refresh_list(&self, query: &str) {
        let ids = self.library.borrow_mut().search(query);
        self.current_ids.replace(ids.clone());
        // Widget calls can synchronously re-enter the controller: release borrows first.
        self.list.set_notes(&ids);
    }

    fn completion(&self, query: &str) -> Option<NoteId> {
        self.library
            .borrow()
            .autocomplete(query, &self.current_ids.borrow())
    }

    fn note_content(&self, id: NoteId) -> Option<(String, String, bool)> {
        self.library
            .borrow()
            .get(id)
            .map(|note| (note.title.clone(), note.body.clone(), note.read_only))
    }

    fn query_changed(self: &Rc<Self>, query: &str, inserted: bool) {
        self.refresh_list(query);
        let completion = if !query.is_empty() && inserted {
            self.completion(query)
        } else {
            None
        };
        self.save_pending();
        if let Some(id) = completion {
            if let Some((title, body, read_only)) = self.note_content(id) {
                self.list.select(Some(id));
                self.editor.show(id, &body, read_only);
                self.search.set_completion(&title, query.chars().count());
                return;
            }
        }
        self.list.select(None);
        self.editor.clear();
    }

    fn selected(self: &Rc<Self>, id: Option<NoteId>) {
        if let Some(id) = id {
            let switch = self.editor.current() != Some(id);
            if switch {
                self.save_pending();
            }
            if let Some((title, body, read_only)) = self.note_content(id) {
                if switch {
                    self.editor.show(id, &body, read_only);
                }
                self.search.show_title(&title);
            }
        } else {
            self.save_pending();
            self.editor.clear();
            self.search.restore_typed();
        }
    }

    fn activate_search(self: &Rc<Self>) {
        if let Some(id) = self.list.selected() {
            if self.editor.current() != Some(id) {
                self.save_pending();
                if let Some((_, body, read_only)) = self.note_content(id) {
                    self.editor.show(id, &body, read_only);
                }
            }
            self.editor.focus();
            return;
        }
        self.save_pending();
        let typed = self.search.typed();
        let title = typed.trim();
        let title = if title.is_empty() {
            "Untitled Note"
        } else {
            title
        };
        let result = self.library.borrow_mut().create(title, "");
        let id = match result {
            Ok(id) => id,
            Err(error) => {
                self.toast(&format!("Couldn't create the note: {error}"));
                return;
            }
        };
        let row = self.library.borrow().get(id).map(RowData::from_note);
        if let Some(row) = row {
            let actual_title = row.title.clone();
            self.list.update_row(row);
            self.refresh_list(&typed);
            self.list.select(Some(id));
            self.editor.show(id, "", false);
            self.search.show_title(&actual_title);
            self.editor.focus();
        }
    }

    fn clear_search(self: &Rc<Self>) {
        self.save_pending();
        self.search.set_text_silently("");
        self.refresh_list("");
        self.list.select(None);
        self.editor.clear();
        self.search.focus_select_all();
    }

    fn save_pending(self: &Rc<Self>) {
        if let Some((id, text)) = self.editor.flush() {
            self.persist(id, &text);
        }
    }

    fn persist(self: &Rc<Self>, id: NoteId, text: &str) {
        let result = self.library.borrow_mut().save(id, text);
        match result {
            Ok(SaveOutcome::Saved) => {
                let row = self.library.borrow().get(id).map(RowData::from_note);
                if let Some(row) = row {
                    self.list.update_row(row);
                }
            }
            Ok(SaveOutcome::Conflict { copy_id }) => {
                let (original, copy) = {
                    let library = self.library.borrow();
                    (
                        library
                            .get(id)
                            .map(|note| (RowData::from_note(note), note.body.clone())),
                        library.get(copy_id).map(RowData::from_note),
                    )
                };
                if let Some((row, disk_body)) = original {
                    self.list.update_row(row);
                    self.editor.reload(id, &disk_body);
                }
                if let Some(row) = copy {
                    let message = format!("Your edits were kept as {}", row.title);
                    self.list.update_row(row);
                    self.toast(&message);
                }
            }
            Err(Error::ExternalChange(_)) => self.editor.set_banner(Some(
                "This note's file was deleted on disk. Your text is still here.",
            )),
            Err(Error::ReadOnly(_)) => {}
            Err(error) => {
                let title = self
                    .library
                    .borrow()
                    .get(id)
                    .map(|note| note.title.clone())
                    .unwrap_or_else(|| id.to_string());
                let message = format!("Couldn't save {title}: {error}");
                self.toast(&message);
                glib::g_warning!("nv-linux", "{}", message);
            }
        }
    }

    fn toast(self: &Rc<Self>, text: &str) {
        let timer = self.toast_timer.borrow_mut().take();
        if let Some(timer) = timer {
            timer.remove();
        }
        self.toast_label.set_text(text);
        self.toast_revealer.set_reveal_child(true);
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(Duration::from_secs(4), move || {
            if let Some(this) = weak.upgrade() {
                let _ = this.toast_timer.borrow_mut().take();
                this.toast_revealer.set_reveal_child(false);
            }
        });
        let _ = self.toast_timer.replace(Some(timer));
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        if let Some(timer) = self.toast_timer.get_mut().take() {
            timer.remove();
        }
    }
}
