use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use gtk::gio::prelude::*;
use gtk::{gio, glib};
use nv_core::config::Config;
use nv_core::theme::Palette;

const STRUCTURE_CSS: &str = "
.nv-banner { padding: 6px 16px; }
.nv-toast { padding: 8px 16px; border-radius: 8px; }
";

struct ThemeState {
    _provider: gtk::CssProvider,
    _monitor: Option<gio::FileMonitor>,
    _pending: Rc<RefCell<Option<glib::SourceId>>>,
}

thread_local! {
    static THEME: RefCell<Option<ThemeState>> = const { RefCell::new(None) };
}

pub fn install(config: &Config) {
    THEME.with(|state| {
        if state.borrow().is_some() {
            return;
        }
        let Some(display) = gtk::gdk::Display::default() else {
            glib::g_warning!("nv-linux", "Cannot install theme: no default display");
            return;
        };
        let theme_dir = config.theme_dir();
        let provider = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        let palette = match Palette::load(&theme_dir) {
            Ok(palette) => palette,
            Err(error) => {
                glib::g_warning!(
                    "nv-linux",
                    "Cannot load theme; using built-in palette: {error}"
                );
                Palette::default()
            }
        };
        provider.load_from_string(&(palette.to_css() + STRUCTURE_CSS));

        let pending = Rc::new(RefCell::new(None::<glib::SourceId>));
        // Omarchy replaces the entire theme folder, so watch its parent directory.
        let monitor = match gio::File::for_path(&theme_dir)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        {
            Ok(monitor) => {
                let provider = provider.clone();
                let pending = Rc::clone(&pending);
                monitor.connect_changed(move |_, file, other, event| {
                    if !matches!(
                        event,
                        gio::FileMonitorEvent::Changed
                            | gio::FileMonitorEvent::ChangesDoneHint
                            | gio::FileMonitorEvent::Created
                            | gio::FileMonitorEvent::MovedIn
                            | gio::FileMonitorEvent::Renamed
                            | gio::FileMonitorEvent::Deleted
                            | gio::FileMonitorEvent::MovedOut
                    ) {
                        return;
                    }
                    let relevant = is_theme_entry(file)
                        || (matches!(
                            event,
                            gio::FileMonitorEvent::Renamed | gio::FileMonitorEvent::MovedIn
                        ) && other.is_some_and(is_theme_entry));
                    if !relevant {
                        return;
                    }
                    if let Some(source) = pending.borrow_mut().take() {
                        source.remove();
                    }
                    let provider = provider.clone();
                    let theme_dir = theme_dir.clone();
                    let callback_pending = Rc::clone(&pending);
                    let source =
                        glib::timeout_add_local_once(Duration::from_millis(150), move || {
                            let _ = callback_pending.borrow_mut().take();
                            match Palette::load(&theme_dir) {
                                Ok(palette) => {
                                    provider.load_from_string(&(palette.to_css() + STRUCTURE_CSS));
                                    glib::g_debug!(
                                        "nv-linux",
                                        "Reloaded theme from {}",
                                        theme_dir.display()
                                    );
                                }
                                Err(error) => {
                                    // A theme switch may be incomplete; preserve the last good palette.
                                    glib::g_warning!(
                                        "nv-linux",
                                        "Cannot reload theme; keeping current palette: {error}"
                                    );
                                }
                            }
                        });
                    *pending.borrow_mut() = Some(source);
                });
                Some(monitor)
            }
            Err(error) => {
                glib::g_warning!("nv-linux", "Cannot monitor theme directory: {error}");
                None
            }
        };
        *state.borrow_mut() = Some(ThemeState {
            _provider: provider,
            _monitor: monitor,
            _pending: pending,
        });
    });
}

fn is_theme_entry(file: &gio::File) -> bool {
    file.basename()
        .is_some_and(|name| name == Path::new("theme.name") || name == Path::new("theme"))
}
