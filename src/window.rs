//! Main window: file tree sidebar, the sequence of file diffs, and search.
//!
//! The diff is two columns, old files on the left and new ones on the right, each scrolling
//! on its own like meld's panes, with the link map between them. Scrolling one column
//! scrolls the other to the corresponding place: the point at the middle of the view (the
//! top or bottom near the ends) is mapped across through each file's layout anchors.

use crate::comments::{self, Listener};
use crate::diff;
use crate::file_view::{self, FileView, Shared};
use crate::git::{self, Options};
use crate::state;
use crate::tree::{self, Node};
use adw::prelude::*;
use gtk::{gdk, gio, glib, graphene};
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

struct Match {
    file: usize,
    side: usize,
    line: usize,
    cols: std::ops::Range<usize>,
}

pub struct Window {
    window: adw::ApplicationWindow,
    opts: Options,
    title: adw::WindowTitle,
    stack: gtk::Stack,
    status: adw::StatusPage,
    tree_store: gio::ListStore,
    tree_model: gtk::TreeListModel,
    selection: gtk::SingleSelection,
    tree_view: gtk::ListView,
    /// Paths, statuses and staging of the changed files, in file order, and the unchanged
    /// files.
    tree_paths: RefCell<(Vec<(String, char, Option<git::Staging>)>, Vec<String>)>,
    /// Directories whose unchanged files are shown, by full path ("" for the top level).
    unchanged_shown: RefCell<HashSet<String>>,
    /// The top level's row above the tree, for showing its unchanged files.
    root_row: gtk::Box,
    /// The old (left) and new (right) column.
    scrollers: [gtk::ScrolledWindow; 2],
    contents: [gtk::Box; 2],
    linkmap: gtk::DrawingArea,
    /// Overview of each column's changes, beside its scrollbar.
    filemaps: [gtk::DrawingArea; 2],
    /// The column last scrolled by the user, which the other follows.
    master: Cell<usize>,
    /// Set while one column scrolls the other.
    scroll_lock: Cell<bool>,
    sync_queued: Cell<bool>,
    files: RefCell<Vec<Rc<FileView>>>,
    shared: Rc<Shared>,
    synced_file: Cell<Option<usize>>,
    syncing: Cell<bool>,

    search_bar: gtk::SearchBar,
    search_entry: gtk::SearchEntry,
    search_label: gtk::Label,
    matches: RefCell<Vec<Match>>,
    current: Cell<Option<usize>>,
    /// (file, side) whose buffer has the current-match tag applied.
    tagged: Cell<Option<(usize, usize)>>,
    /// Receives the keys other processes write to the comment database.
    listener: Option<Listener>,
}

const LINKMAP_WIDTH: i32 = 44;
const FILEMAP_WIDTH: i32 = 14;

/// Where in the view the columns are synced, as a fraction of its height: the middle, but
/// moving to the top in the first half screen and to the bottom in the last one, so both
/// columns can reach their ends (meld's `calc_syncpoint`).
fn syncpoint(adj: &gtk::Adjustment) -> f64 {
    let half = adj.page_size() / 2.0;
    if half <= 0.0 {
        return 0.0;
    }
    let first = (adj.value() - adj.lower()) / half;
    let last = (adj.upper() - adj.page_size() - adj.value()) / half;
    0.5 * first.min(1.0) + 0.5 * (1.0 - last.clamp(0.0, 1.0))
}

fn style_scheme() -> Option<sourceview5::StyleScheme> {
    let dark = adw::StyleManager::default().is_dark();
    sourceview5::StyleSchemeManager::default().scheme(if dark { "Adwaita-dark" } else { "Adwaita" })
}

fn node_of(row: &glib::Object) -> Option<Rc<Node>> {
    let row = row.downcast_ref::<gtk::TreeListRow>()?;
    let obj = row.item().and_downcast::<glib::BoxedAnyObject>()?;
    let node = obj.borrow::<Rc<Node>>().clone();
    Some(node)
}

/// The "(+)" / "(-)" after a directory's name that shows or hides its unchanged files.
fn unchanged_toggle() -> gtk::Label {
    let toggle = gtk::Label::builder()
        .css_classes(["dim-label", "unchanged-toggle"])
        .cursor(&gdk::Cursor::from_name("pointer", None).unwrap())
        .build();
    let click = gtk::GestureClick::new();
    // Claim the click so it doesn't also expand or collapse the row.
    click.connect_pressed(|g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
    });
    toggle.add_controller(click);
    toggle
}

fn gesture_of(toggle: &impl IsA<gtk::Widget>) -> gtk::GestureClick {
    toggle.observe_controllers().item(0).and_downcast().unwrap()
}

fn set_unchanged_toggle(toggle: &gtk::Label, shown: Option<bool>) {
    toggle.set_visible(shown.is_some());
    let shown = shown.unwrap_or(false);
    toggle.set_label(if shown { "(-)" } else { "(+)" });
    toggle.set_tooltip_text(Some(if shown {
        "Hide unchanged files"
    } else {
        "Show unchanged files"
    }));
}

impl Window {
    pub fn new(app: &adw::Application, opts: Options) -> Rc<Window> {
        let title = adw::WindowTitle::new("Twain", "");
        let pointer = gdk::Cursor::from_name("pointer", None).unwrap();
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));

        let search_button = gtk::ToggleButton::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text("Search (Ctrl+F)")
            .cursor(&pointer)
            .build();
        header.pack_end(&search_button);

        let reload_button = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Reload (F5)")
            .action_name("win.reload")
            .cursor(&pointer)
            .build();
        header.pack_start(&reload_button);

        // Search bar
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Search the diff")
            .width_chars(32)
            .build();
        let search_label = gtk::Label::new(None);
        search_label.add_css_class("dim-label");
        search_label.add_css_class("numeric");
        let prev = gtk::Button::builder()
            .icon_name("go-up-symbolic")
            .tooltip_text("Previous match (Shift+Enter)")
            .action_name("win.find-prev")
            .cursor(&pointer)
            .build();
        let next = gtk::Button::builder()
            .icon_name("go-down-symbolic")
            .tooltip_text("Next match (Enter)")
            .action_name("win.find-next")
            .cursor(&pointer)
            .build();
        let search_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        search_box.append(&search_entry);
        search_box.append(&search_label);
        search_box.append(&prev);
        search_box.append(&next);
        let search_bar = gtk::SearchBar::builder()
            .child(&search_box)
            .show_close_button(true)
            .build();
        search_bar.connect_entry(&search_entry);
        search_button
            .bind_property("active", &search_bar, "search-mode-enabled")
            .bidirectional()
            .build();

        // Sidebar
        let tree_store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let tree_model = gtk::TreeListModel::new(tree_store.clone(), false, false, |item| {
            let node = item
                .downcast_ref::<glib::BoxedAnyObject>()?
                .borrow::<Rc<Node>>()
                .clone();
            if node.children.is_empty() {
                return None;
            }
            let store = gio::ListStore::new::<glib::BoxedAnyObject>();
            for child in &node.children {
                store.append(&glib::BoxedAnyObject::new(child.clone()));
            }
            Some(store.upcast())
        });
        let selection = gtk::SingleSelection::builder()
            .model(&tree_model)
            .autoselect(false)
            .can_unselect(true)
            .build();
        let factory = gtk::SignalListItemFactory::new();
        let tree_view = gtk::ListView::new(Some(selection.clone()), Some(factory.clone()));
        tree_view.add_css_class("navigation-sidebar");
        let root_row = gtk::Box::builder()
            .spacing(6)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        root_row.append(&gtk::Image::from_icon_name("folder-symbolic"));
        root_row.append(
            &gtk::Label::builder()
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .build(),
        );
        root_row.append(&unchanged_toggle());
        // Tree rows use one too.
        root_row.set_cursor_from_name(Some("pointer"));
        let tree_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&tree_view)
            .vexpand(true)
            .build();
        let sidebar = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .width_request(180)
            .build();
        sidebar.append(&root_row);
        sidebar.append(&tree_scroller);

        // Diff content
        let contents = [(); 2].map(|_| {
            let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
            content.add_css_class("diff-content");
            content
        });
        let scrollers = contents.each_ref().map(|content| {
            // Otherwise clicking a line scrolls its whole file's text view into view.
            let viewport = gtk::Viewport::builder()
                .scroll_to_focus(false)
                .child(content)
                .build();
            gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                // A permanent scrollbar rather than an indicator that only shows on hover.
                .overlay_scrolling(false)
                .child(&viewport)
                .hexpand(true)
                .build()
        });
        // Scrollbars on the outer edges, so both sides can sit against the link map.
        scrollers[0].set_placement(gtk::CornerType::TopRight);
        let linkmap = gtk::DrawingArea::builder()
            .content_width(LINKMAP_WIDTH)
            .build();
        linkmap.add_css_class("linkmap");
        let filemaps = [(); 2].map(|_| {
            let map = gtk::DrawingArea::builder()
                .content_width(FILEMAP_WIDTH)
                .visible(false)
                .cursor(&pointer)
                .build();
            map.add_css_class("filemap");
            map
        });
        let columns = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        columns.append(&filemaps[0]);
        columns.append(&scrollers[0]);
        columns.append(&linkmap);
        columns.append(&scrollers[1]);
        columns.append(&filemaps[1]);

        let paned = gtk::Paned::builder()
            .start_child(&sidebar)
            .end_child(&columns)
            .position(280)
            .shrink_start_child(false)
            .resize_start_child(false)
            .build();

        let status = adw::StatusPage::new();
        let stack = gtk::Stack::new();
        stack.add_named(&gtk::Spinner::new(), Some("loading"));
        stack.add_named(&status, Some("status"));
        stack.add_named(&paned, Some("diff"));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&search_bar);
        toolbar.set_content(Some(&stack));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Twain")
            .default_width(1400)
            .default_height(900)
            .content(&toolbar)
            .build();

        let search_settings = sourceview5::SearchSettings::new();
        search_settings.set_wrap_around(false);
        let shared = Rc::new(Shared {
            line_height: Cell::new(0),
            scheme: RefCell::new(style_scheme()),
            search_settings,
            on_line_height: RefCell::default(),
            on_relayout: RefCell::default(),
            on_reload: RefCell::default(),
            on_staged: RefCell::default(),
            root: RefCell::default(),
            author: glib::user_name().to_string_lossy().into_owned(),
        });
        let listener = Listener::bind()
            .inspect_err(|e| eprintln!("twain: won't see comments from other processes: {e}"))
            .ok();

        let this = Rc::new(Window {
            window,
            opts,
            title,
            stack,
            status,
            tree_store,
            tree_model,
            selection,
            tree_view,
            tree_paths: RefCell::default(),
            unchanged_shown: RefCell::default(),
            root_row,
            scrollers,
            contents,
            linkmap,
            filemaps,
            master: Cell::new(0),
            scroll_lock: Cell::new(false),
            sync_queued: Cell::new(false),
            files: RefCell::default(),
            shared,
            synced_file: Cell::new(None),
            syncing: Cell::new(false),
            search_bar,
            search_entry,
            search_label,
            matches: RefCell::default(),
            current: Cell::new(None),
            tagged: Cell::new(None),
            listener,
        });
        // Widgets only hold weak references to this struct; keep it alive as long as the window.
        let keep_alive = RefCell::new(Some(this.clone()));
        this.window
            .connect_destroy(move |_| drop(keep_alive.take()));

        this.setup_tree_factory(&factory);
        this.setup_listener();
        this.setup_signals();
        this.setup_actions();
        this
    }

    pub fn present(&self) {
        self.window.present();
    }

    fn setup_tree_factory(self: &Rc<Self>, factory: &gtk::SignalListItemFactory) {
        let weak = Rc::downgrade(self);
        let toggle = self.root_row.last_child().unwrap();
        gesture_of(&toggle).connect_released(move |_, _, _, _| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            let shown = this.unchanged_shown.borrow().contains("");
            if shown {
                this.unchanged_shown.borrow_mut().clear();
            } else {
                this.unchanged_shown.borrow_mut().insert(String::new());
            }
            this.fill_tree();
        });

        // Right click the top level folder name to hide or choose how much to show for all
        // the files under it.
        let click = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |gesture, _, x, y| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            let mut nodes = Vec::new();
            let mut pos = 0;
            // Top level rows hold the root's children, their nodes cover the whole tree.
            while let Some(item) = this.tree_model.item(pos) {
                if let Some(row) = item
                    .downcast_ref::<gtk::TreeListRow>()
                    .filter(|row| row.depth() == 0)
                    .and_then(|row| node_of(row.upcast_ref()))
                {
                    nodes.push(row);
                }
                pos += 1;
            }
            let files = tree::file_order(&nodes);
            if files.is_empty() {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            this.show_file_menu(files, this.root_row.upcast_ref::<gtk::Widget>(), x, y, true);
        });
        self.root_row.add_controller(click);

        let weak = Rc::downgrade(self);
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            hbox.set_cursor_from_name(Some("pointer"));
            hbox.append(&gtk::Image::new());
            hbox.append(
                &gtk::Label::builder()
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .build(),
            );
            let toggle = unchanged_toggle();
            hbox.append(&toggle);
            hbox.append(
                &gtk::Label::builder()
                    .hexpand(true)
                    .halign(gtk::Align::End)
                    .build(),
            );
            let expander = gtk::TreeExpander::builder().child(&hbox).build();
            item.set_child(Some(&expander));

            // Click a file to jump to it, double click to show it in full.
            let click = gtk::GestureClick::new();
            let (weak, item) = (weak.clone(), item.downgrade());
            let (toggle_weak, toggle_item) = (weak.clone(), item.clone());
            let (menu_weak, menu_item) = (weak.clone(), item.clone());
            click.connect_released(move |_, n_press, _, _| {
                let (Some(this), Some(item)) = (weak.upgrade(), item.upgrade()) else {
                    return;
                };
                let Some(row) = item.item().and_downcast::<gtk::TreeListRow>() else {
                    return;
                };
                let Some(node) = node_of(row.upcast_ref()) else {
                    return;
                };
                match (node.dir, node.file, n_press) {
                    (true, _, 1) => row.set_expanded(!row.is_expanded()),
                    (false, Some(i), 1) => this.scroll_to_file(i),
                    (false, Some(i), _) => this.open_file(i),
                    _ => {}
                }
            });
            hbox.add_controller(click);

            // Right click a file to hide it or choose how much of it to show, or a directory
            // to do it to all the files under it.
            let click = gtk::GestureClick::builder()
                .button(gdk::BUTTON_SECONDARY)
                .build();
            click.connect_pressed(move |gesture, _, x, y| {
                let (Some(this), Some(item)) = (menu_weak.upgrade(), menu_item.upgrade()) else {
                    return;
                };
                let Some(node) = item.item().and_then(|row| node_of(&row)) else {
                    return;
                };
                let files = match node.file {
                    Some(i) => vec![i],
                    None => tree::file_order(&node.children),
                };
                if files.is_empty() {
                    return;
                }
                gesture.set_state(gtk::EventSequenceState::Claimed);
                if let Some(widget) = gesture.widget() {
                    this.show_file_menu(files, &widget, x, y, false);
                }
            });
            hbox.add_controller(click);

            let click = gesture_of(&toggle);
            click.connect_released(move |_, _, _, _| {
                let (Some(this), Some(item)) = (toggle_weak.upgrade(), toggle_item.upgrade())
                else {
                    return;
                };
                if let Some(node) = item.item().and_then(|row| node_of(&row)) {
                    this.toggle_unchanged(&node);
                }
            });
        });
        factory.connect_bind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row = item.item().and_downcast::<gtk::TreeListRow>().unwrap();
            let expander = item.child().and_downcast::<gtk::TreeExpander>().unwrap();
            expander.set_list_row(Some(&row));
            let Some(node) = node_of(row.upcast_ref()) else {
                return;
            };
            let hbox = expander.child().unwrap();
            let icon = hbox.first_child().and_downcast::<gtk::Image>().unwrap();
            let label = icon.next_sibling().and_downcast::<gtk::Label>().unwrap();
            let toggle = label.next_sibling().and_downcast::<gtk::Label>().unwrap();
            let status = toggle.next_sibling().and_downcast::<gtk::Label>().unwrap();
            set_unchanged_toggle(&toggle, node.unchanged_shown);
            label.set_label(&node.name);
            label.set_tooltip_text(Some(&node.name));
            // Unchanged files (and directories without changes) are dimmed and inert.
            hbox.set_css_classes(if node.changed { &[] } else { &["unchanged"] });
            hbox.set_cursor_from_name((node.dir || node.changed).then_some("pointer"));
            if node.dir {
                icon.set_icon_name(Some("folder-symbolic"));
                status.set_visible(false);
            } else {
                icon.set_icon_name(Some("text-x-generic-symbolic"));
                status.set_label(&node.status.to_string());
                file_view::style_status(&status, node.status, node.staging);
                status.set_visible(node.changed);
            }
        });
    }

    /// Requeries the comments of files that other processes wrote to.
    fn setup_listener(self: &Rc<Self>) {
        let Some(listener) = &self.listener else {
            return;
        };
        let socket = match listener
            .dup_fd()
            .map_err(|e| e.to_string())
            .and_then(|fd| gio::Socket::from_fd(fd).map_err(|e| e.to_string()))
        {
            Ok(s) => s,
            Err(e) => return eprintln!("twain: won't see comments from other processes: {e}"),
        };
        let weak = Rc::downgrade(self);
        let source = gio::prelude::SocketExtManual::create_source(
            &socket,
            glib::IOCondition::IN,
            None::<&gio::Cancellable>,
            None,
            glib::Priority::DEFAULT,
            move |_, _| {
                let Some(this) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let keys = this
                    .listener
                    .as_ref()
                    .map(Listener::drain)
                    .unwrap_or_default();
                let root = this.shared.root.borrow().clone();
                let paths: std::collections::HashSet<String> = keys
                    .into_iter()
                    .filter(|k| k.root == root)
                    .map(|k| k.path)
                    .collect();
                for f in this.files.borrow().iter() {
                    if paths.contains(&f.change.path) {
                        f.reload_comments();
                    }
                }
                glib::ControlFlow::Continue
            },
        );
        source.attach(None);
    }

    fn setup_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.selection.connect_selection_changed(move |sel, _, _| {
            let Some(this) = weak.upgrade() else { return };
            if this.syncing.get() {
                return;
            }
            if let Some(i) = sel
                .selected_item()
                .as_ref()
                .and_then(node_of)
                .and_then(|n| n.file)
            {
                this.scroll_to_file(i);
            }
        });

        let weak = Rc::downgrade(self);
        self.tree_view.connect_activate(move |_, pos| {
            let Some(this) = weak.upgrade() else { return };
            let Some(row) = this.tree_model.item(pos) else {
                return;
            };
            let Some(node) = node_of(&row) else { return };
            if node.dir {
                let row = row.downcast::<gtk::TreeListRow>().unwrap();
                row.set_expanded(!row.is_expanded());
            } else if let Some(i) = node.file {
                this.open_file(i);
            }
        });

        for (side, scroller) in self.scrollers.iter().enumerate() {
            let adj = scroller.vadjustment();
            let weak = Rc::downgrade(self);
            adj.connect_value_changed(move |_| {
                let Some(this) = weak.upgrade() else { return };
                if !this.scroll_lock.get() {
                    this.master.set(side);
                    this.sync_scroll();
                }
                this.linkmap.queue_draw();
                this.filemaps[side].queue_draw();
                this.build_visible();
                this.sync_sidebar();
            });
            // The size of the content or the view changed, which moves the anchors. This is
            // emitted during allocation, so the other column follows once that's done.
            let weak = Rc::downgrade(self);
            adj.connect_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.queue_sync();
                    this.filemaps[side].queue_draw();
                    this.build_visible();
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.linkmap.set_draw_func(move |_, cr, w, h| {
            if let Some(this) = weak.upgrade() {
                this.draw_linkmap(cr, w, h);
            }
        });
        // Scrolling over the link map scrolls the column that leads.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |ctl, _, dy| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let adj = this.scrollers[this.master.get()].vadjustment();
            let step = match ctl.unit() {
                gdk::ScrollUnit::Wheel => adj.page_size().powf(2.0 / 3.0),
                _ => 1.0,
            };
            adj.set_value(adj.value() + dy * step);
            glib::Propagation::Stop
        });
        self.linkmap.add_controller(scroll);

        for (side, map) in self.filemaps.iter().enumerate() {
            let weak = Rc::downgrade(self);
            map.set_draw_func(move |_, cr, w, h| {
                if let Some(this) = weak.upgrade() {
                    this.draw_filemap(side, cr, w, h);
                }
            });
            // Clicking or dragging on the map centers the view there.
            let drag = gtk::GestureDrag::new();
            let start = Rc::new(Cell::new(0.0));
            let weak = Rc::downgrade(self);
            let start_y = start.clone();
            drag.connect_drag_begin(move |_, _, y| {
                start_y.set(y);
                if let Some(this) = weak.upgrade() {
                    this.scroll_to_filemap(side, y);
                }
            });
            let weak = Rc::downgrade(self);
            drag.connect_drag_update(move |_, _, dy| {
                if let Some(this) = weak.upgrade() {
                    this.scroll_to_filemap(side, start.get() + dy);
                }
            });
            map.add_controller(drag);
        }

        let weak = Rc::downgrade(self);
        *self.shared.on_line_height.borrow_mut() = Some(Box::new(move || {
            if let Some(this) = weak.upgrade() {
                for f in this.files.borrow().iter() {
                    f.relayout();
                }
            }
        }));
        let weak = Rc::downgrade(self);
        *self.shared.on_reload.borrow_mut() = Some(Box::new(move || {
            if let Some(this) = weak.upgrade() {
                this.load();
            }
        }));
        let weak = Rc::downgrade(self);
        *self.shared.on_staged.borrow_mut() = Some(Box::new(move |path| {
            if let Some(this) = weak.upgrade() {
                this.sync_staged(path);
            }
        }));
        let weak = Rc::downgrade(self);
        *self.shared.on_relayout.borrow_mut() = Some(Box::new(move || {
            if let Some(this) = weak.upgrade() {
                this.queue_sync();
            }
        }));

        let weak = Rc::downgrade(self);
        self.search_entry.connect_search_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.refresh_search(true);
            }
        });
        let weak = Rc::downgrade(self);
        self.search_entry.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.step_search(true);
            }
        });
        let weak = Rc::downgrade(self);
        self.search_entry.connect_next_match(move |_| {
            if let Some(this) = weak.upgrade() {
                this.step_search(true);
            }
        });
        let weak = Rc::downgrade(self);
        self.search_entry.connect_previous_match(move |_| {
            if let Some(this) = weak.upgrade() {
                this.step_search(false);
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let is_enter = matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter);
            if is_enter && mods.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                if let Some(this) = weak.upgrade() {
                    this.step_search(false);
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.search_entry.add_controller(keys);

        let weak = Rc::downgrade(self);
        self.search_bar
            .connect_search_mode_enabled_notify(move |bar| {
                if let Some(this) = weak.upgrade() {
                    this.refresh_search(bar.is_search_mode());
                }
            });

        let weak = Rc::downgrade(self);
        adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                let scheme = style_scheme();
                for f in this.files.borrow().iter() {
                    f.set_style_scheme(scheme.as_ref());
                }
                *this.shared.scheme.borrow_mut() = scheme;
            }
        });
    }

    fn setup_actions(self: &Rc<Self>) {
        let add = |name: &str, f: fn(&Rc<Window>)| {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    f(&this);
                }
            });
            self.window.add_action(&action);
        };
        add("find", |this| {
            this.search_bar.set_search_mode(true);
            this.search_entry.grab_focus();
            this.search_entry.select_region(0, -1);
        });
        add("find-next", |this| this.step_search(true));
        add("find-prev", |this| this.step_search(false));
        add("reload", |this| this.load());
    }

    pub fn load(self: &Rc<Self>) {
        let scroll = self.scrollers[0].vadjustment().value();
        if self.files.borrow().is_empty() {
            self.stack.set_visible_child_name("loading");
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let opts = this.opts.clone();
            match gio::spawn_blocking(move || git::load(&opts)).await {
                Ok(Ok(cmp)) => this.show(cmp, scroll),
                Ok(Err(err)) => {
                    this.show_status("dialog-error-symbolic", "Could not load diff", &err)
                }
                Err(_) => this.show_status(
                    "dialog-error-symbolic",
                    "Could not load diff",
                    "internal error",
                ),
            }
        });
    }

    fn show_status(&self, icon: &str, title: &str, description: &str) {
        self.status.set_icon_name(Some(icon));
        self.status.set_title(title);
        self.status
            .set_description(Some(&glib::markup_escape_text(description)));
        self.stack.set_visible_child_name("status");
    }

    fn show(self: &Rc<Self>, cmp: git::Comparison, scroll: f64) {
        let repo_name = cmp
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        self.title
            .set_title(repo_name.as_deref().unwrap_or("Twain"));
        self.title.set_subtitle(&cmp.description);
        *self.shared.root.borrow_mut() = cmp.root.to_string_lossy().into_owned();
        self.window.set_title(Some(&format!(
            "{} — Twain",
            repo_name.as_deref().unwrap_or("")
        )));

        // Clear any previous contents (reload).
        for content in &self.contents {
            while let Some(child) = content.first_child() {
                content.remove(&child);
            }
        }
        self.tree_store.remove_all();
        self.matches.borrow_mut().clear();
        self.current.set(None);
        self.tagged.set(None);
        self.synced_file.set(None);
        // Keep files that were open in full open, and folded ones folded, after a reload.
        let old_files = self.files.take();
        let paths_where = |pred: fn(&FileView) -> bool| -> HashSet<String> {
            old_files
                .iter()
                .filter(|f| pred(f))
                .map(|f| f.change.path.clone())
                .collect()
        };
        let expanded = paths_where(|f| !f.is_collapsed());
        let folded = paths_where(|f| f.is_folded());
        let known = paths_where(|_| true);
        drop(old_files);

        if cmp.files.is_empty() {
            self.show_status("emblem-ok-symbolic", "No changes", &cmp.description);
            return;
        }

        // Order files like the tree (directories first), so the sidebar reads top to bottom.
        let mut changes = cmp.files;
        let order = {
            let paths: Vec<_> = changes
                .iter()
                .map(|f| (f.path.as_str(), f.status, f.staging))
                .collect();
            tree::file_order(&tree::build(&paths, &[], &HashSet::new()).children)
        };
        let mut slots: Vec<_> = changes.drain(..).map(Some).collect();
        let changes: Vec<git::FileChange> =
            order.iter().map(|&i| slots[i].take().unwrap()).collect();
        *self.tree_paths.borrow_mut() = (
            changes
                .iter()
                .map(|f| (f.path.clone(), f.status, f.staging))
                .collect(),
            cmp.unchanged,
        );

        let default_full = state::show_full_files();
        let mut files = Vec::with_capacity(changes.len());
        // Build the first screenful right away; the rest is built as it scrolls into view.
        let mut eager_height = 0;
        for change in changes {
            let full = if known.contains(&change.path) {
                expanded.contains(&change.path)
            } else {
                default_full
            };
            // Files outside the paths given on the command line start out hidden.
            let unfocused = !cmp.focus.is_empty()
                && !git::in_focus(&cmp.focus, &change.path)
                && !change
                    .old_path
                    .as_ref()
                    .is_some_and(|p| git::in_focus(&cmp.focus, p));
            let fold =
                folded.contains(&change.path) || (unfocused && !known.contains(&change.path));
            let fv = FileView::new(change, self.shared.clone(), !full);
            fv.set_folded(fold);
            if !fold && eager_height < 2000 && scroll == 0.0 {
                fv.ensure_built();
                eager_height += fv.layout.borrow().height[0] + 40;
            }
            let weak = Rc::downgrade(self);
            fv.connect_toggle(move || {
                if let Some(this) = weak.upgrade() {
                    this.refresh_search(false);
                    this.update_filemaps();
                }
            });
            for (side, content) in self.contents.iter().enumerate() {
                content.append(fv.section(side));
            }
            files.push(fv);
        }
        let mut by_path: std::collections::HashMap<String, Vec<comments::Thread>> =
            std::collections::HashMap::new();
        match comments::store().and_then(|s| s.threads(&self.shared.root.borrow(), None)) {
            Ok(list) => list
                .into_iter()
                .for_each(|t| by_path.entry(t.comment.path.clone()).or_default().push(t)),
            Err(e) => eprintln!("twain: {e}"),
        }
        for f in &files {
            f.set_comments(by_path.remove(&f.change.path).unwrap_or_default());
        }
        *self.files.borrow_mut() = files;
        self.update_filemaps();
        self.fill_tree();
        self.stack.set_visible_child_name("diff");

        let this = self.clone();
        glib::idle_add_local_once(move || {
            this.master.set(0);
            this.scrollers[0].vadjustment().set_value(scroll);
            this.sync_scroll();
            this.build_visible();
            this.sync_sidebar();
        });
    }

    /// (Re)builds the sidebar tree, with the unchanged files of the directories they're shown
    /// in. Directories keep whether they're expanded; new ones start out expanded if they have
    /// changes or shown unchanged files.
    fn fill_tree(&self) {
        let mut expanded = HashMap::new();
        let mut pos = 0;
        while let Some(row) = self.tree_model.item(pos) {
            let row = row.downcast::<gtk::TreeListRow>().unwrap();
            if let Some(n) = node_of(row.upcast_ref()).filter(|n| n.dir) {
                expanded.insert(n.chain.last().unwrap().clone(), row.is_expanded());
            }
            pos += 1;
        }
        let adj = self.tree_view.vadjustment().unwrap();
        let scroll = adj.value();

        let paths = self.tree_paths.borrow();
        let changed: Vec<_> = paths
            .0
            .iter()
            .map(|(p, s, st)| (p.as_str(), *s, *st))
            .collect();
        let unchanged: Vec<_> = paths.1.iter().map(String::as_str).collect();
        let root = tree::build(&changed, &unchanged, &self.unchanged_shown.borrow());
        let label = self.root_row.first_child().unwrap().next_sibling().unwrap();
        let name = std::path::Path::new(&*self.shared.root.borrow())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        label.downcast_ref::<gtk::Label>().unwrap().set_label(&name);
        let toggle = label.next_sibling().and_downcast::<gtk::Label>().unwrap();
        set_unchanged_toggle(&toggle, root.unchanged_shown);
        self.root_row.set_visible(root.unchanged_shown.is_some());

        self.tree_store.remove_all();
        for node in root.children {
            self.tree_store.append(&glib::BoxedAnyObject::new(node));
        }
        // Expanding a row inserts its children right after it, so they're visited too.
        let mut pos = 0;
        while let Some(row) = self.tree_model.item(pos) {
            if let Some(n) = node_of(&row).filter(|n| n.dir) {
                let path = n.chain.last().unwrap();
                let expand = expanded
                    .get(path)
                    .copied()
                    .unwrap_or(n.changed || n.unchanged_shown == Some(true));
                row.downcast::<gtk::TreeListRow>()
                    .unwrap()
                    .set_expanded(expand);
            }
            pos += 1;
        }
        drop(paths);
        let weak = self.tree_view.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(adj) = weak.upgrade().and_then(|v| v.vadjustment()) {
                adj.set_value(scroll);
            }
        });
        self.synced_file.set(None);
        self.sync_sidebar();
    }

    /// One file's index changed in place (a stage or unstage): refresh just its sidebar row.
    fn sync_staged(self: &Rc<Self>, path: &str) {
        let (status, staging) = {
            let files = self.files.borrow();
            let Some(f) = files.iter().find(|f| f.change.path == path) else {
                return;
            };
            f.staging_state()
        };
        {
            let mut paths = self.tree_paths.borrow_mut();
            if let Some(i) = paths.0.iter().position(|(p, _, _)| p == path) {
                paths.0[i] = (path.to_string(), status, staging);
            }
        }
        self.fill_tree();
    }

    /// Shows or hides the unchanged files of a directory. Showing them shows those of all
    /// the directories merged into its row, and expands it; hiding them also hides those of
    /// the directories inside.
    fn toggle_unchanged(&self, node: &Node) {
        let Some(shown) = node.unchanged_shown else {
            return;
        };
        {
            let mut set = self.unchanged_shown.borrow_mut();
            if shown {
                let first = &node.chain[0];
                let prefix = format!("{first}/");
                set.retain(|p| p != first && (first.is_empty() || !p.starts_with(&prefix)));
            } else {
                set.extend(node.chain.iter().cloned());
            }
        }
        if !shown && let Some(row) = self.find_row(node.chain.last().unwrap()) {
            row.set_expanded(true);
        }
        self.fill_tree();
    }

    /// The tree row of a directory, by full path.
    fn find_row(&self, path: &str) -> Option<gtk::TreeListRow> {
        let mut pos = 0;
        while let Some(row) = self.tree_model.item(pos) {
            if node_of(&row).is_some_and(|n| n.dir && n.chain.last().unwrap() == path) {
                return row.downcast().ok();
            }
            pos += 1;
        }
        None
    }

    /// y of the top of a file in a column.
    fn file_top(&self, side: usize, i: usize) -> Option<f64> {
        let files = self.files.borrow();
        let p = files
            .get(i)?
            .section(side)
            .compute_point(&self.contents[side], &graphene::Point::zero())?;
        Some(p.y() as f64)
    }

    /// Scrolls one column to `y`, and the other one along.
    fn scroll_column(&self, side: usize, y: f64) {
        self.master.set(side);
        self.scrollers[side].vadjustment().set_value(y);
        self.sync_scroll();
    }

    fn scroll_to_file(&self, i: usize) {
        // Scroll the current (right) column; the old one follows via the diff anchors.
        if let Some(y) = self.file_top(1, i) {
            self.scroll_column(1, y);
        }
    }

    /// Shows the whole file and jumps to it.
    fn open_file(&self, i: usize) {
        if let Some(f) = self.files.borrow().get(i) {
            f.set_collapsed(!f.is_collapsed());
        }
        self.scroll_to_file(i);
    }

    /// The sidebar's context menu for files (one, or those under a directory): hide them, or
    /// show them in full or as changes only.
    ///
    /// From a project's root folder, the choice of full files or changes only is also remembered
    /// as the default for the next runs.
    fn show_file_menu(
        self: &Rc<Self>,
        files: Vec<usize>,
        parent: &gtk::Widget,
        x: f64,
        y: f64,
        root: bool,
    ) {
        let mode_of = |f: &FileView| match (f.is_folded(), f.is_collapsed()) {
            (true, _) => "hidden",
            (false, true) => "changes",
            (false, false) => "full",
        };
        let (mode, has_texts) = {
            let all = self.files.borrow();
            let fs: Vec<_> = files.iter().filter_map(|&i| all.get(i)).collect();
            let Some(first) = fs.first() else { return };
            let mode = mode_of(first);
            // No choice is marked when the files differ.
            let mode = if fs.iter().all(|f| mode_of(f) == mode) {
                mode
            } else {
                ""
            };
            (mode, fs.iter().any(|f| f.has_texts()))
        };
        let many = files.len() > 1;
        let others_hidden = {
            let all = self.files.borrow();
            all.iter()
                .enumerate()
                .any(|(i, f)| f.is_folded() && !files.contains(&i))
        };
        let these = files.clone();
        let action = gio::SimpleAction::new_stateful(
            "mode",
            Some(glib::VariantTy::STRING),
            &mode.to_variant(),
        );
        let weak = Rc::downgrade(self);
        action.connect_activate(move |action, target| {
            let Some(this) = weak.upgrade() else { return };
            let Some(mode) = target.and_then(|t| t.str()) else {
                return;
            };
            action.set_state(&mode.to_variant());
            if root && mode != "hidden" {
                state::set_show_full_files(mode == "full");
            }
            for &i in &files {
                let Some(f) = this.files.borrow().get(i).cloned() else {
                    continue;
                };
                f.set_folded(mode == "hidden");
                if mode != "hidden" {
                    f.set_collapsed(mode == "changes");
                }
            }
            if mode != "hidden" {
                this.scroll_to_file(files[0]);
            }
        });
        let group = gio::SimpleActionGroup::new();
        group.add_action(&action);

        let only = gio::SimpleAction::new("only", None);
        let weak = Rc::downgrade(self);
        only.connect_activate(move |_, _| {
            let Some(this) = weak.upgrade() else { return };
            let all: Vec<_> = this.files.borrow().clone();
            for (i, f) in all.iter().enumerate() {
                f.set_folded(!these.contains(&i));
            }
            this.scroll_to_file(these[0]);
        });
        group.add_action(&only);
        let unhide = gio::SimpleAction::new("unhide", None);
        let weak = Rc::downgrade(self);
        unhide.connect_activate(move |_, _| {
            let Some(this) = weak.upgrade() else { return };
            let all: Vec<_> = this.files.borrow().clone();
            for f in &all {
                f.set_folded(false);
            }
        });
        group.add_action(&unhide);

        let menu = gio::Menu::new();
        menu.append(
            Some(if many { "Hide All" } else { "Hide" }),
            Some("file.mode::hidden"),
        );
        if has_texts {
            menu.append(
                Some(if many {
                    "Show Full Files"
                } else {
                    "Show Full File"
                }),
                Some("file.mode::full"),
            );
            menu.append(Some("Show Changes Only"), Some("file.mode::changes"));
        } else {
            menu.append(
                Some(if many { "Show All" } else { "Show" }),
                Some("file.mode::changes"),
            );
        }

        let section = gio::Menu::new();
        section.append(
            Some(if many {
                "Show Only These Files"
            } else {
                "Show Only This File"
            }),
            Some("file.only"),
        );
        if others_hidden {
            section.append(Some("Show All Files"), Some("file.unhide"));
        }
        menu.append_section(None, &section);

        parent.insert_action_group("file", Some(&group));
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(parent);
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|popover| {
            // Not right away: the chosen item is activated after the menu closes.
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
    }

    /// Corresponding heights of the two columns, increasing on both, in content coordinates:
    /// the top of each file's sections and its layout's anchors.
    fn anchors(&self) -> Vec<[f64; 2]> {
        let mut anchors = vec![[0.0, 0.0]];
        for f in self.files.borrow().iter() {
            let tops = [0, 1].map(|s| {
                f.section(s)
                    .compute_point(&self.contents[s], &graphene::Point::zero())
                    .map_or(0.0, |p| p.y() as f64)
            });
            anchors.push(tops);
            // Folded, there's only the header.
            let (Some(l), Some(r)) = (
                f.body_y(0, &self.contents[0]),
                f.body_y(1, &self.contents[1]),
            ) else {
                continue;
            };
            let layout = f.layout.borrow();
            anchors.extend(
                layout
                    .anchors
                    .iter()
                    .map(|&[a, b]| [l + a as f64, r + b as f64]),
            );
        }
        anchors.push(self.contents.each_ref().map(|c| c.height() as f64));
        // Allocations can lag behind; keep it monotonic.
        for i in 1..anchors.len() {
            let prev = anchors[i - 1];
            anchors[i] = [anchors[i][0].max(prev[0]), anchors[i][1].max(prev[1])];
        }
        anchors
    }

    /// Scrolls the other column to follow the master one, like meld's `_sync_vscroll`.
    fn sync_scroll(&self) {
        if self.scroll_lock.get() {
            return;
        }
        let master = self.master.get();
        let src = self.scrollers[master].vadjustment();
        let dst = self.scrollers[1 - master].vadjustment();
        let point = syncpoint(&src);
        let y = src.value() + src.page_size() * point;
        let other = diff::map_y(&self.anchors(), master, y);
        let max = (dst.upper() - dst.page_size()).max(dst.lower());
        let value = (other - dst.page_size() * point)
            .clamp(dst.lower(), max)
            .floor();
        self.scroll_lock.set(true);
        dst.set_value(value);
        self.scroll_lock.set(false);
        self.linkmap.queue_draw();
    }

    /// Syncs the columns once the current allocation is done.
    fn queue_sync(self: &Rc<Self>) {
        self.linkmap.queue_draw();
        for map in &self.filemaps {
            map.queue_draw();
        }
        if self.sync_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(this) = weak.upgrade() {
                this.sync_queued.set(false);
                this.sync_scroll();
            }
        });
    }

    fn draw_linkmap(&self, cr: &gtk::cairo::Context, width: i32, height: i32) {
        let (w, h) = (width as f64, height as f64);
        let values = self.scrollers.each_ref().map(|s| s.vadjustment().value());
        // Where each column's top is in the link map; normally both at 0.
        let offsets = [0, 1].map(|s| {
            self.scrollers[s]
                .compute_point(&self.linkmap, &graphene::Point::zero())
                .map_or(0.0, |p| p.y() as f64)
        });
        for f in self.files.borrow().iter() {
            let mut tops = [0.0; 2];
            let mut bottoms = [0.0; 2];
            for s in 0..2 {
                let section = f.section(s);
                let Some(p) = section.compute_point(&self.contents[s], &graphene::Point::zero())
                else {
                    continue;
                };
                tops[s] = p.y() as f64 - values[s] + offsets[s];
                bottoms[s] = tops[s] + section.height() as f64;
            }
            if tops[0] > h && tops[1] > h {
                break;
            }
            if bottoms[0] < 0.0 && bottoms[1] < 0.0 {
                continue;
            }
            f.draw_links(cr, w, h, tops);
        }
    }

    /// Shows the file maps while any file is shown in full; with only changes shown,
    /// the whole column is changes and the map would say nothing.
    fn update_filemaps(&self) {
        let any_full = self.files.borrow().iter().any(|f| !f.is_collapsed());
        for map in &self.filemaps {
            map.set_visible(any_full);
            map.queue_draw();
        }
    }

    /// Draws one column's file map: its whole content scaled to the map's height, with a
    /// mark for each change and a box around what's in view.
    fn draw_filemap(&self, side: usize, cr: &gtk::cairo::Context, width: i32, height: i32) {
        let adj = self.scrollers[side].vadjustment();
        let total = adj.upper() - adj.lower();
        if total <= 0.0 {
            return;
        }
        let (w, scale) = (width as f64, height as f64 / total);
        // Divider between one file's change marks and the next file's,
        // in the middle of the gap between the two sections.
        let fg = self.filemaps[side].color();
        let (fr, fgc, fb) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
        let mut prev_bottom = f64::NAN;
        for f in self.files.borrow().iter() {
            let section = f.section(side);
            let top = match section.compute_point(&self.contents[side], &graphene::Point::zero()) {
                Some(p) => p.y() as f64,
                None => continue,
            };
            let bottom = top + section.height() as f64;
            if prev_bottom.is_finite() {
                let mid = ((prev_bottom + top) / 2.0 * scale).floor() + 0.5;
                if mid > 0.5 && mid < height as f64 - 0.5 {
                    cr.rectangle(0.0, mid, w, 1.0);
                    cr.set_source_rgba(fr, fgc, fb, 0.35);
                    let _ = cr.fill();
                }
            }
            prev_bottom = bottom;
            let Some(body) = f.body_y(side, &self.contents[side]) else {
                continue;
            };
            for chunk in &f.layout.borrow().chunks {
                let (top, bottom) = if side == 0 { chunk.left } else { chunk.right };
                let y0 = ((body + top as f64) * scale).floor();
                let y1 = ((body + bottom as f64) * scale).ceil().max(y0 + 2.0);
                let (r, g, b) = file_view::kind_color(chunk.kind);
                cr.set_source_rgba(r, g, b, 0.9);
                cr.rectangle(2.0, y0, w - 4.0, y1 - y0);
                let _ = cr.fill();
            }
        }
        let fg = self.filemaps[side].color();
        let (r, g, b) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
        let y0 = ((adj.value() - adj.lower()) * scale).floor() + 0.5;
        let h = (adj.page_size() * scale).max(4.0).floor() - 1.0;
        cr.rectangle(0.5, y0, w - 1.0, h);
        cr.set_source_rgba(r, g, b, 0.12);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(r, g, b, 0.5);
        cr.set_line_width(1.0);
        let _ = cr.stroke();
    }

    /// Scrolls a column so the point at `y` in its file map is in the middle of the view.
    fn scroll_to_filemap(&self, side: usize, y: f64) {
        let adj = self.scrollers[side].vadjustment();
        let height = self.filemaps[side].height() as f64;
        if height <= 0.0 {
            return;
        }
        let content = adj.lower() + y / height * (adj.upper() - adj.lower());
        self.scroll_column(side, content - adj.page_size() / 2.0);
    }

    /// Builds the text views of files in or near the viewport of either column.
    fn build_visible(&self) {
        for (side, scroller) in self.scrollers.iter().enumerate() {
            let adj = scroller.vadjustment();
            if adj.page_size() <= 0.0 {
                continue;
            }
            let (lo, hi) = (
                adj.value() - adj.page_size(),
                adj.value() + 2.0 * adj.page_size(),
            );
            for f in self.files.borrow().iter() {
                let section = f.section(side);
                let Some(p) = section.compute_point(&self.contents[side], &graphene::Point::zero())
                else {
                    continue;
                };
                let top = p.y() as f64;
                if top > hi {
                    break;
                }
                if top + section.height() as f64 >= lo {
                    f.ensure_built();
                }
            }
        }
    }

    /// Selects the file at the top of the viewport in the sidebar.
    fn sync_sidebar(&self) {
        let side = self.master.get();
        let top = self.scrollers[side].vadjustment().value() + 1.0;
        let n = self.files.borrow().len();
        let mut current = None;
        for i in 0..n {
            match self.file_top(side, i) {
                Some(y) if y <= top => current = Some(i),
                Some(_) => break,
                None => {}
            }
        }
        if current.is_none() || current == self.synced_file.get() {
            return;
        }
        self.synced_file.set(current);
        for pos in 0..self.tree_model.n_items() {
            let Some(row) = self.tree_model.item(pos) else {
                continue;
            };
            if node_of(&row).and_then(|n| n.file) == current {
                self.syncing.set(true);
                self.selection.set_selected(pos);
                self.tree_view
                    .scroll_to(pos, gtk::ListScrollFlags::NONE, None);
                self.syncing.set(false);
                break;
            }
        }
    }

    /// Recomputes the match list. With `jump`, moves to the first match below the viewport top.
    fn refresh_search(&self, jump: bool) {
        let text = self.search_entry.text();
        let active = self.search_bar.is_search_mode() && !text.is_empty();
        self.shared
            .search_settings
            .set_search_text(active.then_some(text.as_str()));
        let files = self.files.borrow();
        let mut matches = Vec::new();
        if active {
            for (fi, fv) in files.iter().enumerate() {
                let hits = fv.find(&text);
                matches.extend(hits.into_iter().map(|h| Match {
                    file: fi,
                    side: h.side,
                    line: h.line,
                    cols: h.cols,
                }));
            }
            matches.sort_by_cached_key(|m| {
                (
                    m.file,
                    files[m.file].layout.borrow().line_y(m.side, m.line),
                    m.side,
                    m.cols.start,
                )
            });
        }
        let count = matches.len();
        *self.matches.borrow_mut() = matches;
        self.current.set(None);
        drop(files);
        self.clear_current_tag();

        if active && count == 0 {
            self.search_entry.add_css_class("error");
        } else {
            self.search_entry.remove_css_class("error");
        }
        if jump && count > 0 {
            let first = (0..count).find(|&i| self.match_below_top(i));
            self.goto_match(first.unwrap_or(0));
        } else {
            self.update_search_label();
        }
    }

    fn update_search_label(&self) {
        let n = self.matches.borrow().len();
        let text = match (self.search_entry.text().is_empty(), n, self.current.get()) {
            (true, _, _) => String::new(),
            (false, 0, _) => "No results".into(),
            (false, n, Some(i)) => format!("{} of {n}", i + 1),
            (false, n, None) => format!("{n} matches"),
        };
        self.search_label.set_label(&text);
    }

    /// Column and y of a match in that column's content coordinates.
    fn match_y(&self, i: usize) -> Option<(usize, f64)> {
        let matches = self.matches.borrow();
        let m = matches.get(i)?;
        let files = self.files.borrow();
        let fv = &files[m.file];
        let y = fv.layout.borrow().line_y(m.side, m.line);
        Some((
            m.side,
            fv.body_y(m.side, &self.contents[m.side])? + y as f64,
        ))
    }

    /// Whether a match is below the top of the view of its column.
    fn match_below_top(&self, i: usize) -> bool {
        self.match_y(i)
            .is_some_and(|(side, y)| y >= self.scrollers[side].vadjustment().value())
    }

    fn step_search(&self, forward: bool) {
        if !self.search_bar.is_search_mode() {
            self.search_bar.set_search_mode(true);
            self.search_entry.grab_focus();
            return;
        }
        let n = self.matches.borrow().len();
        if n == 0 {
            return;
        }
        let i = match self.current.get() {
            Some(c) if forward => (c + 1) % n,
            Some(c) => (c + n - 1) % n,
            None => {
                let below = (0..n).find(|&i| self.match_below_top(i));
                match (forward, below) {
                    (true, b) => b.unwrap_or(0),
                    (false, Some(b)) => (b + n - 1) % n,
                    (false, None) => n - 1,
                }
            }
        };
        self.goto_match(i);
    }

    fn clear_current_tag(&self) {
        let Some((file, side)) = self.tagged.take() else {
            return;
        };
        let files = self.files.borrow();
        if let Some(side) = files
            .get(file)
            .and_then(|f| f.sides())
            .map(|sides| &sides[side])
        {
            let (start, end) = side.buffer.bounds();
            side.buffer.remove_tag(&side.current, &start, &end);
        }
    }

    fn goto_match(&self, i: usize) {
        self.clear_current_tag();
        self.current.set(Some(i));
        self.update_search_label();
        let Some((column, y)) = self.match_y(i) else {
            return;
        };
        let matches = self.matches.borrow();
        let m = &matches[i];
        let files = self.files.borrow();
        files[m.file].ensure_built();
        let side = &files[m.file].sides().unwrap()[m.side];
        let iter_at = |col: usize| {
            side.buffer
                .iter_at_line_offset(m.line as i32, col as i32)
                .unwrap_or_else(|| side.buffer.end_iter())
        };
        let (start, end) = (iter_at(m.cols.start), iter_at(m.cols.end));
        // Search highlights are tags too; keep ours on top.
        side.current
            .set_priority(side.buffer.tag_table().size() - 1);
        side.buffer.apply_tag(&side.current, &start, &end);
        self.tagged.set(Some((m.file, m.side)));

        let vadj = self.scrollers[column].vadjustment();
        let page = vadj.page_size();
        if y < vadj.value() + 20.0 || y > vadj.value() + page - 60.0 {
            self.scroll_column(column, y - page / 3.0);
        }
        let hadj = side.scroller.hadjustment();
        let x = side.view.iter_location(&start).x() as f64;
        if x < hadj.value() || x + 60.0 > hadj.value() + hadj.page_size() {
            hadj.set_value((x - hadj.page_size() / 3.0).max(0.0));
        }
    }
}
