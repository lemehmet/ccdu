//! ccdu's own export format.
//!
//! The file mirrors the in-memory arena: a header, the name blob, then the nodes as fixed 48-byte
//! records. That makes writing close to a memcpy and reading a single allocation plus a validation
//! pass, which matters when the tree has millions of entries.
//!
//! No compression. The format is already about as small as the data gets without spending CPU, and
//! leaving it out means `ccdu scan -o - | zstd` is the user's choice rather than ours.

use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use super::{invalid, read_exact};
use crate::model::{Node, Tree};

pub const MAGIC: &[u8; 4] = b"CCDU";

/// Bumped when the layout changes incompatibly. A file from the future is refused.
pub const VERSION: u16 = 1;

/// Guards against a corrupt header asking for an absurd allocation before anything is read.
const MAX_NODES: u64 = 1 << 32;
const MAX_NAMES: u64 = 1 << 36;

pub fn write(tree: &Tree, mut out: impl Write) -> io::Result<()> {
    let nodes = tree.raw_nodes();
    let names = tree.raw_names();
    let root = tree.root_path().as_os_str().as_bytes();

    out.write_all(MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&(root.len() as u32).to_le_bytes())?;
    out.write_all(root)?;
    out.write_all(&tree.errors.to_le_bytes())?;
    out.write_all(&[u8::from(tree.cancelled)])?;
    out.write_all(&(names.len() as u64).to_le_bytes())?;
    out.write_all(&(nodes.len() as u64).to_le_bytes())?;
    out.write_all(names)?;

    let mut record = [0u8; 48];
    for node in nodes {
        encode(node, &mut record);
        out.write_all(&record)?;
    }
    Ok(())
}

pub fn read(mut input: impl Read) -> io::Result<Tree> {
    let magic: [u8; 4] = read_exact(&mut input)?;
    if &magic != MAGIC {
        return Err(invalid("not a ccdu export"));
    }
    let version = u16::from_le_bytes(read_exact(&mut input)?);
    if version > VERSION {
        return Err(invalid(format!(
            "export format v{version} is newer than this build understands (v{VERSION})"
        )));
    }

    let root_len = u32::from_le_bytes(read_exact::<4>(&mut input)?) as usize;
    if root_len > 1 << 16 {
        return Err(invalid("implausible root path length"));
    }
    let mut root = vec![0u8; root_len];
    input.read_exact(&mut root)?;

    let errors = u64::from_le_bytes(read_exact(&mut input)?);
    let cancelled = read_exact::<1>(&mut input)?[0] != 0;
    let names_len = u64::from_le_bytes(read_exact(&mut input)?);
    let node_count = u64::from_le_bytes(read_exact(&mut input)?);

    // Checked before allocating: a corrupt length should be an error, not an out-of-memory abort.
    if names_len > MAX_NAMES {
        return Err(invalid(format!("name blob claims {names_len} bytes")));
    }
    if node_count == 0 || node_count > MAX_NODES {
        return Err(invalid(format!("node count claims {node_count}")));
    }

    let mut names = vec![0u8; names_len as usize];
    input.read_exact(&mut names)?;

    let mut nodes = Vec::with_capacity(node_count as usize);
    let mut record = [0u8; 48];
    for _ in 0..node_count {
        input.read_exact(&mut record)?;
        nodes.push(decode(&record));
    }

    Tree::from_raw(
        PathBuf::from(std::ffi::OsString::from_vec(root)),
        nodes,
        names,
        errors,
        cancelled,
    )
    .map_err(invalid)
}

fn encode(node: &Node, out: &mut [u8; 48]) {
    out[0..8].copy_from_slice(&node.apparent.to_le_bytes());
    out[8..16].copy_from_slice(&node.disk.to_le_bytes());
    out[16..24].copy_from_slice(&node.mtime.to_le_bytes());
    out[24..28].copy_from_slice(&node.parent.to_le_bytes());
    out[28..32].copy_from_slice(&node.first_child.to_le_bytes());
    out[32..36].copy_from_slice(&node.next_sibling.to_le_bytes());
    out[36..40].copy_from_slice(&node.items.to_le_bytes());
    out[40..44].copy_from_slice(&node.name_off.to_le_bytes());
    out[44..46].copy_from_slice(&node.name_len.to_le_bytes());
    out[46..48].copy_from_slice(&node.flags.to_le_bytes());
}

fn decode(bytes: &[u8; 48]) -> Node {
    let u64_at = |i: usize| u64::from_le_bytes(bytes[i..i + 8].try_into().expect("fixed width"));
    let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().expect("fixed width"));
    let u16_at = |i: usize| u16::from_le_bytes(bytes[i..i + 2].try_into().expect("fixed width"));

    Node {
        apparent: u64_at(0),
        disk: u64_at(8),
        mtime: u64_at(16) as i64,
        parent: u32_at(24),
        first_child: u32_at(28),
        next_sibling: u32_at(32),
        items: u32_at(36),
        name_off: u32_at(40),
        name_len: u16_at(44),
        flags: u16_at(46),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::tests::{contents, fixture};
    use crate::model::ROOT;

    fn encoded(tree: &Tree) -> Vec<u8> {
        let mut buffer = Vec::new();
        write(tree, &mut buffer).unwrap();
        buffer
    }

    #[test]
    fn a_tree_survives_the_round_trip_unchanged() {
        let (_d, tree) = fixture();
        let back = read(io::Cursor::new(encoded(&tree))).unwrap();

        assert_eq!(contents(&back), contents(&tree));
        assert_eq!(back.len(), tree.len());
        assert_eq!(back.root_path(), tree.root_path());
        assert_eq!(back.node(ROOT).items, tree.node(ROOT).items);
    }

    #[test]
    fn a_cancelled_scan_stays_marked_as_partial() {
        let (_d, mut tree) = fixture();
        tree.cancelled = true;
        tree.errors = 7;

        let back = read(io::Cursor::new(encoded(&tree))).unwrap();
        assert!(back.cancelled, "a partial scan must not come back looking complete");
        assert_eq!(back.errors, 7);
    }

    #[test]
    fn a_non_utf8_name_survives() {
        use crate::model::{Kind, Meta};
        let mut tree = Tree::new(PathBuf::from("/data"), &Meta::unknown(Kind::Dir));
        let meta =
            Meta { kind: Kind::File, apparent: 10, disk: 4096, mtime: 5, dev: 1, ino: 2, nlink: 1 };
        tree.push_child(ROOT, b"broken\xffname", &meta, 0, true);
        tree.finish();

        let back = read(io::Cursor::new(encoded(&tree))).unwrap();
        let child = back.children(ROOT).next().unwrap();
        assert_eq!(back.name(child).as_bytes(), b"broken\xffname");
    }

    #[test]
    fn a_newer_version_is_refused() {
        let (_d, tree) = fixture();
        let mut bytes = encoded(&tree);
        bytes[4..6].copy_from_slice(&(VERSION + 1).to_le_bytes());

        let err = read(io::Cursor::new(bytes)).unwrap_err();
        assert!(err.to_string().contains("newer than this build"), "{err}");
    }

    #[test]
    fn a_truncated_file_is_an_error_not_a_panic() {
        let (_d, tree) = fixture();
        let bytes = encoded(&tree);
        for cut in [1, 8, 20, bytes.len() / 2, bytes.len() - 1] {
            assert!(read(io::Cursor::new(&bytes[..cut])).is_err(), "accepted a {cut}-byte file");
        }
    }

    #[test]
    fn an_out_of_range_index_is_rejected() {
        let (_d, tree) = fixture();
        let mut bytes = encoded(&tree);

        // The last node's parent, pointed somewhere that does not exist.
        let last = bytes.len() - 48;
        bytes[last + 24..last + 28].copy_from_slice(&9_999_999u32.to_le_bytes());

        let err = read(io::Cursor::new(bytes)).unwrap_err();
        assert!(err.to_string().contains("past the end"), "{err}");
    }

    #[test]
    fn a_name_reaching_past_the_blob_is_rejected() {
        let (_d, tree) = fixture();
        let mut bytes = encoded(&tree);
        let last = bytes.len() - 48;
        bytes[last + 40..last + 44].copy_from_slice(&u32::MAX.to_le_bytes());

        let err = read(io::Cursor::new(bytes)).unwrap_err();
        assert!(err.to_string().contains("names bytes"), "{err}");
    }

    #[test]
    fn an_absurd_header_does_not_try_to_allocate_it() {
        let (_d, tree) = fixture();
        let mut bytes = encoded(&tree);
        let count_at = bytes.len() - 48 * tree.len() - 8 - tree.raw_names().len();

        // Claim more nodes than could exist. This must fail on the claim, not on the allocation.
        bytes[count_at..count_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        let err = read(io::Cursor::new(bytes)).unwrap_err();
        assert!(err.to_string().contains("node count claims"), "{err}");
    }

    #[test]
    fn the_record_size_matches_the_node_size() {
        // If `Node` grows, the format must be versioned rather than silently reinterpreted.
        assert_eq!(std::mem::size_of::<Node>(), 48);
    }
}
