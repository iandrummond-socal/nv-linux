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

/// Everything the app loads for one palette. Both the first install and each
/// live Omarchy reload go through here, so every class follows theme switches.
fn full_css(palette: &Palette) -> String {
    palette.to_css() + STRUCTURE_CSS + &sync_css(palette)
}

/// A theme colour, or the built-in one when it isn't a plain `#hex` value
/// (mirrors the fallback `Palette::to_css` applies to its own rules).
fn color<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    let hex = value.strip_prefix('#').unwrap_or("");
    if matches!(hex.len(), 3 | 4 | 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        value
    } else {
        fallback
    }
}

/// The Dropbox sync indicator (`sync_ui::SyncIndicator`): small, bottom-right,
/// on a translucent panel of the theme's own background so editor text under
/// it stays legible; tone classes pick the text colour.
fn sync_css(palette: &Palette) -> String {
    let defaults = Palette::default();
    let panel = color(&palette.lighter_background, &defaults.lighter_background);
    let quiet = color(&palette.dark_foreground, &defaults.dark_foreground);
    let busy = color(&palette.accent, &defaults.accent);
    let warn = color(&palette.red, &defaults.red);
    let edge = color(&palette.muted, &defaults.muted);
    format!(
        "
.nv-sync {{
    background-color: alpha({panel}, 0.88);
    border: 1px solid alpha({edge}, 0.6);
    border-radius: 6px;
    padding: 1px 8px;
    font-size: smaller;
    color: {quiet};
}}
.nv-sync.nv-sync-quiet {{ color: {quiet}; }}
.nv-sync.nv-sync-busy {{ color: {busy}; }}
.nv-sync.nv-sync-warn {{ color: {warn}; }}
"
    )
}

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
        provider.load_from_string(&full_css(&palette));

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
                                    provider.load_from_string(&full_css(&palette));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn palette(accent: &str, red: &str) -> Palette {
        Palette {
            accent: accent.into(),
            red: red.into(),
            lighter_background: "#241a0f".into(),
            dark_foreground: "#8a5c1c".into(),
            muted: "#6b4a14".into(),
            ..Palette::default()
        }
    }

    #[test]
    fn sync_ui_css_has_the_indicator_in_theme_colours() {
        let css = full_css(&palette("#ff8c00", "#ff4d1a"));
        for class in [
            ".nv-sync {",
            ".nv-sync-quiet",
            ".nv-sync-busy",
            ".nv-sync-warn",
        ] {
            assert!(css.contains(class), "{class} missing");
        }
        for colour in [
            "alpha(#241a0f, 0.88)",
            "#8a5c1c",
            "#ff8c00",
            "#ff4d1a",
            "#6b4a14",
        ] {
            assert!(css.contains(colour), "{colour} missing");
        }
        // The banner and structure rules are still there.
        assert!(css.contains(".nv-banner"));
        assert!(css.contains(".nv-toast"));
    }

    #[test]
    fn sync_ui_css_follows_a_theme_switch() {
        let before = full_css(&palette("#ff8c00", "#ff4d1a"));
        let after = full_css(&palette("#7aa2f7", "#f7768e"));
        assert!(after.contains(".nv-sync-busy { color: #7aa2f7; }"));
        assert!(after.contains(".nv-sync-warn { color: #f7768e; }"));
        assert!(!after.contains("#ff8c00"));
        assert_ne!(before, after);
    }

    #[test]
    fn sync_ui_css_never_emits_a_bad_colour() {
        let css = sync_css(&palette("red; } * { color: blue", "nope"));
        let defaults = Palette::default();
        assert!(!css.contains("blue"));
        assert!(css.contains(&format!(".nv-sync-busy {{ color: {}; }}", defaults.accent)));
        assert!(css.contains(&format!(".nv-sync-warn {{ color: {}; }}", defaults.red)));
        assert_eq!(color("#abc", "#000"), "#abc");
        assert_eq!(color("#12345678", "#000"), "#12345678");
        assert_eq!(color("#12345", "#000"), "#000");
        assert_eq!(color("123456", "#000"), "#000");
    }
}
