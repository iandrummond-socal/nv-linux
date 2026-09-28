mod app;
mod editor;
mod note_list;
mod search_field;
mod theming;
mod window;

fn main() -> gtk::glib::ExitCode {
    app::run()
}
