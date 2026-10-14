//! One file of the diff, as two halves: the old side goes in the window's left column and
//! the new side in the right one, each a clickable header and a body.
//!
//! Each side is a GtkSourceView holding the *whole* file, so syntax highlighting and line
//! numbers are correct. Unchanged regions are hidden with an invisible tag. Like meld, the
//! sides aren't padded to line up: each column scrolls on its own, kept in sync through the
//! layout's anchors, and the curved links between them are drawn by the window.

use crate::comment_card::{Action, Card};
use crate::comments::{self, Comment, Location, Side as CommentSide};
use crate::diff::{self, Block, ChunkKind, Gap, Layout, Note};
use crate::git::{Content, FileChange};
use gtk::prelude::*;
use gtk::{cairo, gdk, gio, glib, graphene};
use sourceview5::prelude::*;
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::{Rc, Weak};

const GAP_HEIGHT: i32 = 24;
/// Space between a comment card and the text around it, and the side of its pane.
const CARD_MARGIN: i32 = 6;
const COMMENT_COLOR: (f64, f64, f64) = (0.96, 0.69, 0.13);
const RESOLVED_COLOR: (f64, f64, f64) = (0.55, 0.55, 0.55);

pub fn kind_color(kind: ChunkKind) -> (f64, f64, f64) {
    match kind {
        ChunkKind::Insert => (0.20, 0.72, 0.30),
        ChunkKind::Delete => (0.90, 0.27, 0.27),
        ChunkKind::Replace => (0.26, 0.52, 0.96),
    }
}

fn rgba((r, g, b): (f64, f64, f64), a: f64) -> gdk::RGBA {
    gdk::RGBA::new(r as f32, g as f32, b as f32, a as f32)
}

pub struct Side {
    pub view: sourceview5::View,
    pub buffer: sourceview5::Buffer,
    /// Scrolls horizontally; vertically, the whole column scrolls.
    pub scroller: gtk::ScrolledWindow,
    pub hidden: gtk::TextTag,
    /// Marks the current search match.
    pub current: gtk::TextTag,
    pads: RefCell<Vec<gtk::TextTag>>,
}

impl Side {
    fn new(text: &str, path: &str, scheme: Option<&sourceview5::StyleScheme>) -> Side {
        let buffer = sourceview5::Buffer::new(None);
        buffer.set_enable_undo(false);
        buffer.set_highlight_matching_brackets(false);
        if let Some(lang) = sourceview5::LanguageManager::default().guess_language(Some(path), None)
        {
            buffer.set_language(Some(&lang));
        }
        buffer.set_style_scheme(scheme);
        // The buffer would show a trailing newline as an extra empty line.
        buffer.set_text(text.strip_suffix('\n').unwrap_or(text));

        let view = sourceview5::View::with_buffer(&buffer);
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_monospace(true);
        view.set_highlight_current_line(false);
        view.set_left_margin(6);
        view.set_right_margin(12);
        view.add_css_class("diff-text");

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .child(&view)
            .build();

        let hidden = gtk::TextTag::builder().invisible(true).build();
        buffer.tag_table().add(&hidden);
        add_line_numbers(&view, &buffer, &hidden);
        let current = gtk::TextTag::builder()
            .background_rgba(&gdk::RGBA::new(1.0, 0.55, 0.0, 0.85))
            .foreground_rgba(&gdk::RGBA::BLACK)
            .build();
        buffer.tag_table().add(&current);
        Side {
            view,
            buffer,
            scroller,
            hidden,
            current,
            pads: RefCell::default(),
        }
    }

    fn line_iter(&self, line: usize) -> gtk::TextIter {
        self.buffer
            .iter_at_line(line as i32)
            .unwrap_or_else(|| self.buffer.end_iter())
    }

    fn tag_lines(&self, tag: &gtk::TextTag, lines: std::ops::Range<usize>) {
        self.buffer.apply_tag(
            tag,
            &self.line_iter(lines.start),
            &self.line_iter(lines.end),
        );
    }

    fn tag_chars(&self, tag: &gtk::TextTag, base: i32, chars: std::ops::Range<usize>) {
        let start = self.buffer.iter_at_offset(base + chars.start as i32);
        let end = self.buffer.iter_at_offset(base + chars.end as i32);
        self.buffer.apply_tag(tag, &start, &end);
    }

    fn apply_layout(&self, layout: &diff::SideLayout) {
        let (start, end) = self.buffer.bounds();
        self.buffer.remove_tag(&self.hidden, &start, &end);
        let table = self.buffer.tag_table();
        for tag in self.pads.take() {
            table.remove(&tag);
        }
        let line_count = self.buffer.line_count() as usize;
        for range in &layout.hidden {
            let mut start = self.line_iter(range.start);
            // When hiding up to the end, also hide the preceding newline: otherwise the last
            // (newline-less) line still takes up a row.
            if range.end >= line_count && range.start > 0 {
                start.backward_char();
            }
            self.buffer
                .apply_tag(&self.hidden, &start, &self.line_iter(range.end));
        }
        let mut pads = self.pads.borrow_mut();
        for (&line, &px) in &layout.pad_above {
            if line as i32 >= self.buffer.line_count() {
                continue;
            }
            let tag = gtk::TextTag::builder().pixels_above_lines(px).build();
            table.add(&tag);
            self.tag_lines(&tag, line..line + 1);
            pads.push(tag);
        }
    }
}

/// Line height used for files that haven't been built yet, until a real one is measured.
const ESTIMATED_LINE_HEIGHT: i32 = 18;

/// A search hit: side (0 = old, 1 = new), line, and char columns within the line.
pub struct Hit {
    pub side: usize,
    pub line: usize,
    pub cols: std::ops::Range<usize>,
}

/// State shared by all file views of a window.
pub struct Shared {
    /// Measured height of a text line, 0 until the first view is realized.
    pub line_height: Cell<i32>,
    pub scheme: RefCell<Option<sourceview5::StyleScheme>>,
    pub search_settings: sourceview5::SearchSettings,
    /// Called when the first measured line height differs from what layouts were made with.
    pub on_line_height: RefCell<Option<Box<dyn Fn()>>>,
    /// Called after a file's layout changes.
    pub on_relayout: RefCell<Option<Box<dyn Fn()>>>,
    /// Repository root, as comments are keyed by it.
    pub root: RefCell<String>,
    /// Author of comments written in this window.
    pub author: String,
}

/// What a file shows in one column: a header and the body below it.
struct Half {
    widget: gtk::Box,
    body: gtk::Overlay,
    placeholder: gtk::Box,
    decorations: gtk::DrawingArea,
    toggle_label: gtk::Label,
    comment_count: gtk::Label,
    /// Width the cards were last measured for.
    width: Cell<i32>,
}

impl Half {
    fn new(change: &FileChange, side: usize, stat: usize, message: Option<&str>) -> Half {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("file-section");

        let header_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let status = gtk::Label::new(Some(&change.status.to_string()));
        status.add_css_class("status");
        status.add_css_class(status_class(change.status));
        header_box.append(&status);
        let title = match (&change.old_path, side) {
            (Some(old), 0) => old,
            _ => &change.path,
        };
        let path = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Start)
            .build();
        path.add_css_class("file-path");
        header_box.append(&path);
        if stat > 0 {
            let (text, class) = match side {
                0 => (format!("−{stat}"), "stat-del"),
                _ => (format!("+{stat}"), "stat-add"),
            };
            let l = gtk::Label::new(Some(&text));
            l.add_css_class(class);
            header_box.append(&l);
        }
        let comment_count = gtk::Label::new(None);
        comment_count.add_css_class("comment-count");
        comment_count.set_visible(false);
        header_box.append(&comment_count);
        let toggle_label = gtk::Label::new(None);
        toggle_label.add_css_class("dim-label");
        toggle_label.add_css_class("caption");
        header_box.append(&toggle_label);
        let header = gtk::Button::builder()
            .child(&header_box)
            .cursor(&gdk::Cursor::from_name("pointer", None).unwrap())
            .build();
        header.add_css_class("file-header");
        widget.append(&header);

        let body = gtk::Overlay::new();
        let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        match message {
            None => {
                header.set_tooltip_text(Some("Toggle between changes only and the full file"));
                body.set_child(Some(&placeholder));
            }
            Some(msg) => {
                toggle_label.set_visible(false);
                let l = gtk::Label::new(Some(msg));
                l.add_css_class("dim-label");
                l.add_css_class("placeholder");
                body.set_child(Some(&l));
            }
        }
        widget.append(&body);
        let decorations = gtk::DrawingArea::new();
        decorations.set_can_target(false);
        body.add_overlay(&decorations);
        Half {
            widget,
            body,
            placeholder,
            decorations,
            toggle_label,
            comment_count,
            width: Cell::new(0),
        }
    }

    fn header(&self) -> gtk::Button {
        self.widget.first_child().and_downcast().unwrap()
    }
}

pub struct FileView {
    halves: [Half; 2],
    pub change: FileChange,
    shared: Rc<Shared>,
    /// Sanitized old/new text; `None` for binary files and files without content changes.
    texts: Option<[String; 2]>,
    /// The text views are expensive, so they're only built once the file scrolls into view.
    sides: OnceCell<[Side; 2]>,
    search_contexts: OnceCell<[sourceview5::SearchContext; 2]>,
    blocks: Vec<Block>,
    collapsed: Cell<bool>,
    /// Hidden unchanged lines (on the old side) shown anyway, while collapsed.
    revealed: RefCell<Vec<std::ops::Range<usize>>>,
    /// The region (on the old side) the text's context menu offers to hide again.
    menu_region: RefCell<Option<std::ops::Range<usize>>>,
    pub layout: RefCell<Layout>,
    /// Comment cards (including one being written) and where their lines are.
    cards: RefCell<Vec<(Card, Location)>>,
    on_toggle: RefCell<Option<Box<dyn Fn()>>>,
    weak_self: Weak<FileView>,
}

impl FileView {
    pub fn new(change: FileChange, shared: Rc<Shared>, collapsed: bool) -> Rc<FileView> {
        let old_text = diff::sanitize(change.old.text());
        let new_text = diff::sanitize(change.new.text());
        let is_text = change.old.is_text() && change.new.is_text();
        let blocks = if is_text {
            diff::diff_lines(&old_text, &new_text)
        } else {
            Vec::new()
        };
        let has_changes = blocks.iter().any(|b| matches!(b, Block::Change(_)));

        let (added, removed) = blocks.iter().fold((0, 0), |(a, r), b| match b {
            Block::Change(c) => (a + c.new.len(), r + c.old.len()),
            _ => (a, r),
        });
        let texts = (is_text && has_changes).then_some([old_text, new_text]);
        let message = match (&change.old, &change.new) {
            _ if texts.is_some() => None,
            (Content::Binary, _) | (_, Content::Binary) => Some("Binary file"),
            (Content::TooLarge, _) | (_, Content::TooLarge) => Some("File too large to display"),
            _ if change.old_path.is_some() => Some("Renamed without content changes"),
            _ => Some("No content changes"),
        };
        let halves = [
            Half::new(&change, 0, removed, message),
            Half::new(&change, 1, added, message),
        ];

        let this = Rc::new_cyclic(|weak_self| FileView {
            halves,
            change,
            shared,
            texts,
            sides: OnceCell::new(),
            search_contexts: OnceCell::new(),
            blocks,
            collapsed: Cell::new(collapsed),
            revealed: RefCell::default(),
            menu_region: RefCell::default(),
            layout: RefCell::default(),
            cards: RefCell::default(),
            on_toggle: RefCell::default(),
            weak_self: weak_self.clone(),
        });
        for (side, half) in this.halves.iter().enumerate() {
            let weak = Rc::downgrade(&this);
            half.decorations.connect_resize(move |_, width, _| {
                let Some(this) = weak.upgrade() else { return };
                let changed = this.halves[side].width.replace(width) != width;
                if changed && !this.cards.borrow().is_empty() {
                    // Cards wrap differently now; not during allocation, though.
                    let weak = this.weak_self.clone();
                    glib::idle_add_local_once(move || {
                        if let Some(this) = weak.upgrade() {
                            this.relayout();
                        }
                    });
                }
            });
            let weak = Rc::downgrade(&this);
            half.body.connect_get_child_position(move |_, child| {
                weak.upgrade().and_then(|this| this.card_rect(side, child))
            });
            if this.texts.is_some() {
                this.setup_gap_input(side);
                let weak = Rc::downgrade(&this);
                half.header().connect_clicked(move |_| {
                    if let Some(this) = weak.upgrade() {
                        this.set_collapsed(!this.collapsed.get());
                    }
                });
            }
        }
        if this.texts.is_some() {
            this.update_toggle_label();
            this.relayout();
        }
        this
    }

    /// The file's part of the left (0) or right (1) column.
    pub fn section(&self, side: usize) -> &gtk::Box {
        &self.halves[side].widget
    }

    /// Old and new side, if the text views have been built.
    pub fn sides(&self) -> Option<&[Side; 2]> {
        self.sides.get()
    }

    /// Creates the text views, replacing the placeholder.
    pub fn ensure_built(&self) {
        let Some([old_text, new_text]) = &self.texts else {
            return;
        };
        if self.sides.get().is_some() {
            return;
        }
        let scheme = self.shared.scheme.borrow().clone();
        let path = &self.change.path;
        let sides = [
            Side::new(old_text, path, scheme.as_ref()),
            Side::new(new_text, path, scheme.as_ref()),
        ];
        self.apply_change_tags(&sides);
        let settings = &self.shared.search_settings;
        let _ = self.search_contexts.set(sides.each_ref().map(|s| {
            let ctx = sourceview5::SearchContext::new(&s.buffer, Some(settings));
            ctx.set_highlight(true);
            // The context raises its highlight tag to the top when it finishes scanning;
            // keep the current-match tag above it.
            let (current, table) = (s.current.clone(), s.buffer.tag_table());
            ctx.connect_occurrences_count_notify(move |_| current.set_priority(table.size() - 1));
            ctx
        }));

        // The line height is only known once a view has its font, i.e. once it's realized.
        let weak = self.weak_self.clone();
        sides[0].view.connect_realize(move |view| {
            let Some(this) = weak.upgrade() else { return };
            let (_, h) = view.create_pango_layout(Some("Ag|")).size();
            let measured = (h + 512) >> 10; // PANGO_PIXELS, as GtkTextLayout does
            if measured != this.shared.line_height.replace(measured)
                && let Some(f) = &*this.shared.on_line_height.borrow()
            {
                f();
            }
            this.relayout();
        });
        for (i, (side, half)) in sides.iter().zip(&self.halves).enumerate() {
            let weak = self.weak_self.clone();
            half.decorations.set_draw_func(move |_, cr, w, h| {
                if let Some(this) = weak.upgrade() {
                    this.draw_decorations(i, cr, w, h);
                }
            });
            self.setup_comment_input(i, side);
        }
        let _ = self.sides.set(sides);
        self.relayout();
        for (side, half) in self.sides().unwrap().iter().zip(&self.halves) {
            half.body.set_child(Some(&side.scroller));
        }
    }

    pub fn is_collapsed(&self) -> bool {
        self.collapsed.get()
    }

    pub fn set_collapsed(&self, collapsed: bool) {
        if self.texts.is_none() || self.collapsed.get() == collapsed {
            return;
        }
        self.collapsed.set(collapsed);
        if collapsed {
            self.revealed.borrow_mut().clear();
        }
        self.update_toggle_label();
        self.relayout();
        if let Some(f) = &*self.on_toggle.borrow() {
            f();
        }
    }

    /// Shows these hidden lines (on the old side).
    fn reveal(&self, lines: std::ops::Range<usize>) {
        self.revealed.borrow_mut().push(lines);
        self.relayout();
    }

    /// Hides again the revealed lines of a region.
    fn unreveal(&self, region: &std::ops::Range<usize>) {
        let revealed = diff::subtract(&self.revealed.borrow(), region);
        *self.revealed.borrow_mut() = revealed;
        self.relayout();
    }

    /// The gap band at `y` in one side's body.
    fn gap_at(&self, side: usize, y: f64) -> Option<Gap> {
        self.sides()?;
        let layout = self.layout.borrow();
        let y = y.floor() as i32;
        let gap = layout
            .gaps
            .iter()
            .find(|g| (g.y[side]..g.y[side] + g.height).contains(&y));
        gap.cloned()
    }

    /// Clicking a gap band shows its lines; right-clicking it offers to show only some of
    /// them. Revealed lines are hidden again from the text's context menu.
    fn setup_gap_input(&self, side: usize) {
        let body = &self.halves[side].body;
        let click = gtk::GestureClick::builder().button(0).build();
        // Before the text view under the band sees it.
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = self.weak_self.clone();
        click.connect_pressed(move |gesture, n, x, y| {
            let Some(this) = weak.upgrade() else { return };
            let Some(gap) = this.gap_at(side, y) else {
                return;
            };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            match gesture.current_button() {
                gdk::BUTTON_SECONDARY => this.show_gap_menu(side, &gap, x, y),
                gdk::BUTTON_PRIMARY if n == 1 => this.reveal(gap.region),
                _ => {}
            }
        });
        body.add_controller(click);

        let motion = gtk::EventControllerMotion::new();
        let weak = self.weak_self.clone();
        let update_cursor = move |y: Option<f64>| {
            let Some(this) = weak.upgrade() else { return };
            let Some(sides) = this.sides() else { return };
            let over = y.is_some_and(|y| this.gap_at(side, y).is_some());
            sides[side]
                .view
                .set_cursor_from_name(Some(if over { "pointer" } else { "text" }));
        };
        let update = update_cursor.clone();
        motion.connect_motion(move |_, _, y| update(Some(y)));
        motion.connect_leave(move |_| update_cursor(None));
        body.add_controller(motion);
    }

    fn show_gap_menu(&self, side: usize, gap: &Gap, x: f64, y: f64) {
        let body = &self.halves[side].body;
        let group = gio::SimpleActionGroup::new();
        let menu = gio::Menu::new();
        let shows = gio::Menu::new();
        let add = |menu: &gio::Menu, label: &str, f: Box<dyn Fn(&FileView)>| {
            let name = format!("a{}", group.list_actions().len());
            let action = gio::SimpleAction::new(&name, None);
            let weak = self.weak_self.clone();
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    f(&this);
                }
            });
            group.add_action(&action);
            menu.append(Some(label), Some(&format!("gap.{name}")));
        };
        let lines = gap.lines.clone();
        let n = lines.len();
        let all = lines.clone();
        add(
            &shows,
            &format!("Show All {n} Line{}", if n == 1 { "" } else { "s" }),
            Box::new(move |t| t.reveal(all.clone())),
        );
        for k in [10, 50] {
            // Unless what would be left is too short to stay hidden.
            if n > k + diff::MIN_HIDDEN {
                let (s, e) = (lines.start, lines.end);
                add(
                    &shows,
                    &format!("Show First {k} Lines"),
                    Box::new(move |t| t.reveal(s..s + k)),
                );
                add(
                    &shows,
                    &format!("Show Last {k} Lines"),
                    Box::new(move |t| t.reveal(e - k..e)),
                );
            }
        }
        menu.append_section(None, &shows);
        if lines != gap.region {
            let hides = gio::Menu::new();
            let region = gap.region.clone();
            add(
                &hides,
                &format!("Hide {} Unchanged Lines", region.len()),
                Box::new(move |t| t.unreveal(&region)),
            );
            menu.append_section(None, &hides);
        }

        body.insert_action_group("gap", Some(&group));
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(body);
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|popover| {
            // Not right away: the chosen item is activated after the menu closes.
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
    }

    fn update_toggle_label(&self) {
        let collapsed = self.collapsed.get();
        for half in &self.halves {
            half.toggle_label.set_label(if collapsed {
                "Show full file"
            } else {
                "Show changes only"
            });
        }
    }

    /// Called after the file is switched between "changes only" and "full file".
    pub fn connect_toggle(&self, f: impl Fn() + 'static) {
        *self.on_toggle.borrow_mut() = Some(Box::new(f));
    }

    pub fn set_style_scheme(&self, scheme: Option<&sourceview5::StyleScheme>) {
        for side in self.sides().into_iter().flatten() {
            side.buffer.set_style_scheme(scheme);
        }
    }

    /// Case-insensitive search through the visible lines of both sides. Works on the raw
    /// text, so it doesn't need the views to be built.
    pub fn find(&self, needle: &str) -> Vec<Hit> {
        let Some(texts) = &self.texts else {
            return Vec::new();
        };
        let needle: Vec<char> = needle.chars().map(fold_case).collect();
        let layout = self.layout.borrow();
        let mut hits = Vec::new();
        for (side, text) in texts.iter().enumerate() {
            let hidden = &layout.sides[side].hidden;
            let mut ranges = hidden.iter().peekable();
            for (line, content) in text.split('\n').enumerate() {
                while ranges.next_if(|r| r.end <= line).is_some() {}
                if ranges.peek().is_some_and(|r| r.contains(&line)) {
                    continue;
                }
                let chars: Vec<char> = content.chars().map(fold_case).collect();
                let mut col = 0;
                while col + needle.len() <= chars.len() {
                    if chars[col..col + needle.len()] == needle[..] {
                        hits.push(Hit {
                            side,
                            line,
                            cols: col..col + needle.len(),
                        });
                        col += needle.len();
                    } else {
                        col += 1;
                    }
                }
            }
        }
        hits
    }

    /// y of the top of one side's body (where line y = 0 is) in `target`'s coordinates.
    pub fn body_y(&self, side: usize, target: &impl IsA<gtk::Widget>) -> Option<f64> {
        self.halves[side]
            .body
            .compute_point(target, &graphene::Point::zero())
            .map(|p| p.y() as f64)
    }

    fn apply_change_tags(&self, [old, new]: &[Side; 2]) {
        let tags = |side: &Side| {
            let table = side.buffer.tag_table();
            let mk = |kind, line_alpha, inline_alpha| {
                let line = gtk::TextTag::builder()
                    .paragraph_background_rgba(&rgba(kind_color(kind), line_alpha))
                    .build();
                let inline = gtk::TextTag::builder()
                    .background_rgba(&rgba(kind_color(kind), inline_alpha))
                    .build();
                table.add(&line);
                table.add(&inline);
                (line, inline)
            };
            [
                mk(ChunkKind::Insert, 0.16, 0.0),
                mk(ChunkKind::Delete, 0.16, 0.0),
                mk(ChunkKind::Replace, 0.14, 0.32),
            ]
        };
        let (old_tags, new_tags) = (tags(old), tags(new));
        let idx = |k| match k {
            ChunkKind::Insert => 0,
            ChunkKind::Delete => 1,
            ChunkKind::Replace => 2,
        };
        for block in &self.blocks {
            let Block::Change(c) = block else { continue };
            let i = idx(c.kind);
            old.tag_lines(&old_tags[i].0, c.old.clone());
            new.tag_lines(&new_tags[i].0, c.new.clone());
            if c.kind != ChunkKind::Replace {
                continue;
            }
            let (os, oe) = (old.line_iter(c.old.start), old.line_iter(c.old.end));
            let (ns, ne) = (new.line_iter(c.new.start), new.line_iter(c.new.end));
            let otext = old.buffer.text(&os, &oe, true);
            let ntext = new.buffer.text(&ns, &ne, true);
            if let Some((oranges, nranges)) = diff::diff_inline(&otext, &ntext) {
                for r in oranges {
                    old.tag_chars(&old_tags[i].1, os.offset(), r);
                }
                for r in nranges {
                    new.tag_chars(&new_tags[i].1, ns.offset(), r);
                }
            }
        }
    }

    /// Recomputes which lines are hidden and how they're padded.
    pub fn relayout(&self) {
        if self.texts.is_none() {
            return;
        }
        let lh = match self.shared.line_height.get() {
            0 => ESTIMATED_LINE_HEIGHT,
            lh => lh,
        };
        let notes = self.notes();
        let layout = diff::layout(
            &self.blocks,
            &notes,
            self.collapsed.get(),
            &self.revealed.borrow(),
            lh,
            GAP_HEIGHT,
        );
        for ((card, _), y) in self.cards.borrow().iter().zip(&layout.notes) {
            card.widget.set_visible(y.is_some());
        }
        for (side, half) in self.halves.iter().enumerate() {
            half.body.queue_allocate();
            // Also makes room for a gap band below the last visible line.
            let height = layout.height[side];
            match self.sides() {
                Some(sides) => {
                    sides[side].apply_layout(&layout.sides[side]);
                    sides[side].scroller.set_height_request(height);
                    half.decorations.queue_draw();
                }
                None => half.placeholder.set_height_request(height),
            }
        }
        *self.layout.borrow_mut() = layout;
        if let Some(f) = &*self.shared.on_relayout.borrow() {
            f();
        }
    }

    fn root(&self) -> String {
        self.shared.root.borrow().clone()
    }

    /// Re-reads this file's comments from the database.
    pub fn reload_comments(&self) {
        let root = self.root();
        match comments::store().and_then(|s| s.list(&root, Some(&self.change.path))) {
            Ok(list) => self.set_comments(list),
            Err(e) => eprintln!("twain: {e}"),
        }
    }

    /// Shows these comments, keeping any that are being edited as they are.
    pub fn set_comments(&self, list: Vec<Comment>) {
        for (side, half) in self.halves.iter().enumerate() {
            let n = list.iter().filter(|c| c.side.index() == side).count();
            half.comment_count
                .set_label(&format!("{n} comment{}", if n == 1 { "" } else { "s" }));
            half.comment_count.set_visible(n > 0);
        }
        let mut cards = self.cards.borrow_mut();
        let mut kept = Vec::new();
        for (card, loc) in cards.drain(..) {
            if card.editor.is_some() {
                kept.push((card, loc));
            } else {
                self.remove_card_widget(&card);
            }
        }
        drop(cards);
        if let Some(texts) = &self.texts {
            for comment in list {
                if kept.iter().any(|(c, _)| c.comment.id == comment.id) {
                    continue;
                }
                let loc = comment.locate(&texts[comment.side.index()]);
                let outdated = loc.outdated;
                kept.push((self.make_card(comment, outdated, false), loc));
            }
        } else {
            for (card, _) in &kept {
                self.remove_card_widget(card);
            }
            kept.clear();
        }
        kept.sort_by_key(|(c, loc)| {
            (
                c.comment.side,
                loc.lines.start,
                c.comment.id.is_empty(),
                c.comment.created,
            )
        });
        *self.cards.borrow_mut() = kept;
        self.relayout();
    }

    fn make_card(&self, comment: Comment, outdated: bool, editing: bool) -> Card {
        let (weak, id) = (self.weak_self.clone(), comment.id.clone());
        let act = Rc::new(move |action| {
            if let Some(this) = weak.upgrade() {
                this.on_card_action(&id, action);
            }
        });
        let side = comment.side.index();
        let card = Card::new(comment, outdated, editing, act);
        self.halves[side].body.add_overlay(&card.widget);
        card
    }

    fn remove_card_widget(&self, card: &Card) {
        self.halves[card.comment.side.index()]
            .body
            .remove_overlay(&card.widget);
    }

    fn focus_editor(&self, id: &str) {
        let editor = self
            .cards
            .borrow()
            .iter()
            .find(|(c, _)| c.comment.id == id)
            .and_then(|(c, _)| c.editor.clone());
        if let Some(editor) = editor {
            // Once it has been allocated.
            glib::idle_add_local_once(move || {
                editor.grab_focus();
            });
        }
    }

    /// Replaces a card with a fresh one, as an editor or not; `None` removes it.
    fn replace_card(&self, id: &str, editing: Option<bool>) {
        let mut cards = self.cards.borrow_mut();
        let Some(i) = cards.iter().position(|(c, _)| c.comment.id == id) else {
            return;
        };
        let (old, loc) = cards.remove(i);
        self.remove_card_widget(&old);
        if let Some(editing) = editing {
            let card = self.make_card(old.comment, loc.outdated, editing);
            cards.insert(i, (card, loc));
        }
        drop(cards);
        self.relayout();
    }

    fn on_card_action(&self, id: &str, action: Action) {
        let comment = self
            .cards
            .borrow()
            .iter()
            .find(|(c, _)| c.comment.id == id)
            .map(|(c, _)| c.comment.clone());
        let Some(mut comment) = comment else { return };
        if matches!(action, Action::Save(_) | Action::Cancel)
            && let Some(sides) = self.sides()
        {
            // Rather than wherever focus would go once the editor is gone.
            sides[comment.side.index()].view.grab_focus();
        }
        let root = self.root();
        let result = match action {
            Action::Resize => {
                self.relayout();
                return;
            }
            Action::Edit => {
                self.replace_card(id, Some(true));
                self.focus_editor(id);
                return;
            }
            Action::Cancel => {
                self.replace_card(id, if id.is_empty() { None } else { Some(false) });
                return;
            }
            Action::Save(text) => {
                if text.is_empty() {
                    return;
                }
                comment.body = text;
                comment.updated = comments::now();
                // Stop editing, so that the reload below shows the saved version.
                self.replace_card(id, None);
                comments::store().and_then(|s| s.put(&root, &mut [comment]))
            }
            Action::Delete => comments::store().and_then(|s| s.delete(&root, &[comment])),
            Action::ToggleResolved => {
                comment.resolved = !comment.resolved;
                comment.updated = comments::now();
                comments::store().and_then(|s| s.put(&root, &mut [comment]))
            }
        };
        if let Err(e) = result {
            eprintln!("twain: {e}");
        }
        self.reload_comments();
    }

    /// Opens an editor for a new comment on the selected lines (or the cursor's line).
    fn comment_on_selection(&self, side: usize) {
        let (Some(sides), Some(texts)) = (self.sides(), &self.texts) else {
            return;
        };
        let buffer = &sides[side].buffer;
        let (a, b) = buffer.selection_bounds().unwrap_or_else(|| {
            let i = buffer.iter_at_mark(&buffer.get_insert());
            (i, i)
        });
        let start = a.line() as usize;
        let mut end = b.line() as usize;
        if end > start && b.starts_line() {
            end -= 1;
        }
        // Only one new comment at a time.
        self.replace_card("", None);
        let comment = Comment::new(
            &self.change.path,
            CommentSide::from_index(side),
            start..end + 1,
            "",
            &self.shared.author,
            &texts[side],
        );
        let loc = Location {
            lines: start..end + 1,
            outdated: false,
        };
        let card = self.make_card(comment, false, true);
        self.cards.borrow_mut().push((card, loc));
        self.relayout();
        self.focus_editor("");
    }

    fn setup_comment_input(&self, i: usize, side: &Side) {
        let group = gio::SimpleActionGroup::new();
        let action = gio::SimpleAction::new("comment", None);
        let weak = self.weak_self.clone();
        action.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.comment_on_selection(i);
            }
        });
        group.add_action(&action);
        let action = gio::SimpleAction::new("hide-unchanged", None);
        let weak = self.weak_self.clone();
        action.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade()
                && let Some(region) = this.menu_region.take()
            {
                this.unreveal(&region);
            }
        });
        group.add_action(&action);
        side.view.insert_action_group("side", Some(&group));
        let menu = gio::Menu::new();
        menu.append(Some("Add Comment (C)"), Some("side.comment"));
        side.view.set_extra_menu(Some(&menu));

        let keys = gtk::EventControllerKey::new();
        let weak = self.weak_self.clone();
        keys.connect_key_pressed(move |_, key, _, mods| {
            let plain = !mods.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::SUPER_MASK,
            );
            if plain
                && matches!(key, gdk::Key::c | gdk::Key::C)
                && let Some(this) = weak.upgrade()
            {
                this.comment_on_selection(i);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        side.view.add_controller(keys);

        // A right click outside the selection comments on the clicked line. On a revealed
        // unchanged line, the menu also offers to hide its region again.
        let click = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let view = side.view.downgrade();
        let weak = self.weak_self.clone();
        click.connect_pressed(move |_, _, x, y| {
            if menu.n_items() > 1 {
                menu.remove(1);
            }
            let (Some(view), Some(this)) = (view.upgrade(), weak.upgrade()) else {
                return;
            };
            let (bx, by) =
                view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
            let Some(iter) = view.iter_at_location(bx, by) else {
                return;
            };
            let buffer = view.buffer();
            let inside = buffer
                .selection_bounds()
                .is_some_and(|(a, b)| a.line() <= iter.line() && iter.line() <= b.line());
            if !inside {
                buffer.place_cursor(&iter);
            }
            let line = iter.line() as usize;
            let layout = this.layout.borrow();
            let region = layout.revealed.iter().find(|r| r[i].contains(&line));
            if let Some(region) = region {
                let n = region[i].len();
                let label = format!("Hide {n} Unchanged Line{}", if n == 1 { "" } else { "s" });
                menu.append(Some(&label), Some("side.hide-unchanged"));
            }
            *this.menu_region.borrow_mut() = region.map(|r| r[0].clone());
        });
        side.view.add_controller(click);
    }

    fn side_width(&self, side: usize) -> i32 {
        match self.halves[side].body.width() {
            0 => 600,
            w => w,
        }
    }

    /// Space each card needs, measured at the width of its side.
    fn notes(&self) -> Vec<Note> {
        self.cards
            .borrow()
            .iter()
            .map(|(card, loc)| {
                let side = card.comment.side.index();
                let width = self.side_width(side) - 2 * CARD_MARGIN;
                Note {
                    side,
                    lines: loc.lines.clone(),
                    height: card.height_for(width) + 2 * CARD_MARGIN,
                }
            })
            .collect()
    }

    /// Where a card goes in its side's body overlay.
    fn card_rect(&self, side: usize, child: &gtk::Widget) -> Option<gdk::Rectangle> {
        let cards = self.cards.borrow();
        let layout = self.layout.borrow();
        let i = cards
            .iter()
            .position(|(c, _)| c.widget.upcast_ref::<gtk::Widget>() == child)?;
        let y = (*layout.notes.get(i)?)?;
        let w = self.side_width(side) - 2 * CARD_MARGIN;
        let h = cards[i].0.height_for(w);
        Some(gdk::Rectangle::new(CARD_MARGIN, y + CARD_MARGIN, w, h))
    }

    /// Draws the meld-style bands linking this file's header and chunks on the left to their
    /// counterparts on the right, in the window's link map. `tops` is where this file's
    /// sections start on each side, in the link map's coordinates.
    pub fn draw_links(&self, cr: &cairo::Context, width: f64, height: f64, tops: [f64; 2]) {
        let fg = self.halves[0].widget.color();
        let body = [0, 1].map(|s| {
            let half = &self.halves[s];
            tops[s] + self.body_y(s, &half.widget).unwrap_or(0.0)
        });
        cr.set_line_width(1.0);
        let band = |cr: &cairo::Context, (l0, l1): (f64, f64), (r0, r1): (f64, f64)| {
            let (l1, r1) = (l1.max(l0), r1.max(r0));
            // If one end is off screen, only a stub on the other side, like meld.
            let off = |a: f64, b: f64| b < 0.0 || a > height;
            if off(r0, r1) {
                if l1 <= l0 + 1.0 {
                    return false;
                }
                rounded_stub(cr, 0.0, l0, l1, 1.0);
            } else if off(l0, l1) {
                if r1 <= r0 + 1.0 {
                    return false;
                }
                rounded_stub(cr, width, r0, r1, -1.0);
            } else {
                cr.move_to(-0.5, l0);
                cr.curve_to(width * 0.5, l0, width * 0.5, r0, width + 0.5, r0);
                cr.line_to(width + 0.5, r1);
                cr.curve_to(width * 0.5, r1, width * 0.5, l1, -0.5, l1);
                cr.close_path();
            }
            true
        };

        // The headers, so it's clear which files go together.
        let (r, g, b) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
        if band(cr, (tops[0], body[0]), (tops[1], body[1])) {
            cr.set_source_rgba(r, g, b, 0.07);
            let _ = cr.fill();
        }

        let layout = self.layout.borrow();
        if self.sides().is_none() {
            return;
        }
        for chunk in &layout.chunks {
            let l = (
                body[0] + chunk.left.0 as f64 + 0.5,
                body[0] + chunk.left.1 as f64 - 0.5,
            );
            let r = (
                body[1] + chunk.right.0 as f64 + 0.5,
                body[1] + chunk.right.1 as f64 - 0.5,
            );
            if l.1.max(l.0) < 0.0 && r.1.max(r.0) < 0.0 || l.0 > height && r.0 > height {
                continue;
            }
            if !band(cr, l, r) {
                continue;
            }
            let (r, g, b) = kind_color(chunk.kind);
            cr.set_source_rgba(r, g, b, 0.28);
            let _ = cr.fill_preserve();
            cr.set_source_rgba(r, g, b, 0.85);
            let _ = cr.stroke();
        }
    }

    /// One side's chunk borders, comment highlights and "hidden lines" bands.
    fn draw_decorations(&self, side: usize, cr: &cairo::Context, width: i32, _height: i32) {
        let layout = self.layout.borrow();
        if self.sides().is_none() {
            return;
        }
        let area = &self.halves[side].decorations;
        let w = width as f64;
        let hline = |y: f64| {
            cr.move_to(0.0, y);
            cr.line_to(w, y);
        };

        let lh = layout.line_height() as f64;
        for (card, loc) in self.cards.borrow().iter() {
            if card.comment.side.index() != side {
                continue;
            }
            let y0 = layout.line_y(side, loc.lines.start) as f64;
            let y1 = layout.line_y(side, loc.lines.end - 1) as f64 + lh;
            let (r, g, b) = if card.comment.resolved {
                RESOLVED_COLOR
            } else {
                COMMENT_COLOR
            };
            cr.set_source_rgba(r, g, b, 0.10);
            cr.rectangle(0.0, y0, w, y1 - y0);
            let _ = cr.fill();
            cr.set_source_rgba(r, g, b, 0.9);
            cr.rectangle(0.0, y0, 3.0, y1 - y0);
            let _ = cr.fill();
        }

        cr.set_line_width(1.0);
        for chunk in &layout.chunks {
            let (r, g, b) = kind_color(chunk.kind);
            let (y0, y1) = [chunk.left, chunk.right][side];
            hline(y0 as f64 + 0.5);
            if y1 > y0 {
                hline(y1 as f64 - 0.5);
            }
            cr.set_source_rgba(r, g, b, 0.7);
            let _ = cr.stroke();
        }

        let fg = area.color();
        for gap in &layout.gaps {
            let (y, h) = (gap.y[side] as f64, gap.height as f64);
            cr.rectangle(0.0, y, w, h);
            cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.05);
            let _ = cr.fill();
            cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.25);
            cr.set_dash(&[3.0, 3.0], 0.0);
            hline(y + 0.5);
            hline(y + h - 0.5);
            let _ = cr.stroke();
            cr.set_dash(&[], 0.0);

            let text = format!(
                "⋯  {} unchanged line{}  ⋯",
                gap.hidden,
                if gap.hidden == 1 { "" } else { "s" }
            );
            let text_layout = area.create_pango_layout(Some(&text));
            let (tw, th) = text_layout.pixel_size();
            cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.6);
            cr.move_to((w - tw as f64) / 2.0, y + (h - th as f64) / 2.0);
            pangocairo::functions::show_layout(cr, &text_layout);
        }
    }
}

/// A band hugging one edge of the link map (`dir` 1 for the left, -1 for the right), for a
/// chunk whose counterpart is off screen.
fn rounded_stub(cr: &cairo::Context, x: f64, y0: f64, y1: f64, dir: f64) {
    const RADIUS: f64 = 3.0;
    let q = std::f64::consts::FRAC_PI_2;
    if dir > 0.0 {
        cr.arc(x - 0.5, y0 + RADIUS, RADIUS, q * 3.0, 0.0);
        cr.arc(x - 0.5, y1 - RADIUS, RADIUS, 0.0, q);
    } else {
        cr.arc_negative(x + 0.5, y0 + RADIUS, RADIUS, q * 3.0, q * 2.0);
        cr.arc_negative(x + 0.5, y1 - RADIUS, RADIUS, q * 2.0, q);
    }
    cr.close_path();
}

/// Line numbers drawn by our own renderer: the built-in one squashes the numbers of hidden
/// lines on top of each other, and puts numbers at the top of the alignment padding instead
/// of next to the text.
fn add_line_numbers(view: &sourceview5::View, buffer: &sourceview5::Buffer, hidden: &gtk::TextTag) {
    let renderer = sourceview5::GutterRendererText::new();
    renderer.set_xalign(1.0);
    renderer.set_yalign(1.0);
    renderer.set_xpad(6);
    renderer.add_css_class("line-numbers");
    let digits = buffer.line_count().max(10).to_string().len();
    renderer.connect_realize(move |renderer| {
        let (width, _) = GutterRendererTextExt::measure(renderer, &"9".repeat(digits));
        renderer.set_width_request(width + 2 * renderer.xpad());
    });
    let hidden = hidden.clone();
    renderer.connect_query_data(move |renderer, _, line| {
        let visible = renderer
            .buffer()
            .and_then(|b| b.iter_at_line(line as i32))
            .is_some_and(|iter| !(iter.has_tag(&hidden) || line == 0 && iter.is_end()));
        renderer.set_text(
            if visible {
                (line + 1).to_string()
            } else {
                String::new()
            }
            .as_str(),
        );
    });
    let gutter = sourceview5::prelude::ViewExt::gutter(view, gtk::TextWindowType::Left);
    gutter.insert(&renderer, 0);
}

fn fold_case(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

pub fn status_class(status: char) -> &'static str {
    match status {
        'A' | '?' => "status-added",
        'D' => "status-deleted",
        'R' | 'C' => "status-renamed",
        'U' => "status-conflict",
        _ => "status-modified",
    }
}
