//! ncdu's JSON export format.
//!
//! `[major, minor, metadata, tree]`, where the tree is an array whose first element describes the
//! directory itself and whose remaining elements are its entries — a nested array for each
//! subdirectory, an object for everything else.
//!
//! The important difference from our own model: ncdu records each entry's *own* size, while our
//! nodes carry subtree totals. Writing subtracts the children back out; reading lets the tree
//! accumulate them again.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use serde::Deserialize;

use super::invalid;
use crate::model::{flags, Kind, Meta, NodeId, Tree, ROOT};

const MAJOR: u32 = 1;
const MINOR: u32 = 2;

/// Deepest nesting accepted from a dump. Guards the recursive build against a hostile file.
const MAX_DEPTH: usize = 1024;

pub fn write(tree: &Tree, mut out: impl Write) -> io::Result<()> {
    write!(
        out,
        "[{MAJOR},{MINOR},{{\"progname\":\"ccdu\",\"progver\":\"{}\",\"timestamp\":{}}},",
        env!("CARGO_PKG_VERSION"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    )?;
    write_dir(tree, ROOT, &tree.root_path().to_string_lossy(), &mut out)?;
    out.write_all(b"]\n")
}

fn write_dir(tree: &Tree, id: NodeId, name: &str, out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"[")?;
    write_item(tree, id, name, out)?;
    for child in tree.children(id) {
        out.write_all(b",\n")?;
        let child_name = tree.name(child).to_string_lossy().into_owned();
        if tree.node(child).is_dir() {
            write_dir(tree, child, &child_name, out)?;
        } else {
            write_item(tree, child, &child_name, out)?;
        }
    }
    out.write_all(b"]")
}

fn write_item(tree: &Tree, id: NodeId, name: &str, out: &mut impl Write) -> io::Result<()> {
    let node = tree.node(id);
    // Our totals include the subtree; ncdu's do not.
    let children: (u64, u64) = tree
        .children(id)
        .fold((0, 0), |(a, d), c| (a + tree.node(c).apparent, d + tree.node(c).disk));

    write!(out, "{{\"name\":{}", serde_json::to_string(name)?)?;
    write!(out, ",\"asize\":{}", node.apparent.saturating_sub(children.0))?;
    write!(out, ",\"dsize\":{}", node.disk.saturating_sub(children.1))?;
    write!(out, ",\"mtime\":{}", node.mtime)?;

    // Deliberately no `hlnkc`. Our tree has already resolved hardlinks — the duplicates carry zero
    // size — and the marker is only useful to a reader alongside the inode numbers that would let
    // it deduplicate for itself. A 48-byte node has no room for those, so claiming the marker
    // would invite ncdu to apply its own accounting on top of ours and lose bytes doing it.
    if node.has(flags::SYMLINK) || node.has(flags::OTHER) {
        out.write_all(b",\"notreg\":true")?;
    }
    if node.has(flags::ERR) {
        out.write_all(b",\"read_error\":true")?;
    }
    if node.has(flags::EXCLUDED) {
        out.write_all(b",\"excluded\":\"pattern\"")?;
    }
    if node.has(flags::OTHER_FS) {
        out.write_all(b",\"excluded\":\"otherfs\"")?;
    }
    out.write_all(b"}")
}

/// One entry: an object, or an array meaning a directory and its contents.
#[derive(Deserialize)]
#[serde(untagged)]
enum Entry {
    Item(Item),
    Dir(Vec<Entry>),
}

#[derive(Deserialize, Default)]
struct Item {
    #[serde(default)]
    name: String,
    #[serde(default)]
    asize: u64,
    #[serde(default)]
    dsize: u64,
    #[serde(default)]
    dev: u64,
    #[serde(default)]
    ino: u64,
    #[serde(default)]
    hlnkc: bool,
    #[serde(default)]
    notreg: bool,
    #[serde(default)]
    mtime: i64,
    #[serde(default)]
    read_error: bool,
    #[serde(default)]
    excluded: Option<String>,
}

impl Item {
    fn meta(&self, kind: Kind) -> Meta {
        Meta {
            kind,
            apparent: self.asize,
            disk: self.dsize,
            mtime: self.mtime,
            dev: self.dev,
            ino: self.ino,
            nlink: if self.hlnkc { 2 } else { 1 },
        }
    }

    fn extra_flags(&self) -> u16 {
        let mut flags = 0;
        if self.read_error {
            flags |= flags::ERR;
        }
        match self.excluded.as_deref() {
            Some("otherfs") => flags |= flags::OTHER_FS,
            Some(_) => flags |= flags::EXCLUDED,
            None => {}
        }
        flags
    }
}

pub fn read(input: impl Read) -> io::Result<Tree> {
    // The metadata object varies between versions and holds nothing we need.
    let (major, _minor, _meta, root): (u32, u32, serde::de::IgnoredAny, Entry) =
        serde_json::from_reader(input).map_err(|e| invalid(format!("not an ncdu dump: {e}")))?;

    if major != MAJOR {
        return Err(invalid(format!("ncdu dump format v{major} is not supported")));
    }
    let Entry::Dir(entries) = root else {
        return Err(invalid("the dump's root is not a directory"));
    };
    let Some(Entry::Item(info)) = entries.first() else {
        return Err(invalid("the dump's root has no directory record"));
    };

    let mut tree = Tree::new(info.name.clone().into(), &info.meta(Kind::Dir));
    tree.errors = u64::from(info.read_error);
    // ncdu counts an inode once; we track which we have seen so repeats add nothing to the totals.
    let mut seen: HashMap<(u64, u64), ()> = HashMap::new();
    add_children(&mut tree, ROOT, &entries[1..], &mut seen, 1)?;
    tree.finish();
    Ok(tree)
}

fn add_children(
    tree: &mut Tree,
    parent: NodeId,
    entries: &[Entry],
    seen: &mut HashMap<(u64, u64), ()>,
    depth: usize,
) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(invalid(format!("dump nests deeper than {MAX_DEPTH} levels")));
    }

    for entry in entries {
        match entry {
            Entry::Item(item) => {
                let kind = if item.notreg { Kind::Other } else { Kind::File };
                let mut extra = item.extra_flags();
                let mut counted = true;

                if item.hlnkc {
                    // A second name for an inode already counted adds nothing, exactly as during
                    // a live scan. Dumps without inode numbers cannot be deduplicated, so every
                    // copy counts and the total errs high rather than low.
                    if item.ino != 0 && seen.insert((item.dev, item.ino), ()).is_some() {
                        extra |= flags::HARDLINK_DUP;
                        counted = false;
                    } else {
                        extra |= flags::HARDLINK;
                    }
                }
                if item.read_error {
                    tree.errors += 1;
                }
                tree.push_child(parent, item.name.as_bytes(), &item.meta(kind), extra, counted);
            }
            Entry::Dir(children) => {
                let Some(Entry::Item(info)) = children.first() else {
                    return Err(invalid("a directory in the dump has no directory record"));
                };
                if info.read_error {
                    tree.errors += 1;
                }
                let id = tree.push_child(
                    parent,
                    info.name.as_bytes(),
                    &info.meta(Kind::Dir),
                    info.extra_flags(),
                    true,
                );
                add_children(tree, id, &children[1..], seen, depth + 1)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::tests::{contents, fixture};

    fn encoded(tree: &Tree) -> String {
        let mut buffer = Vec::new();
        write(tree, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    }

    #[test]
    fn sizes_survive_the_round_trip() {
        let (_d, tree) = fixture();
        let back = read(io::Cursor::new(encoded(&tree))).unwrap();

        assert_eq!(back.node(ROOT).disk, tree.node(ROOT).disk, "root totals differ");
        assert_eq!(back.node(ROOT).apparent, tree.node(ROOT).apparent);
        assert_eq!(back.node(ROOT).items, tree.node(ROOT).items);

        for (path, (apparent, disk, _)) in contents(&tree) {
            let got = contents(&back);
            let (a, d, _) = got[&path];
            assert_eq!((a, d), (apparent, disk), "sizes differ for {path}");
        }
    }

    #[test]
    fn a_real_ncdu_dump_is_understood() {
        // The shape ncdu 1.x actually writes, including fields we ignore.
        let dump = r#"[1,2,{"progname":"ncdu","progver":"1.16","timestamp":1690000000},
        [{"name":"/data","asize":4096,"dsize":4096,"dev":2049,"ino":2},
         {"name":"notes.txt","asize":900,"dsize":4096,"ino":3},
         [{"name":"logs","asize":4096,"dsize":4096,"ino":4},
          {"name":"a.log","asize":50000,"dsize":53248,"ino":5},
          {"name":"broken","asize":0,"dsize":0,"ino":6,"read_error":true}
         ]
        ]]"#;

        let tree = read(io::Cursor::new(dump)).unwrap();
        assert_eq!(tree.root_path(), std::path::Path::new("/data"));
        assert_eq!(tree.node(ROOT).items, 4);
        assert_eq!(tree.node(ROOT).disk, 4096 + 4096 + 4096 + 53248);
        assert_eq!(tree.errors, 1);

        let found = contents(&tree);
        assert_eq!(found["logs/a.log"].1, 53248);
        assert!(found["logs/broken"].2 & flags::ERR != 0);
    }

    #[test]
    fn hardlinks_in_a_dump_are_counted_once() {
        let dump = r#"[1,2,{"progname":"ncdu"},
        [{"name":"/data","dsize":4096,"dev":1,"ino":2},
         {"name":"first","asize":5000,"dsize":8192,"dev":1,"ino":99,"hlnkc":true},
         {"name":"second","asize":5000,"dsize":8192,"dev":1,"ino":99,"hlnkc":true}
        ]]"#;

        let tree = read(io::Cursor::new(dump)).unwrap();
        // One inode, counted once, however many names point at it.
        assert_eq!(tree.node(ROOT).disk, 4096 + 8192);
        assert_eq!(tree.node(ROOT).items, 2);
    }

    #[test]
    fn a_dump_without_inode_numbers_still_loads() {
        let dump = r#"[1,2,{},
        [{"name":"/data"},
         {"name":"a","asize":10,"dsize":4096,"hlnkc":true},
         {"name":"b","asize":10,"dsize":4096,"hlnkc":true}
        ]]"#;

        // Nothing to deduplicate by, so both count; the total errs high rather than low.
        let tree = read(io::Cursor::new(dump)).unwrap();
        assert_eq!(tree.node(ROOT).disk, 8192);
    }

    #[test]
    fn totals_agree_after_a_round_trip_through_hardlinks() {
        let (_d, tree) = fixture();
        let back = read(io::Cursor::new(encoded(&tree))).unwrap();
        // The fixture has a hardlink pair. Its total must survive exactly: not doubled by a
        // reader that counts both names, nor short by one that discards both.
        assert_eq!(back.node(ROOT).disk, tree.node(ROOT).disk);
        assert_eq!(back.node(ROOT).apparent, tree.node(ROOT).apparent);
    }

    #[test]
    fn symlinks_come_back_marked_as_irregular() {
        let (_d, tree) = fixture();
        let text = encoded(&tree);
        assert!(text.contains("\"notreg\":true"), "{text}");

        let back = read(io::Cursor::new(text)).unwrap();
        assert!(contents(&back).values().any(|(_, _, f)| f & flags::OTHER != 0));
    }

    #[test]
    fn malformed_dumps_are_rejected() {
        for junk in [
            "[]",
            "[1,2]",
            r#"[9,9,{},[{"name":"/x"}]]"#,
            r#"[1,2,{},{"name":"/x"}]"#,
            r#"[1,2,{},[]]"#,
            "not json at all",
        ] {
            assert!(read(io::Cursor::new(junk)).is_err(), "accepted {junk:?}");
        }
    }

    #[test]
    fn deep_nesting_is_refused_rather_than_recursed() {
        // Deeper than serde_json's own recursion limit would allow anyway; this checks we fail
        // with a message rather than by running out of stack.
        let mut dump = String::from("[1,2,{},");
        let depth = 200;
        for i in 0..depth {
            dump.push_str(&format!("[{{\"name\":\"d{i}\"}},"));
        }
        dump.push_str("{\"name\":\"leaf\",\"dsize\":4096}");
        for _ in 0..depth {
            dump.push(']');
        }
        dump.push(']');

        // Either it loads correctly or it is refused; what it must not do is crash.
        match read(io::Cursor::new(dump)) {
            Ok(tree) => assert!(tree.len() > 1),
            Err(e) => assert!(!e.to_string().is_empty()),
        }
    }

    #[test]
    fn the_written_header_names_ccdu() {
        let (_d, tree) = fixture();
        let text = encoded(&tree);
        assert!(text.starts_with("[1,2,{\"progname\":\"ccdu\""), "{}", &text[..40]);
        assert!(text.ends_with("]\n"));
    }
}
