// GTK 4.10 deprecated TreeView/TreeStore in favor of ColumnView +
// TreeListModel + factories. Migrating is a larger undertaking — both
// require defining a `glib::Object` subclass for entries — so we
// deliberately stick with the classic API for this iteration and silence
// the deprecation lints rather than letting them drown out real warnings.
// Switching to ColumnView + factories is tracked in SESSION.md.
#![allow(deprecated)]

//! # Diskalyzer GUI
//!
//! GTK4 front-end for the `diskalyzer` library. The window appears
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
//! cargo run --features gui --bin diskalyzer-gui -- ~/dev
//! ```

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use async_channel::{unbounded, Sender};
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    Application, ApplicationWindow, CellRendererText, HeaderBar, Label,
    Orientation, ScrolledWindow, TreeIter, TreeStore, TreeView, TreeViewColumn,
};

use diskalyzer::{enumerate_children, subtree_size, AnalyzeOptions, Entry};

// ==============================================================================
// Custom bar cell renderer
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

/// `gtk::CellRenderer` subclass that paints a single colored bar.
///
/// We can't use the stock `CellRendererProgress` here because its fill
/// color is owned by the GTK theme — there's no per-cell hook to override
/// it — and we want each tree depth in its own color. Compositing the
/// bar from unicode block glyphs (the previous attempt) leaves visible
/// hairlines between cells under most fonts; drawing a single filled
/// rectangle via `gtk::Snapshot::append_color` produces a seamless block.
mod bar_renderer {
    use std::cell::Cell;

    use gtk::glib;
    use gtk::glib::Properties;
    use gtk::glib::subclass::prelude::*;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk::{gdk, graphene};

    /// Width of the inner bar that we draw, capped to keep the bar from
    /// overpowering the row at very tall row heights.
    const BAR_HEIGHT_PX: f32 = 14.0;
    const HORIZONTAL_PADDING_PX: f32 = 2.0;

    #[derive(Default, Properties)]
    #[properties(wrapper_type = super::DepthBarRenderer)]
    pub struct DepthBarRenderer {
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
    impl ObjectSubclass for DepthBarRenderer {
        const NAME: &'static str = "DiskalyzerDepthBarRenderer";
        type Type = super::DepthBarRenderer;
        type ParentType = gtk::CellRenderer;
    }

    #[glib::derived_properties]
    impl ObjectImpl for DepthBarRenderer {}

    impl CellRendererImpl for DepthBarRenderer {
        fn preferred_width<P: IsA<gtk::Widget>>(&self, _widget: &P) -> (i32, i32) {
            // Min keeps the column from collapsing past the point where
            // the bar is meaningful; nat is what gtk uses when the column
            // is `expand`ed.
            (60, 240)
        }

        fn preferred_height<P: IsA<gtk::Widget>>(&self, _widget: &P) -> (i32, i32) {
            (BAR_HEIGHT_PX as i32 + 4, BAR_HEIGHT_PX as i32 + 4)
        }

        fn snapshot<P: IsA<gtk::Widget>>(
            &self,
            snapshot: &gtk::Snapshot,
            widget: &P,
            _background_area: &gdk::Rectangle,
            cell_area: &gdk::Rectangle,
            _flags: gtk::CellRendererState,
        ) {
            let percent_i = self.percent.get().clamp(0, 100);
            let percent = percent_i as f32;
            let depth = self.depth.get();
            let fill_color = super::palette_rgba(depth);

            let cell_w = cell_area.width() as f32;
            let cell_h = cell_area.height() as f32;
            let bar_h = (cell_h - 2.0 * HORIZONTAL_PADDING_PX).min(BAR_HEIGHT_PX);
            let y = cell_area.y() as f32 + (cell_h - bar_h) / 2.0;
            let x = cell_area.x() as f32 + HORIZONTAL_PADDING_PX;
            let w_total = cell_w - 2.0 * HORIZONTAL_PADDING_PX;
            let w_filled = w_total * percent / 100.0;

            // ----- Bar -----

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
            // We draw the text *twice* — once clipped to the filled
            // portion, once clipped to the unfilled portion — so the
            // characters that sit on top of the colored fill use the
            // contrast-chosen foreground while the characters that sit
            // over the muted track use the theme's normal foreground.
            // Without this two-pass clipping we'd have to pick a single
            // color that compromises on either side.

            let label = format!("{percent_i}%");
            let layout = widget.as_ref().create_pango_layout(Some(&label));
            let (text_w, text_h) = layout.pixel_size();
            // Center the text horizontally within the bar's full width
            // (not the filled portion) so its position is stable as
            // the row's percent changes.
            let text_x = x + (w_total - text_w as f32) / 2.0;
            let text_y = cell_area.y() as f32 + (cell_h - text_h as f32) / 2.0;

            let text_pt = graphene::Point::new(text_x, text_y);
            let theme_fg = widget.as_ref().color();
            let on_fill_fg = super::contrasting_text_rgba(&fill_color);

            // Pass 1: portion of text that lies over the empty track.
            // Clip to the right of the fill.
            if w_filled < w_total {
                let track_clip = graphene::Rect::new(
                    x + w_filled,
                    cell_area.y() as f32,
                    w_total - w_filled,
                    cell_h,
                );
                snapshot.push_clip(&track_clip);
                snapshot.save();
                snapshot.translate(&text_pt);
                snapshot.append_layout(&layout, &theme_fg);
                snapshot.restore();
                snapshot.pop();
            }

            // Pass 2: portion of text that lies over the filled bar.
            if w_filled > 0.5 {
                let fill_clip = graphene::Rect::new(x, cell_area.y() as f32, w_filled, cell_h);
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
    pub struct DepthBarRenderer(ObjectSubclass<bar_renderer::DepthBarRenderer>)
        @extends gtk::CellRenderer;
}

impl Default for DepthBarRenderer {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl DepthBarRenderer {
    fn new() -> Self {
        Self::default()
    }
}

// ==============================================================================
// TreeStore schema
// ==============================================================================
//
// We index columns through this little group of constants rather than via
// raw integers scattered through the code. The order *must* match the
// `glib::Type` array passed to `TreeStore::new` below.

const COL_NAME: u32 = 0; // String — display name (with trailing slash on dirs)
const COL_SIZE_HUMAN: u32 = 1; // String — pretty-printed size, e.g. "12.3 MiB"
const COL_SIZE_BYTES: u32 = 2; // u64 — raw bytes, used for sorting
const COL_PERCENT: u32 = 3; // i32 — 0..=100, drives the bar fill width
const COL_PATH: u32 = 4; // String — absolute path of this entry
const COL_IS_DIR: u32 = 5; // bool
const COL_LOADED: u32 = 6; // bool — whether children have been populated yet
const COL_SIZED: u32 = 7; // bool — whether the recursive size is final
const COL_DEPTH: u32 = 8; // i32 — 0 for top-level, +1 per nesting level (drives bar color)

/// Column type vector. Keep in lockstep with the `COL_*` constants above.
fn store_column_types() -> [glib::Type; 9] {
    [
        glib::Type::STRING, // name
        glib::Type::STRING, // size_human
        glib::Type::U64,    // size_bytes
        glib::Type::I32,    // percent
        glib::Type::STRING, // path
        glib::Type::BOOL,   // is_dir
        glib::Type::BOOL,   // loaded
        glib::Type::BOOL,   // sized
        glib::Type::I32,    // depth
    ]
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

    /// Account for `n` directories that have just been handed to a
    /// worker thread. Updates the visible label.
    fn enqueue(&self, n: usize) {
        self.queued.set(self.queued.get() + n);
        self.refresh();
    }

    /// Account for one directory whose subtree size has arrived from a
    /// worker. Updates the visible label.
    fn complete_one(&self) {
        self.sized.set(self.sized.get() + 1);
        self.refresh();
    }

    fn refresh(&self) {
        let queued = self.queued.get();
        let sized = self.sized.get();
        let pending = queued.saturating_sub(sized);
        let text = if queued == 0 {
            // Pre-population: the initial enumeration hasn't run yet,
            // or the directory only contained files.
            "Ready".to_string()
        } else if pending == 0 {
            // ✓ U+2713 — present in standard symbol blocks; renders as
            // a plain check on every modern terminal/font we care about.
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

/// English-only quick pluralizer for the "directory/directories" word
/// used in the status bar. Inlined to avoid pulling a fluent stack for
/// one label.
fn plural_y(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

// ==============================================================================
// Worker → main-thread protocol
// ==============================================================================

/// Message sent by a worker thread to the GTK main loop.
///
/// We pass paths rather than `TreeIter`s because:
///
/// - `TreeIter` is not safe to send across threads.
/// - Sort-induced reorderings would invalidate any iter the worker
///   captured before sending.
///
/// Identifying rows by `COL_PATH` is robust against both.
#[derive(Debug)]
enum WorkerMsg {
    /// A worker finished enumerating the immediate children of a
    /// directory. The main thread inserts the rows and adds placeholder
    /// children for any sub-directories.
    Enumerated {
        /// Path of the directory whose children are being delivered.
        /// `None` means these are top-level rows.
        parent_path: Option<PathBuf>,
        /// The freshly-enumerated entries. Files have their real
        /// `metadata().len()` size; directories carry a 0 placeholder
        /// size that will be filled in by a later [`WorkerMsg::Sized`].
        entries: Vec<Entry>,
    },
    /// A worker computed the recursive size of a single directory.
    Sized {
        /// Path that was sized.
        path: PathBuf,
        /// Path of the directory's parent in the tree (`None` for
        /// top-level dirs). Carried to scope the row lookup; without it
        /// we'd re-walk the whole model to find the row.
        parent_path: Option<PathBuf>,
        /// The recursively-computed size in bytes.
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
        .application_id("dev.diskalyzer.gui")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let root_for_activate = Rc::new(root);
    app.connect_activate(move |app| build_ui(app, root_for_activate.as_path()));

    app.run_with_args::<&str>(&[])
}

// ==============================================================================
// UI construction
// ==============================================================================

/// Build the main window, the column-aware tree view, and wire up the
/// lazy-expansion handler. The function ends without taking ownership of
/// the widgets — GTK keeps them alive through the application instance —
/// so we leak nothing despite returning early on display.
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

    let store = TreeStore::new(&store_column_types());
    let view = build_tree_view(&store);

    // Status bar at the bottom of the window. A single Label inside a
    // Box leaves room to add other widgets later (a spinner, perhaps, or
    // a cancel button) without restructuring the layout.
    let status_label = Label::builder()
        .xalign(0.0)
        .margin_start(8)
        .margin_end(8)
        .margin_top(4)
        .margin_bottom(4)
        .label("Ready")
        .build();
    let status = Rc::new(Status::new(status_label.clone()));

    // Channel pair shared by every worker we ever spawn: workers send
    // `WorkerMsg`s, the consumer task on the GTK main loop receives them
    // and updates the store. One unbounded channel for the whole app
    // keeps the wiring simple — there's never more than a few hundred
    // updates in flight in practice.
    let (tx, rx) = unbounded::<WorkerMsg>();

    // Spawn the consumer on the GTK main context. `spawn_future_local`
    // is the modern replacement for the (deprecated) `glib::MainContext::
    // channel`; the closure is pinned to the main thread so it can mutate
    // the store directly without any extra synchronization.
    {
        let store = store.clone();
        let status = status.clone();
        glib::spawn_future_local(async move {
            run_consumer(store, status, rx).await;
        });
    }

    // Kick off the root: dispatching to a worker keeps the GTK main
    // thread free to render the empty window immediately. The user sees
    // "Sizing…" appear in the status bar within milliseconds rather than
    // staring at an unresponsive window while we walk the filesystem.
    request_populate(None, canonical.clone(), &tx);

    // Lazy expansion. We use `connect_test_expand_row` rather than
    // `connect_row_expanded` because the former runs *before* the row
    // visually expands, so the placeholder swap is invisible.
    let store_for_expand = store.clone();
    let tx_for_expand = tx.clone();
    view.connect_test_expand_row(move |_view, iter, _path| {
        let already_loaded: bool = store_for_expand.get::<bool>(iter, COL_LOADED as i32);
        if !already_loaded {
            // Mark loaded *up front* so a re-entrant expand attempt
            // (rare, but possible if the user keys through the tree
            // quickly) doesn't dispatch a second worker for the same
            // directory.
            store_for_expand.set(iter, &[(COL_LOADED, &true)]);
            let path_str: String = store_for_expand.get::<String>(iter, COL_PATH as i32);
            let parent_path = PathBuf::from(&path_str);
            request_populate(Some(parent_path.clone()), parent_path, &tx_for_expand);
        }
        glib::Propagation::Proceed
    });

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
    let title = Label::new(Some(&format!("Diskalyzer — {}", canonical.display())));
    header.set_title_widget(Some(&title));

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Diskalyzer")
        .default_width(900)
        .default_height(600)
        .child(&layout)
        .build();
    window.set_titlebar(Some(&header));
    window.present();
}

/// Build the `TreeView` with three columns: name (with expander), size,
/// and a progress-bar percent column. The view is configured for sorting
/// on the size column — that's the question this tool exists to answer,
/// so we make it the default sort.
fn build_tree_view(store: &TreeStore) -> TreeView {
    let view = TreeView::builder()
        .model(store)
        .headers_visible(true)
        .enable_tree_lines(true)
        .build();

    // --- Name column (with expander triangle) ---
    let name_col = TreeViewColumn::new();
    name_col.set_title("Name");
    name_col.set_resizable(true);
    name_col.set_min_width(220);
    let name_renderer = CellRendererText::new();
    name_col.pack_start(&name_renderer, true);
    name_col.add_attribute(&name_renderer, "text", COL_NAME as i32);
    view.append_column(&name_col);
    view.set_expander_column(Some(&name_col));

    // --- Size column ---
    let size_col = TreeViewColumn::new();
    size_col.set_title("Size");
    size_col.set_resizable(true);
    size_col.set_min_width(110);
    size_col.set_sort_column_id(COL_SIZE_BYTES as i32);
    let size_renderer = CellRendererText::new();
    size_renderer.set_xalign(1.0); // right-align numbers
    size_col.pack_start(&size_renderer, false);
    size_col.add_attribute(&size_renderer, "text", COL_SIZE_HUMAN as i32);
    view.append_column(&size_col);

    // --- Share column (custom-drawn colored bar) ---
    //
    // `DepthBarRenderer` paints a single filled rectangle whose width
    // is `cell.width * percent / 100` and whose color is picked from a
    // palette indexed by tree depth. Drawing a true rectangle (rather
    // than an approximation built from block glyphs) avoids the
    // hairline seams that block-glyph bars exhibit on most fonts.
    let share_col = TreeViewColumn::new();
    share_col.set_title("Share");
    share_col.set_resizable(true);
    share_col.set_min_width(180);
    let bar_renderer = DepthBarRenderer::new();
    share_col.pack_start(&bar_renderer, true);
    share_col.add_attribute(&bar_renderer, "percent", COL_PERCENT as i32);
    share_col.add_attribute(&bar_renderer, "depth", COL_DEPTH as i32);
    view.append_column(&share_col);

    // Default sort: largest first. Without an explicit sort the rows
    // would appear in `read_dir` order, which is filesystem-defined and
    // never the order a user wants.
    store.set_sort_column_id(
        gtk::SortColumn::Index(COL_SIZE_BYTES),
        gtk::SortType::Descending,
    );

    view
}

/// Dispatch a worker thread to enumerate `target_path` and then size
/// each of its sub-directories. Returns immediately so the GTK main
/// thread is never blocked on filesystem work, even for directories
/// containing tens of thousands of children.
///
/// `parent_path_for_lookup` identifies which row in the store will host
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
/// The consumer batches incoming `Sized` updates and only re-computes
/// percentage columns at batch boundaries, rather than per-message.
/// Three things make this scalable to directories with tens of
/// thousands of children:
///
/// 1. **Periodic yield.** Every `YIELD_EVERY` messages, the loop awaits
///    a zero-duration timeout, which returns control to GTK so it can
///    paint, dispatch input, and stay responsive. Without this the
///    consumer would drain a backlog of thousands of queued messages
///    in a single uninterrupted run.
///
/// 2. **Deferred percentage recomputation.** A `Sized` message updates
///    only its own row's size cell and marks the parent as "dirty".
///    The dirty set is flushed (one `recompute_percentages` per parent)
///    when we yield, when we hit a batch boundary, or when an
///    `Enumerated` message comes through. This collapses what was
///    O(parent_size) per message into O(parent_size) per batch.
///
/// 3. **Last-hit parent-iter cache.** `Sized` messages from the same
///    worker arrive in clusters by parent. Caching the most recently
///    found parent iter avoids re-walking the whole model with
///    `find_row_by_path` for every message in the cluster.
async fn run_consumer(
    store: TreeStore,
    status: Rc<Status>,
    rx: async_channel::Receiver<WorkerMsg>,
) {
    /// How many messages we process before forcing a yield+flush.
    /// Picked by feel: large enough that yield overhead is negligible,
    /// small enough that the UI wakes up at least every ~16 ms during
    /// heavy bursts.
    const YIELD_EVERY: usize = 64;

    let mut dirty_parents: HashSet<Option<PathBuf>> = HashSet::new();
    let mut parent_cache: Option<(PathBuf, TreeIter)> = None;
    let mut since_yield: usize = 0;

    while let Ok(msg) = rx.recv().await {
        match msg {
            WorkerMsg::Enumerated {
                parent_path,
                entries,
            } => {
                // Bulk inserts can shuffle existing rows; invalidate
                // the parent cache and flush any pending recomputes
                // before we touch the store wholesale.
                flush_dirty(&store, &mut dirty_parents);
                parent_cache = None;
                apply_enumerated(&store, &status, parent_path, entries).await;
                since_yield = since_yield.saturating_add(1);
            }
            WorkerMsg::Sized {
                path,
                parent_path,
                size,
            } => {
                let parent_iter = lookup_parent_with_cache(
                    &store,
                    parent_path.as_ref(),
                    &mut parent_cache,
                );
                update_row_size(&store, parent_iter.as_ref(), &path, size);
                dirty_parents.insert(parent_path);
                status.complete_one();
                since_yield += 1;
            }
            WorkerMsg::EnumerateFailed {
                parent_path,
                message,
            } => {
                // Keep the exact OS error text — a user debugging a
                // permission issue can usually recognize the system
                // string (`Permission denied (os error 13)`) faster
                // than a generic "could not read".
                let parent_iter = lookup_parent_with_cache(
                    &store,
                    parent_path.as_ref(),
                    &mut parent_cache,
                );
                if let Some(parent_iter) = parent_iter.as_ref() {
                    while let Some(child) = store.iter_children(Some(parent_iter)) {
                        store.remove(&child);
                    }
                    let label = format!("(error: {message})");
                    store.insert_with_values(
                        Some(parent_iter),
                        None,
                        &[
                            (COL_NAME, &label),
                            (COL_LOADED, &true),
                            (COL_SIZED, &true),
                        ],
                    );
                }
                status.refresh();
                since_yield += 1;
            }
        }

        // Flush+yield on either of two conditions:
        //
        // 1. We've hit the per-batch budget — keeps the UI responsive
        //    during a sustained burst.
        // 2. The channel has drained to empty — without this, a small
        //    burst (say a directory with 10 children) would leave its
        //    final dirty set un-flushed indefinitely, since
        //    `since_yield` would never cross `YIELD_EVERY`. The
        //    user-visible symptom was percentage columns frozen at
        //    whatever value they had at the previous flush.
        if since_yield >= YIELD_EVERY || rx.is_empty() {
            flush_dirty(&store, &mut dirty_parents);
            glib::timeout_future(Duration::ZERO).await;
            since_yield = 0;
        }
    }

    // Final flush in case the channel closed mid-batch with pending
    // dirty parents — keeps the bar columns accurate after the last
    // worker exits.
    flush_dirty(&store, &mut dirty_parents);
}

/// Update only the size cells (`COL_SIZE_BYTES`, `COL_SIZE_HUMAN`,
/// `COL_SIZED`) of the row identified by `(parent_iter, target_path)`.
/// Percentages are *not* recomputed here — that's done in batch by
/// [`flush_dirty`].
fn update_row_size(
    store: &TreeStore,
    parent_iter: Option<&TreeIter>,
    target_path: &Path,
    size: u64,
) {
    let Some(first_child) = store.iter_children(parent_iter) else {
        return;
    };
    let Some(target) = find_iter_by_path(store, &first_child, target_path) else {
        return;
    };
    let size_human = humansize::format_size(size, humansize::BINARY);
    store.set(
        &target,
        &[
            (COL_SIZE_BYTES, &size),
            (COL_SIZE_HUMAN, &size_human),
            (COL_SIZED, &true),
        ],
    );
}

/// Recompute percentages for every parent path in `dirty`, then clear
/// the set. Each parent is touched once even if many `Sized` updates
/// landed under it during the batch.
fn flush_dirty(store: &TreeStore, dirty: &mut HashSet<Option<PathBuf>>) {
    for parent_path in dirty.drain() {
        let parent_iter = match parent_path.as_ref() {
            None => None,
            Some(p) => find_row_by_path(store, p),
        };
        recompute_percentages(store, parent_iter.as_ref());
    }
}

/// Look up the iter for `parent_path`, reusing `cache` if it's a hit.
///
/// `Sized` messages from the same worker arrive in clusters by parent,
/// so a one-slot cache catches almost all of them after the first
/// lookup. On miss we fall back to the full-tree `find_row_by_path`
/// and refresh the cache with the result.
fn lookup_parent_with_cache(
    store: &TreeStore,
    parent_path: Option<&PathBuf>,
    cache: &mut Option<(PathBuf, TreeIter)>,
) -> Option<TreeIter> {
    let p = parent_path?;
    if let Some((cp, ci)) = cache.as_ref()
        && cp == p
    {
        return Some(*ci);
    }
    let it = find_row_by_path(store, p)?;
    *cache = Some((p.clone(), it));
    Some(it)
}

/// Apply an `Enumerated` message: insert the freshly-listed children
/// under their parent row, dropping any placeholder rows already there.
///
/// Three things keep this scalable to directories with tens of
/// thousands of children:
///
/// 1. The descending-by-size sort is suspended for the duration of the
///    inserts. With sort active, GTK reorders the parent's children on
///    every single insert; suspending turns `O(n²)` worst-case
///    bookkeeping into `O(n log n)`, applied once when we re-enable
///    the sort.
///
/// 2. Inserts are chunked and the function `await`s between chunks. The
///    `glib::timeout_future(Duration::ZERO)` returns control to the GTK
///    main loop, giving it a chance to paint, dispatch input, and
///    react to the user — so the window stays responsive even when a
///    huge directory is mid-population.
///
/// 3. Placeholder removal and `recompute_percentages` happen once at
///    the end, not per-row, so they don't compound the per-insert cost.
async fn apply_enumerated(
    store: &TreeStore,
    status: &Rc<Status>,
    parent_path: Option<PathBuf>,
    entries: Vec<Entry>,
) {
    let parent_iter = match parent_path.as_ref() {
        None => None,
        Some(p) => find_row_by_path(store, p),
    };

    // Pre-count directories before we move `entries`, so the status
    // counter is updated once for the whole batch.
    let n_dirs = entries.iter().filter(|e| e.is_dir).count();
    if n_dirs > 0 {
        status.enqueue(n_dirs);
    }

    // Suspend the descending-by-size sort while we bulk-insert. With
    // sort active, GTK reorders the parent's children on every single
    // insert, which is the actual hang trigger when N is large.
    let sort_was_active = store
        .sort_column_id()
        .filter(|(col, _)| matches!(col, gtk::SortColumn::Index(_)))
        .is_some();
    if sort_was_active {
        store.set_unsorted();
    }

    // Insert real children *before* removing the placeholder row. If we
    // removed the placeholder first, GTK would briefly see the parent
    // row with zero children and auto-collapse it — making the row the
    // user just expanded snap shut on its own. Keeping at least one
    // child present at every moment avoids that collapse.
    //
    // Inserts are chunked, awaiting between chunks to yield to the
    // main loop. 256 was picked by feel: small enough that an event
    // queued during a chunk is processed within ~16 ms (one frame on a
    // 60 Hz display), large enough that the per-yield scheduling cost
    // doesn't dominate.
    const INSERT_CHUNK: usize = 256;
    for (i, entry) in entries.iter().enumerate() {
        let row_iter = append_entry_row(store, parent_iter.as_ref(), entry);
        if entry.is_dir {
            append_placeholder_row(store, &row_iter);
        }
        if (i + 1) % INSERT_CHUNK == 0 {
            glib::timeout_future(Duration::ZERO).await;
        }
    }

    // Now drop any pre-existing placeholders. We identify them by their
    // empty `COL_PATH`, which is the sentinel `append_placeholder_row`
    // sets (real entries always have a non-empty path).
    let placeholders = collect_placeholder_iters(store, parent_iter.as_ref());
    for it in &placeholders {
        store.remove(it);
    }

    if sort_was_active {
        store.set_sort_column_id(
            gtk::SortColumn::Index(COL_SIZE_BYTES),
            gtk::SortType::Descending,
        );
    }

    recompute_percentages(store, parent_iter.as_ref());

    if n_dirs == 0 {
        // Refresh the bar so a directory that turned out to contain no
        // sub-directories still flips back to "Done" if nothing else
        // is in flight.
        status.refresh();
    }
}

/// Collect every direct child iter of `parent` whose `COL_PATH` is
/// empty — the marker that distinguishes synthetic "(loading…)" rows
/// from real entries.
fn collect_placeholder_iters(store: &TreeStore, parent: Option<&TreeIter>) -> Vec<TreeIter> {
    let mut out = Vec::new();
    let Some(first) = store.iter_children(parent) else {
        return out;
    };
    let mut iter = first;
    loop {
        let path: String = store.get::<String>(&iter, COL_PATH as i32);
        if path.is_empty() {
            out.push(iter);
        }
        if !store.iter_next(&mut iter) {
            break;
        }
    }
    out
}

/// Append a single entry row and return its iter. The `Entry`'s
/// `size_in_bytes` is used as-is, so files appear at their real size
/// immediately while directories appear at zero (and are updated later
/// by the worker pipeline).
fn append_entry_row(
    store: &TreeStore,
    parent: Option<&TreeIter>,
    entry: &Entry,
) -> TreeIter {
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
    let depth = child_depth(store, parent);

    store.insert_with_values(
        parent,
        None,
        &[
            (COL_NAME, &display_name),
            (COL_SIZE_HUMAN, &size_human),
            (COL_SIZE_BYTES, &entry.size_in_bytes),
            (COL_PERCENT, &0i32),
            (COL_PATH, &entry.path.display().to_string()),
            (COL_IS_DIR, &entry.is_dir),
            // Files have no children, so they're trivially "loaded".
            (COL_LOADED, &!entry.is_dir),
            // Files have a final size already; dirs do not.
            (COL_SIZED, &!entry.is_dir),
            (COL_DEPTH, &depth),
        ],
    )
}

/// Append a synthetic placeholder child to `parent`. Its sole purpose is
/// to make GTK render a disclosure triangle on the parent — we replace
/// it with real children the first time the user expands the row.
fn append_placeholder_row(store: &TreeStore, parent: &TreeIter) {
    let depth = child_depth(store, Some(parent));
    store.insert_with_values(
        Some(parent),
        None,
        &[
            (COL_NAME, &"(loading…)"),
            (COL_SIZE_HUMAN, &""),
            (COL_SIZE_BYTES, &0u64),
            // Zero percent leaves the bar empty for placeholder rows so
            // the user doesn't mistake them for sized entries.
            (COL_PERCENT, &0i32),
            (COL_PATH, &""),
            (COL_IS_DIR, &false),
            (COL_LOADED, &true),
            (COL_SIZED, &true),
            (COL_DEPTH, &depth),
        ],
    );
}

/// Read the depth a freshly-inserted child should carry. Top-level
/// rows are depth 0; everything else is `parent.depth + 1`.
fn child_depth(store: &TreeStore, parent: Option<&TreeIter>) -> i32 {
    match parent {
        None => 0,
        Some(p) => store.get::<i32>(p, COL_DEPTH as i32) + 1,
    }
}

/// Walk the children of `parent` (or the top level if `None`) and
/// update each row's percent column to its share of the level's total.
/// Cheap O(n²) since rows-per-level is small in practice.
fn recompute_percentages(store: &TreeStore, parent: Option<&TreeIter>) {
    let mut total: u64 = 0;
    if let Some(first) = store.iter_children(parent) {
        let mut iter = first;
        loop {
            let placeholder_path: String = store.get::<String>(&iter, COL_PATH as i32);
            // Skip the synthetic "(loading…)" placeholder row which has
            // an empty COL_PATH and would otherwise inflate the total
            // with its zero size — harmless, but it's also wrong to
            // include it in the count.
            if !placeholder_path.is_empty() {
                let size: u64 = store.get::<u64>(&iter, COL_SIZE_BYTES as i32);
                total = total.saturating_add(size);
            }
            if !store.iter_next(&mut iter) {
                break;
            }
        }
    }

    if let Some(first) = store.iter_children(parent) {
        let mut iter = first;
        loop {
            let placeholder_path: String = store.get::<String>(&iter, COL_PATH as i32);
            if !placeholder_path.is_empty() {
                let size: u64 = store.get::<u64>(&iter, COL_SIZE_BYTES as i32);
                let percent = if total == 0 {
                    0
                } else {
                    ((size as u128 * 100) / total as u128) as i32
                };
                store.set(&iter, &[(COL_PERCENT, &percent)]);
            }
            if !store.iter_next(&mut iter) {
                break;
            }
        }
    }
}

/// Locate any iter in the model whose `COL_PATH` matches `path`,
/// regardless of nesting depth. Used to scope child-row lookups when
/// applying a `SizeUpdate` whose parent might live anywhere in the tree.
///
/// Earlier this function only searched the top level, which silently
/// dropped updates for deeply-nested rows: a level-3 entry's parent
/// lives at level 2, not at the top, so its size never made it into
/// the store.
fn find_row_by_path(store: &TreeStore, path: &Path) -> Option<TreeIter> {
    use std::cell::RefCell;
    let target = path.to_string_lossy().into_owned();
    // `foreach` takes an `Fn` closure (note: not `FnMut`) so we shuttle
    // the find result through a `RefCell`. Returning `true` from the
    // closure stops the walk early.
    let found: RefCell<Option<TreeIter>> = RefCell::new(None);
    store.foreach(|model, _tree_path, iter| {
        let path_str: String = model.get::<String>(iter, COL_PATH as i32);
        if path_str == target {
            *found.borrow_mut() = Some(*iter);
            true
        } else {
            false
        }
    });
    found.into_inner()
}

/// Walk `iter` and its later siblings until a row whose `COL_PATH`
/// equals `target.display()` is found.
fn find_iter_by_path(store: &TreeStore, first: &TreeIter, target: &Path) -> Option<TreeIter> {
    let target = target.to_string_lossy().into_owned();
    find_iter_by_path_inner(store, *first, &target)
}

fn find_iter_by_path_inner(
    store: &TreeStore,
    mut iter: TreeIter,
    target: &str,
) -> Option<TreeIter> {
    loop {
        let path_str: String = store.get::<String>(&iter, COL_PATH as i32);
        if path_str == target {
            return Some(iter);
        }
        if !store.iter_next(&mut iter) {
            return None;
        }
    }
}

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
        .title("Diskalyzer — error")
        .default_width(520)
        .default_height(160)
        .child(&container)
        .build();
    window.present();
}
