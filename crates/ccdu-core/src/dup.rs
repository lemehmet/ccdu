//! Finding files with identical contents.
//!
//! Three stages, each cheaper than the next and each shrinking the input to the one after:
//!
//! 1. **Group by size.** Files of different lengths cannot be identical, and this costs nothing —
//!    the scanner already knows every size. On a real tree it discards the overwhelming majority.
//! 2. **Sample the ends.** Hash the first and last few kilobytes. Files that differ usually differ
//!    near the start, and this reads a fixed amount however large they are.
//! 3. **Hash in full.** Only what survived, and only the parts that were not already read whole.
//!
//! Hardlinks are excluded rather than reported: two names for one inode are not two copies, and
//! deleting one reclaims nothing.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crossbeam_channel::Sender;

use crate::model::{flags, NodeId, Tree, ROOT};

/// Bytes read from each end during the sampling stage.
const SAMPLE: u64 = 4096;

#[derive(Clone, Debug)]
pub struct DupOptions {
    /// Files smaller than this are ignored. Duplicated small files are common, numerous, and not
    /// worth the reading it takes to prove it.
    pub min_size: u64,
    pub threads: usize,
}

impl Default for DupOptions {
    fn default() -> Self {
        let threads =
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
        DupOptions { min_size: 4096, threads }
    }
}

/// A set of files with identical contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DupGroup {
    /// Every copy, largest disk usage first, so the one to keep is the one listed first.
    pub nodes: Vec<NodeId>,
    /// Apparent size of each copy; they are all the same.
    pub size: u64,
    /// Disk bytes reclaimable by keeping the first copy and removing the rest.
    ///
    /// A member that has further hardlinks elsewhere in the filesystem contributes nothing:
    /// removing one of its names frees no space until the last one goes, and a total that
    /// pretended otherwise would promise space that never arrives.
    pub wasted: u64,
}

#[derive(Clone, Debug, Default)]
pub struct DupProgress {
    /// Files that still might be duplicates.
    pub candidates: usize,
    pub hashed: usize,
    pub bytes_read: u64,
}

/// Find groups of identical files under `tree`, largest waste first.
pub fn find_duplicates(
    tree: &Tree,
    opts: &DupOptions,
    progress: Option<&Sender<DupProgress>>,
    cancel: Option<&AtomicBool>,
) -> Vec<DupGroup> {
    let by_size = candidates_by_size(tree, opts);
    let candidates: Vec<NodeId> = by_size.values().flatten().copied().collect();
    report(progress, DupProgress { candidates: candidates.len(), ..Default::default() });

    if candidates.is_empty() {
        return Vec::new();
    }

    // Stage two: the ends only.
    let sampled = hash_files(tree, &candidates, Depth::Ends, opts, progress, cancel);
    let survivors = regroup(&sampled);
    if survivors.is_empty() {
        return Vec::new();
    }

    // Stage three: everything, but only for files the sample did not already read whole.
    let needs_full: Vec<NodeId> = survivors
        .iter()
        .flatten()
        .copied()
        .filter(|&id| tree.node(id).apparent > SAMPLE * 2)
        .collect();
    let full = hash_files(tree, &needs_full, Depth::Whole, opts, progress, cancel);

    let mut digests: HashMap<NodeId, [u8; 32]> = sampled.into_iter().collect();
    digests.extend(full);

    let confirmed: Vec<(NodeId, [u8; 32])> =
        survivors.iter().flatten().filter_map(|&id| digests.get(&id).map(|d| (id, *d))).collect();

    let mut groups: Vec<DupGroup> = regroup(&confirmed)
        .into_iter()
        .map(|mut nodes| {
            // Largest first: the copy that is kept should be the one that reclaims least by going.
            nodes.sort_by_key(|&id| std::cmp::Reverse(tree.node(id).disk));
            let wasted = nodes
                .iter()
                .skip(1)
                .filter(|&&id| !tree.node(id).has(flags::HARDLINK))
                .map(|&id| tree.node(id).disk)
                .sum();
            DupGroup { size: tree.node(nodes[0]).apparent, wasted, nodes }
        })
        .collect();

    groups.sort_by_key(|g| std::cmp::Reverse(g.wasted));
    groups
}

/// Total reclaimable across every group.
pub fn total_wasted(groups: &[DupGroup]) -> u64 {
    groups.iter().map(|g| g.wasted).sum()
}

/// True when removing this name would not free its blocks, because other names still point at the
/// same inode. Frontends should say so rather than offer space that will not appear.
pub fn shares_inode(tree: &Tree, id: NodeId) -> bool {
    tree.node(id).has(flags::HARDLINK) || tree.node(id).has(flags::HARDLINK_DUP)
}

/// Regular files big enough to matter, bucketed by size, singletons dropped.
fn candidates_by_size(tree: &Tree, opts: &DupOptions) -> HashMap<u64, Vec<NodeId>> {
    const SKIP: u16 = flags::DIR
        | flags::SYMLINK
        | flags::OTHER
        | flags::ERR
        | flags::EXCLUDED
        // A second name for an inode we already have is not a second copy of the data.
        | flags::HARDLINK_DUP;

    let mut by_size: HashMap<u64, Vec<NodeId>> = HashMap::new();
    let mut stack = vec![ROOT];
    while let Some(id) = stack.pop() {
        for child in tree.children(id) {
            let node = tree.node(child);
            if node.is_dir() {
                stack.push(child);
                continue;
            }
            if node.flags & SKIP == 0 && node.apparent >= opts.min_size {
                by_size.entry(node.apparent).or_default().push(child);
            }
        }
    }
    by_size.retain(|_, group| group.len() > 1);
    by_size
}

/// Collect ids that share a digest, dropping anything left alone.
fn regroup(digests: &[(NodeId, [u8; 32])]) -> Vec<Vec<NodeId>> {
    let mut by_digest: HashMap<[u8; 32], Vec<NodeId>> = HashMap::new();
    for (id, digest) in digests {
        by_digest.entry(*digest).or_default().push(*id);
    }
    by_digest.into_values().filter(|group| group.len() > 1).collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// The first and last [`SAMPLE`] bytes.
    Ends,
    Whole,
}

/// Hash a set of files across several threads.
fn hash_files(
    tree: &Tree,
    ids: &[NodeId],
    depth: Depth,
    opts: &DupOptions,
    progress: Option<&Sender<DupProgress>>,
    cancel: Option<&AtomicBool>,
) -> Vec<(NodeId, [u8; 32])> {
    if ids.is_empty() {
        return Vec::new();
    }

    let next = AtomicUsize::new(0);
    let hashed = AtomicUsize::new(0);
    let bytes = AtomicUsize::new(0);
    let threads = opts.threads.max(1).min(ids.len());
    let mut out = Vec::new();

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..threads {
            handles.push(scope.spawn(|| {
                let mut mine = Vec::new();
                loop {
                    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&id) = ids.get(i) else { break };

                    let path = tree.path_of(id);
                    // A file we cannot read is one we cannot prove is a duplicate, so it is left
                    // out rather than guessed at.
                    if let Ok((digest, read)) = digest_of(&path, tree.node(id).apparent, depth) {
                        mine.push((id, digest));
                        bytes.fetch_add(read as usize, Ordering::Relaxed);
                    }
                    let done = hashed.fetch_add(1, Ordering::Relaxed) + 1;
                    if done.is_multiple_of(64) {
                        report(
                            progress,
                            DupProgress {
                                candidates: ids.len(),
                                hashed: done,
                                bytes_read: bytes.load(Ordering::Relaxed) as u64,
                            },
                        );
                    }
                }
                mine
            }));
        }
        for handle in handles {
            out.extend(handle.join().unwrap_or_default());
        }
    });

    out
}

/// Digest a file, reading either both ends or all of it. Returns the digest and how much was read.
fn digest_of(path: &PathBuf, size: u64, depth: Depth) -> std::io::Result<([u8; 32], u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; SAMPLE as usize];

    if depth == Depth::Whole || size <= SAMPLE * 2 {
        let mut read = 0u64;
        let mut chunk = vec![0u8; 1 << 20];
        loop {
            let got = file.read(&mut chunk)?;
            if got == 0 {
                break;
            }
            hasher.update(&chunk[..got]);
            read += got as u64;
        }
        return Ok((*hasher.finalize().as_bytes(), read));
    }

    let head = file.read(&mut buffer)?;
    hasher.update(&buffer[..head]);
    file.seek(SeekFrom::End(-(SAMPLE as i64)))?;
    let tail = file.read(&mut buffer)?;
    hasher.update(&buffer[..tail]);
    // Length is part of the identity: without it, two files whose ends agree but whose middles
    // differ in length would collide at this stage. They are the same size by construction here,
    // but mixing the size in keeps that true if the bucketing ever changes.
    hasher.update(&size.to_le_bytes());
    Ok((*hasher.finalize().as_bytes(), (head + tail) as u64))
}

fn report(progress: Option<&Sender<DupProgress>>, update: DupProgress) {
    if let Some(tx) = progress {
        tx.send(update).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan, ScanOptions};
    use std::fs;
    use std::path::Path;

    fn scanned(root: &Path) -> Tree {
        scan(root, &ScanOptions::default(), None, None).unwrap()
    }

    fn names(tree: &Tree, group: &DupGroup) -> Vec<String> {
        let mut out: Vec<String> = group
            .nodes
            .iter()
            .map(|&id| {
                tree.path_of(id)
                    .strip_prefix(tree.root_path())
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        out.sort();
        out
    }

    fn opts() -> DupOptions {
        DupOptions { min_size: 1, threads: 2 }
    }

    #[test]
    fn finds_identical_files_and_ignores_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("a.bin"), vec![1u8; 20_000]).unwrap();
        fs::write(root.join("sub/b.bin"), vec![1u8; 20_000]).unwrap();
        fs::write(root.join("different.bin"), vec![2u8; 20_000]).unwrap();
        fs::write(root.join("shorter.bin"), vec![1u8; 19_000]).unwrap();

        let tree = scanned(root);
        let groups = find_duplicates(&tree, &opts(), None, None);

        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(names(&tree, &groups[0]), ["a.bin", "sub/b.bin"]);
        assert_eq!(groups[0].size, 20_000);
        assert!(groups[0].wasted >= 20_000);
    }

    #[test]
    fn files_that_differ_only_in_the_middle_are_not_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Identical first and last 4 KiB, different in between: exactly what the sampling stage
        // cannot see, so this is what proves the full pass runs.
        let mut a = vec![7u8; 40_000];
        let mut b = vec![7u8; 40_000];
        a[20_000] = 1;
        b[20_000] = 2;
        fs::write(root.join("a.bin"), &a).unwrap();
        fs::write(root.join("b.bin"), &b).unwrap();

        let tree = scanned(root);
        assert!(find_duplicates(&tree, &opts(), None, None).is_empty());
    }

    #[test]
    fn hardlinks_are_not_reported_as_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("original"), vec![3u8; 30_000]).unwrap();
        fs::hard_link(root.join("original"), root.join("second-name")).unwrap();

        let tree = scanned(root);
        // Two names, one inode, nothing to reclaim by deleting either.
        assert!(find_duplicates(&tree, &opts(), None, None).is_empty());
    }

    #[test]
    fn a_hardlink_pair_still_counts_once_against_a_real_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("original"), vec![3u8; 30_000]).unwrap();
        fs::hard_link(root.join("original"), root.join("second-name")).unwrap();
        fs::write(root.join("real-copy"), vec![3u8; 30_000]).unwrap();

        let tree = scanned(root);
        let groups = find_duplicates(&tree, &opts(), None, None);

        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].nodes.len(), 2, "the hardlink should not appear as a third copy");

        // Which of the two names represents the inode depends on the order the scan met them.
        let found = names(&tree, &groups[0]);
        assert!(found.contains(&"real-copy".to_string()), "{found:?}");
        assert!(
            found.contains(&"original".to_string()) || found.contains(&"second-name".to_string()),
            "{found:?}"
        );

        // Removing the hardlinked name frees nothing while its twin still points at the inode,
        // so only the standalone copy counts towards the reclaimable total.
        let linked = groups[0].nodes.iter().filter(|&&id| shares_inode(&tree, id)).count();
        assert_eq!(linked, 1);
        assert!(
            groups[0].wasted <= tree.node(groups[0].nodes[0]).disk,
            "waste {} counts space that removing one name would not free",
            groups[0].wasted
        );
    }

    #[test]
    fn small_files_are_skipped_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), b"tiny").unwrap();
        fs::write(root.join("b.txt"), b"tiny").unwrap();

        let tree = scanned(root);
        assert!(find_duplicates(&tree, &DupOptions::default(), None, None).is_empty());
        assert_eq!(find_duplicates(&tree, &opts(), None, None).len(), 1);
    }

    #[test]
    fn symlinks_and_directories_are_never_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("target"), vec![1u8; 10_000]).unwrap();
        std::os::unix::fs::symlink(root.join("target"), root.join("link-a")).unwrap();
        std::os::unix::fs::symlink(root.join("target"), root.join("link-b")).unwrap();
        fs::create_dir(root.join("dir-a")).unwrap();
        fs::create_dir(root.join("dir-b")).unwrap();

        let tree = scanned(root);
        assert!(find_duplicates(&tree, &opts(), None, None).is_empty());
    }

    #[test]
    fn three_copies_report_two_copies_worth_of_waste() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in ["one", "two", "three"] {
            fs::write(root.join(name), vec![5u8; 8192]).unwrap();
        }

        let tree = scanned(root);
        let groups = find_duplicates(&tree, &opts(), None, None);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].nodes.len(), 3);
        let each = tree.node(groups[0].nodes[0]).disk;
        assert_eq!(groups[0].wasted, each * 2, "keeping one of three should free the other two");
        assert_eq!(total_wasted(&groups), each * 2);
    }

    #[test]
    fn groups_come_back_worst_waste_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("small-a"), vec![1u8; 8_000]).unwrap();
        fs::write(root.join("small-b"), vec![1u8; 8_000]).unwrap();
        fs::write(root.join("big-a"), vec![2u8; 200_000]).unwrap();
        fs::write(root.join("big-b"), vec![2u8; 200_000]).unwrap();

        let tree = scanned(root);
        let groups = find_duplicates(&tree, &opts(), None, None);

        assert_eq!(groups.len(), 2);
        assert!(groups[0].wasted > groups[1].wasted);
        assert_eq!(groups[0].size, 200_000);
    }

    #[test]
    fn cancelling_stops_early_without_inventing_groups() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..50 {
            fs::write(root.join(format!("f{i:02}")), vec![9u8; 20_000]).unwrap();
        }

        let tree = scanned(root);
        let cancel = AtomicBool::new(true);
        let groups = find_duplicates(&tree, &opts(), None, Some(&cancel));

        // Whatever it managed, every group it reports must be a real group.
        for group in &groups {
            assert!(group.nodes.len() > 1);
        }
    }

    #[test]
    fn progress_is_reported_while_hashing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..200 {
            fs::write(root.join(format!("f{i:03}")), vec![4u8; 20_000]).unwrap();
        }

        let tree = scanned(root);
        let (tx, rx) = crossbeam_channel::unbounded();
        find_duplicates(&tree, &opts(), Some(&tx), None);
        drop(tx);

        let updates: Vec<DupProgress> = rx.into_iter().collect();
        assert!(updates.len() > 1, "expected progress while hashing 200 files");
        assert!(updates.iter().any(|u| u.hashed > 0));
    }

    #[test]
    fn an_unreadable_file_is_left_out_rather_than_grouped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.bin"), vec![6u8; 20_000]).unwrap();
        fs::write(root.join("b.bin"), vec![6u8; 20_000]).unwrap();
        fs::write(root.join("locked.bin"), vec![6u8; 20_000]).unwrap();

        let tree = scanned(root);
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("locked.bin"), fs::Permissions::from_mode(0o000)).unwrap();

        let groups = find_duplicates(&tree, &opts(), None, None);
        fs::set_permissions(root.join("locked.bin"), fs::Permissions::from_mode(0o644)).unwrap();

        // Running as root can read it anyway; assert only what holds either way.
        assert_eq!(groups.len(), 1);
        assert!(groups[0].nodes.len() >= 2);
        assert!(names(&tree, &groups[0]).contains(&"a.bin".to_string()));
    }
}
