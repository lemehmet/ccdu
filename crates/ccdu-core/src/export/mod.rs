//! Reading and writing scanned trees.
//!
//! Two formats. The native one mirrors the in-memory arena, so writing it is close to a memcpy and
//! reading it back is exact. The ncdu one exists so a scan can cross between the two tools in
//! either direction — scan on a server with whichever is installed, analyse wherever you like.
//!
//! Both readers treat their input as untrusted. A file that claims a node's parent is index four
//! billion gets an error, not a panic somewhere much later.

pub mod native;
pub mod ncdu;

use std::io::{self, BufRead, Read, Write};

use crate::model::Tree;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// ccdu's own format: exact, compact, and fast to load.
    Native,
    /// ncdu's JSON export, as produced by `ncdu -o`.
    NcduJson,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Native => "ccdu",
            Format::NcduJson => "ncdu-json",
        }
    }
}

pub fn write(tree: &Tree, out: impl Write, format: Format) -> io::Result<()> {
    match format {
        Format::Native => native::write(tree, out),
        Format::NcduJson => ncdu::write(tree, out),
    }
}

/// Read a tree, working out which format it is from its first bytes.
///
/// The native format starts with a magic number and ncdu's with `[`, so this needs no filename
/// conventions and works on a pipe.
pub fn read(mut input: impl BufRead) -> io::Result<Tree> {
    let start = input.fill_buf()?;
    if start.starts_with(native::MAGIC) {
        native::read(input)
    } else if start.first().is_some_and(|b| b.is_ascii_whitespace() || *b == b'[') {
        ncdu::read(input)
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidData, "not a ccdu export or an ncdu JSON dump"))
    }
}

/// Read from a path, or from standard input when it is `-`.
pub fn read_path(path: &std::path::Path) -> io::Result<Tree> {
    if path == std::path::Path::new("-") {
        let stdin = io::stdin();
        let locked = stdin.lock();
        return read(io::BufReader::new(locked));
    }
    let file = std::fs::File::open(path)?;
    read(io::BufReader::new(file))
}

/// Write to a path, or to standard output when it is `-`.
pub fn write_path(tree: &Tree, path: &std::path::Path, format: Format) -> io::Result<()> {
    if path == std::path::Path::new("-") {
        let stdout = io::stdout();
        let mut locked = io::BufWriter::new(stdout.lock());
        write(tree, &mut locked, format)?;
        return locked.flush();
    }
    let file = std::fs::File::create(path)?;
    let mut out = io::BufWriter::new(file);
    write(tree, &mut out, format)?;
    out.flush()
}

/// Helpers shared by both readers.
pub(crate) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub(crate) fn read_exact<const N: usize>(input: &mut impl Read) -> io::Result<[u8; N]> {
    let mut buffer = [0u8; N];
    input.read_exact(&mut buffer)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{flags, ROOT};
    use crate::scan::{scan, ScanOptions};
    use std::collections::BTreeMap;
    use std::fs;

    /// A tree with the awkward cases: nesting, a symlink, a hardlink pair, an unreadable entry.
    pub(super) fn fixture() -> (tempfile::TempDir, Tree) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/deep.bin"), vec![1u8; 40_000]).unwrap();
        fs::write(root.join("a/plain.txt"), vec![2u8; 900]).unwrap();
        fs::write(root.join("linked"), vec![3u8; 5_000]).unwrap();
        fs::hard_link(root.join("linked"), root.join("a/linked-too")).unwrap();
        std::os::unix::fs::symlink("plain.txt", root.join("a/pointer")).unwrap();
        fs::create_dir(root.join("empty")).unwrap();

        let tree = scan(root, &ScanOptions::default(), None, None).unwrap();
        (dir, tree)
    }

    /// Relative path -> (apparent, disk, flags), for comparing trees.
    pub(super) fn contents(tree: &Tree) -> BTreeMap<String, (u64, u64, u16)> {
        let mut out = BTreeMap::new();
        let mut stack = vec![ROOT];
        while let Some(id) = stack.pop() {
            for child in tree.children(id) {
                let rel = tree
                    .path_of(child)
                    .strip_prefix(tree.root_path())
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let node = tree.node(child);
                out.insert(rel, (node.apparent, node.disk, node.flags));
                if node.is_dir() {
                    stack.push(child);
                }
            }
        }
        out
    }

    #[test]
    fn the_native_format_is_sniffed_and_round_trips_exactly() {
        let (_d, tree) = fixture();
        let mut buffer = Vec::new();
        write(&tree, &mut buffer, Format::Native).unwrap();

        let back = read(io::Cursor::new(&buffer)).unwrap();
        assert_eq!(contents(&back), contents(&tree));
        assert_eq!(back.root_path(), tree.root_path());
        assert_eq!(back.errors, tree.errors);
        assert_eq!(back.node(ROOT).disk, tree.node(ROOT).disk);
    }

    #[test]
    fn the_ncdu_format_is_sniffed_and_round_trips_what_it_can_carry() {
        let (_d, tree) = fixture();
        let mut buffer = Vec::new();
        write(&tree, &mut buffer, Format::NcduJson).unwrap();
        assert!(buffer.starts_with(b"[1,2,"), "{}", String::from_utf8_lossy(&buffer[..20]));

        let back = read(io::Cursor::new(&buffer)).unwrap();
        let (before, after) = (contents(&tree), contents(&back));
        assert_eq!(before.keys().collect::<Vec<_>>(), after.keys().collect::<Vec<_>>());
        for (path, (apparent, disk, _)) in &before {
            let (got_apparent, got_disk, _) = after[path];
            assert_eq!((got_apparent, got_disk), (*apparent, *disk), "sizes differ for {path}");
        }
    }

    #[test]
    fn rubbish_is_rejected_rather_than_guessed_at() {
        for junk in [&b"hello"[..], &b"\x00\x01\x02"[..], &b""[..]] {
            assert!(read(io::Cursor::new(junk)).is_err(), "accepted {junk:?}");
        }
    }

    #[test]
    fn flags_survive_the_native_format() {
        let (_d, tree) = fixture();
        let mut buffer = Vec::new();
        write(&tree, &mut buffer, Format::Native).unwrap();
        let back = read(io::Cursor::new(&buffer)).unwrap();

        let has = |tree: &Tree, flag: u16| contents(tree).values().any(|(_, _, f)| f & flag != 0);
        assert!(has(&tree, flags::SYMLINK) && has(&back, flags::SYMLINK));
        assert!(has(&tree, flags::HARDLINK_DUP) && has(&back, flags::HARDLINK_DUP));
    }
}
