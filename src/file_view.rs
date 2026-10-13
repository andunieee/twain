//! One file of the diff: a clickable header and a meld-style side-by-side body.
//!
//! Each side is a GtkSourceView holding the *whole* file, so syntax highlighting and line
//! numbers are correct. Unchanged regions are hidden with an invisible tag, and lines are
//! padded so both sides stay aligned; the curved links between the sides are drawn in a
//! narrow drawing area in the middle.

use crate::diff::{self, Block, ChunkKind, Layout};
use crate::git::{Content, FileChange};
use gtk::prelude::*;
use gtk::{cairo, gdk, graphene};
use sourceview5::prelude::*;
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::{Rc, Weak};

const GAP_HEIGHT: i32 = 24;
const LINKMAP_WIDTH: i32 = 44;

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
}

pub struct FileView {
    pub widget: gtk::Box,
    pub change: FileChange,
    shared: Rc<Shared>,
    /// Sanitized old/new text; `None` for binary files and files without content changes.
    texts: Option<[String; 2]>,
    /// The text views are expensive, so they're only built once the file scrolls into view.
    sides: OnceCell<[Side; 2]>,
    search_contexts: OnceCell<[sourceview5::SearchContext; 2]>,
    blocks: Vec<Block>,
    collapsed: Cell<bool>,
    pub layout: RefCell<Layout>,
    body: gtk::Overlay,
    placeholder: gtk::Box,
    linkmap: gtk::DrawingArea,
    decorations: gtk::DrawingArea,
    toggle_label: gtk::Label,
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

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("file-section");

        // Header
        let (added, removed) = blocks.iter().fold((0, 0), |(a, r), b| match b {
            Block::Change(c) => (a + c.new.len(), r + c.old.len()),
            _ => (a, r),
        });
        let header_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let status = gtk::Label::new(Some(&change.status.to_string()));
        status.add_css_class("status");
        status.add_css_class(status_class(change.status));
        header_box.append(&status);
        let title = match &change.old_path {
            Some(old) => format!("{old} → {}", change.path),
            None => change.path.clone(),
        };
        let path = gtk::Label::builder()
            .label(&title)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Start)
            .build();
        path.add_css_class("file-path");
        header_box.append(&path);
        if added > 0 {
            let l = gtk::Label::new(Some(&format!("+{added}")));
            l.add_css_class("stat-add");
            header_box.append(&l);
        }
        if removed > 0 {
            let l = gtk::Label::new(Some(&format!("−{removed}")));
            l.add_css_class("stat-del");
            header_box.append(&l);
        }
        let toggle_label = gtk::Label::new(None);
        toggle_label.add_css_class("dim-label");
        toggle_label.add_css_class("caption");
        header_box.append(&toggle_label);
        let header = gtk::Button::builder().child(&header_box).build();
        header.add_css_class("file-header");
        widget.append(&header);

        let texts = (is_text && has_changes).then_some([old_text, new_text]);
        let body = gtk::Overlay::new();
        let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        if texts.is_some() {
            header.set_tooltip_text(Some("Toggle between changes only and the full file"));
            body.set_child(Some(&placeholder));
        } else {
            toggle_label.set_visible(false);
            let msg = match (&change.old, &change.new) {
                (Content::Binary, _) | (_, Content::Binary) => "Binary file",
                (Content::TooLarge, _) | (_, Content::TooLarge) => "File too large to display",
                _ if change.old_path.is_some() => "Renamed without content changes",
                _ => "No content changes",
            };
            let l = gtk::Label::new(Some(msg));
            l.add_css_class("dim-label");
            l.add_css_class("placeholder");
            body.set_child(Some(&l));
        }
        widget.append(&body);

        let this = Rc::new_cyclic(|weak_self| FileView {
            widget,
            change,
            shared,
            texts,
            sides: OnceCell::new(),
            search_contexts: OnceCell::new(),
            blocks,
            collapsed: Cell::new(collapsed),
            layout: RefCell::default(),
            body,
            placeholder,
            linkmap: gtk::DrawingArea::new(),
            decorations: gtk::DrawingArea::new(),
            toggle_label,
            on_toggle: RefCell::default(),
            weak_self: weak_self.clone(),
        });
        if this.texts.is_some() {
            let weak = Rc::downgrade(&this);
            header.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.set_collapsed(!this.collapsed.get());
                }
            });
            this.update_toggle_label();
            this.relayout();
        }
        this
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
        self.linkmap
            .set_draw_func(with_weak(&self.weak_self, FileView::draw_linkmap));
        self.decorations
            .set_draw_func(with_weak(&self.weak_self, FileView::draw_decorations));

        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        self.linkmap.set_content_width(LINKMAP_WIDTH);
        self.linkmap.add_css_class("linkmap");
        hbox.append(&sides[0].scroller);
        hbox.append(&self.linkmap);
        hbox.append(&sides[1].scroller);
        let _ = self.sides.set(sides);
        self.relayout();
        self.body.set_child(Some(&hbox));
        self.decorations.set_can_target(false);
        self.body.add_overlay(&self.decorations);
    }

    pub fn is_collapsed(&self) -> bool {
        self.collapsed.get()
    }

    pub fn set_collapsed(&self, collapsed: bool) {
        if self.texts.is_none() || self.collapsed.get() == collapsed {
            return;
        }
        self.collapsed.set(collapsed);
        self.update_toggle_label();
        self.relayout();
        if let Some(f) = &*self.on_toggle.borrow() {
            f();
        }
    }

    fn update_toggle_label(&self) {
        let collapsed = self.collapsed.get();
        self.toggle_label.set_label(if collapsed {
            "Show full file"
        } else {
            "Show changes only"
        });
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

    /// y of the top of the diff body (where line y = 0 is) in `target`'s coordinates.
    pub fn body_y(&self, target: &impl IsA<gtk::Widget>) -> Option<f64> {
        self.body
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
        let layout = diff::layout(&self.blocks, self.collapsed.get(), lh, GAP_HEIGHT);
        if let Some([old, new]) = self.sides() {
            old.apply_layout(&layout.sides[0]);
            new.apply_layout(&layout.sides[1]);
            self.linkmap.set_content_height(layout.height);
            self.linkmap.queue_draw();
            self.decorations.queue_draw();
        } else {
            self.placeholder.set_height_request(layout.height);
        }
        *self.layout.borrow_mut() = layout;
    }

    /// The meld-style curved bands connecting each chunk on the left to its counterpart.
    fn draw_linkmap(&self, cr: &cairo::Context, width: i32, _height: i32) {
        let layout = self.layout.borrow();
        let w = width as f64;
        cr.set_line_width(1.0);
        for chunk in &layout.chunks {
            let (l0, l1) = (chunk.left.0 as f64 + 0.5, chunk.left.1 as f64 - 0.5);
            let (r0, r1) = (chunk.right.0 as f64 + 0.5, chunk.right.1 as f64 - 0.5);
            let (l1, r1) = (l1.max(l0), r1.max(r0));
            cr.move_to(0.0, l0);
            cr.curve_to(w * 0.5, l0, w * 0.5, r0, w, r0);
            cr.line_to(w, r1);
            cr.curve_to(w * 0.5, r1, w * 0.5, l1, 0.0, l1);
            cr.close_path();
            let (r, g, b) = kind_color(chunk.kind);
            cr.set_source_rgba(r, g, b, 0.28);
            let _ = cr.fill_preserve();
            cr.set_source_rgba(r, g, b, 0.85);
            let _ = cr.stroke();
        }
    }

    /// Chunk borders across both views, insertion markers and the "hidden lines" bands.
    fn draw_decorations(&self, cr: &cairo::Context, width: i32, _height: i32) {
        let (Some([old, new]), layout) = (self.sides(), self.layout.borrow()) else {
            return;
        };
        let area = &self.decorations;
        let bounds = |s: &Side| {
            s.scroller
                .compute_bounds(area)
                .map(|b| (b.x() as f64, (b.x() + b.width()) as f64))
                .unwrap_or((0.0, 0.0))
        };
        let (left, right) = (bounds(old), bounds(new));
        let hline = |(x0, x1): (f64, f64), y: f64| {
            cr.move_to(x0, y);
            cr.line_to(x1, y);
        };

        cr.set_line_width(1.0);
        for chunk in &layout.chunks {
            let (r, g, b) = kind_color(chunk.kind);
            for ((y0, y1), span) in [(chunk.left, left), (chunk.right, right)] {
                hline(span, y0 as f64 + 0.5);
                if y1 > y0 {
                    hline(span, y1 as f64 - 0.5);
                }
            }
            cr.set_source_rgba(r, g, b, 0.7);
            let _ = cr.stroke();
        }

        let fg = area.color();
        let w = width as f64;
        for gap in &layout.gaps {
            let (y, h) = (gap.y as f64, gap.height as f64);
            cr.rectangle(0.0, y, w, h);
            cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.05);
            let _ = cr.fill();
            cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.25);
            cr.set_dash(&[3.0, 3.0], 0.0);
            hline((0.0, w), y + 0.5);
            hline((0.0, w), y + h - 0.5);
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

fn with_weak(
    weak: &Weak<FileView>,
    f: fn(&FileView, &cairo::Context, i32, i32),
) -> impl Fn(&gtk::DrawingArea, &cairo::Context, i32, i32) + 'static {
    let weak = weak.clone();
    move |_, cr, w, h| {
        if let Some(this) = weak.upgrade() {
            f(&this, cr, w, h)
        }
    }
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
