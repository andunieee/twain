mod cli;
mod comment_card;
mod comments;
mod diff;
mod file_view;
mod git;
mod syntax;
mod tree;
mod window;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

const CSS: &str = r#"
.diff-content { padding-bottom: 30px; }
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
    border: 1px solid transparent;
}
.status.staged { border-color: currentColor; }
.status.partly-staged { border-style: dashed; border-color: currentColor; }
.status-added { color: @success_color; background: alpha(@success_color, 0.15); }
.status-deleted { color: @error_color; background: alpha(@error_color, 0.15); }
.status-modified { color: @accent_color; background: alpha(@accent_color, 0.15); }
.status-renamed { color: @warning_color; background: alpha(@warning_color, 0.15); }
.status-conflict { color: @error_color; background: alpha(@warning_color, 0.25); }
.navigation-sidebar .unchanged { opacity: 0.5; }
.stat-add { color: @success_color; font-family: monospace; }
.stat-del { color: @error_color; font-family: monospace; }
.placeholder { padding: 14px; }
.linkmap { background: alpha(currentColor, 0.03); }
.filemap { background: alpha(currentColor, 0.05); }
.comment-card {
    padding: 8px 10px;
    border-radius: 8px;
    background: @view_bg_color;
    border: 1px solid alpha(#f5b021, 0.7);
    box-shadow: 0 1px 4px alpha(black, 0.15);
}
.comment-card.resolved { border-color: alpha(currentColor, 0.2); }
.comment-card.resolved .comment-body { opacity: 0.6; }
.comment-card button.circular { min-width: 24px; min-height: 24px; padding: 0; }
.comment-editor { border-radius: 6px; border: 1px solid alpha(currentColor, 0.2); min-height: 60px; }
.comment-badge { font-size: smaller; font-weight: bold; border-radius: 4px; padding: 0 5px; }
.comment-badge.warning { color: @warning_color; background: alpha(@warning_color, 0.15); }
.comment-badge.success { color: @success_color; background: alpha(@success_color, 0.15); }
.comment-count { color: #c88a0e; font-size: smaller; font-weight: bold; }
scrollbar slider {
    min-width: 12px;
    min-height: 12px;
    margin: 0;
    border: 3px solid transparent;
    border-radius: 9px;
    background-clip: padding-box;
    background-color: alpha(currentColor, 0.45);
}
scrollbar.vertical slider { min-height: 48px; }
scrollbar.horizontal slider { min-width: 48px; }
scrollbar slider:hover { background-color: alpha(currentColor, 0.6); }
scrollbar slider:active { background-color: alpha(currentColor, 0.8); }
scrollbar.overlay-indicator:not(.hovering) slider {
    min-width: 8px;
    min-height: 8px;
    border-width: 2px;
    background-color: alpha(currentColor, 0.45);
}
scrollbar.overlay-indicator:not(.hovering).vertical slider { min-height: 48px; }
scrollbar.overlay-indicator:not(.hovering).horizontal slider { min-width: 48px; }
"#;

fn main() -> glib::ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    if args
        .next_if(|a| a == "comment" || a == "comments")
        .is_some()
    {
        return glib::ExitCode::from(cli::run(args) as u8);
    }
    let opts = match git::Options::parse(args) {
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
        .application_id("io.github.andunieee.twain")
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
