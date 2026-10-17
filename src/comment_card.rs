//! The card showing one comment and its replies below the lines it's attached to, with an
//! editor for the comment or one of its replies when they're being written.

use crate::comments::{Comment, Reply};
use gtk::prelude::*;
use gtk::{gdk, glib};
use std::rc::Rc;

/// What in a card an action is about.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Comment,
    /// A reply by id; empty for the one being written.
    Reply(String),
}

pub enum Action {
    Save(Target, String),
    Cancel,
    Edit(Target),
    Delete(Target),
    ToggleResolved,
    /// The card's height may have changed.
    Resize,
}

pub struct Card {
    pub comment: Comment,
    pub replies: Vec<Reply>,
    pub widget: gtk::Box,
    /// Set while the card has an editor.
    pub editor: Option<gtk::TextView>,
}

fn icon_button(icon: &str, tooltip: &str, on_click: impl Fn() + 'static) -> gtk::Button {
    let b = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .cursor(&gdk::Cursor::from_name("pointer", None).unwrap())
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

fn caption(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("dim-label");
    l.add_css_class("caption");
    l
}

fn time(t: i64) -> gtk::Label {
    let when = glib::DateTime::from_unix_local(t)
        .and_then(|d| d.format("%b %e, %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    caption(&when)
}

fn author(name: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(name));
    l.add_css_class("heading");
    l
}

fn spacer() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    b.set_hexpand(true);
    b
}

fn body(text: &str) -> gtk::Label {
    let l = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .selectable(true)
        .build();
    l.add_css_class("comment-body");
    l
}

/// Edit and delete buttons for the comment or a reply.
fn edit_buttons(header: &gtk::Box, target: Target, act: &Rc<dyn Fn(Action)>) {
    let (a, t) = (act.clone(), target.clone());
    header.append(&icon_button("document-edit-symbolic", "Edit", move || {
        a(Action::Edit(t.clone()))
    }));
    let a = act.clone();
    header.append(&icon_button("user-trash-symbolic", "Delete", move || {
        a(Action::Delete(target.clone()))
    }));
}

/// A text editor with its buttons, saving `target`.
fn editor(
    text: &str,
    save_label: &str,
    target: Target,
    act: &Rc<dyn Fn(Action)>,
) -> (gtk::Box, gtk::TextView) {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .accepts_tab(false)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(6)
        .right_margin(6)
        .build();
    view.add_css_class("comment-editor");
    view.buffer().set_text(text);
    let a = act.clone();
    view.buffer().connect_changed(move |_| a(Action::Resize));
    let keys = gtk::EventControllerKey::new();
    let (a, v, t) = (act.clone(), view.downgrade(), target.clone());
    keys.connect_key_pressed(move |_, key, _, mods| {
        let Some(v) = v.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
        if enter && mods.contains(gdk::ModifierType::CONTROL_MASK) {
            a(Action::Save(t.clone(), buffer_text(&v)));
        } else if key == gdk::Key::Escape {
            a(Action::Cancel);
        } else {
            return glib::Propagation::Proceed;
        }
        glib::Propagation::Stop
    });
    view.add_controller(keys);
    container.append(&view);

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
    cancel.set_cursor_from_name(Some("pointer"));
    buttons.append(&cancel);
    let save = gtk::Button::with_label(save_label);
    save.add_css_class("suggested-action");
    save.set_cursor_from_name(Some("pointer"));
    let (a, v) = (act.clone(), view.downgrade());
    save.connect_clicked(move |_| {
        if let Some(v) = v.upgrade() {
            a(Action::Save(target.clone(), buffer_text(&v)));
        }
    });
    buttons.append(&save);
    container.append(&buttons);
    (container, view)
}

impl Card {
    /// A card for a comment and its replies, with an editor for `editing`. Nothing else can
    /// be acted on while something is being edited.
    pub fn new(
        comment: Comment,
        replies: Vec<Reply>,
        outdated: bool,
        editing: Option<Target>,
        act: Rc<dyn Fn(Action)>,
    ) -> Card {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 4);
        widget.add_css_class("comment-card");
        if comment.resolved && editing.is_none() {
            widget.add_css_class("resolved");
        }

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let is_new = comment.id.is_empty();
        header.append(&author(if is_new {
            "New comment"
        } else {
            &comment.author
        }));
        if !is_new {
            header.append(&time(comment.updated));
        }
        let lines = if comment.start == comment.end {
            format!("line {}", comment.start)
        } else {
            format!("lines {}–{}", comment.start, comment.end)
        };
        header.append(&caption(&lines));
        if outdated {
            let b = badge("outdated", "warning");
            b.set_tooltip_text(Some("The lines this comment was written on have changed"));
            header.append(&b);
        }
        if comment.resolved {
            header.append(&badge("resolved", "success"));
        }
        header.append(&spacer());
        widget.append(&header);

        let mut editor_view = None;
        if editing == Some(Target::Comment) {
            let label = if is_new { "Comment" } else { "Save" };
            let (container, view) = editor(&comment.body, label, Target::Comment, &act);
            widget.append(&container);
            editor_view = Some(view);
        } else {
            if editing.is_none() {
                let a = act.clone();
                let (icon, tip) = if comment.resolved {
                    ("edit-undo-symbolic", "Reopen")
                } else {
                    ("object-select-symbolic", "Resolve")
                };
                header.append(&icon_button(icon, tip, move || a(Action::ToggleResolved)));
                edit_buttons(&header, Target::Comment, &act);
            }
            widget.append(&body(&comment.body));
        }

        for reply in &replies {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
            row.add_css_class("comment-reply");
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            header.append(&author(&reply.author));
            header.append(&time(reply.updated));
            header.append(&spacer());
            row.append(&header);
            let target = Target::Reply(reply.id.clone());
            if editing.as_ref() == Some(&target) {
                let (container, view) = editor(&reply.body, "Save", target, &act);
                row.append(&container);
                editor_view = Some(view);
            } else {
                if editing.is_none() {
                    edit_buttons(&header, target, &act);
                }
                row.append(&body(&reply.body));
            }
            widget.append(&row);
        }

        let new_reply = Target::Reply(String::new());
        if editing.as_ref() == Some(&new_reply) {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
            row.add_css_class("comment-reply");
            let (container, view) = editor("", "Reply", new_reply, &act);
            row.append(&container);
            widget.append(&row);
            editor_view = Some(view);
        } else if editing.is_none() {
            let reply = gtk::Button::builder()
                .label("Reply")
                .halign(gtk::Align::Start)
                .cursor(&gdk::Cursor::from_name("pointer", None).unwrap())
                .build();
            reply.add_css_class("flat");
            reply.add_css_class("comment-reply-button");
            let a = act.clone();
            reply.connect_clicked(move |_| a(Action::Edit(new_reply.clone())));
            widget.append(&reply);
        }

        Card {
            comment,
            replies,
            widget,
            editor: editor_view,
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
