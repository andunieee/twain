//! The card showing one comment below the lines it's attached to, or an editor for one.

use crate::comments::Comment;
use gtk::prelude::*;
use gtk::{gdk, glib};
use std::rc::Rc;

pub enum Action {
    Save(String),
    Cancel,
    Edit,
    Delete,
    ToggleResolved,
    /// The card's height may have changed.
    Resize,
}

pub struct Card {
    pub comment: Comment,
    pub widget: gtk::Box,
    /// Set while the card is an editor.
    pub editor: Option<gtk::TextView>,
}

fn icon_button(icon: &str, tooltip: &str, on_click: impl Fn() + 'static) -> gtk::Button {
    let b = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .build();
    b.add_css_class("flat");
    b.add_css_class("circular");
    b.connect_clicked(move |_| on_click());
    b
}

fn badge(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("comment-badge");
    l.add_css_class(class);
    l
}

impl Card {
    pub fn new(comment: Comment, outdated: bool, editing: bool, act: Rc<dyn Fn(Action)>) -> Card {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 4);
        widget.add_css_class("comment-card");
        if comment.resolved && !editing {
            widget.add_css_class("resolved");
        }

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let is_new = comment.id.is_empty();
        let author = gtk::Label::new(Some(if is_new {
            "New comment"
        } else {
            &comment.author
        }));
        author.add_css_class("heading");
        header.append(&author);
        if !is_new {
            let when = glib::DateTime::from_unix_local(comment.updated)
                .and_then(|d| d.format("%b %e, %H:%M"))
                .map(|s| s.to_string())
                .unwrap_or_default();
            let time = gtk::Label::new(Some(&when));
            time.add_css_class("dim-label");
            time.add_css_class("caption");
            header.append(&time);
        }
        let lines = if comment.start == comment.end {
            format!("line {}", comment.start)
        } else {
            format!("lines {}–{}", comment.start, comment.end)
        };
        let range = gtk::Label::new(Some(&lines));
        range.add_css_class("dim-label");
        range.add_css_class("caption");
        header.append(&range);
        if outdated {
            let b = badge("outdated", "warning");
            b.set_tooltip_text(Some("The lines this comment was written on have changed"));
            header.append(&b);
        }
        if comment.resolved {
            header.append(&badge("resolved", "success"));
        }
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header.append(&spacer);
        widget.append(&header);

        let mut editor = None;
        if editing {
            let view = gtk::TextView::builder()
                .wrap_mode(gtk::WrapMode::WordChar)
                .accepts_tab(false)
                .top_margin(6)
                .bottom_margin(6)
                .left_margin(6)
                .right_margin(6)
                .build();
            view.add_css_class("comment-editor");
            view.buffer().set_text(&comment.body);
            let a = act.clone();
            view.buffer().connect_changed(move |_| a(Action::Resize));
            let keys = gtk::EventControllerKey::new();
            let (a, v) = (act.clone(), view.downgrade());
            keys.connect_key_pressed(move |_, key, _, mods| {
                let Some(v) = v.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                let enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
                if enter && mods.contains(gdk::ModifierType::CONTROL_MASK) {
                    a(Action::Save(buffer_text(&v)));
                } else if key == gdk::Key::Escape {
                    a(Action::Cancel);
                } else {
                    return glib::Propagation::Proceed;
                }
                glib::Propagation::Stop
            });
            view.add_controller(keys);
            widget.append(&view);

            let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let hint = gtk::Label::builder()
                .label("Ctrl+Enter to save · Esc to cancel")
                .xalign(0.0)
                .hexpand(true)
                .build();
            hint.add_css_class("dim-label");
            hint.add_css_class("caption");
            buttons.append(&hint);
            let cancel = gtk::Button::with_label("Cancel");
            let a = act.clone();
            cancel.connect_clicked(move |_| a(Action::Cancel));
            buttons.append(&cancel);
            let save = gtk::Button::with_label(if is_new { "Comment" } else { "Save" });
            save.add_css_class("suggested-action");
            let (a, v) = (act.clone(), view.downgrade());
            save.connect_clicked(move |_| {
                if let Some(v) = v.upgrade() {
                    a(Action::Save(buffer_text(&v)));
                }
            });
            buttons.append(&save);
            widget.append(&buttons);
            editor = Some(view);
        } else {
            let a = act.clone();
            let (icon, tip) = if comment.resolved {
                ("edit-undo-symbolic", "Reopen")
            } else {
                ("object-select-symbolic", "Resolve")
            };
            header.append(&icon_button(icon, tip, move || a(Action::ToggleResolved)));
            let a = act.clone();
            header.append(&icon_button("document-edit-symbolic", "Edit", move || {
                a(Action::Edit)
            }));
            let a = act.clone();
            header.append(&icon_button("user-trash-symbolic", "Delete", move || {
                a(Action::Delete)
            }));
            let body = gtk::Label::builder()
                .label(&comment.body)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .selectable(true)
                .build();
            body.add_css_class("comment-body");
            widget.append(&body);
        }
        Card {
            comment,
            widget,
            editor,
        }
    }

    /// Height the card needs at a given width.
    pub fn height_for(&self, width: i32) -> i32 {
        let (_, natural, _, _) = self
            .widget
            .measure(gtk::Orientation::Vertical, width.max(1));
        natural
    }
}

fn buffer_text(view: &gtk::TextView) -> String {
    let buffer = view.buffer();
    let (start, end) = buffer.bounds();
    buffer.text(&start, &end, false).trim().to_string()
}
