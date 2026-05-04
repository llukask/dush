//! # dush GUI
//!
//! GTK4 front-end for the `dush` library. The window appears
//! immediately with the layout of every directory level the user has
//! visited so far; subtree sizes stream in afterwards from background
//! workers. Clicking a directory's expander triggers the same
//! enumerate-then-stream cycle for that subdirectory.
//!
//! Build with the `gui` feature and the GTK4 system libraries available
//! (the included `flake.nix` provides them):
//!
//! ```text
//! nix develop
//! cargo run --features gui --bin dush-gui -- ~/dev
//! ```
//!
//! ## Architecture
//!
//! The view is a `gtk::ColumnView` driven by a `gtk::TreeListModel` whose
//! root is a `gio::ListStore` of [`EntryItem`] objects. Per-cell rendering
//! is done with `gtk::SignalListItemFactory` instances, one per column.
//! This is the modern (GTK 4.10+) replacement for the `TreeView` /
//! `TreeStore` / `CellRenderer*` stack, which is deprecated.
//!
//! Lazy population is wired via the `TreeListModel`'s create-children
//! closure plus a per-row `notify::expanded` signal: the closure registers
//! an empty child store eagerly (so the expander triangle appears) but
//! the worker thread that fills it is only dispatched once the user
//! actually expands the row.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use async_channel::{Sender, unbounded};
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    Application, ApplicationWindow, ColumnView, ColumnViewColumn, HeaderBar, Label, ListItem,
    NoSelection, Orientation, ScrolledWindow, SignalListItemFactory, SortListModel, TreeExpander,
    TreeListModel, TreeListRow, TreeListRowSorter,
};

use dush::{AnalyzeOptions, Entry, enumerate_children, subtree_size};

// ==============================================================================
// Color palette
// ==============================================================================

/// Per-depth color palette for the share bar. Cycles through these so
/// nested levels get distinct hues — picked from the GNOME palette so
/// they read on both light and dark themes.
const PALETTE_RGB: &[(f32, f32, f32)] = &[
    (0.208, 0.518, 0.894), // blue
    (0.200, 0.820, 0.478), // green
    (0.965, 0.827, 0.176), // yellow
    (1.000, 0.471, 0.000), // orange
    (0.878, 0.106, 0.141), // red
    (0.569, 0.255, 0.675), // purple
    (0.710, 0.514, 0.353), // brown
];

/// Map a row's `depth` to its bar color.
fn palette_rgba(depth: i32) -> gtk::gdk::RGBA {
    let (r, g, b) = PALETTE_RGB[(depth.max(0) as usize) % PALETTE_RGB.len()];
    gtk::gdk::RGBA::new(r, g, b, 1.0)
}

/// Pick a foreground color (black or white) that contrasts well
/// against the supplied fill color. Uses the gamma-approximated sRGB
/// luminance formula `Y = 0.299R + 0.587G + 0.114B` and a 0.55
/// threshold — the threshold was tuned against the GNOME palette so
/// each color in [`PALETTE_RGB`] gets the side of the boundary that's
/// readable in practice (light blue / red / purple → white text;
/// green / yellow / orange / brown → black text).
fn contrasting_text_rgba(bg: &gtk::gdk::RGBA) -> gtk::gdk::RGBA {
    let y = 0.299 * bg.red() + 0.587 * bg.green() + 0.114 * bg.blue();
    if y > 0.55 {
        gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 1.0)
    } else {
        gtk::gdk::RGBA::new(1.0, 1.0, 1.0, 1.0)
    }
}

// ==============================================================================
// Custom widget: depth-colored share bar
// ==============================================================================
//
// We can't use a stock progress bar widget here because its fill color is
// owned by the GTK theme — there's no per-cell hook to override it — and
// we want each tree depth in its own color. A custom `gtk::Widget`
// subclass that paints a single filled rectangle via
// `gtk::Snapshot::append_color` produces a seamless block, and lets us
// expose `percent` and `depth` as glib properties so the column factory
// can `bind_property` them onto the row's `EntryItem`.

mod depth_bar {
    use super::{contrasting_text_rgba, palette_rgba};
    use std::cell::Cell;

    use gtk::glib;
    use gtk::glib::Properties;
    use gtk::glib::subclass::prelude::*;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk::{gdk, graphene};

    /// Width of the inner bar, capped to keep the bar from overpowering
    /// the row at very tall row heights.
    pub const BAR_HEIGHT_PX: f32 = 14.0;
    pub const HORIZONTAL_PADDING_PX: f32 = 2.0;

    #[derive(Default, Properties)]
    #[properties(wrapper_type = super::DepthBar)]
    pub struct DepthBar {
        /// Fill percentage in the range 0..=100. Anything outside is
        /// clamped at draw time, so a malformed model won't crash the
        /// renderer.
        #[property(get, set, minimum = 0, maximum = 100, default = 0)]
        pub percent: Cell<i32>,
        /// Tree depth used to pick the bar color from the palette.
        #[property(get, set, minimum = 0, default = 0)]
        pub depth: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DepthBar {
        const NAME: &'static str = "DushDepthBar";
        type Type = super::DepthBar;
        type ParentType = gtk::Widget;
    }

    #[glib::derived_properties]
    impl ObjectImpl for DepthBar {
        fn constructed(&self) {
            self.parent_constructed();
            // Property changes don't trigger a redraw automatically — we
            // wire it up here so updates from `bind_property` repaint.
            let obj = self.obj();
            obj.connect_notify_local(Some("percent"), |w, _| w.queue_draw());
            obj.connect_notify_local(Some("depth"), |w, _| w.queue_draw());
        }
    }

    impl WidgetImpl for DepthBar {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // Min width keeps the column from collapsing past the point
            // where the bar is meaningful; nat width is what the layout
            // hands us when the column gets surplus space.
            match orientation {
                gtk::Orientation::Horizontal => (60, 240, -1, -1),
                gtk::Orientation::Vertical => {
                    (BAR_HEIGHT_PX as i32 + 4, BAR_HEIGHT_PX as i32 + 4, -1, -1)
                }
                _ => (0, 0, -1, -1),
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let percent_i = self.percent.get().clamp(0, 100);
            let percent = percent_i as f32;
            let depth = self.depth.get();
            let fill_color = palette_rgba(depth);

            // Custom widgets get a 0,0-anchored coordinate system, so we
            // don't have to translate by the cell-area's offset like the
            // old CellRenderer did.
            let cell_w = widget.width() as f32;
            let cell_h = widget.height() as f32;
            let bar_h = (cell_h - 2.0 * HORIZONTAL_PADDING_PX).min(BAR_HEIGHT_PX);
            let y = (cell_h - bar_h) / 2.0;
            let x = HORIZONTAL_PADDING_PX;
            let w_total = cell_w - 2.0 * HORIZONTAL_PADDING_PX;
            let w_filled = w_total * percent / 100.0;

            // ----- Bar -----
            //
            // Track: subtle gray that respects both light and dark
            // themes by virtue of low alpha — it tints the row's
            // background rather than fighting it.
            let track_color = gdk::RGBA::new(0.5, 0.5, 0.5, 0.18);
            let track_rect = graphene::Rect::new(x, y, w_total, bar_h);
            snapshot.append_color(&track_color, &track_rect);

            if w_filled > 0.5 {
                // Avoid drawing sub-pixel slivers that anti-alias as a
                // washed-out hairline at percent=0 from a non-zero size.
                let fill_rect = graphene::Rect::new(x, y, w_filled, bar_h);
                snapshot.append_color(&fill_color, &fill_rect);
            }

            // ----- Percent text -----
            //
            // Drawn *twice* — once clipped to the filled portion, once
            // clipped to the unfilled portion — so the characters that
            // sit on top of the colored fill use the contrast-chosen
            // foreground while the characters that sit over the muted
            // track use the theme's normal foreground. Without this
            // two-pass clipping we'd have to pick a single color that
            // compromises on either side.
            let label = format!("{percent_i}%");
            let layout = widget.create_pango_layout(Some(&label));
            let (text_w, text_h) = layout.pixel_size();
            // Center the text horizontally within the bar's full width
            // (not the filled portion) so its position is stable as the
            // row's percent changes.
            let text_x = x + (w_total - text_w as f32) / 2.0;
            let text_y = (cell_h - text_h as f32) / 2.0;

            let text_pt = graphene::Point::new(text_x, text_y);
            let theme_fg = widget.color();
            let on_fill_fg = contrasting_text_rgba(&fill_color);

            // Pass 1: portion of text that lies over the empty track.
            if w_filled < w_total {
                let track_clip = graphene::Rect::new(x + w_filled, 0.0, w_total - w_filled, cell_h);
                snapshot.push_clip(&track_clip);
                snapshot.save();
                snapshot.translate(&text_pt);
                snapshot.append_layout(&layout, &theme_fg);
                snapshot.restore();
                snapshot.pop();
            }

            // Pass 2: portion of text that lies over the filled bar.
            if w_filled > 0.5 {
                let fill_clip = graphene::Rect::new(x, 0.0, w_filled, cell_h);
                snapshot.push_clip(&fill_clip);
                snapshot.save();
                snapshot.translate(&text_pt);
                snapshot.append_layout(&layout, &on_fill_fg);
                snapshot.restore();
                snapshot.pop();
            }
        }
    }
}

glib::wrapper! {
    pub struct DepthBar(ObjectSubclass<depth_bar::DepthBar>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for DepthBar {
    fn default() -> Self {
        glib::Object::new()
    }
}

// ==============================================================================
// Custom GObject: per-row data
// ==============================================================================
//
// Every row in the tree is backed by an `EntryItem`. The `ColumnView`
// factories bind their child widgets to its glib properties, so
// asynchronous size updates (via the worker pipeline) propagate to the
// UI just by setting the property — the view's per-row label/bar widget
// observes `notify::*` and updates itself.

mod entry_item {
    use std::cell::{Cell, RefCell};

    use gtk::glib;
    use gtk::glib::Properties;
    use gtk::glib::subclass::prelude::*;
    use gtk::prelude::*;

    #[derive(Default, Properties)]
    #[properties(wrapper_type = super::EntryItem)]
    pub struct EntryItem {
        /// Display name. For directories this includes the trailing `/`
        /// so the eye can pick them out without consulting `is_dir`.
        #[property(get, set)]
        pub name: RefCell<String>,
        /// Pretty-printed size, e.g. `"12.3 MiB"` — what the size column
        /// actually shows. While a directory's size is in flight, this
        /// holds [`super::PENDING_SIZE_LABEL`].
        #[property(get, set)]
        pub size_human: RefCell<String>,
        /// Raw byte count, used to sort the size column.
        #[property(get, set)]
        pub size_bytes: Cell<u64>,
        /// 0..=100, drives the share bar's fill width.
        #[property(get, set, minimum = 0, maximum = 100, default = 0)]
        pub percent: Cell<i32>,
        /// Absolute path of this entry. Empty for synthetic rows
        /// (currently only the `(error: ...)` row that replaces a failed
        /// enumeration).
        #[property(get, set)]
        pub path: RefCell<String>,
        /// Whether this entry is a directory (and therefore expandable).
        #[property(get, set)]
        pub is_dir: Cell<bool>,
        /// Whether a worker has been dispatched to enumerate this
        /// directory's children. Latched to `true` on first expansion
        /// to prevent duplicate dispatches if the user collapses and
        /// re-expands the row.
        #[property(get, set)]
        pub loaded: Cell<bool>,
        /// Whether the recursive size has finished computing. Kept for
        /// future use (e.g. a per-row spinner) — currently informational.
        #[property(get, set)]
        pub sized: Cell<bool>,
        /// 0 for top-level rows, +1 per nesting level. Drives the bar
        /// color via [`super::palette_rgba`].
        #[property(get, set, minimum = 0, default = 0)]
        pub depth: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EntryItem {
        const NAME: &'static str = "DushEntryItem";
        type Type = super::EntryItem;
    }

    #[glib::derived_properties]
    impl ObjectImpl for EntryItem {}
}

glib::wrapper! {
    pub struct EntryItem(ObjectSubclass<entry_item::EntryItem>);
}

impl Default for EntryItem {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl EntryItem {
    /// Build a row item from a freshly-enumerated [`Entry`]. Files get
    /// their real size up-front; directories carry [`PENDING_SIZE_LABEL`]
    /// in the size column until the recursive sizer reports back.
    fn from_entry(entry: &Entry, depth: i32) -> Self {
        let display_name = if entry.is_dir {
            format!("{}/", entry.name)
        } else {
            entry.name.clone()
        };
        let size_human = if entry.is_dir {
            PENDING_SIZE_LABEL.to_string()
        } else {
            humansize::format_size(entry.size_in_bytes, humansize::BINARY)
        };
        glib::Object::builder()
            .property("name", display_name)
            .property("size-human", size_human)
            .property("size-bytes", entry.size_in_bytes)
            .property("percent", 0i32)
            .property("path", entry.path.display().to_string())
            .property("is-dir", entry.is_dir)
            // Files have no children, so they're trivially "loaded".
            .property("loaded", !entry.is_dir)
            // Files have a final size already; dirs do not.
            .property("sized", !entry.is_dir)
            .property("depth", depth)
            .build()
    }

    /// Build a synthetic row that surfaces an enumeration failure to the
    /// user. We give it `path = ""` so it never collides with a real
    /// entry's path-keyed lookup.
    fn error_row(message: &str, depth: i32) -> Self {
        glib::Object::builder()
            .property("name", format!("(error: {message})"))
            .property("size-human", String::new())
            .property("size-bytes", 0u64)
            .property("percent", 0i32)
            .property("path", String::new())
            .property("is-dir", false)
            .property("loaded", true)
            .property("sized", true)
            .property("depth", depth)
            .build()
    }
}

/// Sentinel size string shown for directories whose recursive total is
/// still being computed. Kept short so it doesn't blow out the column.
const PENDING_SIZE_LABEL: &str = "…";

// ==============================================================================
// Status tracking
// ==============================================================================

/// Shared state powering the bottom status bar.
///
/// We track totals across the whole session — every level the user has
/// expanded — rather than per-level counters: the user wants to know
/// "is there anything still happening?", not "how is the most recent
/// expansion progressing?".
///
/// `queued` and `sized` only ever grow; subtract them to get the number
/// of directories still in flight. Both counters live behind `Cell`s
/// because every mutation happens on the GTK main thread.
struct Status {
    queued: Cell<usize>,
    sized: Cell<usize>,
    label: Label,
}

impl Status {
    fn new(label: Label) -> Self {
        Self {
            queued: Cell::new(0),
            sized: Cell::new(0),
            label,
        }
    }

    fn enqueue(&self, n: usize) {
        self.queued.set(self.queued.get() + n);
        self.refresh();
    }

    fn complete_one(&self) {
        self.sized.set(self.sized.get() + 1);
        self.refresh();
    }

    fn refresh(&self) {
        let queued = self.queued.get();
        let sized = self.sized.get();
        let pending = queued.saturating_sub(sized);
        let text = if queued == 0 {
            "Ready".to_string()
        } else if pending == 0 {
            // ✓ U+2713 — present in standard symbol blocks; renders as
            // a plain check on every modern font we care about.
            format!("✓ Done — {sized} director{} sized", plural_y(sized))
        } else {
            format!(
                "Sizing… {sized}/{queued} director{} ({pending} pending)",
                plural_y(queued)
            )
        };
        self.label.set_text(&text);
    }
}

fn plural_y(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

// ==============================================================================
// Worker → main-thread protocol
// ==============================================================================

/// Message sent by a worker thread to the GTK main loop.
///
/// We pass paths rather than direct `EntryItem` references because:
///
/// - Workers can't safely hold GObject references (they're not `Send`).
/// - A row may be replaced (e.g. when a parent's `(loading…)` placeholder
///   is swapped for real children) between dispatch and receipt; looking
///   up by path is robust against that.
#[derive(Debug)]
enum WorkerMsg {
    /// A worker finished enumerating the immediate children of a
    /// directory. The main thread inserts the rows under their parent's
    /// child store.
    Enumerated {
        /// Path of the directory whose children are being delivered.
        /// `None` means these are top-level rows (children of the
        /// canonical root).
        parent_path: Option<PathBuf>,
        entries: Vec<Entry>,
    },
    /// A worker computed the recursive size of a single directory.
    Sized {
        path: PathBuf,
        /// Path of the directory's parent in the tree (`None` for
        /// top-level dirs). Carried so `recompute_percentages` knows
        /// which sibling group to touch.
        parent_path: Option<PathBuf>,
        size: u64,
    },
    /// `enumerate_children` failed — usually a permissions error. We
    /// surface it as a single child row rather than failing silently.
    EnumerateFailed {
        parent_path: Option<PathBuf>,
        message: String,
    },
}

// ==============================================================================
// Entry point
// ==============================================================================

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let root = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let app = Application::builder()
        .application_id("dev.dush.gui")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let root_for_activate = Rc::new(root);
    app.connect_activate(move |app| build_ui(app, root_for_activate.as_path()));

    app.run_with_args::<&str>(&[])
}

// ==============================================================================
// UI construction
// ==============================================================================

/// Shared maps that survive across worker callbacks. We keep them in a
/// single struct so each helper takes one parameter rather than four.
///
/// - `root_model` holds the top-level rows (children of the canonical
///   root the app was launched against).
/// - `children_models` is keyed by directory path; entries are inserted
///   eagerly by the `TreeListModel` create-children closure on first
///   query and populated later when the worker for that directory
///   reports back.
/// - `items_by_path` lets `Sized` updates locate the affected
///   `EntryItem` in O(1) without walking the model.
#[derive(Clone)]
struct Models {
    root_model: gio::ListStore,
    children_models: Rc<RefCell<HashMap<PathBuf, gio::ListStore>>>,
    items_by_path: Rc<RefCell<HashMap<PathBuf, EntryItem>>>,
}

impl Models {
    fn new() -> Self {
        Self {
            root_model: gio::ListStore::new::<EntryItem>(),
            children_models: Rc::default(),
            items_by_path: Rc::default(),
        }
    }

    /// Resolve the destination `gio::ListStore` for the given parent
    /// path. `None` ↔ top level. Returns `None` if the worker raced
    /// ahead of the create-children closure (shouldn't happen because
    /// the closure is called eagerly on row exposure, but we treat it
    /// as a no-op rather than a panic).
    fn store_for(&self, parent: Option<&Path>) -> Option<gio::ListStore> {
        match parent {
            None => Some(self.root_model.clone()),
            Some(p) => self.children_models.borrow().get(p).cloned(),
        }
    }
}

fn build_ui(app: &Application, root: &Path) {
    let canonical = match std::fs::canonicalize(root) {
        Ok(p) => p,
        Err(e) => {
            show_error_window(
                app,
                &format!("Could not resolve path {}: {e}", root.display()),
            );
            return;
        }
    };

    let models = Models::new();

    // Status bar at the bottom of the window. A single `Label` inside a
    // `Box` leaves room to add other widgets later (a spinner, perhaps,
    // or a cancel button) without restructuring the layout.
    let status_label = Label::builder()
        .xalign(0.0)
        .margin_start(8)
        .margin_end(8)
        .margin_top(4)
        .margin_bottom(4)
        .label("Ready")
        .build();
    let status = Rc::new(Status::new(status_label.clone()));

    // One unbounded channel for the whole app — workers send `WorkerMsg`s,
    // a consumer task on the GTK main loop receives them and updates the
    // models. Unbounded is fine here because there's never more than a
    // few hundred updates in flight in practice.
    let (tx, rx) = unbounded::<WorkerMsg>();

    // Drive the worker → UI pipeline on the GTK main context.
    // `spawn_future_local` pins the future to the main thread so it can
    // touch the models directly without any extra synchronization.
    {
        let models = models.clone();
        let status = status.clone();
        glib::spawn_future_local(async move {
            run_consumer(models, status, rx).await;
        });
    }

    // ----- Tree model (lazy children registration) -----
    //
    // The closure runs the first time `TreeListModel` needs an item's
    // child model — typically when the row scrolls into view and the
    // expander indicator has to be drawn. We register an *empty* store
    // synchronously and return it immediately; no filesystem walk happens
    // here. Population is deferred until the user actually clicks the
    // expander, observed via `notify::expanded` in the name column
    // factory below.
    let create_func = {
        let children_models = models.children_models.clone();
        move |obj: &glib::Object| -> Option<gio::ListModel> {
            let item = obj.downcast_ref::<EntryItem>()?;
            if !item.is_dir() {
                return None;
            }
            let path = PathBuf::from(item.path());
            let mut map = children_models.borrow_mut();
            if let Some(existing) = map.get(&path) {
                return Some(existing.clone().upcast());
            }
            let store = gio::ListStore::new::<EntryItem>();
            map.insert(path, store.clone());
            Some(store.upcast())
        }
    };
    // `passthrough = false` means the outer model exposes `TreeListRow`s
    // (so columns can pick the depth/expander up); `autoexpand = false`
    // keeps the tree collapsed by default.
    let tree_model = TreeListModel::new(models.root_model.clone(), false, false, create_func);

    // ----- ColumnView -----
    let view = ColumnView::builder()
        .show_row_separators(false)
        .show_column_separators(false)
        .reorderable(false)
        .build();

    let name_col = build_name_column(tx.clone());
    let size_col = build_size_column();
    let share_col = build_share_column();
    view.append_column(&name_col);
    view.append_column(&size_col);
    view.append_column(&share_col);

    // Default sort: largest first. That's the question this tool exists
    // to answer, so we make it the column the user sees as already-sorted.
    view.sort_by_column(Some(&size_col), gtk::SortType::Descending);

    // Wrap the tree model in a `TreeListRowSorter` so the column-view's
    // sorter is applied *per level*. Without this wrapper the flat DFS
    // output would be re-sorted globally, which mangles parent/child
    // grouping.
    let row_sorter = TreeListRowSorter::new(view.sorter());
    let sort_model = SortListModel::new(Some(tree_model), Some(row_sorter));
    let selection = NoSelection::new(Some(sort_model));
    view.set_model(Some(&selection));

    // Kick off the root: dispatching to a worker keeps the GTK main
    // thread free to render the empty window immediately. The user sees
    // "Sizing…" appear in the status bar within milliseconds rather than
    // staring at an unresponsive window while we walk the filesystem.
    request_populate(None, canonical.clone(), &tx);

    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&view)
        .vexpand(true)
        .build();

    // Vertical layout: scrolled tree fills, status bar pinned at the
    // bottom. A thin separator above the status row gives it visual
    // weight without resorting to colors that wouldn't survive a theme
    // change.
    let layout = gtk::Box::new(Orientation::Vertical, 0);
    layout.append(&scrolled);
    layout.append(&gtk::Separator::new(Orientation::Horizontal));
    layout.append(&status_label);

    let header = HeaderBar::new();
    let title = Label::new(Some(&format!("dush — {}", canonical.display())));
    header.set_title_widget(Some(&title));

    let window = ApplicationWindow::builder()
        .application(app)
        .title("dush")
        .default_width(900)
        .default_height(600)
        .child(&layout)
        .build();
    window.set_titlebar(Some(&header));
    window.present();
}

// ==============================================================================
// Column factories
// ==============================================================================

/// Name column: `TreeExpander` containing a `Label`. The expander draws
/// the indent and disclosure triangle; binding it to the row's
/// `TreeListRow` is what hooks the click into the tree model.
///
/// We also attach a `notify::expanded` listener on the row in `bind` so
/// that the worker that enumerates a directory's children only fires
/// when the user actually expands it — not when GTK eagerly queries
/// expandability. The listener is disconnected in `unbind`, since
/// factories recycle widgets across rows.
fn build_name_column(tx: Sender<WorkerMsg>) -> ColumnViewColumn {
    let factory = SignalListItemFactory::new();

    factory.connect_setup(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().expect("ListItem");
        let expander = TreeExpander::new();
        let label = Label::builder().xalign(0.0).build();
        expander.set_child(Some(&label));
        list_item.set_child(Some(&expander));
    });

    factory.connect_bind(move |_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().expect("ListItem");
        let row: TreeListRow = list_item.item().and_downcast().expect("TreeListRow");
        let item: EntryItem = row.item().and_downcast().expect("EntryItem");
        let expander: TreeExpander = list_item.child().and_downcast().expect("TreeExpander");
        expander.set_list_row(Some(&row));
        let label: Label = expander.child().and_downcast().expect("Label");

        // Bind the visible text to the property so a future rename (or
        // any other property edit) reflects without a re-bind. Stored on
        // the `ListItem` so `connect_unbind` can release it before the
        // widget is recycled onto a different row.
        let binding = item
            .bind_property("name", &label, "label")
            .sync_create()
            .build();
        unsafe {
            list_item.set_data::<glib::Binding>("dz-name-binding", binding);
        }

        // One-shot worker dispatch on first expansion. The `loaded` flag
        // on the item itself is the ground truth — even if the same
        // factory bind runs twice for the same item (rare, but possible
        // through scrolling churn), the second call is a no-op because
        // the first one set `loaded`.
        if item.is_dir() && !item.loaded() {
            let item_for_handler = item.clone();
            let tx_for_handler = tx.clone();
            let handler = row.connect_expanded_notify(move |r| {
                if r.is_expanded() && !item_for_handler.loaded() {
                    item_for_handler.set_loaded(true);
                    let p = PathBuf::from(item_for_handler.path());
                    request_populate(Some(p.clone()), p, &tx_for_handler);
                }
            });
            unsafe {
                list_item.set_data::<glib::SignalHandlerId>("dz-expand-handler", handler);
            }
        }
    });

    factory.connect_unbind(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().expect("ListItem");
        unsafe {
            if let Some(binding) = list_item.steal_data::<glib::Binding>("dz-name-binding") {
                binding.unbind();
            }
            if let Some(handler) =
                list_item.steal_data::<glib::SignalHandlerId>("dz-expand-handler")
            {
                if let Some(row) = list_item.item().and_downcast::<TreeListRow>() {
                    row.disconnect(handler);
                }
            }
        }
    });

    let col = ColumnViewColumn::new(Some("Name"), Some(factory));
    col.set_resizable(true);
    col.set_expand(true);

    // Sortable by name — handy when users want alphabetical, even though
    // size-descending is the default.
    let name_expr =
        gtk::PropertyExpression::new(EntryItem::static_type(), None::<&gtk::Expression>, "name");
    let sorter = gtk::StringSorter::new(Some(name_expr));
    col.set_sorter(Some(&sorter));
    col
}

/// Size column: a right-aligned label bound to `size-human`. Sorts by
/// the underlying `size-bytes` so "1.0 MiB" sorts above "999 KiB"
/// (string sort would reverse them).
fn build_size_column() -> ColumnViewColumn {
    let factory = SignalListItemFactory::new();

    factory.connect_setup(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().expect("ListItem");
        let label = Label::builder().xalign(1.0).build();
        list_item.set_child(Some(&label));
    });

    factory.connect_bind(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().expect("ListItem");
        let row: TreeListRow = list_item.item().and_downcast().expect("TreeListRow");
        let item: EntryItem = row.item().and_downcast().expect("EntryItem");
        let label: Label = list_item.child().and_downcast().expect("Label");
        let binding = item
            .bind_property("size-human", &label, "label")
            .sync_create()
            .build();
        unsafe {
            list_item.set_data::<glib::Binding>("dz-size-binding", binding);
        }
    });

    factory.connect_unbind(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().expect("ListItem");
        unsafe {
            if let Some(binding) = list_item.steal_data::<glib::Binding>("dz-size-binding") {
                binding.unbind();
            }
        }
    });

    let col = ColumnViewColumn::new(Some("Size"), Some(factory));
    col.set_resizable(true);

    let size_expr = gtk::PropertyExpression::new(
        EntryItem::static_type(),
        None::<&gtk::Expression>,
        "size-bytes",
    );
    let sorter = gtk::NumericSorter::builder()
        .expression(&size_expr)
        .sort_order(gtk::SortType::Ascending)
        .build();
    col.set_sorter(Some(&sorter));
    col
}

/// Share column: a `DepthBar` widget bound to the row's `percent` and
/// `depth`. Two bindings → two `glib::Binding`s to release on unbind;
/// stored as a `Vec` under one data key.
fn build_share_column() -> ColumnViewColumn {
    let factory = SignalListItemFactory::new();

    factory.connect_setup(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().expect("ListItem");
        let bar = DepthBar::default();
        list_item.set_child(Some(&bar));
    });

    factory.connect_bind(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().expect("ListItem");
        let row: TreeListRow = list_item.item().and_downcast().expect("TreeListRow");
        let item: EntryItem = row.item().and_downcast().expect("EntryItem");
        let bar: DepthBar = list_item.child().and_downcast().expect("DepthBar");
        let bindings: Vec<glib::Binding> = vec![
            item.bind_property("percent", &bar, "percent")
                .sync_create()
                .build(),
            item.bind_property("depth", &bar, "depth")
                .sync_create()
                .build(),
        ];
        unsafe {
            list_item.set_data::<Vec<glib::Binding>>("dz-share-bindings", bindings);
        }
    });

    factory.connect_unbind(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().expect("ListItem");
        unsafe {
            if let Some(bindings) = list_item.steal_data::<Vec<glib::Binding>>("dz-share-bindings")
            {
                for b in bindings {
                    b.unbind();
                }
            }
        }
    });

    let col = ColumnViewColumn::new(Some("Share"), Some(factory));
    col.set_resizable(true);
    col.set_expand(true);
    col
}

// ==============================================================================
// Workers and the consumer task
// ==============================================================================

/// Dispatch a worker thread to enumerate `target_path` and then size
/// each of its sub-directories. Returns immediately so the GTK main
/// thread is never blocked on filesystem work, even for directories
/// containing tens of thousands of children.
///
/// `parent_path_for_lookup` identifies which row in the model will host
/// the resulting children — `None` for the very first level under the
/// app's root. We carry it explicitly so the main-thread handler
/// doesn't have to remember which worker it dispatched for which row.
fn request_populate(
    parent_path_for_lookup: Option<PathBuf>,
    target_path: PathBuf,
    tx: &Sender<WorkerMsg>,
) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let entries = match enumerate_children(&target_path) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send_blocking(WorkerMsg::EnumerateFailed {
                    parent_path: parent_path_for_lookup,
                    message: e.to_string(),
                });
                return;
            }
        };

        // Collect the directory paths we'll need to size *before* we
        // hand the entries off to the main thread, since we move
        // `entries` into the message.
        let dirs: Vec<PathBuf> = entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.path.clone())
            .collect();

        let _ = tx.send_blocking(WorkerMsg::Enumerated {
            parent_path: parent_path_for_lookup.clone(),
            entries,
        });

        // Per-directory recursive sizing. We do this serially: `diskus`
        // is already internally parallel, so spawning N threads here
        // would just oversubscribe the CPU and slow each walk down.
        let opts = AnalyzeOptions::default();
        for path in dirs {
            let (size, _errors) = subtree_size(&path, &opts);
            let _ = tx.send_blocking(WorkerMsg::Sized {
                path,
                parent_path: parent_path_for_lookup.clone(),
                size,
            });
        }
    });
}

/// Drive the GTK-main-thread side of the worker → UI pipeline.
///
/// Yields control back to GTK every `YIELD_EVERY` messages so a sustained
/// burst (e.g. sizing a directory with thousands of sub-directories)
/// can't starve the main loop and freeze the window.
async fn run_consumer(models: Models, status: Rc<Status>, rx: async_channel::Receiver<WorkerMsg>) {
    /// Picked by feel: large enough that yield overhead is negligible,
    /// small enough that the UI wakes up at least every ~16 ms during
    /// heavy bursts.
    const YIELD_EVERY: usize = 64;
    let mut since_yield: usize = 0;

    while let Ok(msg) = rx.recv().await {
        match msg {
            WorkerMsg::Enumerated {
                parent_path,
                entries,
            } => {
                apply_enumerated(&models, &status, parent_path, entries);
            }
            WorkerMsg::Sized {
                path,
                parent_path,
                size,
            } => {
                apply_sized(&models, &path, parent_path.as_deref(), size);
                status.complete_one();
            }
            WorkerMsg::EnumerateFailed {
                parent_path,
                message,
            } => {
                apply_enumerate_failed(&models, parent_path, message);
                status.refresh();
            }
        }

        since_yield += 1;
        // Yield on either of two conditions:
        // 1. Per-batch budget exceeded — keeps UI responsive in bursts.
        // 2. Channel just emptied — without this, a small burst would
        //    leave the loop blocked on `recv` with the GTK main loop
        //    not having repainted since before the burst started.
        if since_yield >= YIELD_EVERY || rx.is_empty() {
            glib::timeout_future(Duration::ZERO).await;
            since_yield = 0;
        }
    }
}

/// Replace the children of `parent_path` with freshly-enumerated entries
/// and recompute the share percentages for that level.
///
/// We use `extend_from_slice` rather than per-row `append` so the
/// `SortListModel` wrapping the tree model only re-sorts once per batch,
/// turning O(n²) sort bookkeeping into O(n log n).
fn apply_enumerated(
    models: &Models,
    status: &Rc<Status>,
    parent_path: Option<PathBuf>,
    entries: Vec<Entry>,
) {
    let parent_depth = parent_path
        .as_ref()
        .and_then(|p| models.items_by_path.borrow().get(p).map(|it| it.depth()))
        .unwrap_or(-1);
    let depth = parent_depth + 1;

    let Some(store) = models.store_for(parent_path.as_deref()) else {
        // Defensive: should not happen because the create-children
        // closure registers a store before the worker can fire. If it
        // ever does, dropping the message is preferable to panicking
        // (the user just sees an empty subtree).
        return;
    };

    let n_dirs = entries.iter().filter(|e| e.is_dir).count();
    if n_dirs > 0 {
        status.enqueue(n_dirs);
    }

    // Build the EntryItems and register them by path for later size
    // updates. We register *before* inserting so a Sized message that
    // races between extend_from_slice and the next event-loop tick
    // still finds its target.
    let new_items: Vec<EntryItem> = entries
        .iter()
        .map(|e| EntryItem::from_entry(e, depth))
        .collect();
    {
        let mut by_path = models.items_by_path.borrow_mut();
        for (entry, item) in entries.iter().zip(new_items.iter()) {
            by_path.insert(entry.path.clone(), item.clone());
        }
    }

    // Replace the store's contents in one shot. The previous contents
    // are typically empty (a freshly-registered child store) but we
    // call `remove_all` for the rare cases where a re-enumeration
    // reuses the same store.
    if store.n_items() > 0 {
        store.remove_all();
    }
    if !new_items.is_empty() {
        store.extend_from_slice(&new_items);
    }

    recompute_percentages(&store);

    if n_dirs == 0 {
        // No further Sized messages will arrive for this level — flip
        // the status bar back to "Done" if nothing else is in flight.
        status.refresh();
    }
}

/// Apply a `Sized` update: write the new size to the affected
/// `EntryItem` and recompute its parent's percentages.
fn apply_sized(models: &Models, path: &Path, parent_path: Option<&Path>, size: u64) {
    {
        let by_path = models.items_by_path.borrow();
        if let Some(item) = by_path.get(path) {
            item.set_size_bytes(size);
            item.set_size_human(humansize::format_size(size, humansize::BINARY));
            item.set_sized(true);
        } else {
            // A Sized message for a row we don't know about: probably
            // the parent was re-enumerated and the row no longer exists.
            // Drop silently.
            return;
        }
    }
    if let Some(store) = models.store_for(parent_path) {
        recompute_percentages(&store);
    }
}

/// Replace the children of `parent_path` with a single synthetic error
/// row carrying the OS error message. We keep the exact OS error text
/// so a user debugging a permission issue can recognize the system
/// string (`Permission denied (os error 13)`) faster than a generic
/// "could not read".
fn apply_enumerate_failed(models: &Models, parent_path: Option<PathBuf>, message: String) {
    let parent_depth = parent_path
        .as_ref()
        .and_then(|p| models.items_by_path.borrow().get(p).map(|it| it.depth()))
        .unwrap_or(-1);
    let depth = parent_depth + 1;
    let Some(store) = models.store_for(parent_path.as_deref()) else {
        return;
    };
    if store.n_items() > 0 {
        store.remove_all();
    }
    let err_item = EntryItem::error_row(&message, depth);
    store.append(&err_item);
}

/// Walk the items in `store` and update each `percent` property to its
/// share of the level's total size. Called once per batched update —
/// cheap because it's O(n) in the *level's* size, not the whole tree.
fn recompute_percentages(store: &gio::ListStore) {
    let n = store.n_items();
    let mut total: u64 = 0;
    for i in 0..n {
        if let Some(item) = store.item(i).and_downcast::<EntryItem>() {
            // Skip synthetic error rows — they have an empty path and
            // would otherwise inflate the total with their zero size
            // (harmless, but it's also wrong to count them).
            if !item.path().is_empty() {
                total = total.saturating_add(item.size_bytes());
            }
        }
    }
    for i in 0..n {
        if let Some(item) = store.item(i).and_downcast::<EntryItem>() {
            if item.path().is_empty() {
                continue;
            }
            let percent = if total == 0 {
                0
            } else {
                ((item.size_bytes() as u128 * 100) / total as u128) as i32
            };
            item.set_percent(percent);
        }
    }
}

// ==============================================================================
// Failure window
// ==============================================================================

/// Build a minimal error window when the analysis can't even start. We
/// intentionally don't reuse the main window scaffolding — it's helpful
/// for the user to see *just* the error message rather than an empty
/// tree with a tooltip somewhere.
fn show_error_window(app: &Application, message: &str) {
    let label = Label::builder()
        .label(message)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .wrap(true)
        .build();
    let container = gtk::Box::new(Orientation::Vertical, 12);
    container.append(&label);
    let window = ApplicationWindow::builder()
        .application(app)
        .title("dush — error")
        .default_width(520)
        .default_height(160)
        .child(&container)
        .build();
    window.present();
}
