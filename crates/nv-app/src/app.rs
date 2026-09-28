use gtk::prelude::*;

pub fn run() -> gtk::glib::ExitCode {
    let development = std::env::var("NV_DEV").is_ok_and(|value| value == "1");
    let (application_id, flags) = if development {
        (
            "com.parkavenueprojects.NvLinux.Devel",
            gtk::gio::ApplicationFlags::NON_UNIQUE,
        )
    } else {
        (
            "com.parkavenueprojects.NvLinux",
            gtk::gio::ApplicationFlags::empty(),
        )
    };
    let app = gtk::Application::builder()
        .application_id(application_id)
        .flags(flags)
        .build();
    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            window.present();
        } else {
            crate::window::build_window(app).present();
        }
    });
    app.run()
}
