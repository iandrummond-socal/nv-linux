use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::SystemTime;

use chrono::{DateTime, Datelike, Local};
use gtk::prelude::*;
use gtk::{gio, glib};
use nv_core::model::{Note, NoteId};

/// Cached display data, independent of the note's full body.
#[derive(Debug, Clone)]
pub struct RowData {
    pub id: NoteId,
    pub title: String,
    pub preview: String,
    pub modified: SystemTime,
}

impl RowData {
    #[allow(dead_code)]
    pub fn from_note(note: &Note) -> Self {
        Self {
            id: note.id,
            title: note.title.clone(),
            preview: note
                .body
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
            modified: note.modified,
        }
    }
}

type SelectedCallback = Rc<dyn Fn(Option<NoteId>, bool)>;
type ActivateCallback = Rc<dyn Fn(NoteId)>;

#[derive(Clone)]
pub struct NoteList {
    inner: Rc<Inner>,
}

struct Inner {
    scroller: gtk::ScrolledWindow,
    list: gtk::ListView,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    cache: RefCell<HashMap<NoteId, glib::BoxedAnyObject>>,
    order: RefCell<Vec<NoteId>>,
    positions: RefCell<HashMap<NoteId, u32>>,
    guarded: Cell<bool>,
    on_selected: RefCell<Option<SelectedCallback>>,
    on_activate: RefCell<Option<ActivateCallback>>,
}

/// Restore the previous guard even when programmatic operations nest.
struct SelectionGuard<'a> {
    flag: &'a Cell<bool>,
    previous: bool,
}

impl<'a> SelectionGuard<'a> {
    fn new(flag: &'a Cell<bool>) -> Self {
        Self {
            flag,
            previous: flag.replace(true),
        }
    }
}

impl Drop for SelectionGuard<'_> {
    fn drop(&mut self) {
        self.flag.set(self.previous);
    }
}

impl Default for NoteList {
    fn default() -> Self {
        Self::new()
    }
}

impl NoteList {
    #[allow(dead_code)]
    pub fn new() -> Self {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::SingleSelection::new(Some(store.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let list = gtk::ListView::new(Some(selection.clone()), Some(row_factory()));
        list.add_css_class("nv-list");
        list.set_single_click_activate(false);
        let scroller = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build();
        let inner = Rc::new(Inner {
            scroller,
            list,
            store,
            selection,
            cache: RefCell::new(HashMap::new()),
            order: RefCell::new(Vec::new()),
            positions: RefCell::new(HashMap::new()),
            guarded: Cell::new(false),
            on_selected: RefCell::new(None),
            on_activate: RefCell::new(None),
        });
        let weak = Rc::downgrade(&inner);
        inner.selection.connect_selection_changed(move |_, _, _| {
            if let Some(inner) = weak.upgrade() {
                if !inner.guarded.get() {
                    let list = NoteList { inner };
                    list.notify_selected(list.selected());
                }
            }
        });
        let weak = Rc::downgrade(&inner);
        inner.list.connect_activate(move |_, position| {
            if let Some(inner) = weak.upgrade() {
                if inner.guarded.get() {
                    return;
                }
                let id = inner.order.borrow().get(position as usize).copied();
                let callback = inner.on_activate.borrow().clone();
                if let (Some(id), Some(callback)) = (id, callback) {
                    callback(id);
                }
            }
        });
        Self { inner }
    }

    /// Borrow the list's scrolling container for window layout.
    #[allow(dead_code)]
    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.inner.scroller
    }

    #[allow(dead_code)]
    pub fn set_notes(&self, ids: &[NoteId]) {
        let selected = self.selected();
        let mut order = Vec::with_capacity(ids.len());
        let mut objects = Vec::with_capacity(ids.len());
        let mut positions = HashMap::with_capacity(ids.len());
        {
            let cache = self.inner.cache.borrow();
            for &id in ids {
                if positions.contains_key(&id) {
                    continue;
                }
                if let Some(object) = cache.get(&id) {
                    positions.insert(id, order.len() as u32);
                    order.push(id);
                    objects.push(object.clone());
                } else {
                    glib::g_warning!("nv-linux", "No cached row for note {id}");
                }
            }
        }
        let _guard = SelectionGuard::new(&self.inner.guarded);
        *self.inner.order.borrow_mut() = order;
        *self.inner.positions.borrow_mut() = positions;
        self.inner
            .store
            .splice(0, self.inner.store.n_items(), &objects);
        self.restore_selection(selected);
    }

    #[allow(dead_code)]
    pub fn update_row(&self, row: RowData) {
        let id = row.id;
        // A fresh object forces a rebind instead of GTK reusing the unchanged tile.
        let object = glib::BoxedAnyObject::new(row);
        self.inner.cache.borrow_mut().insert(id, object.clone());
        let position = self.inner.positions.borrow().get(&id).copied();
        if let Some(position) = position {
            let selected = self.selected();
            let _guard = SelectionGuard::new(&self.inner.guarded);
            self.inner.store.splice(position, 1, &[object]);
            self.restore_selection(selected);
        }
    }

    #[allow(dead_code)]
    pub fn remove_row(&self, id: NoteId) {
        let selected = self.selected();
        self.inner.cache.borrow_mut().remove(&id);
        let position = self.inner.positions.borrow().get(&id).copied();
        if let Some(position) = position {
            let _guard = SelectionGuard::new(&self.inner.guarded);
            {
                let mut order = self.inner.order.borrow_mut();
                order.remove(position as usize);
                *self.inner.positions.borrow_mut() = order
                    .iter()
                    .enumerate()
                    .map(|(position, &id)| (id, position as u32))
                    .collect();
            }
            self.inner.store.remove(position);
            self.restore_selection(selected);
        }
    }

    #[allow(dead_code)]
    pub fn select(&self, id: Option<NoteId>) {
        let _guard = SelectionGuard::new(&self.inner.guarded);
        self.restore_selection(id);
        self.scroll_to_selection();
    }

    #[allow(dead_code)]
    pub fn selected(&self) -> Option<NoteId> {
        self.inner
            .order
            .borrow()
            .get(self.inner.selection.selected() as usize)
            .copied()
    }

    /// Search-field navigation is a user action, reported exactly once.
    #[allow(dead_code)]
    pub fn move_selection(&self, delta: i32) -> Option<NoteId> {
        let id = {
            let order = self.inner.order.borrow();
            let position = self.inner.selection.selected();
            let next = if position == gtk::INVALID_LIST_POSITION {
                if delta > 0 {
                    0
                } else if delta < 0 {
                    order.len() as i64 - 1
                } else {
                    -1
                }
            } else {
                (i64::from(position) + i64::from(delta)).min(order.len() as i64 - 1)
            };
            usize::try_from(next)
                .ok()
                .and_then(|index| order.get(index))
                .copied()
        };
        self.select(id);
        self.notify_selected(id);
        id
    }

    #[allow(dead_code)]
    pub fn on_selected(&self, callback: impl Fn(Option<NoteId>, bool) + 'static) {
        *self.inner.on_selected.borrow_mut() = Some(Rc::new(callback));
    }

    #[allow(dead_code)]
    pub fn on_activate(&self, callback: impl Fn(NoteId) + 'static) {
        *self.inner.on_activate.borrow_mut() = Some(Rc::new(callback));
    }

    fn restore_selection(&self, id: Option<NoteId>) {
        let position = id
            .and_then(|id| self.inner.positions.borrow().get(&id).copied())
            .unwrap_or(gtk::INVALID_LIST_POSITION);
        self.inner.selection.set_selected(position);
    }

    fn scroll_to_selection(&self) {
        let position = self.inner.selection.selected();
        if position != gtk::INVALID_LIST_POSITION {
            self.inner
                .list
                .scroll_to(position, gtk::ListScrollFlags::NONE, None);
        }
    }

    fn notify_selected(&self, id: Option<NoteId>) {
        let callback = self.inner.on_selected.borrow().clone();
        if let Some(callback) = callback {
            callback(id, true);
        }
    }
}

fn row_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.set_margin_start(12);
        row.set_margin_end(12);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        let title = row_label("nv-title");
        let attributes = gtk::pango::AttrList::new();
        attributes.insert(gtk::pango::AttrInt::new_weight(gtk::pango::Weight::Bold));
        title.set_attributes(Some(&attributes));
        let preview = row_label("nv-preview");
        text.append(&title);
        text.append(&preview);
        let date = gtk::Label::new(None);
        date.add_css_class("nv-date");
        date.set_valign(gtk::Align::Center);
        row.append(&text);
        row.append(&date);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(object) = item.item().and_downcast::<glib::BoxedAnyObject>() else {
            return;
        };
        let Some(row) = item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let Some(text) = row.first_child().and_downcast::<gtk::Box>() else {
            return;
        };
        let Some(title) = text.first_child().and_downcast::<gtk::Label>() else {
            return;
        };
        let Some(preview) = text.last_child().and_downcast::<gtk::Label>() else {
            return;
        };
        let Some(date) = row.last_child().and_downcast::<gtk::Label>() else {
            return;
        };
        let data = object.borrow::<RowData>();
        title.set_text(&data.title);
        preview.set_text(&data.preview);
        date.set_text(&relative_date(data.modified, Local::now()));
    });
    factory
}

fn row_label(class: &str) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.add_css_class(class);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_single_line_mode(true);
    label
}

fn relative_date(modified: SystemTime, now: DateTime<Local>) -> String {
    let modified: DateTime<Local> = modified.into();
    let age = now.signed_duration_since(modified);
    if age.num_seconds() < 60 {
        "now".to_owned()
    } else if age.num_seconds() < 3600 {
        format!("{}m", age.num_minutes())
    } else if modified.date_naive() == now.date_naive() {
        modified.format("%H:%M").to_string()
    } else if Some(modified.date_naive()) == now.date_naive().pred_opt() {
        "Yesterday".to_owned()
    } else if modified.year() == now.year() {
        modified.format("%b %-d").to_string()
    } else {
        modified.format("%Y-%m-%d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn local(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn relative_date_thresholds() {
        let now = local(2026, 9, 28, 12, 0);
        for (seconds, expected) in [
            (-60, "now"),
            (0, "now"),
            (59, "now"),
            (60, "1m"),
            (300, "5m"),
            (3599, "59m"),
            (3600, "11:00"),
        ] {
            let modified = now - Duration::seconds(seconds);
            assert_eq!(relative_date(modified.into(), now), expected);
        }
    }

    #[test]
    fn relative_date_calendar_boundaries() {
        let now = local(2026, 9, 28, 12, 0);
        for (modified, expected) in [
            (local(2026, 9, 28, 8, 5), "08:05"),
            (local(2026, 9, 27, 12, 0), "Yesterday"),
            (local(2026, 9, 12, 12, 0), "Sep 12"),
            (local(2026, 9, 2, 12, 0), "Sep 2"),
            (local(2025, 1, 3, 12, 0), "2025-01-03"),
        ] {
            assert_eq!(relative_date(modified.into(), now), expected);
        }
        assert_eq!(
            relative_date(local(2025, 12, 31, 12, 0).into(), local(2026, 1, 1, 12, 0)),
            "Yesterday"
        );
        assert_eq!(
            relative_date(local(2026, 9, 27, 23, 50).into(), local(2026, 9, 28, 0, 10)),
            "20m"
        );
    }

    #[test]
    fn preview_skips_blank_lines_and_limits_unicode_characters() {
        let mut note = Note {
            id: 7,
            title: "Title".to_owned(),
            body: format!("\n  \n  {}  \nignored", "é".repeat(201)),
            path: Default::default(),
            modified: SystemTime::UNIX_EPOCH,
            labels: String::new(),
            read_only: false,
        };
        let row = RowData::from_note(&note);
        assert_eq!(row.id, note.id);
        assert_eq!(row.title, note.title);
        assert_eq!(row.modified, note.modified);
        assert_eq!(row.preview, "é".repeat(200));
        note.body = "\n \t\n".to_owned();
        assert!(RowData::from_note(&note).preview.is_empty());
    }
}
