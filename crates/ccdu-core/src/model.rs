//! The in-memory directory tree.
//!
//! Nodes live in one flat arena and refer to each other by index, and every name lives in a single
//! byte buffer. A [`Node`] is exactly 48 bytes with no padding, which is what makes it practical to
//! hold a tree with millions of entries in memory while browsing it.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Index of a node in a [`Tree`]'s arena.
pub type NodeId = u32;

/// The root of every tree.
pub const ROOT: NodeId = 0;

/// Sentinel for "no such node" (end of a sibling chain, parent of the root).
pub const NO_NODE: NodeId = u32::MAX;

/// Bit flags stored on each [`Node`].
pub mod flags {
    /// Directory.
    pub const DIR: u16 = 1 << 0;
    /// Symbolic link. Never followed; the link itself is what we account for.
    pub const SYMLINK: u16 = 1 << 1;
    /// Something that is neither a directory, a regular file, nor a symlink.
    pub const OTHER: u16 = 1 << 2;
    /// Lives on a different filesystem than the scan root.
    pub const OTHER_FS: u16 = 1 << 3;
    /// Could not be stat'd or read. Its size is unknown, not zero.
    pub const ERR: u16 = 1 << 4;
    /// A file with `nlink > 1` whose blocks are accounted for at *this* node.
    pub const HARDLINK: u16 = 1 << 5;
    /// A file with `nlink > 1` already accounted for elsewhere in this tree, so it contributes
    /// nothing to disk usage. Without this, hardlink farms report wildly inflated totals.
    pub const HARDLINK_DUP: u16 = 1 << 6;
    /// Skipped because it matched an exclude rule.
    pub const EXCLUDED: u16 = 1 << 7;
    /// Not descended into: a directory we had already visited (symlink or bind-mount loop).
    pub const LOOP: u16 = 1 << 8;
}

/// What kind of thing a directory entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    Other,
}

impl Kind {
    pub(crate) fn to_flags(self) -> u16 {
        match self {
            Kind::Dir => flags::DIR,
            Kind::File => 0,
            Kind::Symlink => flags::SYMLINK,
            Kind::Other => flags::OTHER,
        }
    }
}

/// Everything we learn about an entry from a single `fstatat`.
#[derive(Clone, Copy, Debug)]
pub struct Meta {
    pub kind: Kind,
    /// `st_size`: the size the file claims to be.
    pub apparent: u64,
    /// `st_blocks * 512`: the space it actually occupies. This is the number that matters when
    /// you are trying to free space, and it differs from `apparent` for sparse and compressed
    /// files as well as for tiny files rounded up to a block.
    pub disk: u64,
    pub mtime: i64,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
}

impl Meta {
    /// Placeholder for an entry we could not stat.
    pub fn unknown(kind: Kind) -> Self {
        Meta { kind, apparent: 0, disk: 0, mtime: 0, dev: 0, ino: 0, nlink: 1 }
    }
}

/// One entry in the tree. Field order is chosen so this packs to exactly 48 bytes.
#[derive(Clone, Copy, Debug)]
pub struct Node {
    /// Apparent size of this node's whole subtree, including itself.
    pub apparent: u64,
    /// Disk usage of this node's whole subtree, including itself, with hardlinks counted once.
    pub disk: u64,
    pub mtime: i64,
    pub parent: NodeId,
    pub first_child: NodeId,
    pub next_sibling: NodeId,
    /// Number of entries in this subtree, excluding the node itself.
    pub items: u32,
    name_off: u32,
    name_len: u16,
    pub flags: u16,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.flags & flags::DIR != 0
    }

    pub fn has(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }
}

/// A scanned directory tree.
pub struct Tree {
    nodes: Vec<Node>,
    names: Vec<u8>,
    root_path: PathBuf,
    /// Entries that could not be read or stat'd.
    pub errors: u64,
}

impl Tree {
    /// Create a tree containing only its root.
    pub fn new(root_path: PathBuf, meta: &Meta) -> Self {
        let mut tree = Tree { nodes: Vec::new(), names: Vec::new(), root_path, errors: 0 };
        let name = tree.intern(b"");
        tree.nodes.push(Node {
            apparent: meta.apparent,
            disk: meta.disk,
            mtime: meta.mtime,
            parent: NO_NODE,
            first_child: NO_NODE,
            next_sibling: NO_NODE,
            items: 0,
            name_off: name.0,
            name_len: name.1,
            flags: meta.kind.to_flags(),
        });
        tree
    }

    fn intern(&mut self, name: &[u8]) -> (u32, u16) {
        let off = self.names.len() as u32;
        let len = name.len().min(u16::MAX as usize);
        self.names.extend_from_slice(&name[..len]);
        (off, len as u16)
    }

    /// Add a child under `parent` and propagate its size up to every ancestor.
    ///
    /// `counted` is false for entries whose blocks are already accounted for elsewhere (hardlink
    /// duplicates) or unknown (stat errors); such nodes still appear in the tree but add nothing
    /// to the totals.
    pub fn push_child(
        &mut self,
        parent: NodeId,
        name: &[u8],
        meta: &Meta,
        extra_flags: u16,
        counted: bool,
    ) -> NodeId {
        let (name_off, name_len) = self.intern(name);
        let id = self.nodes.len() as NodeId;
        let (apparent, disk) = if counted { (meta.apparent, meta.disk) } else { (0, 0) };
        self.nodes.push(Node {
            apparent,
            disk,
            mtime: meta.mtime,
            parent,
            first_child: NO_NODE,
            next_sibling: self.nodes[parent as usize].first_child,
            items: 0,
            name_off,
            name_len,
            flags: meta.kind.to_flags() | extra_flags,
        });
        self.nodes[parent as usize].first_child = id;

        // Walk to the root adding this entry's weight. Depth is small in practice, and doing it
        // eagerly means a partially scanned tree always shows correct totals for what it has.
        let mut cur = parent;
        while cur != NO_NODE {
            let n = &mut self.nodes[cur as usize];
            n.apparent += apparent;
            n.disk += disk;
            n.items += 1;
            cur = n.parent;
        }
        id
    }

    /// Reverse every sibling chain so children come back in the order they were pushed.
    ///
    /// Children are prepended during the scan (O(1), no per-node tail pointer), which leaves each
    /// chain reversed. Call this once when the scan finishes.
    pub fn finish(&mut self) {
        for i in 0..self.nodes.len() {
            let mut prev = NO_NODE;
            let mut cur = self.nodes[i].first_child;
            while cur != NO_NODE {
                let next = self.nodes[cur as usize].next_sibling;
                self.nodes[cur as usize].next_sibling = prev;
                prev = cur;
                cur = next;
            }
            self.nodes[i].first_child = prev;
        }
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn add_flags(&mut self, id: NodeId, flags: u16) {
        self.nodes[id as usize].flags |= flags;
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn name(&self, id: NodeId) -> &OsStr {
        let n = &self.nodes[id as usize];
        let start = n.name_off as usize;
        OsStr::from_bytes(&self.names[start..start + n.name_len as usize])
    }

    /// Bytes used by the arena itself, for reporting.
    pub fn memory_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>() + self.names.capacity()
    }

    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children { tree: self, cur: self.nodes[id as usize].first_child }
    }

    /// Absolute path of a node, rebuilt from the chain of names up to the root.
    pub fn path_of(&self, id: NodeId) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != ROOT && cur != NO_NODE {
            parts.push(cur);
            cur = self.nodes[cur as usize].parent;
        }
        let mut path = self.root_path.clone();
        for &part in parts.iter().rev() {
            path.push(self.name(part));
        }
        path
    }
}

pub struct Children<'a> {
    tree: &'a Tree,
    cur: NodeId,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        if self.cur == NO_NODE {
            return None;
        }
        let id = self.cur;
        self.cur = self.tree.nodes[id as usize].next_sibling;
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(size: u64) -> Meta {
        Meta { kind: Kind::File, apparent: size, disk: size, mtime: 0, dev: 1, ino: 0, nlink: 1 }
    }

    fn dir() -> Meta {
        Meta { kind: Kind::Dir, apparent: 0, disk: 0, mtime: 0, dev: 1, ino: 0, nlink: 2 }
    }

    #[test]
    fn node_is_48_bytes() {
        assert_eq!(std::mem::size_of::<Node>(), 48);
    }

    #[test]
    fn sizes_propagate_to_every_ancestor() {
        let mut t = Tree::new(PathBuf::from("/r"), &dir());
        let a = t.push_child(ROOT, b"a", &dir(), 0, true);
        let b = t.push_child(a, b"b", &dir(), 0, true);
        t.push_child(b, b"f", &file(100), 0, true);
        t.push_child(a, b"g", &file(30), 0, true);

        assert_eq!(t.node(ROOT).disk, 130);
        assert_eq!(t.node(a).disk, 130);
        assert_eq!(t.node(b).disk, 100);
        assert_eq!(t.node(ROOT).items, 4);
        assert_eq!(t.node(a).items, 3);
    }

    #[test]
    fn uncounted_nodes_exist_but_weigh_nothing() {
        let mut t = Tree::new(PathBuf::from("/r"), &dir());
        t.push_child(ROOT, b"real", &file(100), flags::HARDLINK, true);
        let dup = t.push_child(ROOT, b"dup", &file(100), flags::HARDLINK_DUP, false);

        assert_eq!(t.node(ROOT).disk, 100);
        assert_eq!(t.node(ROOT).items, 2);
        assert_eq!(t.node(dup).disk, 0);
        assert!(t.node(dup).has(flags::HARDLINK_DUP));
    }

    #[test]
    fn finish_restores_insertion_order() {
        let mut t = Tree::new(PathBuf::from("/r"), &dir());
        for name in [b"a", b"b", b"c"] {
            t.push_child(ROOT, name, &file(1), 0, true);
        }
        t.finish();
        let names: Vec<_> =
            t.children(ROOT).map(|c| t.name(c).to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[test]
    fn paths_are_rebuilt_from_the_name_chain() {
        let mut t = Tree::new(PathBuf::from("/r"), &dir());
        let a = t.push_child(ROOT, b"a", &dir(), 0, true);
        let f = t.push_child(a, b"f.txt", &file(1), 0, true);
        assert_eq!(t.path_of(f), PathBuf::from("/r/a/f.txt"));
        assert_eq!(t.path_of(ROOT), PathBuf::from("/r"));
    }
}
