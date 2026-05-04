//! # dush
//!
//! A library for analyzing disk space usage of a directory by computing
//! the total recursive size of each of its immediate children.
//!
//! The heavy lifting — parallel directory traversal and (where supported)
//! deduplication of files referenced by multiple hardlinks — is done by the
//! [`diskus`] crate. `dush` composes that primitive into a per-child
//! breakdown suitable for rendering as a "what is taking up space here?"
//! report.
//!
//! ```no_run
//! use dush::{analyze, AnalyzeOptions};
//!
//! let analysis = analyze(".", &AnalyzeOptions::default()).unwrap();
//! for entry in analysis.entries() {
//!     println!("{:>10} bytes  {}", entry.size_in_bytes, entry.name);
//! }
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, Result};
use diskus::DiskUsage;

// ==============================================================================
// Public types
// ==============================================================================

/// Options controlling how a directory is analyzed.
///
/// Mirrors the subset of `diskus` knobs that are user-facing in our CLI. We
/// intentionally do not expose worker-thread tuning here — the `diskus`
/// default (3× CPU cores, capped at 64) is well-chosen for a tool that is
/// invoked interactively.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnalyzeOptions {
    /// When `true`, count "apparent size" (bytes a file claims) rather than
    /// "disk usage" (blocks actually allocated). Matches `du -b` semantics.
    pub apparent_size: bool,
}

/// A single inaccessible path encountered during traversal.
///
/// Mirrors `diskus::Error` but lives in our public API so that callers
/// don't need to depend on the `diskus` crate directly to render warnings.
///
/// The serde representation is tagged: `{"kind": "no_metadata", "path":
/// "..."}`. We use `snake_case` for the variant tags so the JSON output
/// looks like other unix-y tools.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
pub enum AnalysisError {
    /// A path could not be `stat`ed — typically because it disappeared
    /// between `read_dir` and `symlink_metadata`, or the caller lacks
    /// permission to read its parent directory's metadata.
    NoMetadata(PathBuf),
    /// A directory could not be enumerated — typically a permissions
    /// problem or a filesystem-level read error.
    CouldNotReadDir(PathBuf),
}

impl AnalysisError {
    /// The path that could not be accessed.
    pub fn path(&self) -> &Path {
        match self {
            AnalysisError::NoMetadata(p) | AnalysisError::CouldNotReadDir(p) => p,
        }
    }

    /// A short human-readable reason, suitable for prefixing a warning line.
    pub fn reason(&self) -> &'static str {
        match self {
            AnalysisError::NoMetadata(_) => "could not stat",
            AnalysisError::CouldNotReadDir(_) => "could not read directory",
        }
    }
}

impl From<&diskus::Error> for AnalysisError {
    fn from(e: &diskus::Error) -> Self {
        match e {
            diskus::Error::NoMetadataForPath(p) => AnalysisError::NoMetadata(p.clone()),
            diskus::Error::CouldNotReadDir(p) => AnalysisError::CouldNotReadDir(p.clone()),
        }
    }
}

/// A single child of the analyzed root directory, together with the total
/// number of bytes attributed to its subtree.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Entry {
    /// The full path of this entry.
    pub path: PathBuf,
    /// The file or directory name (last path component).
    pub name: String,
    /// Total size of this entry's subtree, in bytes.
    pub size_in_bytes: u64,
    /// Whether this entry is a directory.
    pub is_dir: bool,
}

// `serde` is a public-API dependency: deriving `Serialize` on the result
// types lets the CLI emit machine-readable formats (JSON, CSV, TSV) without
// hand-rolling escaping. We do not derive `Deserialize` because the
// `Analysis` types are produced by walking a live filesystem — there is no
// reasonable round-trip from a serialized form back into a real analysis.

/// Progress events emitted by [`analyze_with_progress`] as each immediate
/// child of the root is processed.
///
/// Borrowing the name avoids allocating a fresh `String` per event for what
/// is almost always a transient `eprint!`-and-discard at the call site.
#[derive(Debug)]
pub enum Progress<'a> {
    /// A child is about to be sized. `index` is 1-based; `total` is the
    /// total number of immediate children discovered.
    Started {
        index: usize,
        total: usize,
        name: &'a str,
    },
    /// A child has been fully sized.
    Finished {
        index: usize,
        total: usize,
        name: &'a str,
        size_in_bytes: u64,
    },
    /// All children have been processed; the reporter should clear any UI.
    Done,
}

/// The result of analyzing a directory.
///
/// `entries` are pre-sorted in descending order of size — that is the order
/// the user almost always wants to see, and pre-sorting keeps the bin's
/// rendering code trivial.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Analysis {
    root: PathBuf,
    total_in_bytes: u64,
    entries: Vec<Entry>,
    errors: Vec<AnalysisError>,
}

impl Analysis {
    /// The directory that was analyzed.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The sum of sizes of all immediate children.
    ///
    /// This is *not* the same number `diskus` would report for the root
    /// itself: it excludes the bytes consumed by the root directory inode,
    /// since we don't count the root in `entries`. For interactive use the
    /// difference is negligible, and using the children-sum keeps the
    /// percentage column adding up to 100% exactly.
    pub fn total_in_bytes(&self) -> u64 {
        self.total_in_bytes
    }

    /// The per-child entries, sorted by size descending.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Paths that could not be accessed during traversal. These are
    /// non-fatal: the entries above still have correct sizes for the
    /// portion of the tree that *was* readable, and any unreadable
    /// subtrees simply contributed zero bytes to their parent's total.
    pub fn errors(&self) -> &[AnalysisError] {
        &self.errors
    }
}

// ==============================================================================
// Analysis core
// ==============================================================================

/// Analyze the given directory by computing the recursive size of each of
/// its immediate children, then sorting them descending.
///
/// This is a thin wrapper around [`analyze_with_progress`] that discards
/// progress events. Use it when you don't need per-child status updates.
pub fn analyze<P: AsRef<Path>>(root: P, opts: &AnalyzeOptions) -> Result<Analysis> {
    analyze_with_progress(root, opts, |_| {})
}

/// Analyze the given directory, invoking `on_progress` as each immediate
/// child is enumerated and sized.
///
/// The traversal model is identical to [`analyze`]: each child is walked as
/// its own `diskus::DiskUsage` job rather than walking the entire root in
/// one pass. The latter would only give us a single total, while we want a
/// per-child breakdown — so per-child walks are the natural fit. For
/// typical roots (tens to low-hundreds of children) the bookkeeping
/// overhead is dominated by the actual filesystem traversal.
///
/// Progress events are issued synchronously from the calling thread before
/// and after each child is sized, plus a final [`Progress::Done`] once all
/// children are processed.
pub fn analyze_with_progress<P, F>(
    root: P,
    opts: &AnalyzeOptions,
    mut on_progress: F,
) -> Result<Analysis>
where
    P: AsRef<Path>,
    F: FnMut(Progress<'_>),
{
    let root = root.as_ref();
    let canonical = fs::canonicalize(root)
        .with_context(|| format!("while resolving root path {}", root.display()))?;

    // Collect immediate children. We deliberately do not recurse here — that
    // is `diskus`'s job, on a per-child basis below.
    let mut children: Vec<PathBuf> = Vec::new();
    let read = fs::read_dir(&canonical)
        .with_context(|| format!("while reading directory {}", canonical.display()))?;
    for child in read {
        let child = child.with_context(|| format!("while iterating {}", canonical.display()))?;
        children.push(child.path());
    }

    // Size each child. We size them sequentially because `diskus` itself is
    // already heavily parallel internally — adding an outer rayon layer
    // would mostly just contend on the same CPU cores.
    let total = children.len();
    let mut entries: Vec<Entry> = Vec::with_capacity(total);
    let mut errors: Vec<AnalysisError> = Vec::new();
    for (i, path) in children.into_iter().enumerate() {
        let index = i + 1;

        // We treat a `symlink_metadata` failure on an immediate child as a
        // recoverable per-entry error rather than a hard `?`-style abort:
        // a single broken/permission-denied entry shouldn't sink the
        // entire report, which is the whole reason a user would run a disk
        // analyzer in the first place.
        let metadata = match path.symlink_metadata() {
            Ok(m) => m,
            Err(_) => {
                errors.push(AnalysisError::NoMetadata(path.clone()));
                continue;
            }
        };
        let is_dir = metadata.is_dir();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        on_progress(Progress::Started {
            index,
            total,
            name: &name,
        });

        let (size_in_bytes, mut child_errors) = subtree_size(&path, opts);
        errors.append(&mut child_errors);

        on_progress(Progress::Finished {
            index,
            total,
            name: &name,
            size_in_bytes,
        });

        entries.push(Entry {
            path,
            name,
            size_in_bytes,
            is_dir,
        });
    }

    on_progress(Progress::Done);

    // Largest first — that is the question the tool exists to answer.
    entries.sort_by_key(|e| std::cmp::Reverse(e.size_in_bytes));

    let total_in_bytes = entries.iter().map(|e| e.size_in_bytes).sum();

    Ok(Analysis {
        root: canonical,
        total_in_bytes,
        entries,
        errors,
    })
}

/// Compute the recursive size of a single path by delegating to `diskus`,
/// returning both the size in bytes and any per-path access errors that
/// `diskus` accumulated while walking the subtree.
///
/// For directories this runs a full parallel walk; for plain files
/// `diskus` still does the work so that the apparent-size vs. disk-usage
/// distinction is computed exactly the same way as for directory contents.
///
/// Exposed publicly so a UI can size each child on its own background
/// thread — the eager [`analyze`] path sizes everything serially before
/// returning, which is fine for a CLI but makes the GUI feel sluggish at
/// startup when the root has many large children.
pub fn subtree_size<P: AsRef<Path>>(path: P, opts: &AnalyzeOptions) -> (u64, Vec<AnalysisError>) {
    // We deliberately leave `Directories` at its default `Auto`, which makes
    // `diskus` mirror `du`'s behavior: directory inodes contribute to the
    // disk-usage count but not to the apparent-size count. That keeps the
    // numbers reported by dush comparable to what `du -sh` /
    // `du -sb --apparent-size` users already expect.
    let mut usage = DiskUsage::new([path.as_ref()]);
    if opts.apparent_size {
        usage = usage.apparent_size();
    }
    let result = usage.count();
    let errors: Vec<AnalysisError> = result.errors().iter().map(AnalysisError::from).collect();
    let size = result.ignore_errors().size_in_bytes();
    (size, errors)
}

/// Cheaply enumerate the immediate children of a directory without
/// recursing.
///
/// Each returned [`Entry`] has its `size_in_bytes` set from the child's
/// own `metadata().len()` for files, and `0` for directories — there is
/// no recursive walk. Callers who need full subtree sizes should follow
/// up with [`subtree_size`] per directory entry, ideally on a worker
/// thread so the call site stays responsive.
///
/// This is the cheap initial step the GUI uses to render a tree
/// instantly: the user sees the layout immediately, while the (slow)
/// per-directory totals stream in afterwards.
pub fn enumerate_children<P: AsRef<Path>>(root: P) -> Result<Vec<Entry>> {
    let root = root.as_ref();
    let canonical = fs::canonicalize(root)
        .with_context(|| format!("while resolving root path {}", root.display()))?;
    let read = fs::read_dir(&canonical)
        .with_context(|| format!("while reading directory {}", canonical.display()))?;

    let mut out = Vec::new();
    for child in read {
        let child = child.with_context(|| format!("while iterating {}", canonical.display()))?;
        let path = child.path();
        // Skip entries whose metadata is unreadable; the GUI shows them
        // later via [`subtree_size`]'s error channel if they end up
        // expanded. Aborting here would deny the user the view of the
        // *other* children just because of one denied sibling.
        let metadata = match path.symlink_metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = metadata.is_dir();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        // For directories we report 0 bytes — the eventual recursive
        // total replaces this when [`subtree_size`] finishes. For files
        // we use the metadata length, which is the apparent size; for
        // disk-usage display this is a small undercount that stays put
        // because regular files have no children to summarize over.
        let size_in_bytes = if is_dir { 0 } else { metadata.len() };
        out.push(Entry {
            path,
            name,
            size_in_bytes,
            is_dir,
        });
    }

    Ok(out)
}

// ==============================================================================
// Tests
// ==============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    /// Write `bytes` of payload to a fresh file at `path`.
    fn write_file(path: &Path, bytes: usize) {
        let mut f = File::create(path).expect("create test file");
        f.write_all(&vec![b'x'; bytes]).expect("write test file");
    }

    #[test]
    fn analyzes_flat_directory_sorted_desc() {
        let tmp = TempDir::new().expect("tempdir");
        write_file(&tmp.path().join("small.txt"), 10);
        write_file(&tmp.path().join("medium.txt"), 1_000);
        write_file(&tmp.path().join("large.txt"), 50_000);

        let analysis = analyze(
            tmp.path(),
            &AnalyzeOptions {
                apparent_size: true,
            },
        )
        .expect("analyze");

        let names: Vec<&str> = analysis.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["large.txt", "medium.txt", "small.txt"]);
        assert_eq!(analysis.entries()[0].size_in_bytes, 50_000);
        assert_eq!(analysis.entries()[1].size_in_bytes, 1_000);
        assert_eq!(analysis.entries()[2].size_in_bytes, 10);
        assert_eq!(analysis.total_in_bytes(), 51_010);
    }

    #[test]
    fn nested_directory_size_aggregates() {
        let tmp = TempDir::new().expect("tempdir");
        let nested = tmp.path().join("nested");
        std::fs::create_dir(&nested).expect("mkdir nested");
        write_file(&nested.join("a"), 4_096);
        write_file(&nested.join("b"), 4_096);
        write_file(&tmp.path().join("loose.txt"), 100);

        let analysis = analyze(
            tmp.path(),
            &AnalyzeOptions {
                apparent_size: true,
            },
        )
        .expect("analyze");

        // Find the nested entry; its reported size must include both files.
        let nested_entry = analysis
            .entries()
            .iter()
            .find(|e| e.name == "nested")
            .expect("nested entry present");
        assert!(nested_entry.is_dir);
        assert_eq!(nested_entry.size_in_bytes, 8_192);

        let loose_entry = analysis
            .entries()
            .iter()
            .find(|e| e.name == "loose.txt")
            .expect("loose entry present");
        assert!(!loose_entry.is_dir);
        assert_eq!(loose_entry.size_in_bytes, 100);
    }

    #[test]
    fn empty_directory_yields_no_entries() {
        let tmp = TempDir::new().expect("tempdir");
        let analysis = analyze(tmp.path(), &AnalyzeOptions::default()).expect("analyze");
        assert!(analysis.entries().is_empty());
        assert_eq!(analysis.total_in_bytes(), 0);
    }

    #[test]
    fn progress_callback_emits_started_finished_done() {
        let tmp = TempDir::new().expect("tempdir");
        write_file(&tmp.path().join("a"), 100);
        write_file(&tmp.path().join("b"), 200);

        // We capture every event as a tag string so that ordering and
        // counts can be asserted compactly. The exact order of children
        // depends on `read_dir`, which is unspecified, so we only assert
        // structural properties (paired Start/Finish per child, single
        // terminal Done).
        let mut events: Vec<String> = Vec::new();
        analyze_with_progress(tmp.path(), &AnalyzeOptions::default(), |p| match p {
            Progress::Started { index, total, name } => {
                events.push(format!("S/{index}/{total}/{name}"));
            }
            Progress::Finished {
                index,
                total,
                name,
                size_in_bytes,
            } => {
                events.push(format!("F/{index}/{total}/{name}/{size_in_bytes}"));
            }
            Progress::Done => events.push("D".into()),
        })
        .expect("analyze");

        // Two children → 2 Started + 2 Finished + 1 Done = 5 events.
        assert_eq!(events.len(), 5, "events: {events:?}");
        assert_eq!(events.last().map(String::as_str), Some("D"));
        let starts = events.iter().filter(|e| e.starts_with("S/")).count();
        let finishes = events.iter().filter(|e| e.starts_with("F/")).count();
        assert_eq!(starts, 2);
        assert_eq!(finishes, 2);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_subdirectory_surfaces_as_error() {
        use std::os::unix::fs::PermissionsExt;

        // Running as root bypasses Unix permission checks entirely, so a
        // chmod-0 directory would still be readable and the assertion
        // below would spuriously fail. Detect that case and skip — the
        // type-system parts of the change are still exercised by the
        // happy-path tests above.
        // SAFETY: `geteuid` from libc is a thin syscall wrapper that
        // takes no arguments and reads no thread-local state.
        let euid = unsafe { libc::geteuid() };
        if euid == 0 {
            eprintln!("skipping: running as root cannot exercise permission-denied path");
            return;
        }

        let tmp = TempDir::new().expect("tempdir");
        let restricted = tmp.path().join("restricted");
        std::fs::create_dir(&restricted).expect("mkdir");
        std::fs::write(restricted.join("hidden.txt"), b"x").expect("write child");

        // Strip read+execute permissions on the directory so its contents
        // are unenumerable. We must restore permissions before the
        // tempdir's `Drop` runs, or the cleanup itself will fail.
        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 0");

        let analysis = analyze(tmp.path(), &AnalyzeOptions::default());

        // Restore permissions unconditionally so cleanup succeeds.
        let _ = std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o755));

        let analysis = analysis.expect("analyze should not fatally error");
        assert!(
            !analysis.errors().is_empty(),
            "expected at least one access error, got none"
        );
        // The reported error should reference the restricted path.
        let mentions = analysis
            .errors()
            .iter()
            .any(|e| e.path().starts_with(&restricted));
        assert!(
            mentions,
            "errors should reference the restricted path: {:?}",
            analysis.errors()
        );
    }

    #[test]
    fn missing_root_returns_error() {
        let result = analyze(
            "/nonexistent/path/should/never/exist",
            &AnalyzeOptions::default(),
        );
        assert!(result.is_err());
    }
}
