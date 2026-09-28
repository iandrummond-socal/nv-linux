use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gio, glib};
use nv_core::config::Config;
use nv_core::library::{Change, Library, SaveOutcome};
use nv_core::model::NoteId;
use nv_core::watcher::{FolderWatcher, StoreEvent};
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
    watcher: Option<FolderWatcher>,
    renaming: Cell<Option<NoteId>>,
    toast_revealer: gtk::Revealer,
    toast_label: gtk::Label,
    toast_timer: RefCell<Option<glib::SourceId>>,
}

pub fn build_window(app: &gtk::Application, library: Library, config: &Config) -> Rc<Controller> {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("nv-linux")
        .default_width(900)
        .default_height(700)
        .build();
    window.add_css_class("nv");
    let search = SearchField::new();
    let list = NoteList::new();
    let editor = Editor::new(config.autosave_ms);
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
    let (sender, receiver) = std::sync::mpsc::channel();
    let (async_sender, async_receiver) = async_channel::unbounded();
    let watch_result = FolderWatcher::start(&config.notes_dir, sender).and_then(|watcher| {
        let _bridge = std::thread::Builder::new()
            .name("nv-watch-bridge".into())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    if async_sender.send_blocking(event).is_err() {
                        break;
                    }
                }
            })?;
        Ok(watcher)
    });
    let (watcher, watch_error) = match watch_result {
        Ok(watcher) => (Some(watcher), None),
        Err(error) => (None, Some(format!("Live updates are off: {error}"))),
    };
    let controller = Rc::new(Controller {
        window,
        search,
        list,
        editor,
        library: RefCell::new(library),
        current_ids: RefCell::new(Vec::new()),
        watcher,
        renaming: Cell::new(None),
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
    if let Some(message) = watch_error {
        glib::g_warning!("nv-linux", "{}", message);
        controller.toast(&message);
    }
    let weak = Rc::downgrade(&controller);
    glib::spawn_future_local(async move {
        while let Ok(event) = async_receiver.recv().await {
            let Some(this) = weak.upgrade() else {
                break;
            };
            this.apply_event(event);
        }
    });
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
        self.add_action("restore", |this| this.restore());
        self.add_action("rename", |this| this.begin_rename());
        self.add_action("trash", |this| this.trash());
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

    fn requery(&self) {
        self.refresh_list(&self.search.typed());
    }

    fn update_row(&self, id: NoteId) {
        let row = self.library.borrow().get(id).map(RowData::from_note);
        if let Some(row) = row {
            self.list.update_row(row);
        }
    }

    fn expect_write(&self, id: NoteId) {
        let write = {
            let library = self.library.borrow();
            library.get(id).and_then(|note| {
                library
                    .fingerprint(id)
                    .map(|hash| (note.path.clone(), hash))
            })
        };
        if let (Some(watcher), Some((path, hash))) = (&self.watcher, write) {
            watcher.expect_write(&path, hash);
        }
    }

    fn apply_event(self: &Rc<Self>, event: StoreEvent) {
        // Save before reloading so pending edits take the conflict-copy path.
        self.save_pending();
        let change = self.library.borrow_mut().apply_external(event);
        match change {
            Change::Updated(id) | Change::Renamed(id) => {
                self.update_row(id);
                if let Some((title, body, read_only)) = self.note_content(id) {
                    self.editor.reload(id, &body);
                    if self.editor.current() == Some(id) && !read_only {
                        self.editor.set_banner(None);
                    }
                    if self.list.selected() == Some(id) && !self.search.is_rename_mode() {
                        self.search.show_title(&title);
                    }
                }
                self.requery();
            }
            Change::Added(id) => {
                self.update_row(id);
                self.requery();
            }
            Change::Removed(id) => {
                if self.renaming.get() == Some(id) {
                    self.cancel_rename();
                }
                if self.editor.current() == Some(id) {
                    self.editor
                        .set_banner(Some("Deleted on disk — Ctrl+S restores it"));
                } else {
                    self.library.borrow_mut().forget(id);
                    self.editor.forget(id);
                    self.list.remove_row(id);
                    self.requery();
                }
            }
            Change::None => {}
        }
    }

    fn restore(self: &Rc<Self>) {
        let Some(id) = self.editor.current() else {
            return;
        };
        if !self.library.borrow().is_deleted_on_disk(id) {
            return;
        }
        let text = self
            .editor
            .text_of(id)
            .or_else(|| self.note_content(id).map(|(_, body, _)| body));
        let Some(text) = text else {
            return;
        };
        let result = self.library.borrow_mut().restore(id, &text);
        match result {
            Ok(new_id) => {
                self.expect_write(new_id);
                if self.renaming.get() == Some(id) {
                    self.cancel_rename();
                }
                self.editor.forget(id);
                self.list.remove_row(id);
                self.update_row(new_id);
                self.requery();
                self.list.select(Some(new_id));
                self.editor.show(new_id, &text, false);
                self.editor.set_banner(None);
                if let Some((title, _, _)) = self.note_content(new_id) {
                    self.toast(&format!("Restored {title}"));
                }
            }
            Err(error) => self.toast(&format!("Couldn't restore: {error}")),
        }
    }

    fn begin_rename(self: &Rc<Self>) {
        let Some(id) = self.list.selected() else {
            return;
        };
        self.save_pending();
        if let Some((title, _, _)) = self.note_content(id) {
            self.renaming.set(Some(id));
            self.search.set_rename_mode(true);
            self.search.set_text_silently(&title);
            self.search.focus_select_all();
        }
    }

    fn cancel_rename(&self) {
        self.renaming.set(None);
        self.search.set_rename_mode(false);
        self.search.focus_select_all();
    }

    fn finish_rename(self: &Rc<Self>) {
        let Some(id) = self.renaming.get() else {
            self.cancel_rename();
            return;
        };
        let text = self.search.text();
        let title = text.trim();
        let unchanged = self
            .note_content(id)
            .is_some_and(|(old, _, _)| old == title);
        if title.is_empty() || unchanged {
            self.cancel_rename();
            return;
        }
        let result = self.library.borrow_mut().rename(id, title);
        match result {
            Ok(()) => {
                self.cancel_rename();
                self.update_row(id);
                // Rename keeps the row visible; the next search keystroke re-filters.
                self.list.select(Some(id));
                if let Some((title, _, _)) = self.note_content(id) {
                    self.search.show_title(&title);
                }
            }
            Err(error) => self.toast(&format!("Couldn't rename: {error}")),
        }
    }

    fn select_by_user(self: &Rc<Self>, id: Option<NoteId>) {
        self.list.select(id);
        self.selected(id);
    }

    fn trash(self: &Rc<Self>) {
        let Some(id) = self.list.selected().or_else(|| self.editor.current()) else {
            return;
        };
        self.save_pending();
        let next = {
            let ids = self.current_ids.borrow();
            ids.iter()
                .position(|&candidate| candidate == id)
                .and_then(|index| {
                    ids.get(index + 1)
                        .or_else(|| index.checked_sub(1).and_then(|previous| ids.get(previous)))
                        .copied()
                })
        };
        let result = self.library.borrow_mut().trash(id);
        match result {
            Ok(()) => {
                if self.renaming.get() == Some(id) {
                    self.cancel_rename();
                }
                self.editor.forget(id);
                self.list.remove_row(id);
                self.current_ids
                    .borrow_mut()
                    .retain(|&candidate| candidate != id);
                self.select_by_user(next);
                self.toast("Moved to Trash");
            }
            Err(error) => self.toast(&format!("Couldn't move to Trash: {error}")),
        }
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
        if self.search.is_rename_mode() && self.renaming.get() != id {
            self.cancel_rename();
        }
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
        if self.search.is_rename_mode() {
            self.finish_rename();
            return;
        }
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
        self.expect_write(id);
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
        // Escape cancels the temporary rename view without clearing the saved query.
        if self.search.is_rename_mode() {
            self.cancel_rename();
            return;
        }
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
                self.expect_write(id);
                let row = self.library.borrow().get(id).map(RowData::from_note);
                if let Some(row) = row {
                    self.list.update_row(row);
                }
            }
            Ok(SaveOutcome::Conflict { copy_id }) => {
                self.expect_write(id);
                self.expect_write(copy_id);
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
                self.requery();
            }
            Err(Error::ExternalChange(_)) => self
                .editor
                .set_banner(Some("Deleted on disk — Ctrl+S restores it")),
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
