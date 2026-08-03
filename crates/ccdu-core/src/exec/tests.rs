//! Executor tests.
//!
//! The centrepiece is [`resuming_after_a_fault_anywhere_matches_a_clean_run`]: it interrupts a
//! commit at every journal boundary of every operation, resumes, and demands the result be
//! identical to a run that was never interrupted. Everything ccdu claims about surviving a crash
//! rests on that property, so it is tested exhaustively rather than by example.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::plan::{Conflict, EntryKind, Ident, Op, Plan};

/// A tree with a bit of everything: nested directories, several files, an empty directory, and a
/// symlink pointing outside the part being deleted.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("logs/2026/01")).unwrap();
    fs::write(root.join("logs/2026/01/a.log"), vec![0u8; 40_000]).unwrap();
    fs::write(root.join("logs/2026/01/b.log"), vec![0u8; 8_000]).unwrap();
    fs::write(root.join("logs/2026/index"), vec![0u8; 1_000]).unwrap();
    fs::create_dir(root.join("logs/empty")).unwrap();

    fs::create_dir(root.join("cache")).unwrap();
    fs::write(root.join("cache/blob"), vec![0u8; 20_000]).unwrap();

    fs::write(root.join("keep.txt"), vec![0u8; 500]).unwrap();
    fs::write(root.join("lonely.tmp"), vec![0u8; 700]).unwrap();
    std::os::unix::fs::symlink(root.join("keep.txt"), root.join("cache/link")).unwrap();

    dir
}

fn delete_op(path: &Path) -> Op {
    Op::Delete { path: path.to_path_buf(), ident: Ident::of(path).unwrap(), est_bytes: 0 }
}

/// A plan deleting `logs/`, `cache/` and a stray file.
fn plan_for(root: &Path) -> Plan {
    let mut plan = Plan::new(root.to_path_buf());
    plan.ops = vec![
        delete_op(&root.join("logs")),
        delete_op(&root.join("lonely.tmp")),
        delete_op(&root.join("cache")),
    ];
    plan
}

/// Everything still under `root`, as relative path -> apparent size.
fn snapshot(root: &Path) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            out.insert(rel, meta.len());
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    out
}

fn run(plan: &Plan, dir: &Path) -> io::Result<Outcome> {
    execute(plan, dir, &ExecOptions::default(), &Control::new(), None)
}

#[test]
fn deletes_files_directories_and_symlinks() {
    let fixture = fixture();
    let root = fixture.path();
    let journal_dir = tempfile::tempdir().unwrap();
    let plan = plan_for(root);

    let outcome = run(&plan, journal_dir.path()).unwrap();

    assert_eq!(outcome.done, 3);
    assert_eq!(outcome.failed, 0);
    assert!(outcome.is_complete());
    assert!(outcome.freed >= 69_000, "freed {} looks too low", outcome.freed);

    assert_eq!(snapshot(root).keys().collect::<Vec<_>>(), ["keep.txt"]);
    // The symlink inside cache/ was removed; what it pointed at was not.
    assert!(root.join("keep.txt").exists());
}

#[test]
fn resuming_after_a_fault_anywhere_matches_a_clean_run() {
    let points = [
        FaultPoint::BeforeOpBegin,
        FaultPoint::AfterOpBegin,
        FaultPoint::MidDelete,
        FaultPoint::BeforeOpDone,
        FaultPoint::AfterOpDone,
    ];

    // What a run that is never interrupted leaves behind.
    let clean_fixture = fixture();
    let clean_journal = tempfile::tempdir().unwrap();
    let clean_plan = plan_for(clean_fixture.path());
    let clean_outcome = run(&clean_plan, clean_journal.path()).unwrap();
    let expected = snapshot(clean_fixture.path());

    for point in points {
        for op_index in 0..3 {
            let fixture = fixture();
            let root = fixture.path();
            let journal_dir = tempfile::tempdir().unwrap();
            let plan = plan_for(root);

            let hook = move |p: FaultPoint, op: usize| -> io::Result<()> {
                if p == point && op == op_index {
                    Err(io::Error::other("simulated crash"))
                } else {
                    Ok(())
                }
            };
            let opts = ExecOptions { fault: Some(&hook), ..Default::default() };
            let interrupted = execute(&plan, journal_dir.path(), &opts, &Control::new(), None);

            // MidDelete never fires for an operation with nothing to unlink first, so the run may
            // legitimately complete. Either way, resuming must converge.
            let crashed = interrupted.is_err();
            assert_eq!(
                state(journal_dir.path()).unwrap() == RunState::Interrupted,
                crashed,
                "journal state disagrees with what happened at {point:?} op {op_index}"
            );

            let resumed = run(&plan, journal_dir.path()).unwrap();

            assert_eq!(
                snapshot(root),
                expected,
                "resuming after {point:?} on op {op_index} left a different tree"
            );
            assert_eq!(
                resumed.failed, 0,
                "resuming after {point:?} on op {op_index} reported failures: {resumed:?}"
            );
            assert_eq!(state(journal_dir.path()).unwrap(), RunState::Finished);
            assert!(
                resumed.freed <= clean_outcome.freed,
                "resuming after {point:?} on op {op_index} counted {} bytes freed, more than the \
                 {} a clean run frees — something is being counted twice",
                resumed.freed,
                clean_outcome.freed
            );
        }
    }
}

#[test]
fn a_completed_operation_is_never_run_twice() {
    let fixture = fixture();
    let journal_dir = tempfile::tempdir().unwrap();
    let plan = plan_for(fixture.path());

    let first = run(&plan, journal_dir.path()).unwrap();
    let second = run(&plan, journal_dir.path()).unwrap();

    // The second run recognises all three as finished and does nothing, but still reports the
    // totals of the commit as a whole.
    assert_eq!(second.done, first.done);
    assert_eq!(second.freed, first.freed);
    assert_eq!(
        journal::read_dir(journal_dir.path())
            .unwrap()
            .iter()
            .filter(|r| matches!(r.event, Event::OpDone { .. }))
            .count(),
        3
    );
}

#[test]
fn an_entry_that_changed_since_staging_is_refused() {
    let fixture = fixture();
    let root = fixture.path();
    let journal_dir = tempfile::tempdir().unwrap();

    let mut plan = Plan::new(root.to_path_buf());
    plan.ops = vec![delete_op(&root.join("lonely.tmp"))];

    // Same path, different file.
    fs::remove_file(root.join("lonely.tmp")).unwrap();
    fs::write(root.join("lonely.tmp"), vec![0u8; 999]).unwrap();

    let outcome = run(&plan, journal_dir.path()).unwrap();

    assert_eq!(outcome.failed, 1);
    assert_eq!(outcome.done, 0);
    assert!(root.join("lonely.tmp").exists(), "a file we did not review was deleted");

    // The message names the path and says it is not what was staged. Which detail differs
    // depends on whether the filesystem handed the new file the same inode.
    let records = journal::read_dir(journal_dir.path()).unwrap();
    let failure = records
        .iter()
        .find_map(|r| match &r.event {
            Event::OpFailed { error, .. } => Some(error.clone()),
            _ => None,
        })
        .expect("no failure recorded");
    assert!(failure.contains("is not what was staged"), "unhelpful failure message: {failure}");
    assert!(failure.contains("lonely.tmp"), "failure does not say what failed: {failure}");
}

#[test]
fn an_entry_that_is_already_gone_counts_as_done() {
    let fixture = fixture();
    let root = fixture.path();
    let journal_dir = tempfile::tempdir().unwrap();

    let mut plan = Plan::new(root.to_path_buf());
    plan.ops = vec![delete_op(&root.join("lonely.tmp"))];
    fs::remove_file(root.join("lonely.tmp")).unwrap();

    let outcome = run(&plan, journal_dir.path()).unwrap();
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    assert_eq!(outcome.freed, 0, "nothing was reclaimed by us");
}

#[test]
fn pausing_between_operations_stops_and_resumes() {
    let fixture = fixture();
    let root = fixture.path();
    let journal_dir = tempfile::tempdir().unwrap();
    let plan = plan_for(root);

    let control = Control::new();
    control.pause();
    let paused =
        execute(&plan, journal_dir.path(), &ExecOptions::default(), &control, None).unwrap();

    assert!(paused.paused);
    assert_eq!(paused.done, 0);
    assert_eq!(snapshot(root).len(), snapshot(root).len(), "nothing removed");
    assert!(root.join("logs/2026/01/a.log").exists());
    assert_eq!(state(journal_dir.path()).unwrap(), RunState::Paused);

    let resumed = run(&plan, journal_dir.path()).unwrap();
    assert_eq!(resumed.done, 3);
    assert_eq!(state(journal_dir.path()).unwrap(), RunState::Finished);
    assert_eq!(snapshot(root).keys().collect::<Vec<_>>(), ["keep.txt"]);
}

#[test]
fn pausing_part_way_through_a_tree_resumes_and_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("big")).unwrap();
    for i in 0..40 {
        fs::write(root.join("big").join(format!("f{i:02}")), vec![0u8; 4096]).unwrap();
    }
    let journal_dir = tempfile::tempdir().unwrap();
    let mut plan = Plan::new(root.to_path_buf());
    plan.ops = vec![delete_op(&root.join("big"))];

    // Pause as soon as the first entry has come out, leaving the directory half-emptied.
    let control = Control::new();
    let hook = |point: FaultPoint, _: usize| {
        if point == FaultPoint::MidDelete {
            control.pause();
        }
        Ok(())
    };
    let opts = ExecOptions { fault: Some(&hook), ..Default::default() };
    let paused = execute(&plan, journal_dir.path(), &opts, &control, None).unwrap();

    assert!(paused.paused);
    assert!(root.join("big").exists(), "the directory should still be there, part-emptied");
    assert!(fs::read_dir(root.join("big")).unwrap().count() < 40);
    assert_eq!(state(journal_dir.path()).unwrap(), RunState::Paused);

    // Resuming finishes the job despite the directory's mtime having moved — which we caused.
    let resumed = run(&plan, journal_dir.path()).unwrap();
    assert_eq!(resumed.failed, 0, "{resumed:?}");
    assert!(!root.join("big").exists());

    // The reported total covers both attempts. The resumed run can only measure what was left,
    // so without the partial recorded at the pause it would understate the commit badly.
    assert!(
        paused.freed > 0,
        "the pause reported reclaiming nothing after emptying part of a tree"
    );
    assert!(
        resumed.freed > paused.freed,
        "resume reported {} but the pause alone had freed {}",
        resumed.freed,
        paused.freed
    );
    assert_eq!(
        journal::read_dir(journal_dir.path())
            .unwrap()
            .iter()
            .filter_map(|r| match r.event {
                Event::Paused { freed, .. } => Some(freed),
                _ => None,
            })
            .sum::<u64>(),
        paused.freed,
        "the pause record must carry what it reclaimed, or the total is lost on resume"
    );
}

#[test]
fn a_resumed_operation_still_refuses_a_different_inode() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("target")).unwrap();
    fs::write(root.join("target/f"), vec![0u8; 100]).unwrap();
    let journal_dir = tempfile::tempdir().unwrap();
    let mut plan = Plan::new(root.to_path_buf());
    plan.ops = vec![delete_op(&root.join("target"))];

    // Crash after recording the intent, so the operation resumes with a relaxed check.
    let hook = |point: FaultPoint, _: usize| {
        if point == FaultPoint::AfterOpBegin {
            Err(io::Error::other("simulated crash"))
        } else {
            Ok(())
        }
    };
    let opts = ExecOptions { fault: Some(&hook), ..Default::default() };
    execute(&plan, journal_dir.path(), &opts, &Control::new(), None).unwrap_err();

    // Somebody replaces the directory entirely while we were down. The replacement is created
    // while the original still exists, so it is guaranteed a different inode — recreating at the
    // same path can reuse the old one, which would make this test agree by accident.
    fs::create_dir(root.join("replacement")).unwrap();
    fs::write(root.join("replacement/precious"), vec![0u8; 10]).unwrap();
    fs::remove_dir_all(root.join("target")).unwrap();
    fs::rename(root.join("replacement"), root.join("target")).unwrap();

    let resumed = run(&plan, journal_dir.path()).unwrap();
    assert_eq!(resumed.failed, 1, "relaxing the mtime check must not relax the inode check");
    assert!(root.join("target/precious").exists(), "a directory we never reviewed was deleted");
}

#[test]
fn a_dry_run_checks_everything_and_changes_nothing() {
    let fixture = fixture();
    let root = fixture.path();
    let journal_dir = tempfile::tempdir().unwrap();
    let before = snapshot(root);

    let mut plan = plan_for(root);
    // Give the operations an estimate to report.
    for op in &mut plan.ops {
        if let Op::Delete { est_bytes, .. } = op {
            *est_bytes = 1000;
        }
    }

    let opts = ExecOptions { dry_run: true, ..Default::default() };
    let outcome = execute(&plan, journal_dir.path(), &opts, &Control::new(), None).unwrap();

    assert_eq!(outcome.done, 3);
    assert_eq!(outcome.freed, 3000, "a dry run reports the estimate, not a measurement");
    assert_eq!(snapshot(root), before, "a dry run changed the filesystem");

    // Nothing journaled: otherwise the plan would look executed and the real run would skip it.
    assert_eq!(state(journal_dir.path()).unwrap(), RunState::NotStarted);
    assert!(journal::read_dir(journal_dir.path()).unwrap().is_empty());

    // And the real run afterwards does the whole job.
    let outcome = run(&plan, journal_dir.path()).unwrap();
    assert_eq!(outcome.done, 3);
    assert_eq!(snapshot(root).keys().collect::<Vec<_>>(), ["keep.txt"]);
}

#[test]
fn a_dry_run_still_catches_an_entry_that_changed() {
    let fixture = fixture();
    let root = fixture.path();
    let journal_dir = tempfile::tempdir().unwrap();

    let mut plan = Plan::new(root.to_path_buf());
    plan.ops = vec![delete_op(&root.join("lonely.tmp"))];
    fs::remove_file(root.join("lonely.tmp")).unwrap();
    fs::write(root.join("lonely.tmp"), vec![0u8; 3]).unwrap();

    let opts = ExecOptions { dry_run: true, ..Default::default() };
    let outcome = execute(&plan, journal_dir.path(), &opts, &Control::new(), None).unwrap();
    assert_eq!(outcome.failed, 1, "a dry run that misses this is worthless as a rehearsal");
}

#[test]
fn nested_deletions_run_deepest_first() {
    let fixture = fixture();
    let root = fixture.path();
    let journal_dir = tempfile::tempdir().unwrap();

    let mut plan = Plan::new(root.to_path_buf());
    plan.ops = vec![delete_op(&root.join("logs")), delete_op(&root.join("logs/2026/01"))];

    let outcome = run(&plan, journal_dir.path()).unwrap();
    assert_eq!(outcome.failed, 0, "{outcome:?}");
    assert!(!root.join("logs").exists());

    // The inner operation ran first and accounted for its own bytes; the outer one swept the rest.
    let freed: Vec<u64> = journal::read_dir(journal_dir.path())
        .unwrap()
        .iter()
        .filter_map(|r| match r.event {
            Event::OpDone { op, freed } => Some((op, freed)),
            _ => None,
        })
        .map(|(_, freed)| freed)
        .collect();
    assert_eq!(freed.len(), 2);
    assert!(freed[0] > 0, "the deeper operation should have removed something");
}

/// Deletions and moves in one plan, which is the normal case once both exist.
#[test]
fn a_plan_can_mix_moves_and_deletions() {
    let fixture = fixture();
    let root = fixture.path();
    let journal_dir = tempfile::tempdir().unwrap();
    fs::create_dir(root.join("archive")).unwrap();

    let mut plan = Plan::new(root.to_path_buf());
    plan.ops = vec![
        delete_op(&root.join("logs")),
        Op::Move {
            src: root.join("cache"),
            dst: root.join("archive/cache"),
            ident: Ident::of(&root.join("cache")).unwrap(),
            est_bytes: 0,
            on_conflict: Conflict::Fail,
        },
    ];

    let outcome = run(&plan, journal_dir.path()).unwrap();

    assert_eq!(outcome.done, 2, "{outcome:?}");
    assert_eq!(outcome.failed, 0);
    assert!(!root.join("logs").exists());
    assert!(root.join("archive/cache/blob").exists(), "the move did not arrive");
    assert_eq!(state(journal_dir.path()).unwrap(), RunState::Finished);
}

#[test]
fn run_state_tracks_the_journal() {
    let fixture = fixture();
    let journal_dir = tempfile::tempdir().unwrap();
    let plan = plan_for(fixture.path());

    assert_eq!(state(journal_dir.path()).unwrap(), RunState::NotStarted);

    let hook = |point: FaultPoint, _: usize| {
        if point == FaultPoint::AfterOpBegin {
            Err(io::Error::other("simulated crash"))
        } else {
            Ok(())
        }
    };
    let opts = ExecOptions { fault: Some(&hook), ..Default::default() };
    execute(&plan, journal_dir.path(), &opts, &Control::new(), None).unwrap_err();
    assert_eq!(state(journal_dir.path()).unwrap(), RunState::Interrupted);

    run(&plan, journal_dir.path()).unwrap();
    assert_eq!(state(journal_dir.path()).unwrap(), RunState::Finished);
}

#[test]
fn progress_is_reported_for_every_operation() {
    let fixture = fixture();
    let journal_dir = tempfile::tempdir().unwrap();
    let plan = plan_for(fixture.path());

    let (tx, rx) = crossbeam_channel::unbounded();
    execute(&plan, journal_dir.path(), &ExecOptions::default(), &Control::new(), Some(&tx))
        .unwrap();
    drop(tx);

    let events: Vec<ExecEvent> = rx.into_iter().collect();
    assert_eq!(events.iter().filter(|e| matches!(e, ExecEvent::Started { .. })).count(), 3);
    assert_eq!(events.iter().filter(|e| matches!(e, ExecEvent::Finished { .. })).count(), 3);
}

/// An unwritable directory must be caught before its contents are gone, not after: a recursive
/// removal that destroys half a tree and then reports a failure has left the worst possible state.
#[test]
fn a_permission_error_fails_one_operation_without_stopping_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("locked")).unwrap();
    fs::create_dir(root.join("locked/inner")).unwrap();
    fs::write(root.join("locked/inner/f"), vec![0u8; 10]).unwrap();
    fs::write(root.join("removable"), vec![0u8; 10]).unwrap();

    let journal_dir = tempfile::tempdir().unwrap();
    let mut plan = Plan::new(root.to_path_buf());
    plan.ops = vec![delete_op(&root.join("locked")), delete_op(&root.join("removable"))];

    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o500)).unwrap();

    let outcome = run(&plan, journal_dir.path()).unwrap();

    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();

    // Running as root defeats the permission bits, so only assert the part that always holds.
    assert!(!root.join("removable").exists(), "one failure stopped unrelated work");
    if outcome.failed > 0 {
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.done, 1);
        assert!(
            root.join("locked/inner/f").exists(),
            "the removal got part way in before discovering it could not finish"
        );
    }
}

#[test]
fn the_plan_directory_is_created_if_it_does_not_exist() {
    let fixture = fixture();
    let parent = tempfile::tempdir().unwrap();
    let journal_dir: PathBuf = parent.path().join("nested/plan-dir");
    let plan = plan_for(fixture.path());

    run(&plan, &journal_dir).unwrap();
    assert!(journal_dir.join(journal::JOURNAL_FILE).exists());
}

#[test]
fn a_symlinked_directory_is_removed_as_a_link_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("real")).unwrap();
    fs::write(root.join("real/precious"), vec![0u8; 10]).unwrap();
    fs::create_dir(root.join("staged")).unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("staged/pointer")).unwrap();

    let journal_dir = tempfile::tempdir().unwrap();
    let mut plan = Plan::new(root.to_path_buf());
    plan.ops = vec![delete_op(&root.join("staged"))];

    let outcome = run(&plan, journal_dir.path()).unwrap();

    assert_eq!(outcome.failed, 0, "{outcome:?}");
    assert!(!root.join("staged").exists());
    assert!(root.join("real/precious").exists(), "deletion followed a symlink out of the tree");
}

#[test]
fn deleting_a_symlink_to_a_directory_does_not_touch_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("real")).unwrap();
    fs::write(root.join("real/precious"), vec![0u8; 10]).unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("pointer")).unwrap();

    let journal_dir = tempfile::tempdir().unwrap();
    let mut plan = Plan::new(root.to_path_buf());
    plan.ops = vec![delete_op(&root.join("pointer"))];

    let outcome = run(&plan, journal_dir.path()).unwrap();

    assert_eq!(outcome.done, 1, "{outcome:?}");
    assert!(!root.join("pointer").symlink_metadata().is_ok());
    assert!(root.join("real/precious").exists());
}

#[test]
fn an_empty_plan_is_a_finished_run() {
    let journal_dir = tempfile::tempdir().unwrap();
    let plan = Plan::new(PathBuf::from("/data"));

    let outcome = run(&plan, journal_dir.path()).unwrap();
    assert_eq!(outcome, Outcome { done: 0, failed: 0, skipped: 0, freed: 0, paused: false });
    assert_eq!(state(journal_dir.path()).unwrap(), RunState::Finished);
}

#[test]
fn identity_matching_relaxes_time_but_never_identity() {
    let staged = Ident { dev: 1, ino: 2, size: 10, mtime: 100, kind: EntryKind::Dir };

    let touched = Ident { mtime: 200, ..staged.clone() };
    assert!(!matches(&staged, &touched, true), "a strict check should notice a new mtime");
    assert!(matches(&staged, &touched, false), "a resumed operation caused that mtime itself");

    let replaced = Ident { ino: 99, ..staged.clone() };
    assert!(!matches(&staged, &replaced, false), "a new inode is a different object, always");

    let retyped = Ident { kind: EntryKind::File, ..staged.clone() };
    assert!(!matches(&staged, &retyped, false));
}
