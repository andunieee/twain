//! Main window: file tree sidebar, the sequence of file diffs, and search.

use crate::comments::{self, Listener};
use crate::file_view::{self, FileView, Shared};
use crate::git::{self, Options};
use crate::tree::{self, Node};
use adw::prelude::*;
use gtk::{gio, glib, graphene};
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
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
    scroller: gtk::ScrolledWindow,
    content: gtk::Box,
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

impl Window {
    pub fn new(app: &adw::Application, opts: Options) -> Rc<Window> {
        let title = adw::WindowTitle::new("Twain", "");
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));

        let search_button = gtk::ToggleButton::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text("Search (Ctrl+F)")
            .build();
        header.pack_end(&search_button);

        let menu = gio::Menu::new();
        menu.append(Some("Show All Files in Full"), Some("win.expand-all"));
        menu.append(Some("Show Only Changes"), Some("win.collapse-all"));
        menu.append(Some("Reload"), Some("win.reload"));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .build();
        header.pack_end(&menu_button);

        let reload_button = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Reload (F5)")
            .action_name("win.reload")
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
            .build();
        let next = gtk::Button::builder()
            .icon_name("go-down-symbolic")
            .tooltip_text("Next match (Enter)")
            .action_name("win.find-next")
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
        let tree_model = gtk::TreeListModel::new(tree_store.clone(), false, true, |item| {
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
        let sidebar = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&tree_view)
            .width_request(180)
            .build();

        // Diff content
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.add_css_class("diff-content");
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&content)
            .hexpand(true)
            .build();

        let paned = gtk::Paned::builder()
            .start_child(&sidebar)
            .end_child(&scroller)
            .position(280)
            .shrink_start_child(false)
            .resize_start_child(false)
            .build();

        let status = adw::StatusPage::new();
        let stack = gtk::Stack::new();
        stack.add_named(&adw::Spinner::new(), Some("loading"));
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
            scroller,
            content,
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
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            hbox.append(&gtk::Image::new());
            hbox.append(
                &gtk::Label::builder()
                    .xalign(0.0)
                    .hexpand(true)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .build(),
            );
            hbox.append(&gtk::Label::new(None));
            let expander = gtk::TreeExpander::builder().child(&hbox).build();
            item.set_child(Some(&expander));

            // Click a file to jump to it, double click to show it in full.
            let click = gtk::GestureClick::new();
            let (weak, item) = (weak.clone(), item.downgrade());
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
                match (node.file, n_press) {
                    (Some(i), 1) => this.scroll_to_file(i),
                    (Some(i), _) => this.open_file(i),
                    (None, 1) => row.set_expanded(!row.is_expanded()),
                    _ => {}
                }
            });
            hbox.add_controller(click);
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
            let status = label.next_sibling().and_downcast::<gtk::Label>().unwrap();
            label.set_label(&node.name);
            label.set_tooltip_text(Some(&node.name));
            if node.file.is_some() {
                icon.set_icon_name(Some("text-x-generic-symbolic"));
                status.set_label(&node.status.to_string());
                status.set_css_classes(&["status", file_view::status_class(node.status)]);
                status.set_visible(true);
            } else {
                icon.set_icon_name(Some("folder-symbolic"));
                status.set_visible(false);
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
            match node_of(&row).and_then(|n| n.file) {
                Some(i) => this.open_file(i),
                None => {
                    let row = row.downcast::<gtk::TreeListRow>().unwrap();
                    row.set_expanded(!row.is_expanded());
                }
            }
        });

        let weak = Rc::downgrade(self);
        self.scroller.vadjustment().connect_value_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.build_visible();
                this.sync_sidebar();
            }
        });
        let weak = Rc::downgrade(self);
        self.scroller
            .vadjustment()
            .connect_page_size_notify(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.build_visible();
                }
            });
        let weak = Rc::downgrade(self);
        *self.shared.on_line_height.borrow_mut() = Some(Box::new(move || {
            if let Some(this) = weak.upgrade() {
                for f in this.files.borrow().iter() {
                    f.relayout();
                }
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
        add("expand-all", |this| this.set_all_collapsed(false));
        add("collapse-all", |this| this.set_all_collapsed(true));
    }

    pub fn load(self: &Rc<Self>) {
        let scroll = self.scroller.vadjustment().value();
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
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        self.tree_store.remove_all();
        self.matches.borrow_mut().clear();
        self.current.set(None);
        self.tagged.set(None);
        self.synced_file.set(None);
        // Keep files that were open in full open after a reload.
        let expanded: std::collections::HashSet<String> = self
            .files
            .take()
            .iter()
            .filter(|f| !f.is_collapsed())
            .map(|f| f.change.path.clone())
            .collect();

        if cmp.files.is_empty() {
            self.show_status("emblem-ok-symbolic", "No changes", &cmp.description);
            return;
        }

        // Order files like the tree (directories first), so the sidebar reads top to bottom.
        let mut changes = cmp.files;
        let order = {
            let paths: Vec<_> = changes
                .iter()
                .map(|f| (f.path.as_str(), f.status))
                .collect();
            tree::file_order(&tree::build(&paths))
        };
        let mut slots: Vec<_> = changes.drain(..).map(Some).collect();
        let changes: Vec<git::FileChange> =
            order.iter().map(|&i| slots[i].take().unwrap()).collect();
        let paths: Vec<_> = changes
            .iter()
            .map(|f| (f.path.as_str(), f.status))
            .collect();
        let nodes = tree::build(&paths);

        let mut files = Vec::with_capacity(changes.len());
        // Build the first screenful right away; the rest is built as it scrolls into view.
        let mut eager_height = 0;
        for change in changes {
            let full = expanded.contains(&change.path);
            let fv = FileView::new(change, self.shared.clone(), !full);
            if eager_height < 2000 && scroll == 0.0 {
                fv.ensure_built();
                eager_height += fv.layout.borrow().height + 40;
            }
            let weak = Rc::downgrade(self);
            fv.connect_toggle(move || {
                if let Some(this) = weak.upgrade() {
                    this.refresh_search(false);
                }
            });
            self.content.append(&fv.widget);
            files.push(fv);
        }
        let mut by_path: std::collections::HashMap<String, Vec<comments::Comment>> =
            std::collections::HashMap::new();
        match comments::store().and_then(|s| s.list(&self.shared.root.borrow(), None)) {
            Ok(list) => list
                .into_iter()
                .for_each(|c| by_path.entry(c.path.clone()).or_default().push(c)),
            Err(e) => eprintln!("twain: {e}"),
        }
        for f in &files {
            f.set_comments(by_path.remove(&f.change.path).unwrap_or_default());
        }
        *self.files.borrow_mut() = files;
        for node in nodes {
            self.tree_store.append(&glib::BoxedAnyObject::new(node));
        }
        self.stack.set_visible_child_name("diff");

        let this = self.clone();
        glib::idle_add_local_once(move || {
            this.scroller.vadjustment().set_value(scroll);
            this.build_visible();
            this.sync_sidebar();
        });
    }

    fn set_all_collapsed(&self, collapsed: bool) {
        for f in self.files.borrow().iter() {
            f.set_collapsed(collapsed);
        }
    }

    fn file_top(&self, i: usize) -> Option<f64> {
        let files = self.files.borrow();
        let p = files
            .get(i)?
            .widget
            .compute_point(&self.content, &graphene::Point::zero())?;
        Some(p.y() as f64)
    }

    fn scroll_to_file(&self, i: usize) {
        if let Some(y) = self.file_top(i) {
            self.scroller.vadjustment().set_value(y);
        }
    }

    /// Shows the whole file and jumps to it.
    fn open_file(&self, i: usize) {
        if let Some(f) = self.files.borrow().get(i) {
            f.set_collapsed(!f.is_collapsed());
        }
        self.scroll_to_file(i);
    }

    /// Builds the text views of files in or near the viewport.
    fn build_visible(&self) {
        let adj = self.scroller.vadjustment();
        if adj.page_size() <= 0.0 {
            return;
        }
        let (lo, hi) = (
            adj.value() - adj.page_size(),
            adj.value() + 2.0 * adj.page_size(),
        );
        for f in self.files.borrow().iter() {
            let Some(p) = f
                .widget
                .compute_point(&self.content, &graphene::Point::zero())
            else {
                continue;
            };
            let top = p.y() as f64;
            if top > hi {
                break;
            }
            if top + f.widget.height() as f64 >= lo {
                f.ensure_built();
            }
        }
    }

    /// Selects the file at the top of the viewport in the sidebar.
    fn sync_sidebar(&self) {
        let top = self.scroller.vadjustment().value() + 1.0;
        let n = self.files.borrow().len();
        let mut current = None;
        for i in 0..n {
            match self.file_top(i) {
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
            let top = self.scroller.vadjustment().value();
            let first = (0..count).find(|&i| self.match_y(i).is_some_and(|y| y >= top));
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

    /// y of a match in content coordinates.
    fn match_y(&self, i: usize) -> Option<f64> {
        let matches = self.matches.borrow();
        let m = matches.get(i)?;
        let files = self.files.borrow();
        let fv = &files[m.file];
        let y = fv.layout.borrow().line_y(m.side, m.line);
        Some(fv.body_y(&self.content)? + y as f64)
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
                let top = self.scroller.vadjustment().value();
                let below = (0..n).find(|&i| self.match_y(i).is_some_and(|y| y >= top));
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
        let Some(y) = self.match_y(i) else { return };
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

        let vadj = self.scroller.vadjustment();
        let page = vadj.page_size();
        if y < vadj.value() + 20.0 || y > vadj.value() + page - 60.0 {
            vadj.set_value(y - page / 3.0);
        }
        let hadj = side.scroller.hadjustment();
        let x = side.view.iter_location(&start).x() as f64;
        if x < hadj.value() || x + 60.0 > hadj.value() + hadj.page_size() {
            hadj.set_value((x - hadj.page_size() / 3.0).max(0.0));
        }
    }
}
