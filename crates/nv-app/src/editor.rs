use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use nv_core::model::NoteId;

type ChangedCallback = Rc<dyn Fn(NoteId, String)>;

#[derive(Clone)]
pub struct Editor {
    inner: Rc<Inner>,
}

struct Inner {
    container: gtk::Box,
    banner: gtk::Label,
    text_view: gtk::TextView,
    blank: gtk::TextBuffer,
    buffers: RefCell<HashMap<NoteId, (gtk::TextBuffer, glib::SignalHandlerId)>>,
    current: Cell<Option<NoteId>>,
    programmatic: Cell<bool>,
    autosave_ms: u64,
    timer: RefCell<Option<glib::SourceId>>,
    pending: RefCell<Option<NoteId>>,
    changed: RefCell<Option<ChangedCallback>>,
}

impl Editor {
    /// Construct an editor with the autosave debounce interval in milliseconds.
    #[allow(dead_code)]
    pub fn new(autosave_ms: u64) -> Self {
        let banner = gtk::Label::new(None);
        banner.add_css_class("nv-banner");
        banner.set_xalign(0.0);
        banner.set_wrap(true);
        banner.set_visible(false);

        let blank = gtk::TextBuffer::new(None);
        let text_view = gtk::TextView::with_buffer(&blank);
        text_view.add_css_class("nv-editor");
        text_view.set_wrap_mode(gtk::WrapMode::WordChar);
        text_view.set_monospace(true);
        text_view.set_left_margin(16);
        text_view.set_right_margin(16);
        text_view.set_top_margin(16);
        text_view.set_bottom_margin(16);
        text_view.set_editable(false);
        text_view.set_cursor_visible(false);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_vexpand(true);
        scroller.set_child(Some(&text_view));
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.append(&banner);
        container.append(&scroller);

        Self {
            inner: Rc::new(Inner {
                container,
                banner,
                text_view,
                blank,
                buffers: RefCell::new(HashMap::new()),
                current: Cell::new(None),
                programmatic: Cell::new(false),
                autosave_ms,
                timer: RefCell::new(None),
                pending: RefCell::new(None),
                changed: RefCell::new(None),
            }),
        }
    }

    /// Borrow the container holding the banner and scrolling text view.
    #[allow(dead_code)]
    pub fn widget(&self) -> &gtk::Box {
        &self.inner.container
    }

    /// The caller must flush unsaved edits before switching or replacing a note.
    #[allow(dead_code)]
    pub fn show(&self, id: NoteId, body: &str, read_only: bool) {
        let buffer = self.buffer_for(id);
        self.reload(id, body);
        let previous = self.inner.programmatic.replace(true);
        buffer.place_cursor(&buffer.start_iter());
        self.inner.current.set(Some(id));
        self.inner.text_view.set_buffer(Some(&buffer));
        self.inner.text_view.set_editable(!read_only);
        self.inner.text_view.set_cursor_visible(!read_only);
        self.inner.programmatic.set(previous);
        self.set_banner(
            read_only.then_some("Read-only: this file isn't valid UTF-8, so it's never rewritten."),
        );
    }

    /// Detach the current note without flushing; the caller flushes first.
    #[allow(dead_code)]
    pub fn clear(&self) {
        let previous = self.inner.programmatic.replace(true);
        self.inner.current.set(None);
        self.inner.text_view.set_buffer(Some(&self.inner.blank));
        self.inner.text_view.set_editable(false);
        self.inner.text_view.set_cursor_visible(false);
        self.inner.programmatic.set(previous);
        self.set_banner(None);
    }

    #[allow(dead_code)]
    pub fn focus(&self) {
        self.inner.text_view.grab_focus();
    }

    #[allow(dead_code)]
    pub fn on_changed_debounced(&self, callback: impl Fn(NoteId, String) + 'static) {
        *self.inner.changed.borrow_mut() = Some(Rc::new(callback));
    }

    /// Consume the pending change without invoking the autosave callback.
    #[allow(dead_code)]
    pub fn flush(&self) -> Option<(NoteId, String)> {
        self.inner.cancel_timer();
        self.inner.take_pending()
    }

    #[allow(dead_code)]
    pub fn set_banner(&self, banner: Option<&str>) {
        self.inner.banner.set_text(banner.unwrap_or_default());
        self.inner.banner.set_visible(banner.is_some());
    }

    #[allow(dead_code)]
    pub fn current(&self) -> Option<NoteId> {
        self.inner.current.get()
    }

    #[allow(dead_code)]
    pub fn text_of(&self, id: NoteId) -> Option<String> {
        self.inner.text_of(id)
    }

    /// Discard the cached note and any pending autosave for it.
    #[allow(dead_code)]
    pub fn forget(&self, id: NoteId) {
        self.inner.discard_pending(id);
        if self.current() == Some(id) {
            self.clear();
        }
        let removed = self.inner.buffers.borrow_mut().remove(&id);
        if let Some((buffer, handler)) = removed {
            buffer.disconnect(handler);
        }
    }

    /// Replace cached text silently without switching notes. Unknown ids are ignored.
    /// Flush local edits first; a replacement supersedes pending edits for this id.
    #[allow(dead_code)]
    pub fn reload(&self, id: NoteId, body: &str) {
        let buffers = self.inner.buffers.borrow();
        let Some((buffer, handler)) = buffers.get(&id) else {
            return;
        };
        if buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), true)
            .as_str()
            == body
        {
            return;
        }
        self.inner.discard_pending(id);
        let previous = self.inner.programmatic.replace(true);
        buffer.block_signal(handler);
        buffer.begin_irreversible_action();
        buffer.set_text(body);
        buffer.place_cursor(&buffer.start_iter());
        buffer.end_irreversible_action();
        buffer.unblock_signal(handler);
        self.inner.programmatic.set(previous);
    }

    fn buffer_for(&self, id: NoteId) -> gtk::TextBuffer {
        let mut buffers = self.inner.buffers.borrow_mut();
        let (buffer, _) = buffers.entry(id).or_insert_with(|| {
            let buffer = gtk::TextBuffer::new(None);
            buffer.set_enable_undo(true);
            let weak = Rc::downgrade(&self.inner);
            let handler = buffer.connect_changed(move |_| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if inner.programmatic.get() {
                    return;
                }
                inner.cancel_timer();
                *inner.pending.borrow_mut() = Some(id);
                let weak = Rc::downgrade(&inner);
                let timer = glib::timeout_add_local_once(
                    Duration::from_millis(inner.autosave_ms),
                    move || {
                        let Some(inner) = weak.upgrade() else {
                            return;
                        };
                        // This source is firing; do not remove it via flush().
                        let _ = inner.timer.borrow_mut().take();
                        let pending = inner.take_pending();
                        let callback = inner.changed.borrow().clone();
                        if let (Some((id, text)), Some(callback)) = (pending, callback) {
                            callback(id, text);
                        }
                    },
                );
                *inner.timer.borrow_mut() = Some(timer);
            });
            (buffer, handler)
        });
        buffer.clone()
    }
}

impl Inner {
    fn text_of(&self, id: NoteId) -> Option<String> {
        self.buffers.borrow().get(&id).map(|(buffer, _)| {
            buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .to_string()
        })
    }

    fn cancel_timer(&self) {
        let timer = self.timer.borrow_mut().take();
        if let Some(timer) = timer {
            timer.remove();
        }
    }

    fn take_pending(&self) -> Option<(NoteId, String)> {
        let id = self.pending.borrow_mut().take()?;
        self.text_of(id).map(|text| (id, text))
    }

    fn discard_pending(&self, id: NoteId) {
        if *self.pending.borrow() == Some(id) {
            self.cancel_timer();
            let _ = self.pending.borrow_mut().take();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.get_mut().take() {
            timer.remove();
        }
        for (_, (buffer, handler)) in self.buffers.get_mut().drain() {
            buffer.disconnect(handler);
        }
    }
}
