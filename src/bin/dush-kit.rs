//! # dush gpui-kit GUI
//!
//! The same explorer as `dush-gpui`, built from [GPUI Kit](https://gpui-kit.com)
//! components instead of hand-drawn rows: the kit's `Tree` for the
//! hierarchy, keyboard navigation and expand state, `Progress` for the
//! share bars, Lucide icons, and the kit's light and dark themes.
//!
//! ```text
//! nix develop
//! cargo run --release --features gpui-kit --bin dush-kit -- ~/dev
//! ```
//!
//! ## Structure
//!
//! The model is [`dush::tree::Tree`], same as in `dush-gpui`. The kit's
//! `TreeState` owns its own copy of the hierarchy as nested `TreeItem`s,
//! so every change to the model is followed by a rebuild of those items
//! ([`DushKit::sync_items`]). Two details shape that rebuild:
//!
//! - The kit treats an item as a folder only if it has children, and it
//!   keeps expanded subtrees in the item it hands out. Collapsed
//!   directories therefore get a single placeholder child, so they stay
//!   expandable and a rebuild only costs as much as what is visible.
//! - `TreeState::set_items` clears the selection, so the rebuild restores
//!   it by item id.
//!
//! Size updates arrive many times a second while walks run. They only
//! schedule a rebuild, coalesced to one per frame interval; expansion
//! events rebuild right away so a freshly opened directory never shows
//! its placeholder.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use color_eyre::eyre::{Context as _, Result, eyre};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tree::{TreeEntry, TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, Theme, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use dush::tree::{Children, NodeId, ROOT, Tree};
use dush::{AnalyzeOptions, enumerate_children_with, subtree_size};

// ==============================================================================
// CLI surface
// ==============================================================================

/// Explore where disk space goes, one directory level at a time.
#[derive(Debug, Parser)]
#[command(name = "dush-kit", version, about, long_about = None)]
struct Cli {
    /// Directory to explore. Defaults to the current working directory.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Report apparent size (logical bytes) instead of disk usage
    /// (physical blocks). Equivalent to `du --apparent-size`.
    #[arg(long, short = 'a')]
    apparent_size: bool,
}

// ==============================================================================
// Item ids
// ==============================================================================
//
// `TreeItem` ids are strings. Nodes and placeholder rows get distinct
// prefixes so a row can tell which one it is rendering, and so a
// placeholder never collides with a real node of the same number.

fn node_item_id(id: NodeId) -> SharedString {
    format!("n{id}").into()
}

fn note_item_id(parent: NodeId) -> SharedString {
    format!("note{parent}").into()
}

enum ItemKind {
    Node(NodeId),
    /// The placeholder under a directory whose children aren't shown.
    Note(NodeId),
}

fn parse_item_id(id: &str) -> Option<ItemKind> {
    if let Some(parent) = id.strip_prefix("note") {
        return parent.parse().ok().map(ItemKind::Note);
    }
    id.strip_prefix('n')?.parse().ok().map(ItemKind::Node)
}

// ==============================================================================
// Actions
// ==============================================================================

actions!(dush, [Quit]);

/// Arrow keys, enter and clicks come from the kit's tree. These only add
/// ways out.
fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("q", Quit, None),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("ctrl-w", Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}

// ==============================================================================
// The view
// ==============================================================================

const INDENT: f32 = 16.0;
const SIZE_COLUMN: f32 = 88.0;
const BAR_COLUMN: f32 = 160.0;
const PERCENT_COLUMN: f32 = 60.0;

/// How long size updates may pile up before the items are rebuilt. One
/// frame at 60 Hz.
const SYNC_INTERVAL: Duration = Duration::from_millis(16);

struct DushKit {
    tree: Tree,
    tree_state: Entity<TreeState>,
    /// Each visible node's fraction of its parent level, as of the last
    /// [`DushKit::sync_items`]. Rows look their share up here because the
    /// kit's row callback only knows the item id.
    shares: HashMap<NodeId, f32>,
    opts: AnalyzeOptions,
    /// Directory walks queued so far and how many have finished. Both
    /// only grow; the difference is what's still running.
    queued: usize,
    finished: usize,
    sync_scheduled: bool,
    _subscriptions: Vec<Subscription>,
}

impl DushKit {
    fn new(
        root: PathBuf,
        opts: AnalyzeOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tree_state = cx.new(|cx| TreeState::new(cx));
        let subscriptions = vec![
            cx.subscribe(&tree_state, Self::on_tree_event),
            cx.observe_window_appearance(window, |_, window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
            }),
        ];
        tree_state.update(cx, |state, cx| state.focus(window, cx));

        let mut this = Self {
            tree: Tree::new(root),
            tree_state,
            shares: HashMap::new(),
            opts,
            queued: 0,
            finished: 0,
            sync_scheduled: false,
            _subscriptions: subscriptions,
        };
        this.tree.set_expanded(ROOT, true);
        if this.tree.request_load(ROOT) {
            this.load(ROOT, cx);
        }
        this.sync_items(cx);
        this.tree_state
            .update(cx, |state, cx| state.set_selected_index(Some(0), cx));
        this
    }

    /// The kit toggled a folder. Mirror it in the model, start loading if
    /// this is the directory's first expansion, and rebuild right away:
    /// waiting for the next scheduled sync would flash the placeholder.
    fn on_tree_event(&mut self, _: Entity<TreeState>, event: &TreeEvent, cx: &mut Context<Self>) {
        let (id, expanded) = match event {
            TreeEvent::Expanded(id) => (id, true),
            TreeEvent::Collapsed(id) => (id, false),
        };
        let Some(ItemKind::Node(id)) = parse_item_id(id) else {
            return;
        };
        // A collapse doesn't cancel the walks the expansion started. They
        // are already paid for, and their results make re-expanding
        // instant.
        self.tree.set_expanded(id, expanded);
        if expanded && self.tree.request_load(id) {
            self.load(id, cx);
        }
        self.sync_items(cx);
    }

    /// Ask for a rebuild within [`SYNC_INTERVAL`], unless one is already
    /// on its way.
    fn schedule_sync(&mut self, cx: &mut Context<Self>) {
        if self.sync_scheduled {
            return;
        }
        self.sync_scheduled = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SYNC_INTERVAL).await;
            // The view is gone once the window closes; nothing to sync.
            let _ = this.update(cx, |this, cx| this.sync_items(cx));
        })
        .detach();
    }

    /// Rebuild the kit's items from the model and hand them over, keeping
    /// the selection.
    fn sync_items(&mut self, cx: &mut Context<Self>) {
        self.sync_scheduled = false;
        self.tree.settle();
        self.shares.clear();
        self.shares.insert(ROOT, 1.0);
        let root = self.build_item(ROOT);

        self.tree_state.update(cx, |state, cx| {
            let selected = state.selected_item().map(|item| item.id.clone());
            state.set_items(vec![root], cx);
            if let Some(selected) = selected {
                let index = state.index_of(&selected);
                state.set_selected_index(index, cx);
            }
        });
        cx.notify();
    }

    fn build_item(&mut self, id: NodeId) -> TreeItem {
        let node = self.tree.node(id);
        let item = TreeItem::new(node_item_id(id), node.name.clone()).expanded(node.expanded);
        if !node.is_dir {
            return item;
        }
        match (&node.children, node.expanded) {
            // An empty directory has nothing to expand into, and the kit
            // agrees: no children means it renders as a leaf.
            (Children::Loaded(ids), _) if ids.is_empty() => item,
            (Children::Loaded(_), true) => {
                let children = self.tree.children_with_shares(id);
                let mut items = Vec::with_capacity(children.len());
                for (child, share) in children {
                    self.shares.insert(child, share);
                    items.push(self.build_item(child));
                }
                item.children(items)
            }
            // Collapsed, still loading, unloaded or failed: one placeholder
            // row, which keeps the directory expandable and carries the
            // loading or error text once it is open.
            _ => item.child(TreeItem::new(note_item_id(id), "")),
        }
    }

    /// Enumerate `id` on the background executor, then size each of its
    /// subdirectories one at a time, publishing every result as it lands.
    ///
    /// The walks run one after another, not in parallel, because each
    /// `diskus` walk already fans out across all cores. Separate
    /// expansions do run concurrently.
    ///
    /// TODO: shares `dush-gpui`'s re-walk of subdirectories that the
    /// parent's walk already visited; see the TODO on its `load`.
    fn load(&mut self, id: NodeId, cx: &mut Context<Self>) {
        let path = self.tree.node(id).path.clone();
        let opts = self.opts;
        cx.spawn(async move |this, cx| {
            let enumerated = cx
                .background_executor()
                .spawn(async move { enumerate_children_with(&path, &opts) })
                .await;

            // `update` fails only once the window, and with it the view,
            // is gone. There's nobody left to show results to, so stop.
            let Ok(pending_dirs) = this.update(cx, |this, cx| {
                let pending_dirs = match enumerated {
                    Ok(entries) => this.tree.insert_children(id, entries),
                    Err(err) => {
                        this.tree.set_failed(id, err.root_cause().to_string());
                        Vec::new()
                    }
                };
                this.queued += pending_dirs.len();
                this.schedule_sync(cx);
                pending_dirs
            }) else {
                return;
            };

            for (child, path) in pending_dirs {
                let (size, errors) = cx
                    .background_executor()
                    .spawn(async move { subtree_size(&path, &opts) })
                    .await;
                let updated = this.update(cx, |this, cx| {
                    this.tree.set_size(child, size, errors.len());
                    this.finished += 1;
                    this.schedule_sync(cx);
                });
                if updated.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    // ----- Rendering -----

    fn render_entry(&self, ix: usize, entry: &TreeEntry, cx: &App) -> ListItem {
        let row = ListItem::new(ix).pl(px(8.0 + entry.depth() as f32 * INDENT));
        match parse_item_id(&entry.item().id) {
            Some(ItemKind::Node(id)) => self.render_node(row, id, entry, cx),
            Some(ItemKind::Note(parent)) => self.render_note(row, parent, cx),
            None => row,
        }
    }

    fn render_node(&self, row: ListItem, id: NodeId, entry: &TreeEntry, cx: &App) -> ListItem {
        let theme = cx.theme();
        let node = self.tree.node(id);
        let share = self.shares.get(&id).copied().unwrap_or(0.0);

        let chevron = if entry.is_folder() {
            let icon = if entry.is_expanded() {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            };
            Icon::new(icon).xsmall().text_color(theme.muted_foreground)
        } else {
            // Same width as a chevron so names line up across rows.
            Icon::empty().xsmall()
        };
        let kind_icon = match (id, node.is_dir, entry.is_expanded()) {
            (ROOT, _, _) => IconName::HardDrive,
            (_, true, true) => IconName::FolderOpen,
            (_, true, false) => IconName::Folder,
            (_, false, _) => IconName::File,
        };
        let size_label: SharedString = match (node.sized, node.size) {
            (false, 0) => "…".into(),
            (false, size) => format!("{}…", human(size)).into(),
            (true, size) => human(size).into(),
        };
        let percent: SharedString = if node.sized || node.size > 0 {
            format!("{:.1}%", share * 100.0).into()
        } else {
            SharedString::default()
        };
        // Neighbouring levels get different bar colors so a deep path is
        // easy to follow. The theme's chart palette is tuned for both
        // light and dark backgrounds.
        let chart = [
            theme.chart_1,
            theme.chart_2,
            theme.chart_3,
            theme.chart_4,
            theme.chart_5,
        ];
        let bar_color = chart[node.depth % chart.len()];

        row.child(
            h_flex()
                .w_full()
                .gap_2()
                .child(chevron)
                .child(Icon::new(kind_icon).small().text_color(if node.is_dir {
                    theme.primary
                } else {
                    theme.muted_foreground
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .when(id == ROOT, |name| name.font_semibold())
                        .child(SharedString::from(node.name.clone())),
                )
                .when(node.unreadable > 0, |row| {
                    row.child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(format!("{} unreadable", node.unreadable)),
                    )
                })
                .child(
                    div()
                        .w(px(SIZE_COLUMN))
                        .flex_shrink_0()
                        .flex()
                        .justify_end()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(if node.sized {
                            theme.foreground
                        } else {
                            theme.muted_foreground
                        })
                        .child(size_label),
                )
                .child(
                    Progress::new(("share", id))
                        .w(px(BAR_COLUMN))
                        .flex_shrink_0()
                        .small()
                        .color(bar_color)
                        .value(share * 100.0)
                        .accessibility_label(format!("{} of parent", percent)),
                )
                .child(
                    div()
                        .w(px(PERCENT_COLUMN))
                        .flex_shrink_0()
                        .flex()
                        .justify_end()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(percent),
                ),
        )
    }

    fn render_note(&self, row: ListItem, parent: NodeId, cx: &App) -> ListItem {
        let theme = cx.theme();
        let content = match &self.tree.node(parent).children {
            Children::Failed(message) => h_flex()
                .gap_2()
                .text_color(theme.danger)
                .child(Icon::new(IconName::CircleAlert).small())
                .child(message.clone()),
            // Loading, or a collapsed directory whose placeholder the kit
            // briefly shows while the expansion event is in flight.
            _ => h_flex()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(Spinner::new().small())
                .child("Reading directory…"),
        };
        // Line the text up with the names of real rows: past the chevron.
        row.child(content.pl(px(INDENT)).italic())
    }

    fn render_status(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let pending = self.queued - self.finished;
        let progress = if pending > 0 {
            format!(
                "Sizing {} of {} directories…",
                self.finished + 1,
                self.queued
            )
        } else {
            let noun = if self.finished == 1 {
                "directory"
            } else {
                "directories"
            };
            format!("{} {noun} sized", self.finished)
        };
        let mode = if self.opts.apparent_size {
            "apparent size"
        } else {
            "disk usage"
        };
        let root = self.tree.node(ROOT);
        let total = if root.sized {
            format!("{} total", human(root.size))
        } else {
            format!("{} so far", human(root.size))
        };

        h_flex()
            .flex_shrink_0()
            .h(px(28.0))
            .px_3()
            .gap_4()
            .bg(theme.status_bar)
            .border_t_1()
            .border_color(theme.status_bar_border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .when(pending > 0, |status| status.child(Spinner::new().xsmall()))
            .child(div().flex_1().child(progress))
            .child(mode)
            .child(div().text_color(theme.foreground).child(total))
    }
}

impl Render for DushKit {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The kit stores this callback in `TreeState`, which this view
        // owns. A weak handle keeps that from becoming a reference cycle.
        let view = cx.entity().downgrade();
        let rows = tree(
            &self.tree_state,
            move |ix, entry, _selected, _window, cx| match view.upgrade() {
                Some(view) => view.read(cx).render_entry(ix, entry, cx),
                None => ListItem::new(ix),
            },
        );

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_sm()
            .child(div().flex_1().min_h_0().p_1().child(rows))
            .child(self.render_status(cx))
    }
}

fn human(bytes: u64) -> String {
    humansize::format_size(bytes, humansize::BINARY)
}

// ==============================================================================
// Entry point
// ==============================================================================

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    // Fail on the terminal before any window shows up: a bad path is a
    // typo, and an empty window with an error row is a worse way to say so.
    let root = std::fs::canonicalize(&cli.path)
        .with_context(|| format!("while resolving root path {}", cli.path.display()))?;
    if !root.is_dir() {
        return Err(eyre!("{} is not a directory", root.display()));
    }
    let opts = AnalyzeOptions {
        apparent_size: cli.apparent_size,
    };

    // The kit draws its icons from SVGs in an asset source that the app
    // has to register; without it every icon renders as empty space.
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            bind_keys(cx);
            cx.on_window_closed(|cx, _| cx.quit()).detach();

            let bounds = Bounds::centered(None, size(px(960.0), px(640.0)), cx);
            let title = format!("dush · {}", root.display());
            let opened = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(title.into()),
                        ..Default::default()
                    }),
                    app_id: Some("dush".into()),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    Theme::sync_system_appearance(Some(window), cx);
                    cx.new(|cx| DushKit::new(root, opts, window, cx))
                },
            );
            if let Err(err) = opened {
                eprintln!("dush-kit: could not open a window: {err:#}");
                cx.quit();
            }
        });
    Ok(())
}
