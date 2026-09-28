pub fn build_window(app: &gtk::Application) -> gtk::ApplicationWindow {
    gtk::ApplicationWindow::builder()
        .application(app)
        .title("nv-linux")
        .default_width(900)
        .default_height(700)
        .build()
}
