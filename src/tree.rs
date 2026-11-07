//! An incrementally explored size tree.
//!
//! [`Tree`] is the model behind the GUIs: a directory tree that starts as
//! just its root and grows as the user expands directories. It knows
//! nothing about threads or UI toolkits. The caller enumerates a
//! directory (for example with [`crate::enumerate_children_with`]), feeds
//! the entries to [`Tree::insert_children`], sizes the subdirectories it
//! gets back (with [`crate::subtree_size`]) and reports each result with
//! [`Tree::set_size`].
//!
//! Nodes live in an arena and are addressed by [`NodeId`], a plain index
//! that stays valid however the tree re-sorts. That lets a background task
//! hold on to the id of the node it is sizing without borrowing the tree.
//!
//! ```
//! use dush::tree::{Children, ROOT, Tree};
//! use dush::Entry;
//!
//! let mut tree = Tree::new("/home/maria".into());
//! tree.set_expanded(ROOT, true);
//! assert!(tree.request_load(ROOT), "first expansion needs a read_dir");
//!
//! let pending = tree.insert_children(
//!     ROOT,
//!     vec![Entry {
//!         path: "/home/maria/Videos".into(),
//!         name: "Videos".into(),
//!         size_in_bytes: 0,
//!         is_dir: true,
//!     }],
//! );
//! let (videos, _path) = &pending[0];
//! tree.set_size(*videos, 9_000, 0);
//! tree.settle();
//! assert_eq!(tree.node(ROOT).size, 9_000);
//! assert!(matches!(tree.node(ROOT).children, Children::Loaded(_)));
//! ```

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::Entry;

/// Index of a node in a [`Tree`].
pub type NodeId = usize;

/// The root is always the first node in the arena.
pub const ROOT: NodeId = 0;

/// Where a directory is in its enumerate cycle. Files stay `Unloaded`
/// forever; nothing ever looks at their children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Children {
    /// Never expanded, so nobody has called `read_dir` on it yet.
    Unloaded,
    /// The caller is enumerating it right now.
    Loading,
    /// Enumerated. Sorted by size, largest first, as of the last
    /// [`Tree::settle`].
    Loaded(Vec<NodeId>),
    /// `read_dir` failed. Holds the error text so a permission problem
    /// reads as the familiar `Permission denied (os error 13)`.
    Failed(String),
}

/// One file or directory in a [`Tree`].
#[derive(Debug, Clone)]
pub struct Node {
    /// Last path component, or the full path for the root.
    pub name: Arc<str>,
    pub path: PathBuf,
    pub is_dir: bool,
    /// 0 for the root, +1 per level below it.
    pub depth: usize,
    pub parent: Option<NodeId>,
    /// Bytes attributed to this subtree. For a directory whose walk has
    /// not finished this is 0, except for the root, which carries the
    /// running sum of its children (see [`Tree::settle`]).
    pub size: u64,
    /// Whether `size` is final.
    pub sized: bool,
    /// Paths inside this subtree that could not be read. Their bytes are
    /// missing from `size`, so a UI should say so.
    pub unreadable: usize,
    pub expanded: bool,
    pub children: Children,
}

/// See the [module docs](self).
#[derive(Debug, Clone)]
pub struct Tree {
    nodes: Vec<Node>,
    /// Parents whose children changed size or arrived since the last
    /// [`Tree::settle`], and so may be out of order.
    stale: HashSet<NodeId>,
}

impl Tree {
    /// A tree holding only `root`, collapsed and unloaded.
    pub fn new(root: PathBuf) -> Self {
        let name = root.display().to_string().into();
        Self {
            nodes: vec![Node {
                name,
                path: root,
                is_dir: true,
                depth: 0,
                parent: None,
                size: 0,
                sized: false,
                unreadable: 0,
                expanded: false,
                children: Children::Unloaded,
            }],
            stale: HashSet::new(),
        }
    }

    /// The node with the given id.
    ///
    /// # Panics
    ///
    /// If `id` did not come from this tree.
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn set_expanded(&mut self, id: NodeId, expanded: bool) {
        self.nodes[id].expanded = expanded;
    }

    /// Mark a directory as being enumerated. Returns `true` if it wasn't
    /// loaded or loading yet, meaning the caller should go and call
    /// `read_dir` on it now; `false` for files and directories that are
    /// already taken care of.
    pub fn request_load(&mut self, id: NodeId) -> bool {
        let node = &mut self.nodes[id];
        if !node.is_dir || node.children != Children::Unloaded {
            return false;
        }
        node.children = Children::Loading;
        true
    }

    /// Record that enumerating `id` failed with `message`.
    pub fn set_failed(&mut self, id: NodeId, message: String) {
        self.nodes[id].children = Children::Failed(message);
    }

    /// Insert freshly enumerated entries under `parent` and return the
    /// directories among them, which still need a recursive size.
    pub fn insert_children(
        &mut self,
        parent: NodeId,
        entries: Vec<Entry>,
    ) -> Vec<(NodeId, PathBuf)> {
        let depth = self.nodes[parent].depth + 1;
        let mut ids = Vec::with_capacity(entries.len());
        let mut pending_dirs = Vec::new();
        for entry in entries {
            let id = self.nodes.len();
            if entry.is_dir {
                pending_dirs.push((id, entry.path.clone()));
            }
            self.nodes.push(Node {
                name: entry.name.into(),
                path: entry.path,
                is_dir: entry.is_dir,
                depth,
                parent: Some(parent),
                size: entry.size_in_bytes,
                // `enumerate_children_with` already gives files their
                // final size; only directories need a walk.
                sized: !entry.is_dir,
                unreadable: 0,
                expanded: false,
                children: Children::Unloaded,
            });
            ids.push(id);
        }
        self.nodes[parent].children = Children::Loaded(ids);
        self.stale.insert(parent);
        pending_dirs
    }

    /// Record the finished walk of `id`.
    pub fn set_size(&mut self, id: NodeId, size: u64, unreadable: usize) {
        let node = &mut self.nodes[id];
        node.size = size;
        node.sized = true;
        node.unreadable = unreadable;
        if let Some(parent) = node.parent {
            self.stale.insert(parent);
        }
    }

    /// Re-sort every sibling group that changed since the last call, and
    /// refresh the root total if its children were among them.
    ///
    /// This is deferred instead of done on every update because a
    /// directory with 10k subdirectories gets 10k size updates, and
    /// re-sorting on each one would be quadratic. A UI should settle once
    /// per frame at most.
    pub fn settle(&mut self) {
        for parent in std::mem::take(&mut self.stale) {
            self.sort_children(parent);
            if parent == ROOT {
                self.refresh_root_size();
            }
        }
    }

    /// The children of `parent` with the fraction of the parent each one
    /// takes up, in 0..=1. Empty unless `parent` is loaded.
    ///
    /// Once the parent's walk is done, its size is the denominator, so
    /// shares hold still while the children's walks trickle in. Before
    /// that, the siblings sized so far are all there is. `max` covers
    /// hardlinks: the parent's walk counts a shared inode once, but
    /// separate walks of two children count it once each, so their sum
    /// can come out larger than the parent.
    pub fn children_with_shares(&self, parent: NodeId) -> Vec<(NodeId, f32)> {
        let node = &self.nodes[parent];
        let Children::Loaded(ids) = &node.children else {
            return Vec::new();
        };
        let siblings: u64 = ids.iter().map(|&c| self.nodes[c].size).sum();
        let total = if node.sized {
            node.size.max(siblings)
        } else {
            siblings
        };
        ids.iter()
            .map(|&child| {
                let share = if total == 0 {
                    0.0
                } else {
                    (self.nodes[child].size as f64 / total as f64) as f32
                };
                (child, share)
            })
            .collect()
    }

    /// Settle, then flatten the expanded part of the tree into display
    /// order.
    pub fn visible_rows(&mut self) -> Vec<Row> {
        self.settle();
        let mut rows = Vec::new();
        self.push_rows(ROOT, 1.0, &mut rows);
        rows
    }

    fn push_rows(&self, id: NodeId, share: f32, rows: &mut Vec<Row>) {
        rows.push(Row::Node { id, share });
        let node = &self.nodes[id];
        if !node.expanded {
            return;
        }
        let note = match &node.children {
            Children::Loaded(ids) if ids.is_empty() => Note::Empty,
            Children::Loaded(_) => {
                for (child, share) in self.children_with_shares(id) {
                    self.push_rows(child, share, rows);
                }
                return;
            }
            Children::Loading | Children::Unloaded => Note::Loading,
            Children::Failed(_) => Note::Failed,
        };
        rows.push(Row::Note { parent: id, note });
    }

    /// The root has no parent whose walk would size it, so its size is
    /// the sum of its children, same as the CLI's `total` line. Updating
    /// it as each child finishes gives a number that climbs towards the
    /// final total instead of sitting at "unknown" for minutes.
    fn refresh_root_size(&mut self) {
        let Children::Loaded(ids) = &self.nodes[ROOT].children else {
            return;
        };
        let size = ids.iter().map(|&id| self.nodes[id].size).sum();
        let sized = ids.iter().all(|&id| self.nodes[id].sized);
        let unreadable = ids.iter().map(|&id| self.nodes[id].unreadable).sum();
        let root = &mut self.nodes[ROOT];
        root.size = size;
        root.sized = sized;
        root.unreadable = unreadable;
    }

    /// Largest first, which is the question the tool exists to answer.
    /// Ties (most visibly: directories still waiting for their walk, all
    /// at 0) fall back to the name so rows don't shuffle on every update.
    fn sort_children(&mut self, parent: NodeId) {
        let Children::Loaded(ids) = &mut self.nodes[parent].children else {
            return;
        };
        let mut ids = std::mem::take(ids);
        ids.sort_by(|&a, &b| {
            let (a, b) = (&self.nodes[a], &self.nodes[b]);
            b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name))
        });
        self.nodes[parent].children = Children::Loaded(ids);
    }
}

/// One line in a flattened view of the tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Row {
    Node {
        id: NodeId,
        /// Fraction of the parent level this node takes up, in 0..=1.
        share: f32,
    },
    /// A line under an expanded directory that has no children to show
    /// (yet).
    Note { parent: NodeId, note: Note },
}

/// Why an expanded directory shows no children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    Loading,
    Empty,
    Failed,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn entry(name: &str, size_in_bytes: u64, is_dir: bool) -> Entry {
        Entry {
            path: PathBuf::from("/home/maria").join(name),
            name: name.to_string(),
            size_in_bytes,
            is_dir,
        }
    }

    fn child_names(tree: &Tree, parent: NodeId) -> Vec<&str> {
        let Children::Loaded(ids) = &tree.node(parent).children else {
            panic!("children of {parent} are not loaded");
        };
        ids.iter().map(|&id| tree.node(id).name.as_ref()).collect()
    }

    /// A tree rooted at `/home/maria` with two pending directories and a
    /// file, with the root expanded the way the GUIs start out.
    fn home_tree() -> (Tree, Vec<(NodeId, PathBuf)>) {
        let mut tree = Tree::new(PathBuf::from("/home/maria"));
        tree.set_expanded(ROOT, true);
        let pending = tree.insert_children(
            ROOT,
            vec![
                entry("notes.txt", 300, false),
                entry("Videos", 0, true),
                entry("Downloads", 0, true),
            ],
        );
        tree.settle();
        (tree, pending)
    }

    #[test]
    fn only_directories_wait_for_a_walk() {
        let (tree, pending) = home_tree();
        let pending: Vec<&Path> = pending.iter().map(|(_, path)| path.as_path()).collect();
        assert_eq!(
            pending,
            [
                Path::new("/home/maria/Videos"),
                Path::new("/home/maria/Downloads")
            ]
        );
        assert!(!tree.node(ROOT).sized, "root waits for both directories");
        assert_eq!(tree.node(ROOT).size, 300, "root shows the running sum");
    }

    #[test]
    fn request_load_fires_once_per_directory() {
        let (mut tree, pending) = home_tree();
        let videos = pending[0].0;
        assert!(tree.request_load(videos));
        assert!(!tree.request_load(videos), "already loading");
        assert!(!tree.request_load(ROOT), "already loaded");

        let notes = child_names(&tree, ROOT)
            .iter()
            .position(|&name| name == "notes.txt")
            .expect("notes.txt is a child of the root");
        let Children::Loaded(ids) = &tree.node(ROOT).children else {
            unreachable!("checked by child_names");
        };
        assert!(!tree.request_load(ids[notes]), "files have no children");
    }

    #[test]
    fn sizes_resort_siblings_and_finish_the_root() {
        let (mut tree, pending) = home_tree();
        // Pending directories tie at 0 and fall back to name order.
        assert_eq!(
            child_names(&tree, ROOT),
            ["notes.txt", "Downloads", "Videos"]
        );

        let videos = pending[0].0;
        let downloads = pending[1].0;
        tree.set_size(videos, 9_000, 0);
        tree.settle();
        assert_eq!(
            child_names(&tree, ROOT),
            ["Videos", "notes.txt", "Downloads"]
        );
        assert!(!tree.node(ROOT).sized);

        tree.set_size(downloads, 700, 2);
        tree.settle();
        assert_eq!(
            child_names(&tree, ROOT),
            ["Videos", "Downloads", "notes.txt"]
        );
        assert!(tree.node(ROOT).sized);
        assert_eq!(tree.node(ROOT).size, 10_000);
        assert_eq!(tree.node(ROOT).unreadable, 2);
    }

    #[test]
    fn shares_of_each_level_add_up_to_one() {
        let (mut tree, pending) = home_tree();
        tree.set_size(pending[0].0, 9_000, 0);
        tree.set_size(pending[1].0, 700, 0);

        let shares: Vec<f32> = tree
            .visible_rows()
            .iter()
            .skip(1)
            .map(|row| match row {
                Row::Node { share, .. } => *share,
                Row::Note { .. } => panic!("no notes expected under a loaded root"),
            })
            .collect();
        assert_eq!(shares.len(), 3);
        assert!((shares.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        assert!((shares[0] - 0.9).abs() < 1e-6);
    }

    /// With the parent's size known, a child's share must not jump around
    /// while its siblings are still being walked.
    #[test]
    fn shares_use_the_parent_size_once_known() {
        let (mut tree, pending) = home_tree();
        let videos = pending[0].0;
        tree.set_size(videos, 9_000, 0);
        tree.set_size(pending[1].0, 700, 0);

        tree.set_expanded(videos, true);
        let grandchildren = tree.insert_children(
            videos,
            vec![entry("Videos/trip", 0, true), entry("Videos/old", 0, true)],
        );
        tree.set_size(grandchildren[0].0, 4_500, 0);
        tree.settle();

        let trip_share = tree
            .children_with_shares(videos)
            .into_iter()
            .find_map(|(id, share)| (id == grandchildren[0].0).then_some(share))
            .expect("trip is a child of Videos");
        assert!((trip_share - 0.5).abs() < 1e-6, "share: {trip_share}");
    }

    #[test]
    fn expanded_directories_without_children_get_a_note() {
        let (mut tree, pending) = home_tree();
        let (videos, downloads) = (pending[0].0, pending[1].0);

        tree.set_expanded(videos, true);
        tree.request_load(videos);
        tree.set_expanded(downloads, true);
        tree.insert_children(downloads, Vec::new());

        let notes: Vec<(NodeId, Note)> = tree
            .visible_rows()
            .iter()
            .filter_map(|row| match row {
                Row::Note { parent, note } => Some((*parent, *note)),
                Row::Node { .. } => None,
            })
            .collect();
        assert_eq!(notes, [(downloads, Note::Empty), (videos, Note::Loading)]);
    }
}
