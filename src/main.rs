//! # dush CLI
//!
//! Interactive disk-usage breakdown for a directory. Given a path, prints
//! its immediate children sorted by size descending, with a unicode bar
//! visualization and a percent-of-total column. Sizing uses the `diskus`
//! crate via the `dush` library, so traversal is parallelized and
//! hardlinks are deduplicated where the platform supports it.
//!
//! Example:
//!
//! ```text
//! $ dush ~/dev
//!   3.4 GiB  ████████████████░░░░░░░░  68.1%  big-monorepo/
//!   1.2 GiB  ████████░░░░░░░░░░░░░░░░  24.3%  rust-projects/
//! 280.0 MiB  █░░░░░░░░░░░░░░░░░░░░░░░   5.5%  scratch/
//! ───────────
//!   5.0 GiB  total
//! ```

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use color_eyre::eyre::{Context, Result};
use humansize::{BINARY, format_size};

use dush::{Analysis, AnalyzeOptions, Entry, Progress, analyze, analyze_with_progress};

// ==============================================================================
// CLI surface
// ==============================================================================

/// Analyze disk space usage of a directory by listing its largest children.
#[derive(Debug, Parser)]
#[command(name = "dush", version, about, long_about = None)]
struct Cli {
    /// Directory to analyze. Defaults to the current working directory.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Report apparent size (logical bytes) instead of disk usage
    /// (physical blocks). Equivalent to `du --apparent-size`.
    #[arg(long, short = 'a')]
    apparent_size: bool,

    /// Limit output to the N largest children.
    #[arg(long, short = 'n', default_value_t = 10, conflicts_with = "all")]
    top: usize,

    /// Show every entry, ignoring `--top`.
    #[arg(long, short = 'A')]
    all: bool,

    /// How many directory levels to expand in the `pretty` format.
    /// `1` (the default) shows only the immediate children of the root,
    /// matching `du -sh`'s output shape. `2` additionally drills into
    /// each top-level directory and shows *its* top-N children, and so
    /// on. Has no effect on the `text`/`csv`/`tsv`/`json` formats.
    #[arg(long, short = 'L', default_value_t = 1)]
    levels: usize,

    /// Output format. Machine-readable formats (`csv`, `tsv`, `json`)
    /// always emit every entry — `--top` and `--all` only affect the
    /// human-readable `pretty` format.
    #[arg(long, short = 'f', value_enum, default_value_t = OutputFormat::Pretty)]
    format: OutputFormat,
}

/// Output format selector. The `pretty` variant produces the unicode-bar
/// report that `dush` shows by default; the others produce machine-
/// readable output suitable for piping into another tool.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Pretty,
    /// Plain two-column human-readable output (size + name, one entry per
    /// line). No bars, no percentages, no decoration — useful when copying
    /// into a doc or comparing two runs with `diff`.
    Text,
    Csv,
    Tsv,
    Json,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    let opts = AnalyzeOptions {
        apparent_size: cli.apparent_size,
    };

    // Live progress is only useful (and only safe to draw) when stderr is
    // attached to a terminal — otherwise the carriage-return redraws would
    // leave noise in pipes and log files.
    let mut reporter = ProgressReporter::new(std::io::stderr().is_terminal());
    let analysis = analyze_with_progress(&cli.path, &opts, |event| reporter.handle(event))?;

    // `--all` overrides `--top`; otherwise apply the requested limit. Both
    // human-readable formats honor it; machine formats always emit every
    // entry so the consumer never has to wonder whether the dataset was
    // truncated upstream.
    let top = if cli.all { None } else { Some(cli.top) };

    let levels = cli.levels.max(1);

    match cli.format {
        OutputFormat::Pretty => print_report(&analysis, top, levels, &opts),
        OutputFormat::Text => render_text(&analysis, top),
        OutputFormat::Csv => render_delimited(&analysis, b',')?,
        OutputFormat::Tsv => render_delimited(&analysis, b'\t')?,
        OutputFormat::Json => render_json(&analysis)?,
    }
    Ok(())
}

// ==============================================================================
// Progress reporting
// ==============================================================================

/// Live progress indicator drawn on stderr.
///
/// Each `Started` event advances a small braille spinner and rewrites the
/// status line in place via carriage-return; `Done` clears the line so that
/// the subsequent report on stdout starts on a clean row. We track the
/// previously-drawn line length so that we can pad with spaces on shorter
/// updates and avoid leaving stale tail characters behind.
///
/// When stderr is not a terminal — e.g. piped to a file — the reporter is
/// inert. We deliberately do not fall back to per-line "[3/12] foo" output
/// in that case: the typical non-tty consumer is a pipeline that wants the
/// stdout report verbatim and would otherwise need to filter stderr noise.
struct ProgressReporter {
    enabled: bool,
    spinner_idx: usize,
    last_len: usize,
}

impl ProgressReporter {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            spinner_idx: 0,
            last_len: 0,
        }
    }

    fn handle(&mut self, event: Progress<'_>) {
        if !self.enabled {
            return;
        }
        match event {
            Progress::Started { index, total, name } => {
                // Braille-pattern spinner — present in essentially every
                // unicode font shipped this decade and visually quieter
                // than the spinning "/-\|" ASCII alternative.
                const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
                let glyph = SPINNER[self.spinner_idx % SPINNER.len()];
                self.spinner_idx = self.spinner_idx.wrapping_add(1);

                // Cap displayed name to avoid wrapping in narrow terminals.
                // 60 chars is conservative for an 80-column shell with a
                // [N/M] prefix.
                let display_name = truncate_for_display(name, 60);
                let line = format!("{glyph} analyzing [{index}/{total}] {display_name}");
                self.draw(&line);
            }
            Progress::Finished { .. } => {
                // We could redraw with "✓" here, but the next Started
                // event will overwrite it instantly; suppressing keeps the
                // spinner from flashing between ticks.
            }
            Progress::Done => self.clear(),
        }
    }

    fn draw(&mut self, line: &str) {
        // Pad the new line out to the previous length so any leftover
        // characters from a longer prior line are blanked. Then carriage-
        // return without newline so the next draw overwrites this one.
        let visible_len = line.chars().count();
        let pad = self.last_len.saturating_sub(visible_len);
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\r{}{}", line, " ".repeat(pad));
        let _ = stderr.flush();
        self.last_len = visible_len;
    }

    fn clear(&mut self) {
        if self.last_len == 0 {
            return;
        }
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\r{}\r", " ".repeat(self.last_len));
        let _ = stderr.flush();
        self.last_len = 0;
    }
}

/// Truncate `s` to at most `max` characters, replacing the trailing run
/// with `…` so the result is always ≤ `max`. We measure in `char` count
/// rather than byte length because the names we render may contain
/// multi-byte unicode — but we deliberately do not handle east-asian
/// double-width glyphs, since adding `unicode-width` for that one cosmetic
/// edge case isn't worth the dependency.
fn truncate_for_display(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

// ==============================================================================
// Rendering
// ==============================================================================

/// Width, in characters, of the unicode bar column. Chosen to fit
/// comfortably inside an 80-column terminal alongside the size and percent
/// columns. If we ever query the actual terminal width we should clamp this
/// upward but never below ~10 to keep the bar visually meaningful.
const BAR_WIDTH: usize = 24;

/// 24-bit ANSI foreground escapes for each depth in the pretty report.
/// The hex values match the GUI's `PALETTE_RGB` so a user looking at
/// both surfaces sees the same per-depth color. Listed in the order
/// `(open_escape, …)` so we can index by `depth % len`.
const ANSI_DEPTH_PALETTE: &[&str] = &[
    "\x1b[38;2;53;132;228m", // blue
    "\x1b[38;2;51;209;122m", // green
    "\x1b[38;2;246;211;45m", // yellow
    "\x1b[38;2;255;120;0m",  // orange
    "\x1b[38;2;224;27;36m",  // red
    "\x1b[38;2;145;65;172m", // purple
    "\x1b[38;2;181;131;90m", // brown
];
const ANSI_RESET: &str = "\x1b[0m";

/// Decide whether the pretty output should use ANSI color, following
/// the de-facto-standard rules:
///
/// - The `NO_COLOR` environment variable, *if set to anything*,
///   disables color (per <https://no-color.org/>).
/// - Otherwise we only emit color if stdout is a terminal — piping
///   into `less` or a file would otherwise leave raw escape sequences
///   in the output.
fn pretty_use_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Per-depth `(open, close)` ANSI escape pair, or empty strings when
/// color is disabled. The pair is splice-friendly: `{open}foo{close}`
/// works in any `format!` string with no width math.
fn ansi_color(depth: usize, enabled: bool) -> (&'static str, &'static str) {
    if !enabled {
        return ("", "");
    }
    let open = ANSI_DEPTH_PALETTE[depth % ANSI_DEPTH_PALETTE.len()];
    (open, ANSI_RESET)
}

/// One row to render: an entry plus the bookkeeping that says where it
/// sits in the tree. `level_total` is the total of the entry's
/// *immediate* sibling group, so the percent column always renormalizes
/// per level (just like `du -sh` users expect).
struct RenderRow {
    size_in_bytes: u64,
    /// Sum of sizes within this entry's sibling group; the divisor for
    /// the percent column.
    level_total: u64,
    is_dir: bool,
    /// Tree-drawing prefix that goes immediately before the name. Empty
    /// for top-level rows; otherwise composed of `│  `, `├─ `, `└─ `,
    /// or `   ` segments depending on each ancestor's last-child status.
    line_prefix: String,
    name: String,
    /// 0 for top-level rows, +1 per nesting level. Used to pick the
    /// row marker.
    depth: usize,
}

/// Print the full report. With `levels == 1` this is a flat top-N
/// listing; with higher values we recursively analyze each top-N
/// directory and indent its top-N children underneath, drawing standard
/// `tree(1)`-style connectors so the structure is obvious.
fn print_report(analysis: &Analysis, top: Option<usize>, levels: usize, opts: &AnalyzeOptions) {
    // Collect every row we plan to print up front. Two reasons:
    //
    // 1. The size column is right-aligned to the widest size string we
    //    render, which we don't know until we've recursed.
    // 2. Recursive analysis happens inside `collect_rows`; doing it as
    //    a side-effect of printing would interleave stdout with the
    //    `diskus` walker's own internal threads' output.
    let mut rows: Vec<RenderRow> = Vec::new();
    collect_rows(analysis, top, levels, opts, "", 0, &mut rows);

    if rows.is_empty() {
        println!("(empty: {} contains no entries)", analysis.root().display());
        return;
    }

    let use_color = pretty_use_color();

    // Pre-format every size string so we can right-align them: human-
    // readable sizes vary in length ("12 B" vs. "999.9 MiB") and right-
    // alignment is the convention for numeric columns.
    let size_strs: Vec<String> = rows
        .iter()
        .map(|r| format_size(r.size_in_bytes, BINARY))
        .collect();
    let size_col_width = size_strs.iter().map(|s| s.len()).max().unwrap_or(0);

    // Header. The column widths match the per-row layout below so the
    // labels line up over their data.
    println!(
        "  {size:>width$}   {bar:<bar_width$}   {pct:>6}   name",
        size = "size",
        width = size_col_width,
        bar = "share",
        // The bar column is `▏` + width interior cells + `▕`, so its
        // visual width is `BAR_WIDTH + 2`.
        bar_width = BAR_WIDTH + 2,
        pct = "  pct",
    );

    for (row, size_str) in rows.iter().zip(size_strs.iter()) {
        let bar = render_bar(row.size_in_bytes, row.level_total, BAR_WIDTH);
        let pct = percentage(row.size_in_bytes, row.level_total);
        // Marker only for top-level rows; nested rows are already
        // visually distinguished by their tree-prefix indent, so a
        // marker there would be redundant noise.
        let marker = if row.depth == 0 {
            if row.is_dir { '▸' } else { '·' }
        } else {
            ' '
        };
        let suffix = if row.is_dir { "/" } else { "" };
        // Color the marker, the bar, and the tree prefix by depth so
        // the eye can track levels visually. The name itself stays in
        // the default foreground for readability.
        let (col_open, col_close) = ansi_color(row.depth, use_color);
        println!(
            "{c0}{marker}{c1} {size:>width$}   {c0}{bar}{c1}   {pct:>5.1}%   {c0}{prefix}{c1}{name}{suffix}",
            c0 = col_open,
            c1 = col_close,
            marker = marker,
            size = size_str,
            width = size_col_width,
            bar = bar,
            pct = pct,
            prefix = row.line_prefix,
            name = row.name,
            suffix = suffix,
        );
    }

    // Total row: a heavy box-drawing rule (U+2501) under the size column,
    // then the grand total prefixed with the summation sign Σ. The total
    // is for the *root* analysis only — sub-level totals are implicit in
    // each parent row's size.
    let total = analysis.total_in_bytes();
    let total_str = format_size(total, BINARY);
    let separator_width = size_col_width.max(total_str.len());
    println!("  {}", "━".repeat(separator_width));
    let truncated = top.is_some_and(|n| n < analysis.entries().len()) || levels > 1;
    let label = if truncated { "total (root)" } else { "total" };
    println!(
        "Σ {:>width$}   {}",
        total_str,
        label,
        width = separator_width
    );

    print_warnings(analysis);
}

/// Recursively gather rows for the pretty report. Top-N truncation
/// applies at every level, so deep trees can't blow up the output.
///
/// Errors from sub-level analyses are intentionally swallowed: we
/// already report root-level errors via `print_warnings`, and we don't
/// want a single permission-denied subdirectory to abort the rest of
/// the listing.
fn collect_rows(
    analysis: &Analysis,
    top: Option<usize>,
    levels: usize,
    opts: &AnalyzeOptions,
    parent_prefix: &str,
    depth: usize,
    out: &mut Vec<RenderRow>,
) {
    let visible: &[Entry] = match top {
        Some(n) => &analysis.entries()[..analysis.entries().len().min(n)],
        None => analysis.entries(),
    };
    let level_total = analysis.total_in_bytes();
    let n = visible.len();
    for (i, entry) in visible.iter().enumerate() {
        let is_last = i + 1 == n;
        let (branch, child_part): (&str, &str) = if depth == 0 {
            // Top level keeps no tree prefix — the row markers (▸/·)
            // already differentiate dirs from files at a glance.
            ("", "")
        } else if is_last {
            ("└─ ", "   ")
        } else {
            ("├─ ", "│  ")
        };
        let line_prefix = format!("{parent_prefix}{branch}");
        out.push(RenderRow {
            size_in_bytes: entry.size_in_bytes,
            level_total,
            is_dir: entry.is_dir,
            line_prefix,
            name: entry.name.clone(),
            depth,
        });
        if entry.is_dir && depth + 1 < levels {
            let child_prefix = format!("{parent_prefix}{child_part}");
            if let Ok(sub) = analyze(&entry.path, opts) {
                collect_rows(&sub, top, levels, opts, &child_prefix, depth + 1, out);
            }
        }
    }
}

/// Print a warning footer summarizing inaccessible paths, if any.
///
/// Warnings go to stderr so they don't pollute the structured stdout
/// report — a user piping dush into another tool still gets clean
/// data while the warnings remain visible interactively. If there are many
/// errors we cap the per-path listing and append a counter; users
/// debugging permissions issues are typically interested in the *kinds* of
/// failures more than the exhaustive list, and an unbounded dump can
/// dwarf the report itself when scanning, e.g., a foreign user's home
/// directory.
const MAX_LISTED_ERRORS: usize = 10;

fn print_warnings(analysis: &Analysis) {
    let errors = analysis.errors();
    if errors.is_empty() {
        return;
    }

    eprintln!();
    // ⚠ (U+26A0) WARNING SIGN — present in standard emoji/symbol blocks
    // and rendered monochrome on most terminals. The sizes reported above
    // are still correct for the readable portion of the tree, but
    // unreadable subtrees contribute zero, so the user should know.
    eprintln!(
        "⚠  {} path(s) could not be fully read; reported sizes may be undercounts:",
        errors.len()
    );
    for err in errors.iter().take(MAX_LISTED_ERRORS) {
        eprintln!("   • {}: {}", err.reason(), err.path().display());
    }
    if errors.len() > MAX_LISTED_ERRORS {
        eprintln!("   … and {} more", errors.len() - MAX_LISTED_ERRORS);
    }
}

// ==============================================================================
// Plain text output
// ==============================================================================

/// Write a no-frills three-column listing to stdout: raw byte count,
/// human-readable size, then the entry name with a trailing slash for
/// directories.
///
/// Intentionally minimal — no header, no bar, no percentage, no marker.
/// The use case is "I want to paste this into a doc / diff two runs / pipe
/// to `awk`": anything beyond the bare data gets in the way of those.
///
/// Why two size columns: the raw byte count makes `awk '$1 > 1e9'` and
/// numeric `sort -n` work without parsing units, while the human-readable
/// column is what a reader actually skims. We render the human-readable
/// size *without* the conventional space between number and unit
/// (`974.79MiB` rather than `974.79 MiB`) so each row stays a clean
/// 3-field record under awk's default whitespace splitting:
/// `$1 = bytes, $2 = human, $3 = name`.
fn render_text(analysis: &Analysis, top: Option<usize>) {
    let visible: &[Entry] = match top {
        Some(n) => &analysis.entries()[..analysis.entries().len().min(n)],
        None => analysis.entries(),
    };

    let size_opts = BINARY.space_after_value(false);

    // Pre-format both size columns so we can right-align them. Right-
    // alignment is the convention for numbers and lets the eye scan
    // magnitudes without having to read every digit.
    let bytes_strs: Vec<String> = visible
        .iter()
        .map(|e| e.size_in_bytes.to_string())
        .collect();
    let human_strs: Vec<String> = visible
        .iter()
        .map(|e| format_size(e.size_in_bytes, size_opts))
        .collect();
    let bytes_width = bytes_strs.iter().map(|s| s.len()).max().unwrap_or(0);
    let human_width = human_strs.iter().map(|s| s.len()).max().unwrap_or(0);

    for ((entry, bytes_str), human_str) in
        visible.iter().zip(bytes_strs.iter()).zip(human_strs.iter())
    {
        let suffix = if entry.is_dir { "/" } else { "" };
        println!(
            "{bytes_str:>bw$}  {human_str:>hw$}  {name}{suffix}",
            bw = bytes_width,
            hw = human_width,
            name = entry.name,
        );
    }
}

// ==============================================================================
// Machine-readable output formats
// ==============================================================================

/// Write a CSV/TSV table to stdout.
///
/// We share a single function for CSV and TSV since they only differ in
/// their delimiter — and the `csv` crate already handles the awkward
/// edge-cases (embedded commas/tabs/newlines/quotes in filenames) so we
/// don't have to. The schema is one header row plus one row per entry,
/// chosen to be useful both for spreadsheet imports and for ad-hoc
/// `awk`/`cut` pipelines. Errors are intentionally *not* included in the
/// tabular formats: they have a different shape (no size, no rank) and
/// mixing them in would force every consumer to filter. Use `--format
/// json` if you need errors alongside entries.
fn render_delimited(analysis: &Analysis, delimiter: u8) -> Result<()> {
    let stdout = std::io::stdout().lock();
    let mut writer = csv::WriterBuilder::new()
        .delimiter(delimiter)
        .from_writer(stdout);

    // Header row. `size_human` is the binary-suffix string we'd render in
    // text mode, included because it is much easier to skim than raw bytes
    // when the data ends up in a spreadsheet.
    writer
        .write_record([
            "rank",
            "name",
            "path",
            "size_in_bytes",
            "size_human",
            "percentage",
            "is_dir",
        ])
        .context("while writing tabular header")?;

    let total = analysis.total_in_bytes();
    for (i, entry) in analysis.entries().iter().enumerate() {
        let rank = (i + 1).to_string();
        let size_bytes = entry.size_in_bytes.to_string();
        let size_human = format_size(entry.size_in_bytes, BINARY);
        // Three decimal places matches what a user would see if they
        // formatted the number from `size_in_bytes` themselves; avoids
        // gratuitous float artifacts in spreadsheets.
        let pct = format!("{:.3}", percentage(entry.size_in_bytes, total));
        let is_dir = if entry.is_dir { "true" } else { "false" };
        writer
            .write_record([
                rank.as_str(),
                &entry.name,
                &entry.path.display().to_string(),
                size_bytes.as_str(),
                size_human.as_str(),
                pct.as_str(),
                is_dir,
            ])
            .with_context(|| format!("while writing row for {}", entry.path.display()))?;
    }

    writer.flush().context("while flushing tabular output")?;
    Ok(())
}

/// Write the analysis as pretty-printed JSON to stdout.
///
/// We serialize the `Analysis` struct directly — its serde derive produces
/// `{root, total_in_bytes, entries: [...], errors: [...]}` which is the
/// natural shape for downstream consumers. Pretty-printing is on by default
/// because the output is short enough that compactness is not a concern,
/// and a human glancing at the result is the more common case than a tight
/// machine-to-machine pipeline.
fn render_json(analysis: &Analysis) -> Result<()> {
    let stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(stdout, analysis).context("while writing JSON output")?;
    println!();
    Ok(())
}

/// Render a unicode block bar of `width` interior characters representing
/// `value`'s share of `total`.
///
/// The bar is framed by half-block "rule" glyphs — `▏` on the left and `▕`
/// on the right — which produce thin vertical lines that visually anchor
/// the bar without the chunky look of square brackets. Inside, we pair
/// full block glyphs (`█`) with the seven eighth-block glyphs for the
/// fractional cell, giving roughly 8× the visual resolution of a pure
/// full/empty bar at the same character width. Empty cells are plain
/// spaces, which we settled on after the light-shade glyph (`░`) caused
/// flicker on some terminal/font combinations as the column rebalanced.
fn render_bar(value: u64, total: u64, width: usize) -> String {
    // Reserve space for the framing rules up-front; the rest of the
    // function only manages the `width` interior cells.
    let mut s = String::with_capacity(width * 3 + 6);
    s.push('▏');

    if total == 0 || width == 0 {
        for _ in 0..width {
            s.push(' ');
        }
        s.push('▕');
        return s;
    }

    let eighths = (value as u128 * (width as u128) * 8 / total as u128) as usize;
    let full_cells = eighths / 8;
    let remainder = eighths % 8;

    // Eighth-block glyphs indexed 1..=7 (1/8 through 7/8 fill). Index 0 is
    // handled by the caller branching on `remainder`.
    const PARTIAL: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];

    for _ in 0..full_cells.min(width) {
        s.push('█');
    }
    if full_cells < width {
        if remainder > 0 {
            s.push(PARTIAL[remainder - 1]);
            for _ in (full_cells + 1)..width {
                s.push(' ');
            }
        } else {
            for _ in full_cells..width {
                s.push(' ');
            }
        }
    }
    s.push('▕');
    s
}

/// Compute a percentage in [0.0, 100.0]. Returns 0.0 if the total is zero,
/// rather than producing a NaN that would then need to be formatted.
fn percentage(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (value as f64 / total as f64) * 100.0
    }
}

// ==============================================================================
// Tests for rendering helpers
// ==============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_full_when_value_equals_total() {
        let bar = render_bar(100, 100, 10);
        assert_eq!(bar, "▏██████████▕");
    }

    #[test]
    fn bar_empty_when_value_is_zero() {
        let bar = render_bar(0, 100, 5);
        assert_eq!(bar, "▏     ▕");
    }

    #[test]
    fn bar_half_when_value_is_half() {
        let bar = render_bar(50, 100, 4);
        // 50% of 4 cells = 2 full cells, then padding between rules.
        assert_eq!(bar, "▏██  ▕");
    }

    #[test]
    fn bar_uses_eighth_block_for_fractional_cell() {
        // 5/16 of width 4 = 1.25 cells = 1 full + 2/8 partial → '▎'.
        let bar = render_bar(5, 16, 4);
        assert_eq!(bar, "▏█▎  ▕");
    }

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_for_display("foo", 10), "foo");
    }

    #[test]
    fn truncate_long_string_with_ellipsis() {
        // Result must be exactly `max` chars: 4 prefix + 1 ellipsis = 5.
        let out = truncate_for_display("abcdefghij", 5);
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('…'));
        assert!(out.starts_with("abcd"));
    }

    #[test]
    fn percentage_zero_total_is_zero() {
        assert_eq!(percentage(10, 0), 0.0);
    }

    #[test]
    fn percentage_basic() {
        assert!((percentage(25, 100) - 25.0).abs() < 1e-9);
    }
}
