//! # dush gpui GUI
//!
//! A [gpui](https://gpui.rs) front-end for the `dush` library: a tree you
//! can drill into to find out where disk space goes.
//!
//! ```text
//! nix develop
//! cargo run --release --features gpui --bin dush-gpui -- ~/dev
//! ```
//!
//! The window opens immediately with the root directory expanded. Its
//! children show up as soon as `read_dir` returns, and directory sizes
//! stream in afterwards from background `diskus` walks. Expanding a
//! directory runs the same enumerate-then-size cycle one level down.
//!
//! ## Structure
//!
//! The model is [`dush::tree::Tree`]. This view keeps a flattened list
//! of its visible rows, rebuilt at most once per frame after the tree
//! changes, and hands it to gpui's `uniform_list`, which only lays out
//! the rows on screen. That keeps a directory with 50k children as cheap
//! to scroll as one with 5.

use std::ops::Range;
use std::path::PathBuf;

use clap::Parser;
use color_eyre::eyre::{Context as _, Result, eyre};
use gpui::{
    App, Application, Bounds, ClickEvent, Context, FocusHandle, Focusable, Hsla, KeyBinding,
    ScrollStrategy, SharedString, UniformListScrollHandle, Window, WindowAppearance, WindowBounds,
    WindowOptions, actions, div, prelude::*, px, relative, rgb, size, uniform_list,
};

use dush::tree::{Children, NodeId, Note, ROOT, Row, Tree};
use dush::{AnalyzeOptions, enumerate_children_with, subtree_size};

// ==============================================================================
// CLI surface
// ==============================================================================

/// Explore where disk space goes, one directory level at a time.
#[derive(Debug, Parser)]
#[command(name = "dush-gpui", version, about, long_about = None)]
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
// Theme
// ==============================================================================

/// Colors for one window appearance. gpui has no theme system of its own,
/// so we pick between two hand-tuned sets based on the platform's
/// light/dark setting.
#[derive(Debug, Clone, Copy)]
struct Theme {
    background: Hsla,
    surface: Hsla,
    border: Hsla,
    text: Hsla,
    muted: Hsla,
    hover: Hsla,
    selected: Hsla,
    track: Hsla,
    error: Hsla,
}

impl Theme {
    fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self {
                background: rgb(0x1d1f23).into(),
                surface: rgb(0x16181b).into(),
                border: rgb(0x2c2f35).into(),
                text: rgb(0xdde1e7).into(),
                muted: rgb(0x8a919c).into(),
                hover: rgb(0x262930).into(),
                selected: rgb(0x2c3a52).into(),
                track: rgb(0x2c2f35).into(),
                error: rgb(0xf08080).into(),
            },
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self {
                background: rgb(0xffffff).into(),
                surface: rgb(0xf4f5f7).into(),
                border: rgb(0xe0e3e8).into(),
                text: rgb(0x1f2328).into(),
                muted: rgb(0x687180).into(),
                hover: rgb(0xf0f2f5).into(),
                selected: rgb(0xd9e6fb).into(),
                track: rgb(0xe8eaee).into(),
                error: rgb(0xc0392b).into(),
            },
        }
    }
}

/// Bar colors, cycled by depth so neighbouring levels are easy to tell
/// apart. Mid-saturation picks that hold up on both backgrounds above.
const DEPTH_COLORS: &[u32] = &[
    0x4c8dff, // blue
    0x2bb3a3, // teal
    0xe0a526, // amber
    0xe8743b, // orange
    0xd9576b, // rose
    0x9a6bd8, // violet
];

fn depth_color(depth: usize) -> Hsla {
    rgb(DEPTH_COLORS[depth % DEPTH_COLORS.len()]).into()
}

/// gpui on Linux asks for "IBM Plex Sans" when given the system UI font
/// and silently renders nothing useful if it isn't installed. Pick the
/// first family from these lists that the system actually has.
const UI_FONTS: &[&str] = &[
    "Inter",
    "Cantarell",
    "Noto Sans",
    "Ubuntu",
    "DejaVu Sans",
    "Liberation Sans",
    "IBM Plex Sans",
];
const MONO_FONTS: &[&str] = &[
    "JetBrains Mono",
    "Fira Code",
    "Noto Sans Mono",
    "Ubuntu Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Lilex",
];

#[derive(Debug, Clone)]
struct Fonts {
    ui: SharedString,
    mono: SharedString,
}

impl Fonts {
    fn detect(cx: &App) -> Self {
        let installed = cx.text_system().all_font_names();
        let pick = |wanted: &[&str], fallback: &str| -> SharedString {
            wanted
                .iter()
                .find(|name| installed.iter().any(|have| have == *name))
                .copied()
                .unwrap_or(fallback)
                .to_string()
                .into()
        };
        Self {
            ui: pick(UI_FONTS, ".SystemUIFont"),
            // ".ZedMono" maps to Lilex, same failure mode as above, but
            // it's the best guess left if nothing in the list exists.
            mono: pick(MONO_FONTS, ".ZedMono"),
        }
    }
}

// ==============================================================================
// Actions and key bindings
// ==============================================================================

actions!(
    dush,
    [
        SelectNext,
        SelectPrevious,
        SelectFirst,
        SelectLast,
        /// Expand a collapsed directory, or step into an expanded one.
        ExpandOrDescend,
        /// Collapse an expanded directory, or step out to the parent.
        CollapseOrAscend,
        ToggleExpanded,
        Quit,
    ]
);

const KEY_CONTEXT: &str = "DushTree";

fn bind_keys(cx: &mut App) {
    let ctx = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, ctx),
        KeyBinding::new("j", SelectNext, ctx),
        KeyBinding::new("up", SelectPrevious, ctx),
        KeyBinding::new("k", SelectPrevious, ctx),
        KeyBinding::new("home", SelectFirst, ctx),
        KeyBinding::new("end", SelectLast, ctx),
        KeyBinding::new("right", ExpandOrDescend, ctx),
        KeyBinding::new("l", ExpandOrDescend, ctx),
        KeyBinding::new("left", CollapseOrAscend, ctx),
        KeyBinding::new("h", CollapseOrAscend, ctx),
        KeyBinding::new("enter", ToggleExpanded, ctx),
        KeyBinding::new("space", ToggleExpanded, ctx),
        KeyBinding::new("q", Quit, ctx),
        KeyBinding::new("ctrl-q", Quit, ctx),
        KeyBinding::new("ctrl-w", Quit, ctx),
    ]);
}

// ==============================================================================
// The view
// ==============================================================================

const ROW_HEIGHT: f32 = 26.0;
const INDENT: f32 = 16.0;
const SIZE_COLUMN: f32 = 96.0;
const BAR_COLUMN: f32 = 180.0;
const PERCENT_COLUMN: f32 = 52.0;

struct DushView {
    tree: Tree,
    rows: Vec<Row>,
    /// Set when `tree` changed after `rows` was built. See
    /// [`DushView::sync_rows`].
    rows_stale: bool,
    selected: NodeId,
    opts: AnalyzeOptions,
    /// Directory walks queued so far and how many have finished. Both
    /// only grow; the difference is what's still running.
    queued: usize,
    finished: usize,
    theme: Theme,
    fonts: Fonts,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
}

impl DushView {
    fn new(
        root: PathBuf,
        opts: AnalyzeOptions,
        fonts: Fonts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe_window_appearance(window, |this, window, cx| {
            this.theme = Theme::for_appearance(window.appearance());
            cx.notify();
        })
        .detach();

        let mut this = Self {
            tree: Tree::new(root),
            rows: Vec::new(),
            rows_stale: true,
            selected: ROOT,
            opts,
            queued: 0,
            finished: 0,
            theme: Theme::for_appearance(window.appearance()),
            fonts,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
        };
        this.expand(ROOT, cx);
        this
    }

    /// Mark the row list as out of date. The rebuild waits for
    /// [`Self::sync_rows`], so a burst of size updates within one frame
    /// costs one rebuild instead of one each.
    fn invalidate_rows(&mut self) {
        self.rows_stale = true;
    }

    /// Bring `rows` up to date. Called before anything reads it: at the
    /// start of `render` and of every keyboard action.
    fn sync_rows(&mut self) {
        if self.rows_stale {
            self.rows = self.tree.visible_rows();
            self.rows_stale = false;
        }
    }

    fn row_index_of(&self, id: NodeId) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::Node { id: row_id, .. } if *row_id == id))
    }

    // ----- Expansion and background work -----

    fn expand(&mut self, id: NodeId, cx: &mut Context<Self>) {
        let node = self.tree.node(id);
        if !node.is_dir || node.expanded {
            return;
        }
        self.tree.set_expanded(id, true);
        if self.tree.request_load(id) {
            self.load(id, cx);
        }
        self.invalidate_rows();
        cx.notify();
    }

    fn collapse(&mut self, id: NodeId, cx: &mut Context<Self>) {
        if !self.tree.node(id).expanded {
            return;
        }
        // A collapse doesn't cancel the walks it started. They are
        // already paid for, and their results make re-expanding instant.
        self.tree.set_expanded(id, false);
        self.invalidate_rows();
        cx.notify();
    }

    fn toggle(&mut self, id: NodeId, cx: &mut Context<Self>) {
        if self.tree.node(id).expanded {
            self.collapse(id, cx);
        } else {
            self.expand(id, cx);
        }
    }

    /// Enumerate `id` on the background executor, then size each of its
    /// subdirectories one at a time, publishing every result as it lands.
    ///
    /// The walks run one after another, not in parallel, because each
    /// `diskus` walk already fans out across all cores; overlapping them
    /// would only make every walk slower. Separate expansions do run
    /// concurrently, so expanding a second directory while the first is
    /// still sizing doesn't queue behind it.
    ///
    /// TODO: every expansion walks the subdirectories again, although the
    /// parent's walk already visited all of them. The page cache makes
    /// the second pass cheap, but a single walk that records a total per
    /// directory would skip it. `diskus` only reports one number per call.
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
                this.invalidate_rows();
                cx.notify();
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
                    this.invalidate_rows();
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    // ----- Selection and keyboard navigation -----

    fn select_row(&mut self, index: usize, strategy: ScrollStrategy, cx: &mut Context<Self>) {
        self.sync_rows();
        let Some(Row::Node { id, .. }) = self.rows.get(index) else {
            return;
        };
        self.selected = *id;
        // `Top` when moving up and `Bottom` when moving down only scroll
        // once the row has left the viewport, and then by exactly one
        // row, which is how arrow keys in a list are expected to behave.
        self.scroll_handle.scroll_to_item(index, strategy);
        cx.notify();
    }

    /// Move the selection by `delta` rows, skipping note rows, which
    /// can't be selected.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.sync_rows();
        let Some(current) = self.row_index_of(self.selected) else {
            return;
        };
        let mut index = current as isize;
        loop {
            index += delta;
            let Some(row) = usize::try_from(index).ok().and_then(|i| self.rows.get(i)) else {
                return;
            };
            if matches!(row, Row::Node { .. }) {
                break;
            }
        }
        let strategy = if delta < 0 {
            ScrollStrategy::Top
        } else {
            ScrollStrategy::Bottom
        };
        self.select_row(index as usize, strategy, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select_row(0, ScrollStrategy::Top, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.sync_rows();
        let last_node = self
            .rows
            .iter()
            .rposition(|row| matches!(row, Row::Node { .. }));
        if let Some(index) = last_node {
            self.select_row(index, ScrollStrategy::Bottom, cx);
        }
    }

    fn expand_or_descend(&mut self, _: &ExpandOrDescend, _: &mut Window, cx: &mut Context<Self>) {
        let node = self.tree.node(self.selected);
        if !node.is_dir {
            return;
        }
        if node.expanded {
            self.move_selection(1, cx);
        } else {
            self.expand(self.selected, cx);
        }
    }

    fn collapse_or_ascend(&mut self, _: &CollapseOrAscend, _: &mut Window, cx: &mut Context<Self>) {
        let node = self.tree.node(self.selected);
        if node.expanded {
            self.collapse(self.selected, cx);
            return;
        }
        let parent = node.parent;
        self.sync_rows();
        if let Some(index) = parent.and_then(|parent| self.row_index_of(parent)) {
            self.select_row(index, ScrollStrategy::Top, cx);
        }
    }

    fn toggle_expanded(&mut self, _: &ToggleExpanded, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle(self.selected, cx);
    }

    fn quit(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    // ----- Rendering -----

    fn render_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        range
            .filter_map(|index| self.rows.get(index).copied())
            .map(|row| match row {
                Row::Node { id, share } => self.render_node_row(id, share, cx).into_any_element(),
                Row::Note { parent, note } => self.render_note_row(parent, note).into_any_element(),
            })
            .collect()
    }

    fn render_node_row(&self, id: NodeId, share: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let node = self.tree.node(id);

        let chevron = match (node.is_dir, node.expanded) {
            (false, _) => "",
            (true, false) => "▸",
            (true, true) => "▾",
        };
        let name: SharedString = if node.is_dir && id != ROOT {
            format!("{}/", node.name).into()
        } else {
            node.name.clone().into()
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

        let row = div()
            .id(("node", id))
            .h(px(ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .pl(px(8.0 + node.depth as f32 * INDENT))
            .pr_3()
            .cursor_pointer()
            .when(id == self.selected, |row| row.bg(theme.selected))
            .when(id != self.selected, |row| {
                row.hover(move |style| style.bg(theme.hover))
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.selected = id;
                window.focus(&this.focus_handle);
                this.toggle(id, cx);
                cx.notify();
            }));

        row.child(
            div()
                .w(px(12.0))
                .flex_shrink_0()
                .text_color(theme.muted)
                .child(chevron),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when(id == ROOT, |name| {
                    name.font_weight(gpui::FontWeight::SEMIBOLD)
                })
                .child(name),
        )
        .when(node.unreadable > 0, |row| {
            row.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(theme.error)
                    .child(format!("{} unreadable", node.unreadable)),
            )
        })
        .child(
            div()
                .w(px(SIZE_COLUMN))
                .flex_shrink_0()
                .flex()
                .justify_end()
                .font_family(self.fonts.mono.clone())
                .text_color(if node.sized { theme.text } else { theme.muted })
                .child(size_label),
        )
        .child(
            div()
                .w(px(BAR_COLUMN))
                .h(px(10.0))
                .flex_shrink_0()
                .rounded_sm()
                .overflow_hidden()
                .bg(theme.track)
                .child(
                    div()
                        .h_full()
                        .w(relative(share.clamp(0.0, 1.0)))
                        .bg(depth_color(node.depth)),
                ),
        )
        .child(
            div()
                .w(px(PERCENT_COLUMN))
                .flex_shrink_0()
                .flex()
                .justify_end()
                .font_family(self.fonts.mono.clone())
                .text_color(theme.muted)
                .child(percent),
        )
    }

    fn render_note_row(&self, parent: NodeId, note: Note) -> impl IntoElement {
        let theme = self.theme;
        let node = self.tree.node(parent);
        let (text, color): (SharedString, Hsla) = match (note, &node.children) {
            (Note::Failed, Children::Failed(message)) => (message.clone().into(), theme.error),
            (Note::Loading, _) => ("Reading directory…".into(), theme.muted),
            (Note::Empty, _) | (Note::Failed, _) => ("Empty".into(), theme.muted),
        };
        div()
            .id(("note", parent))
            .h(px(ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            // Line the text up with the children's names: past the
            // child's indent plus its chevron column and gap.
            .pl(px(8.0 + (node.depth + 1) as f32 * INDENT + 12.0 + 8.0))
            .pr_3()
            .italic()
            .text_color(color)
            .truncate()
            .child(text)
    }

    fn render_header(&self) -> impl IntoElement {
        let theme = self.theme;
        let label = |text: &'static str, width: f32| {
            div()
                .w(px(width))
                .flex_shrink_0()
                .flex()
                .justify_end()
                .child(text)
        };
        div()
            .h(px(ROW_HEIGHT))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            // Name column starts where the root's name does: left
            // padding plus chevron column plus gap.
            .pl(px(8.0 + 12.0 + 8.0))
            .pr_3()
            .bg(theme.surface)
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted)
            .child(div().flex_1().child("Name"))
            .child(label("Size", SIZE_COLUMN))
            .child(
                div()
                    .w(px(BAR_COLUMN))
                    .flex_shrink_0()
                    .child("Share of parent"),
            )
            .child(label("", PERCENT_COLUMN))
    }

    fn render_status(&self) -> impl IntoElement {
        let theme = self.theme;
        let pending = self.queued - self.finished;
        let mode = if self.opts.apparent_size {
            "apparent size"
        } else {
            "disk usage"
        };
        let progress = if pending > 0 {
            format!(
                "Sizing {} of {} directories…",
                self.finished + 1,
                self.queued
            )
        } else {
            format!(
                "{} {} sized",
                self.finished,
                if self.finished == 1 {
                    "directory"
                } else {
                    "directories"
                }
            )
        };
        let root = self.tree.node(ROOT);
        let total = if root.sized {
            format!("{} total", human(root.size))
        } else {
            format!("{} so far", human(root.size))
        };

        div()
            .h(px(ROW_HEIGHT))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_4()
            .px_3()
            .bg(theme.surface)
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted)
            .child(div().flex_1().child(progress))
            .child(mode)
            .child(div().text_color(theme.text).child(total))
    }
}

impl Focusable for DushView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DushView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_rows();
        let theme = self.theme;
        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::expand_or_descend))
            .on_action(cx.listener(Self::collapse_or_ascend))
            .on_action(cx.listener(Self::toggle_expanded))
            .on_action(cx.listener(Self::quit))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.text)
            .text_sm()
            .font_family(self.fonts.ui.clone())
            .child(self.render_header())
            .child(
                uniform_list(
                    "rows",
                    self.rows.len(),
                    cx.processor(|this, range, _window, cx| this.render_rows(range, cx)),
                )
                .track_scroll(self.scroll_handle.clone())
                .flex_1(),
            )
            .child(self.render_status())
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

    Application::new().run(move |cx: &mut App| {
        bind_keys(cx);
        cx.on_window_closed(|cx| cx.quit()).detach();

        let fonts = Fonts::detect(cx);
        let bounds = Bounds::centered(None, size(px(960.0), px(640.0)), cx);
        let title = format!("dush · {}", root.display());
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                app_id: Some("dush".into()),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| DushView::new(root, opts, fonts, window, cx));
                window.focus(&view.focus_handle(cx));
                view
            },
        );
        if let Err(err) = opened {
            eprintln!("dush-gpui: could not open a window: {err:#}");
            cx.quit();
        }
    });
    Ok(())
}
