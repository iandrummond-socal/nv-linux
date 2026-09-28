use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};
use nv_core::config::Config;
use nv_core::library::Library;
use nv_core::storage::{NoteStore, SystemTrasher};

pub fn run() -> glib::ExitCode {
    let development = std::env::var("NV_DEV").is_ok_and(|value| value == "1");
    let (application_id, flags) = if development {
        (
            "com.parkavenueprojects.NvLinux.Devel",
            gio::ApplicationFlags::NON_UNIQUE,
        )
    } else {
        (
            "com.parkavenueprojects.NvLinux",
            gio::ApplicationFlags::empty(),
        )
    };
    let app = gtk::Application::builder()
        .application_id(application_id)
        .flags(flags)
        .build();
    // The run scope owns controllers; all signal callbacks hold only weak references.
    let controllers = Rc::new(RefCell::new(Vec::<Rc<crate::window::Controller>>::new()));
    let weak = Rc::downgrade(&controllers);
    app.connect_activate(move |app| {
        if let Some(window) = app
            .active_window()
            .or_else(|| app.windows().into_iter().next())
        {
            window.present();
            let _ = WidgetExt::activate_action(&window, "win.focus-search", None);
            return;
        }
        let Some(controllers) = weak.upgrade() else {
            return;
        };
        match load_library() {
            Ok((library, autosave_ms)) => {
                let controller = crate::window::build_window(app, library, autosave_ms);
                let window = controller.window().clone();
                controllers.borrow_mut().push(controller);
                window.present();
                let _ = WidgetExt::activate_action(&window, "win.focus-search", None);
            }
            Err(error) => {
                let message = format!("nv-linux couldn't start: {error}");
                glib::g_critical!("nv-linux", "{}", message);
                let label = gtk::Label::new(Some(&message));
                label.set_wrap(true);
                label.set_halign(gtk::Align::Center);
                label.set_valign(gtk::Align::Center);
                let window = gtk::ApplicationWindow::builder()
                    .application(app)
                    .title("nv-linux")
                    .default_width(900)
                    .default_height(700)
                    .child(&label)
                    .build();
                window.present();
            }
        }
    });
    let weak = Rc::downgrade(&controllers);
    app.connect_window_removed(move |_, window| {
        if let Some(controllers) = weak.upgrade() {
            controllers
                .borrow_mut()
                .retain(|controller| controller.window().upcast_ref::<gtk::Window>() != window);
        }
    });
    app.connect_startup(|app| {
        app.set_accels_for_action("win.clear-search", &["Escape"]);
        app.set_accels_for_action("win.focus-search", &["<Control>l"]);
        app.set_accels_for_action("win.next-note", &["<Control>j"]);
        app.set_accels_for_action("win.prev-note", &["<Control>k"]);
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        let quit = gio::SimpleAction::new("quit", None);
        let weak = app.downgrade();
        quit.connect_activate(move |_, _| {
            if let Some(app) = weak.upgrade() {
                flush_windows(&app);
                app.quit();
            }
        });
        app.add_action(&quit);
    });
    app.connect_shutdown(flush_windows);
    let exit_code = app.run();
    drop(controllers);
    exit_code
}

fn load_library() -> nv_core::Result<(Library, u64)> {
    // Propagate errors: a default fallback would bypass the configured notes directory.
    let config = Config::load()?;
    let store = NoteStore::open(
        &config.notes_dir,
        &config.extension,
        Box::new(SystemTrasher),
    )?;
    Ok((Library::new(store, config.sort), config.autosave_ms))
}

fn flush_windows(app: &gtk::Application) {
    for window in app.windows() {
        let _ = WidgetExt::activate_action(&window, "win.flush", None);
    }
}
