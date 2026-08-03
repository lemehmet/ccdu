//! Move tests.
//!
//! Cross-filesystem behaviour needs a second filesystem, which needs privileges this test suite
//! does not assume it has. Where one is available (`CCDU_TEST_OTHER_FS` pointing at a directory on
//! another mount) the cross-device path is exercised for real; otherwise those tests report that
//! they were skipped rather than passing vacuously. Everything that does not need two filesystems
//! — identity checks, conflicts, symlinks, resume bookkeeping — runs unconditionally.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;
use crate::plan::{Conflict, Ident, Op, Plan};

fn move_op(src: &Path, dst: &Path) -> Op {
    Op::Move {
        src: src.to_path_buf(),
        dst: dst.to_path_buf(),
        ident: Ident::of(src).unwrap(),
        est_bytes: 0,
        on_conflict: Conflict::Fail,
    }
}

fn plan_with(root: &Path, ops: Vec<Op>) -> Plan {
    let mut plan = Plan::new(root.to_path_buf());
    plan.ops = ops;
    plan
}

fn run(plan: &Plan, dir: &Path) -> io::Result<Outcome> {
    execute(plan, dir, &ExecOptions::default(), &Control::new(), None)
}

/// Relative path -> (apparent size, mode, is_symlink), for comparing two trees.
///
/// Directory sizes are deliberately excluded: a directory's `st_size` is a property of the
/// filesystem that holds it — 4096 on ext4, a few dozen bytes on tmpfs — so comparing it would
/// make every cross-filesystem move look like a failure.
fn shape(root: &Path) -> BTreeMap<String, (u64, u32, bool)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let size = if meta.is_dir() { 0 } else { meta.len() };
            out.insert(
                rel,
                (size, meta.permissions().mode() & 0o7777, meta.file_type().is_symlink()),
            );
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    out
}

/// A tree covering the cases a copy has to get right.
fn source_tree(root: &Path) {
    fs::create_dir_all(root.join("tree/nested")).unwrap();
    fs::write(root.join("tree/plain.bin"), vec![7u8; 40_000]).unwrap();
    fs::write(root.join("tree/nested/deep.bin"), vec![9u8; 3_000]).unwrap();
    fs::set_permissions(root.join("tree/plain.bin"), fs::Permissions::from_mode(0o640)).unwrap();

    // Two names, one inode: must arrive as two names and one inode.
    fs::write(root.join("tree/linked"), vec![1u8; 5_000]).unwrap();
    fs::hard_link(root.join("tree/linked"), root.join("tree/nested/linked-too")).unwrap();

    std::os::unix::fs::symlink("plain.bin", root.join("tree/pointer")).unwrap();
    fs::create_dir(root.join("tree/empty")).unwrap();
}

/// A directory on a different filesystem, if the environment provides one.
fn other_filesystem() -> Option<PathBuf> {
    let base = PathBuf::from(std::env::var_os("CCDU_TEST_OTHER_FS")?);
    if !base.is_dir() {
        return None;
    }
    // Give each test its own subdirectory so they can run in parallel.
    let unique =
        base.join(format!("ccdu-test-{}-{:?}", std::process::id(), std::thread::current().id()));
    fs::create_dir_all(&unique).ok()?;
    Some(unique)
}

fn devices_differ(a: &Path, b: &Path) -> bool {
    Ident::of(a).unwrap().dev != Ident::of(b).unwrap().dev
}

#[test]
fn a_move_within_one_filesystem_is_a_rename() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    source_tree(root);
    fs::create_dir(root.join("dest")).unwrap();
    let journal = tempfile::tempdir().unwrap();

    let before = shape(&root.join("tree"));
    let inode = Ident::of(&root.join("tree")).unwrap().ino;

    let plan = plan_with(root, vec![move_op(&root.join("tree"), &root.join("dest/tree"))]);
    let outcome = run(&plan, journal.path()).unwrap();

    assert_eq!(outcome.done, 1, "{outcome:?}");
    assert_eq!(outcome.freed, 0, "a rename reclaims nothing: the data never moved");
    assert!(!root.join("tree").exists());
    assert_eq!(shape(&root.join("dest/tree")), before);
    assert_eq!(
        Ident::of(&root.join("dest/tree")).unwrap().ino,
        inode,
        "a rename should not have copied anything"
    );
}

#[test]
fn moving_a_single_file_preserves_its_contents_and_mode() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("file.bin"), vec![3u8; 12_345]).unwrap();
    fs::set_permissions(root.join("file.bin"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::create_dir(root.join("dest")).unwrap();
    let journal = tempfile::tempdir().unwrap();

    let plan = plan_with(root, vec![move_op(&root.join("file.bin"), &root.join("dest/file.bin"))]);
    assert_eq!(run(&plan, journal.path()).unwrap().done, 1);

    let moved = root.join("dest/file.bin");
    assert_eq!(fs::read(&moved).unwrap(), vec![3u8; 12_345]);
    assert_eq!(fs::metadata(&moved).unwrap().permissions().mode() & 0o7777, 0o600);
    assert!(!root.join("file.bin").exists());
}

#[test]
fn a_source_that_changed_since_staging_is_not_moved() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("file.bin"), vec![1u8; 100]).unwrap();
    fs::create_dir(root.join("dest")).unwrap();
    let journal = tempfile::tempdir().unwrap();

    let plan = plan_with(root, vec![move_op(&root.join("file.bin"), &root.join("dest/file.bin"))]);

    fs::remove_file(root.join("file.bin")).unwrap();
    fs::write(root.join("file.bin"), vec![2u8; 200]).unwrap();

    let outcome = run(&plan, journal.path()).unwrap();
    assert_eq!(outcome.failed, 1);
    assert!(root.join("file.bin").exists(), "a file we did not review was moved");
    assert!(!root.join("dest/file.bin").exists());
}

#[test]
fn an_occupied_destination_stops_the_move() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("file.bin"), vec![1u8; 100]).unwrap();
    fs::create_dir(root.join("dest")).unwrap();
    fs::write(root.join("dest/file.bin"), b"someone else's").unwrap();
    let journal = tempfile::tempdir().unwrap();

    let plan = plan_with(root, vec![move_op(&root.join("file.bin"), &root.join("dest/file.bin"))]);
    let outcome = run(&plan, journal.path()).unwrap();

    assert_eq!(outcome.failed, 1);
    assert_eq!(fs::read(root.join("dest/file.bin")).unwrap(), b"someone else's");
    assert!(root.join("file.bin").exists());
}

#[test]
fn skip_on_conflict_leaves_both_sides_alone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("file.bin"), vec![1u8; 100]).unwrap();
    fs::create_dir(root.join("dest")).unwrap();
    fs::write(root.join("dest/file.bin"), b"already here").unwrap();
    let journal = tempfile::tempdir().unwrap();

    let mut plan =
        plan_with(root, vec![move_op(&root.join("file.bin"), &root.join("dest/file.bin"))]);
    let Op::Move { on_conflict, .. } = &mut plan.ops[0] else { unreachable!() };
    *on_conflict = Conflict::Skip;

    let outcome = run(&plan, journal.path()).unwrap();
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    assert!(root.join("file.bin").exists(), "skipping must not remove the source");
    assert_eq!(fs::read(root.join("dest/file.bin")).unwrap(), b"already here");
}

#[test]
fn moves_run_before_deletions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("keep.bin"), vec![1u8; 100]).unwrap();
    fs::write(root.join("junk.bin"), vec![2u8; 100]).unwrap();
    fs::create_dir(root.join("dest")).unwrap();
    let journal = tempfile::tempdir().unwrap();

    let plan = plan_with(
        root,
        vec![
            Op::Delete {
                path: root.join("junk.bin"),
                ident: Ident::of(&root.join("junk.bin")).unwrap(),
                est_bytes: 0,
            },
            move_op(&root.join("keep.bin"), &root.join("dest/keep.bin")),
        ],
    );

    run(&plan, journal.path()).unwrap();

    // Order matters if a run stops half way: what survives should be everything meant to be kept.
    let ops: Vec<usize> = journal::read_dir(journal.path())
        .unwrap()
        .iter()
        .filter_map(|r| match r.event {
            Event::OpBegin { op } => Some(op),
            _ => None,
        })
        .collect();
    assert_eq!(ops, [1, 0], "the move should have been attempted first");
}

#[test]
fn a_dry_run_moves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    source_tree(root);
    fs::create_dir(root.join("dest")).unwrap();
    let journal = tempfile::tempdir().unwrap();
    let before = shape(root);

    let mut plan = plan_with(root, vec![move_op(&root.join("tree"), &root.join("dest/tree"))]);
    let Op::Move { est_bytes, .. } = &mut plan.ops[0] else { unreachable!() };
    *est_bytes = 4242;

    let opts = ExecOptions { dry_run: true, ..Default::default() };
    let outcome = execute(&plan, journal.path(), &opts, &Control::new(), None).unwrap();

    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.freed, 4242);
    assert_eq!(shape(root), before, "a dry run moved something");
}

#[test]
fn a_special_file_is_refused_rather_than_silently_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("tree")).unwrap();
    fs::write(root.join("tree/ordinary"), vec![1u8; 10]).unwrap();

    // A socket cannot be recreated by copying. Moving the tree would otherwise delete the
    // original after quietly leaving it behind.
    let socket = root.join("tree/sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket);
    if listener.is_err() {
        eprintln!("skipped: cannot create a unix socket here");
        return;
    }

    let Some(other) = other_filesystem() else {
        eprintln!("skipped: set CCDU_TEST_OTHER_FS to a directory on another filesystem");
        return;
    };
    if !devices_differ(root, &other) {
        eprintln!("skipped: CCDU_TEST_OTHER_FS is on the same filesystem");
        return;
    }

    let journal = tempfile::tempdir().unwrap();
    let plan = plan_with(root, vec![move_op(&root.join("tree"), &other.join("tree"))]);
    let outcome = run(&plan, journal.path()).unwrap();

    assert_eq!(outcome.failed, 1, "{outcome:?}");
    assert!(root.join("tree/ordinary").exists(), "the source was removed despite the failure");
    fs::remove_dir_all(&other).ok();
}

// ---------------------------------------------------------------------------
// Cross-filesystem: these need a second filesystem to mean anything.
// ---------------------------------------------------------------------------

#[test]
fn a_cross_filesystem_move_copies_verifies_then_reclaims() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    source_tree(root);

    let Some(other) = other_filesystem() else {
        eprintln!("skipped: set CCDU_TEST_OTHER_FS to a directory on another filesystem");
        return;
    };
    if !devices_differ(root, &other) {
        eprintln!("skipped: CCDU_TEST_OTHER_FS is on the same filesystem");
        return;
    }

    let before = shape(&root.join("tree"));
    let journal = tempfile::tempdir().unwrap();
    let plan = plan_with(root, vec![move_op(&root.join("tree"), &other.join("tree"))]);

    let outcome = run(&plan, journal.path()).unwrap();

    assert_eq!(outcome.failed, 0, "{outcome:?}");
    assert!(!root.join("tree").exists(), "the source survived a completed move");
    assert_eq!(shape(&other.join("tree")), before, "the copy is not the same tree");
    assert!(outcome.freed > 40_000, "freed {} does not account for the source", outcome.freed);

    // Contents, not just shapes.
    assert_eq!(fs::read(other.join("tree/plain.bin")).unwrap(), vec![7u8; 40_000]);
    assert_eq!(
        fs::read_link(other.join("tree/pointer")).unwrap(),
        PathBuf::from("plain.bin"),
        "the symlink was resolved instead of recreated"
    );

    // One inode, two names — a copy that duplicated it would double the space used.
    let a = Ident::of(&other.join("tree/linked")).unwrap();
    let b = Ident::of(&other.join("tree/nested/linked-too")).unwrap();
    assert_eq!(a.ino, b.ino, "hardlinked files arrived as two separate copies");

    // No temporary left behind.
    let leftovers: Vec<String> = fs::read_dir(&other)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".ccdu-part"))
        .collect();
    assert!(leftovers.is_empty(), "left a temporary behind: {leftovers:?}");

    fs::remove_dir_all(&other).ok();
}

#[test]
fn a_sparse_file_stays_sparse_across_filesystems() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // 64 MiB of hole with a little data at each end.
    let path = root.join("sparse.bin");
    let file = fs::File::create(&path).unwrap();
    use std::io::{Seek, Write};
    let mut file = file;
    file.write_all(b"start").unwrap();
    file.seek(io::SeekFrom::Start(64 * 1024 * 1024)).unwrap();
    file.write_all(b"end").unwrap();
    file.sync_all().unwrap();
    drop(file);

    let apparent = fs::metadata(&path).unwrap().len();
    let occupied = Ident::of(&path).unwrap();
    let _ = occupied;

    let Some(other) = other_filesystem() else {
        eprintln!("skipped: set CCDU_TEST_OTHER_FS to a directory on another filesystem");
        return;
    };
    if !devices_differ(root, &other) {
        eprintln!("skipped: CCDU_TEST_OTHER_FS is on the same filesystem");
        return;
    }

    let journal = tempfile::tempdir().unwrap();
    let plan = plan_with(root, vec![move_op(&path, &other.join("sparse.bin"))]);
    let outcome = run(&plan, journal.path()).unwrap();
    assert_eq!(outcome.failed, 0, "{outcome:?}");

    let moved = other.join("sparse.bin");
    assert_eq!(fs::metadata(&moved).unwrap().len(), apparent, "length changed");

    // The point of the exercise: the copy must not have written 64 MiB of zeroes.
    let blocks = std::os::unix::fs::MetadataExt::blocks(&fs::metadata(&moved).unwrap()) * 512;
    assert!(
        blocks < apparent / 2,
        "the copy occupies {blocks} bytes for a {apparent}-byte sparse file"
    );

    fs::remove_dir_all(&other).ok();
}

#[test]
fn an_interrupted_cross_filesystem_move_resumes_without_losing_the_source() {
    let points = [
        FaultPoint::AfterOpBegin,
        FaultPoint::MidCopy,
        FaultPoint::BeforeSourceRemoval,
        FaultPoint::BeforeOpDone,
    ];

    let Some(base) = other_filesystem() else {
        eprintln!("skipped: set CCDU_TEST_OTHER_FS to a directory on another filesystem");
        return;
    };

    for (i, point) in points.into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        source_tree(root);
        if !devices_differ(root, &base) {
            eprintln!("skipped: CCDU_TEST_OTHER_FS is on the same filesystem");
            return;
        }

        let other = base.join(format!("run{i}"));
        fs::create_dir_all(&other).unwrap();

        let expected = shape(&root.join("tree"));
        let journal = tempfile::tempdir().unwrap();
        let plan = plan_with(root, vec![move_op(&root.join("tree"), &other.join("tree"))]);

        let hook = move |p: FaultPoint, _: usize| -> io::Result<()> {
            if p == point {
                Err(io::Error::other("simulated crash"))
            } else {
                Ok(())
            }
        };
        let opts = ExecOptions { fault: Some(&hook), ..Default::default() };
        let crashed = execute(&plan, journal.path(), &opts, &Control::new(), None).is_err();

        if crashed {
            // Until the copy is published and the source reclaimed, the data must still exist
            // somewhere complete — and that somewhere is the source.
            assert!(
                root.join("tree/plain.bin").exists() || other.join("tree/plain.bin").exists(),
                "after a crash at {point:?} the data is nowhere"
            );
        }

        let resumed = run(&plan, journal.path()).unwrap();
        assert_eq!(resumed.failed, 0, "resuming after {point:?} failed: {resumed:?}");
        assert_eq!(
            shape(&other.join("tree")),
            expected,
            "resuming after {point:?} produced a different tree"
        );
        assert!(!root.join("tree").exists(), "after {point:?} the source was left behind");
        assert_eq!(fs::read(other.join("tree/plain.bin")).unwrap(), vec![7u8; 40_000]);

        fs::remove_dir_all(&other).ok();
    }
    fs::remove_dir_all(&base).ok();
}

#[test]
fn a_torn_copy_is_redone_rather_than_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("tree")).unwrap();
    fs::write(root.join("tree/big.bin"), vec![5u8; 100_000]).unwrap();

    let Some(other) = other_filesystem() else {
        eprintln!("skipped: set CCDU_TEST_OTHER_FS to a directory on another filesystem");
        return;
    };
    if !devices_differ(root, &other) {
        eprintln!("skipped: CCDU_TEST_OTHER_FS is on the same filesystem");
        return;
    }

    let journal = tempfile::tempdir().unwrap();
    let plan = plan_with(root, vec![move_op(&root.join("tree"), &other.join("tree"))]);

    // Crash after the first file, then truncate what landed, the way a torn write would.
    let hook = |p: FaultPoint, _: usize| -> io::Result<()> {
        if p == FaultPoint::MidCopy {
            Err(io::Error::other("simulated crash"))
        } else {
            Ok(())
        }
    };
    let opts = ExecOptions { fault: Some(&hook), ..Default::default() };
    execute(&plan, journal.path(), &opts, &Control::new(), None).ok();

    let partial = other.join(".ccdu-part-0-tree/big.bin");
    if partial.exists() {
        let handle = fs::OpenOptions::new().write(true).open(&partial).unwrap();
        handle.set_len(10_000).unwrap();
    }

    let resumed = run(&plan, journal.path()).unwrap();
    assert_eq!(resumed.failed, 0, "{resumed:?}");
    assert_eq!(
        fs::read(other.join("tree/big.bin")).unwrap(),
        vec![5u8; 100_000],
        "a short file was accepted as complete"
    );

    fs::remove_dir_all(&other).ok();
}

#[test]
fn hash_verification_rejects_a_corrupted_copy() {
    // Without a second filesystem there is no copy to corrupt.
    let Some(other) = other_filesystem() else {
        eprintln!("skipped: set CCDU_TEST_OTHER_FS to a directory on another filesystem");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("file.bin"), vec![4u8; 50_000]).unwrap();
    if !devices_differ(root, &other) {
        eprintln!("skipped: CCDU_TEST_OTHER_FS is on the same filesystem");
        return;
    }

    let journal = tempfile::tempdir().unwrap();
    let plan = plan_with(root, vec![move_op(&root.join("file.bin"), &other.join("file.bin"))]);

    let opts = ExecOptions { verify: Verify::Hash, ..Default::default() };
    let outcome = execute(&plan, journal.path(), &opts, &Control::new(), None).unwrap();

    assert_eq!(outcome.failed, 0, "a good copy failed hash verification: {outcome:?}");
    assert_eq!(fs::read(other.join("file.bin")).unwrap(), vec![4u8; 50_000]);

    fs::remove_dir_all(&other).ok();
}
