//! Compact arena-backed file tree.
//!
//! Every file and directory is one [`Node`] in a flat `Vec`, linked by `u32`
//! indices (parent / first child / next sibling). This avoids a heap-allocated
//! `Vec` of children per directory and keeps memory around ~40 bytes per entry
//! plus its name, which matters when scanning millions of files.
//!
//! Invariant: a child always has a larger index than its parent, so a single
//! reverse pass over the arena aggregates sizes bottom-up.

use std::ffi::OsStr;
use std::path::PathBuf;

pub type NodeId = u32;
pub const NONE: NodeId = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
pub struct Node {
    /// File name; for the root this is the full path that was scanned.
    pub name: Box<OsStr>,
    /// Size in bytes. For directories this is the aggregated subtree size
    /// once [`Tree::finalize`] has run.
    pub size: u64,
    pub parent: NodeId,
    first_child: NodeId,
    next_sibling: NodeId,
    pub kind: NodeKind,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }

    pub fn name_lossy(&self) -> std::borrow::Cow<'_, str> {
        self.name.to_string_lossy()
    }
}

#[derive(Debug, Default)]
pub struct Tree {
    nodes: Vec<Node>,
    /// Number of entries that could not be read (permission denied etc.).
    pub errors: u64,
}

impl Tree {
    pub fn with_root(path: impl Into<PathBuf>, kind: NodeKind, size: u64) -> Self {
        let path: PathBuf = path.into();
        let mut tree = Tree::default();
        tree.nodes.push(Node {
            name: path.into_os_string().into_boxed_os_str(),
            size,
            parent: NONE,
            first_child: NONE,
            next_sibling: NONE,
            kind,
        });
        tree
    }

    pub fn root(&self) -> NodeId {
        0
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn get(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    /// Append a child under `parent`. Children are prepended to the sibling
    /// list (O(1)); display code sorts them anyway.
    pub fn add_child(&mut self, parent: NodeId, name: &OsStr, kind: NodeKind, size: u64) -> NodeId {
        let id = self.nodes.len() as NodeId;
        let next = self.nodes[parent as usize].first_child;
        self.nodes.push(Node {
            name: name.into(),
            size,
            parent,
            first_child: NONE,
            next_sibling: next,
            kind,
        });
        self.nodes[parent as usize].first_child = id;
        id
    }

    /// Roll file sizes up into their parent directories.
    pub fn finalize(&mut self) {
        for i in (1..self.nodes.len()).rev() {
            let (size, parent) = (self.nodes[i].size, self.nodes[i].parent);
            self.nodes[parent as usize].size += size;
        }
    }

    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            tree: self,
            next: self.nodes[id as usize].first_child,
        }
    }

    /// Children sorted by size, largest first (ties broken by name).
    pub fn sorted_children(&self, id: NodeId) -> Vec<NodeId> {
        let mut kids: Vec<NodeId> = self.children(id).collect();
        kids.sort_unstable_by(|&a, &b| {
            let (na, nb) = (self.get(a), self.get(b));
            nb.size.cmp(&na.size).then_with(|| na.name.cmp(&nb.name))
        });
        kids
    }

    /// Reconstruct the full filesystem path of a node.
    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != NONE {
            let node = self.get(cur);
            parts.push(&*node.name);
            cur = node.parent;
        }
        let mut path = PathBuf::new();
        for part in parts.into_iter().rev() {
            path.push(part);
        }
        path
    }

    /// Path of a node relative to the root, using `/` separators.
    pub fn rel_path(&self, id: NodeId) -> String {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != NONE && cur != self.root() {
            let node = self.get(cur);
            parts.push(node.name_lossy());
            cur = node.parent;
        }
        parts.reverse();
        parts.join("/")
    }

    /// Detach a node (e.g. after deleting it on disk) and subtract its size
    /// from all ancestors. The node stays in the arena but becomes unreachable.
    pub fn remove(&mut self, id: NodeId) {
        let (parent, size) = (self.nodes[id as usize].parent, self.nodes[id as usize].size);
        if parent == NONE {
            return;
        }
        // Unlink from the sibling list.
        let mut cur = self.nodes[parent as usize].first_child;
        if cur == id {
            self.nodes[parent as usize].first_child = self.nodes[id as usize].next_sibling;
        } else {
            while cur != NONE {
                let next = self.nodes[cur as usize].next_sibling;
                if next == id {
                    self.nodes[cur as usize].next_sibling = self.nodes[id as usize].next_sibling;
                    break;
                }
                cur = next;
            }
        }
        // Subtract size from ancestors.
        let mut anc = parent;
        while anc != NONE {
            let n = &mut self.nodes[anc as usize];
            n.size = n.size.saturating_sub(size);
            anc = n.parent;
        }
        let n = &mut self.nodes[id as usize];
        n.parent = NONE;
        n.next_sibling = NONE;
    }

    /// Count `(files, directories)` reachable from the root, excluding the root.
    pub fn counts(&self) -> (u64, u64) {
        let (mut files, mut dirs) = (0, 0);
        for id in self.descendants(self.root()).skip(1) {
            if self.get(id).is_dir() {
                dirs += 1;
            } else {
                files += 1;
            }
        }
        (files, dirs)
    }

    /// Iterate a subtree depth-first (pre-order), including `id` itself.
    pub fn descendants(&self, id: NodeId) -> Descendants<'_> {
        Descendants {
            tree: self,
            stack: vec![id],
        }
    }
}

pub struct Children<'a> {
    tree: &'a Tree,
    next: NodeId,
}

impl Iterator for Children<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        if self.next == NONE {
            return None;
        }
        let id = self.next;
        self.next = self.tree.nodes[id as usize].next_sibling;
        Some(id)
    }
}

pub struct Descendants<'a> {
    tree: &'a Tree,
    stack: Vec<NodeId>,
}

impl Iterator for Descendants<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        let id = self.stack.pop()?;
        self.stack.extend(self.tree.children(id));
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Tree {
        let mut t = Tree::with_root("/r", NodeKind::Dir, 0);
        let a = t.add_child(0, OsStr::new("a"), NodeKind::Dir, 0);
        t.add_child(a, OsStr::new("x"), NodeKind::File, 10);
        t.add_child(a, OsStr::new("y"), NodeKind::File, 30);
        t.add_child(0, OsStr::new("b"), NodeKind::File, 5);
        t.finalize();
        t
    }

    #[test]
    fn aggregates_and_sorts() {
        let t = sample();
        assert_eq!(t.get(0).size, 45);
        let kids = t.sorted_children(0);
        assert_eq!(t.get(kids[0]).name_lossy(), "a");
        assert_eq!(t.get(kids[0]).size, 40);
        assert_eq!(t.rel_path(t.sorted_children(kids[0])[0]), "a/y");
    }

    #[test]
    fn node_stays_compact() {
        assert!(std::mem::size_of::<Node>() <= 40);
    }

    #[test]
    fn remove_updates_ancestors() {
        let mut t = sample();
        let a = t.sorted_children(0)[0];
        let y = t.sorted_children(a)[0];
        t.remove(y);
        assert_eq!(t.get(a).size, 10);
        assert_eq!(t.get(0).size, 15);
        assert_eq!(t.children(a).count(), 1);
    }
}
