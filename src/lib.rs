//! # dush
//!
//! A library for analyzing disk space usage of a directory by computing
//! the total recursive size of each of its immediate children.
//!
//! One pool of worker threads walks all children at the same time. The
//! numbers follow `du`: within a child, a file with several hardlinks
//! counts once (where the platform exposes inode numbers), and directory
//! inodes count toward disk usage but not toward apparent size.
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, mpsc};

use color_eyre::eyre::{Context, Result};

pub mod tree;

// ==============================================================================
// Public types
// ==============================================================================

/// Options controlling how a directory is analyzed.
///
/// Worker-thread tuning is intentionally not exposed. The walker's
/// default (3× CPU cores, see `pool`) suits a tool that is invoked
/// interactively.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnalyzeOptions {
    /// When `true`, count "apparent size" (bytes a file claims) rather than
    /// "disk usage" (blocks actually allocated). Matches `du -b` semantics.
    pub apparent_size: bool,
}

/// A single inaccessible path encountered during traversal.
///
/// The serde representation is tagged: `{"kind": "no_metadata", "path":
/// "..."}`. We use `snake_case` for the variant tags so the JSON output
/// looks like other unix-y tools.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
pub enum AnalysisError {
    /// A path could not be `stat`ed — typically because it disappeared
    /// between `read_dir` and the `stat`, or the caller lacks permission
    /// to read its parent directory's metadata.
    NoMetadata(#[serde(serialize_with = "serialize_path_lossy")] PathBuf),
    /// A directory could not be enumerated — typically a permissions
    /// problem or a filesystem-level read error.
    CouldNotReadDir(#[serde(serialize_with = "serialize_path_lossy")] PathBuf),
}

/// Serialize a path as a UTF-8 string, replacing invalid sequences with
/// U+FFFD.
///
/// serde's built-in `Path` impl returns an error for paths that aren't
/// valid UTF-8, which would let a single oddly-named file abort an entire
/// JSON export. Unix filenames are arbitrary bytes, so such paths do show
/// up in the wild. Lossy conversion matches how [`Entry::name`] is already
/// derived, so `name` and `path` stay consistent with each other.
fn serialize_path_lossy<S: serde::Serializer>(
    path: &Path,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&path.to_string_lossy())
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

/// A single child of the analyzed root directory, together with the total
/// number of bytes attributed to its subtree.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Entry {
    /// The full path of this entry. Serialized lossily as UTF-8; see
    /// `serialize_path_lossy`.
    #[serde(serialize_with = "serialize_path_lossy")]
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

/// Progress events emitted by [`analyze_with_progress`].
///
/// Children are sized concurrently, so `Finished` events arrive in the
/// order the walks complete, not in the order `read_dir` listed them.
///
/// Borrowing the name avoids allocating a fresh `String` per event for what
/// is almost always a transient `eprint!`-and-discard at the call site.
#[derive(Debug)]
pub enum Progress<'a> {
    /// A child has been fully sized. `done` counts the children finished
    /// so far, this one included; `total` is the number of children being
    /// sized.
    Finished {
        done: usize,
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
    #[serde(serialize_with = "serialize_path_lossy")]
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
    /// This is *not* the same number `du -s` would report for the root
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

    /// Paths that could not be accessed during traversal, sorted by path.
    /// These are non-fatal: the entries above still have correct sizes for
    /// the portion of the tree that *was* readable, and any unreadable
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
/// child finishes.
///
/// The calling thread lists the root, then the shared worker pool walks
/// every child directory at the same time. `on_progress` runs on the
/// calling thread: once per child as its walk completes, then a final
/// [`Progress::Done`].
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

    // List the root before any walk starts. A root we can't read is the
    // one failure that leaves nothing to report, so it is a hard error,
    // unlike everything the walks run into below it.
    let mut children: Vec<fs::DirEntry> = Vec::new();
    let read = fs::read_dir(&canonical)
        .with_context(|| format!("while reading directory {}", canonical.display()))?;
    for child in read {
        children.push(child.with_context(|| format!("while iterating {}", canonical.display()))?);
    }

    let walk = Walk::new(opts);
    let mut entries: Vec<Entry> = Vec::with_capacity(children.len());
    let (report, sizes) = mpsc::channel();

    // `in_place_scope` runs this closure on the calling thread, where
    // `on_progress` has to be called, and sends only the spawned walks to
    // the pool.
    pool().in_place_scope(|scope| {
        for child in children {
            let path = child.path();
            // We treat a `stat` failure on an immediate child as a
            // recoverable per-entry error rather than a hard `?`-style
            // abort: a single broken/permission-denied entry shouldn't
            // sink the entire report, which is the whole reason a user
            // would run a disk analyzer in the first place.
            let Ok(metadata) = child.metadata() else {
                walk.fail(AnalysisError::NoMetadata(path));
                continue;
            };
            let is_dir = metadata.is_dir();
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());

            // Each child gets its own tally, so a file hardlinked into
            // two children counts in both, the same as running `du -s`
            // on each of them.
            let tally = Arc::new(Tally {
                index: entries.len(),
                bytes: AtomicU64::new(entry_size(&metadata, opts)),
                hardlinks: Mutex::default(),
                report: report.clone(),
            });
            if is_dir {
                let (walk, path) = (&walk, path.clone());
                scope.spawn(move |scope| walk_dir(scope, walk, path, tally));
            }
            // A file's tally has no other holder, so it drops at the end
            // of this iteration and reports the file's own size.
            entries.push(Entry {
                path,
                name,
                size_in_bytes: 0,
                is_dir,
            });
        }

        // Only tallies hold senders from here on, so `sizes` ends when
        // the last walk does.
        drop(report);
        let total = entries.len();
        for (finished, (index, size_in_bytes)) in sizes.iter().enumerate() {
            entries[index].size_in_bytes = size_in_bytes;
            on_progress(Progress::Finished {
                done: finished + 1,
                total,
                name: &entries[index].name,
                size_in_bytes,
            });
        }
    });

    on_progress(Progress::Done);

    // Largest first — that is the question the tool exists to answer.
    entries.sort_by_key(|e| std::cmp::Reverse(e.size_in_bytes));

    let total_in_bytes = entries.iter().map(|e| e.size_in_bytes).sum();

    Ok(Analysis {
        root: canonical,
        total_in_bytes,
        entries,
        errors: walk.into_errors(),
    })
}

/// Compute the recursive size of a single path, returning both the size
/// in bytes and any per-path access errors hit while walking the subtree.
///
/// Sizes follow `du`: directory inodes contribute to the disk-usage count
/// but not to the apparent-size count, and a file with several hardlinks
/// inside the subtree counts once. That keeps the numbers comparable to
/// what `du -sh` / `du -sb --apparent-size` users already expect.
///
/// Exposed publicly so a UI can size each child as its own unit of
/// background work and show every result as it lands. Calls from several
/// threads share one worker pool, so they don't oversubscribe the CPU.
pub fn subtree_size<P: AsRef<Path>>(path: P, opts: &AnalyzeOptions) -> (u64, Vec<AnalysisError>) {
    let path = path.as_ref();
    let Ok(metadata) = path.symlink_metadata() else {
        return (0, vec![AnalysisError::NoMetadata(path.to_path_buf())]);
    };

    let walk = Walk::new(opts);
    let (report, sizes) = mpsc::channel();
    let tally = Arc::new(Tally {
        index: 0,
        bytes: AtomicU64::new(entry_size(&metadata, opts)),
        hardlinks: Mutex::default(),
        report,
    });
    if metadata.is_dir() {
        pool().scope(|scope| walk_dir(scope, &walk, path.to_path_buf(), tally));
    } else {
        drop(tally);
    }
    let (_, size) = sizes
        .recv()
        .expect("every holder of the tally is gone, so it has reported");
    (size, walk.into_errors())
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
    enumerate_children_with(
        root,
        &AnalyzeOptions {
            apparent_size: true,
        },
    )
}

/// Like [`enumerate_children`], but sizes files according to `opts`.
///
/// With `apparent_size` off, a file's size is the space its allocated
/// blocks occupy, the same number [`subtree_size`] would report for it.
/// This matters for sparse files: a VM image that claims 64 GiB but has
/// only 3 GiB of blocks allocated shows up as 3 GiB, which is what a
/// "where did my disk go?" view has to show.
pub fn enumerate_children_with<P: AsRef<Path>>(
    root: P,
    opts: &AnalyzeOptions,
) -> Result<Vec<Entry>> {
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
        let metadata = match child.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = metadata.is_dir();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        // For directories we report 0 bytes — the eventual recursive
        // total replaces this when [`subtree_size`] finishes. Files get
        // their final size right here, so callers never need to walk
        // them.
        let size_in_bytes = if is_dir {
            0
        } else {
            entry_size(&metadata, opts)
        };
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
// Walker
// ==============================================================================

/// The worker threads that every walk in the process shares.
///
/// Sharing matters because starting a pool spawns each of its threads,
/// which costs more than walking a small directory does. A root with
/// 100k children would spend nearly all of its time starting threads if
/// each child's walk brought its own.
fn pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        // Three threads per core. Over a cold cache a walk spends its
        // time blocked in `getdents` and `statx`, and the extra threads
        // keep more requests queued at the disk. Over a warm cache they
        // cost nothing we could measure. The 64-core cap bounds startup
        // on very wide machines. `diskus` arrived at the same numbers:
        // <https://github.com/sharkdp/diskus/issues/38#issuecomment-612772867>
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        rayon::ThreadPoolBuilder::new()
            .num_threads(3 * cores.min(64))
            .thread_name(|i| format!("dush-walk-{i}"))
            .build()
            .expect("the OS lets a process start a few hundred threads")
    })
}

/// Lock `mutex` whether or not it is poisoned.
///
/// The walker's locks guard `Vec`s that are only ever appended to, so a
/// panic on another thread can't have left one half-updated.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What all tasks of one walk share.
#[derive(Debug)]
struct Walk {
    opts: AnalyzeOptions,
    errors: Mutex<Vec<AnalysisError>>,
}

impl Walk {
    fn new(opts: &AnalyzeOptions) -> Self {
        Self {
            opts: *opts,
            errors: Mutex::default(),
        }
    }

    fn fail(&self, error: AnalysisError) {
        lock(&self.errors).push(error);
    }

    /// The recorded errors, sorted by path. Workers record them in
    /// whatever order they race to, which would otherwise make the
    /// warning list differ between two runs over the same tree.
    fn into_errors(self) -> Vec<AnalysisError> {
        let mut errors = self
            .errors
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        errors.sort_by(|a, b| a.path().cmp(b.path()));
        errors
    }
}

/// Device and inode number, which together identify a file on Unix.
type InodeId = (u64, u64);

/// The running size of one subtree.
///
/// Every task that walks a directory of the subtree holds a handle to its
/// tally. When the last of them drops its handle the subtree is complete,
/// and `Drop` sends the final size through `report`. Completion falls out
/// of ownership this way, with no task counter to keep in step with the
/// spawns.
#[derive(Debug)]
struct Tally {
    /// Which subtree this is. Sent back alongside the size.
    index: usize,
    /// Bytes of all entries that have a single name.
    bytes: AtomicU64,
    /// Size of each regular file that has several names. Such a file can
    /// turn up in the subtree more than once but must count once, and
    /// only the finished walk knows which sightings were repeats.
    hardlinks: Mutex<Vec<(InodeId, u64)>>,
    report: mpsc::Sender<(usize, u64)>,
}

impl Drop for Tally {
    fn drop(&mut self) {
        let hardlinks = self
            .hardlinks
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        hardlinks.sort_unstable_by_key(|&(id, _)| id);
        hardlinks.dedup_by_key(|&mut (id, _)| id);
        let size = *self.bytes.get_mut() + hardlinks.iter().map(|&(_, bytes)| bytes).sum::<u64>();
        // The receiver is only gone if the waiting thread unwound, and
        // then nobody is left to want the size.
        let _ = self.report.send((self.index, size));
    }
}

/// Add everything directly inside `dir` to `tally`, and spawn a task that
/// does the same for each subdirectory.
///
/// Sizes come from `DirEntry::metadata`, which on Unix is an `fstatat`
/// relative to the open directory. The kernel resolves one path component
/// per entry that way, where a `stat` of the full path would resolve
/// every ancestor again.
fn walk_dir<'scope>(
    scope: &rayon::Scope<'scope>,
    walk: &'scope Walk,
    dir: PathBuf,
    tally: Arc<Tally>,
) {
    let Ok(read) = fs::read_dir(&dir) else {
        walk.fail(AnalysisError::CouldNotReadDir(dir));
        return;
    };

    // Sum locally and touch the shared tally once per directory, so that
    // threads walking the same subtree don't trade its cache line on
    // every file.
    let mut bytes = 0;
    let mut hardlinks = Vec::new();
    // `flatten` drops entries the kernel fails to hand over in the middle
    // of a listing. There is no path to report for them.
    for entry in read.flatten() {
        let Ok(metadata) = entry.metadata() else {
            walk.fail(AnalysisError::NoMetadata(entry.path()));
            continue;
        };
        let size = entry_size(&metadata, &walk.opts);
        if metadata.is_dir() {
            bytes += size;
            let (path, tally) = (entry.path(), Arc::clone(&tally));
            scope.spawn(move |scope| walk_dir(scope, walk, path, tally));
        } else if let Some(id) = hardlink_id(&metadata) {
            hardlinks.push((id, size));
        } else {
            bytes += size;
        }
    }

    tally.bytes.fetch_add(bytes, Ordering::Relaxed);
    if !hardlinks.is_empty() {
        lock(&tally.hardlinks).append(&mut hardlinks);
    }
}

/// Bytes that the entry behind `metadata` itself adds to a total under
/// `opts`, not counting anything below it.
///
/// Disk usage is `st_blocks`, which counts 512-byte units regardless of
/// the filesystem's block size. Apparent size is `len()` for everything
/// but directories: a directory's `len()` describes its own listing, not
/// the bytes stored in it, so `du --apparent-size` leaves it out and so
/// do we.
fn entry_size(metadata: &fs::Metadata, opts: &AnalyzeOptions) -> u64 {
    #[cfg(unix)]
    if !opts.apparent_size {
        use std::os::unix::fs::MetadataExt;
        return metadata.blocks() * 512;
    }
    // Windows has no `st_blocks`, so the apparent size stands in for
    // disk usage there.
    #[cfg(not(unix))]
    let _ = opts;
    if metadata.is_dir() { 0 } else { metadata.len() }
}

/// The identity of a regular file that has more than one name. `None`
/// for everything else, which counts once per name.
fn hardlink_id(metadata: &fs::Metadata) -> Option<InodeId> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (metadata.is_file() && metadata.nlink() > 1).then(|| (metadata.dev(), metadata.ino()))
    }
    // TODO: Windows has hardlinks too, but `std` has no stable way to
    // read a file's index there, so every name counts.
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
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
    fn progress_callback_emits_finished_per_child_then_done() {
        let tmp = TempDir::new().expect("tempdir");
        write_file(&tmp.path().join("a"), 100);
        write_file(&tmp.path().join("b"), 200);

        // We capture every event as a tag string so that ordering and
        // counts can be asserted compactly. Children finish in whatever
        // order their walks complete, so we assert the names as a set.
        let mut events: Vec<String> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        analyze_with_progress(tmp.path(), &AnalyzeOptions::default(), |p| match p {
            Progress::Finished {
                done, total, name, ..
            } => {
                events.push(format!("F/{done}/{total}"));
                names.push(name.to_string());
            }
            Progress::Done => events.push("D".into()),
        })
        .expect("analyze");

        assert_eq!(events, ["F/1/2", "F/2/2", "D"]);
        names.sort();
        assert_eq!(names, ["a", "b"]);
    }

    /// Two names for one file inside a child are one file's worth of
    /// bytes. The same file linked into a second child counts there too,
    /// because each child is sized as if `du -s` ran on it alone.
    #[cfg(unix)]
    #[test]
    fn hardlinks_count_once_per_child() {
        let tmp = TempDir::new().expect("tempdir");
        let photos = tmp.path().join("photos");
        let backup = tmp.path().join("backup");
        std::fs::create_dir_all(photos.join("2024")).expect("mkdir photos");
        std::fs::create_dir(&backup).expect("mkdir backup");
        write_file(&photos.join("beach.jpg"), 5_000);
        std::fs::hard_link(photos.join("beach.jpg"), photos.join("2024/beach.jpg"))
            .expect("link within photos");
        std::fs::hard_link(photos.join("beach.jpg"), backup.join("beach.jpg"))
            .expect("link into backup");

        let apparent = AnalyzeOptions {
            apparent_size: true,
        };
        let analysis = analyze(tmp.path(), &apparent).expect("analyze");

        let size_of = |name: &str| {
            analysis
                .entries()
                .iter()
                .find(|e| e.name == name)
                .expect("entry present")
                .size_in_bytes
        };
        assert_eq!(size_of("photos"), 5_000);
        assert_eq!(size_of("backup"), 5_000);
        assert_eq!(subtree_size(tmp.path(), &apparent).0, 5_000);
    }

    /// A child's size covers its whole subtree, however deep, and agrees
    /// with `subtree_size` on the same path.
    #[test]
    fn deep_tree_aggregates_into_top_level_child() {
        let tmp = TempDir::new().expect("tempdir");
        let mut dir = tmp.path().join("projects");
        for level in 0..20 {
            dir.push(format!("level-{level}"));
            std::fs::create_dir_all(&dir).expect("mkdir level");
            write_file(&dir.join("notes.txt"), 100);
            write_file(&dir.join("data.bin"), 1_000);
        }

        for opts in [
            AnalyzeOptions::default(),
            AnalyzeOptions {
                apparent_size: true,
            },
        ] {
            let analysis = analyze(tmp.path(), &opts).expect("analyze");
            let projects = &analysis.entries()[0];
            assert_eq!(projects.name, "projects");
            assert_eq!(
                projects.size_in_bytes,
                subtree_size(&projects.path, &opts).0
            );
            if opts.apparent_size {
                assert_eq!(projects.size_in_bytes, 20 * 1_100);
            }
        }
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

    /// Regression: serde's stock `Path` serializer rejects non-UTF-8
    /// paths, which used to make `dush -f json` fail mid-document as soon
    /// as the analyzed directory contained such a filename.
    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_serialize_lossily() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let tmp = TempDir::new().expect("tempdir");
        let bad_name = OsStr::from_bytes(b"bad\xffname");
        write_file(&tmp.path().join(bad_name), 10);

        let analysis = analyze(tmp.path(), &AnalyzeOptions::default()).expect("analyze");
        let json = serde_json::to_value(&analysis).expect("serialize to JSON");

        let entry = &json["entries"][0];
        assert_eq!(entry["name"], "bad\u{FFFD}name");
        let path = entry["path"].as_str().expect("path is a string");
        assert!(path.ends_with("/bad\u{FFFD}name"), "path: {path}");

        // Errors carry paths too and must not trip over the same thing.
        let err = AnalysisError::CouldNotReadDir(tmp.path().join(bad_name));
        let err_json = serde_json::to_value(&err).expect("serialize error");
        assert_eq!(err_json["kind"], "could_not_read_dir");
        assert!(
            err_json["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("/bad\u{FFFD}name"))
        );
    }

    /// A sparse file has to show its allocated size in disk-usage mode,
    /// and that number must agree with what a walk reports for it.
    #[cfg(unix)]
    #[test]
    fn enumerate_children_with_sizes_sparse_files_by_blocks() {
        let tmp = TempDir::new().expect("tempdir");
        let sparse = tmp.path().join("sparse.img");
        File::create(&sparse)
            .expect("create sparse file")
            .set_len(16 * 1024 * 1024)
            .expect("extend sparse file");

        let disk_usage = AnalyzeOptions::default();
        let apparent = AnalyzeOptions {
            apparent_size: true,
        };

        let by_blocks = enumerate_children_with(tmp.path(), &disk_usage).expect("enumerate");
        let by_len = enumerate_children_with(tmp.path(), &apparent).expect("enumerate");

        assert_eq!(by_len[0].size_in_bytes, 16 * 1024 * 1024);
        assert_eq!(
            by_blocks[0].size_in_bytes,
            subtree_size(&sparse, &disk_usage).0
        );
        assert!(by_blocks[0].size_in_bytes < by_len[0].size_in_bytes);
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
