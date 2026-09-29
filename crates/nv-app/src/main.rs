mod app;
mod editor;
mod note_list;
mod search_field;
mod sync_cli;
mod sync_daemon;
mod sync_ui;
mod theming;
mod window;

fn main() -> gtk::glib::ExitCode {
    // `nv-linux sync …` is handled before GTK sees argv: GTK rejects unknown arguments.
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|first| first == "sync") {
        let rest: Vec<String> = args.map(|arg| arg.to_string_lossy().into_owned()).collect();
        std::process::exit(sync_cli::run_from_env(&rest));
    }
    app::run()
}
