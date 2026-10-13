mod diff;
mod file_view;
mod git;
mod tree;
mod window;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

const CSS: &str = r#"
.diff-content { padding-bottom: 300px; }
.file-section { margin: 0 0 18px 0; }
.file-header {
    border-radius: 0;
    padding: 6px 12px;
    background: alpha(currentColor, 0.07);
    border-top: 1px solid alpha(currentColor, 0.15);
    border-bottom: 1px solid alpha(currentColor, 0.15);
}
.file-header:hover { background: alpha(currentColor, 0.11); }
.file-path { font-weight: bold; }
.status {
    font-family: monospace;
    font-weight: bold;
    min-width: 1.4em;
    border-radius: 4px;
    padding: 0 4px;
}
.status-added { color: @success_color; background: alpha(@success_color, 0.15); }
.status-deleted { color: @error_color; background: alpha(@error_color, 0.15); }
.status-modified { color: @accent_color; background: alpha(@accent_color, 0.15); }
.status-renamed { color: @warning_color; background: alpha(@warning_color, 0.15); }
.status-conflict { color: @error_color; background: alpha(@warning_color, 0.25); }
.stat-add { color: @success_color; font-family: monospace; }
.stat-del { color: @error_color; font-family: monospace; }
.placeholder { padding: 14px; }
.linkmap { background: alpha(currentColor, 0.03); }
"#;

fn main() -> glib::ExitCode {
    let opts = match git::Options::parse(std::env::args().skip(1)) {
        Ok(opts) => opts,
        Err(err) => {
            if !err.is_empty() {
                eprintln!("twain: {err}\n");
            }
            eprintln!("{}", git::USAGE);
            return if err.is_empty() {
                glib::ExitCode::SUCCESS
            } else {
                glib::ExitCode::FAILURE
            };
        }
    };

    let app = adw::Application::builder()
        .application_id("io.github.twain")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|app| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("no display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        app.set_accels_for_action("win.find", &["<Ctrl>f"]);
        app.set_accels_for_action("win.find-next", &["<Ctrl>g", "F3"]);
        app.set_accels_for_action("win.find-prev", &["<Ctrl><Shift>g", "<Shift>F3"]);
        app.set_accels_for_action("win.reload", &["F5", "<Ctrl>r"]);
        app.set_accels_for_action("window.close", &["<Ctrl>w", "<Ctrl>q"]);
    });
    app.connect_activate(move |app| {
        let window = window::Window::new(app, opts.clone());
        window.present();
        window.load();
    });
    // Arguments were parsed above; don't let GApplication interpret them.
    app.run_with_args::<&str>(&[])
}
