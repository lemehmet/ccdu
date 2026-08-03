//! End-to-end crash recovery against the real binary.
//!
//! The in-process fault tests in `ccdu-core` cover the logic exhaustively, but they simulate a
//! crash by returning an error, which still unwinds and still runs destructors. These tests kill
//! the process outright — `abort()`, then `SIGKILL` — so nothing is flushed on the way out and the
//! journal on disk is whatever actually reached it. That is the situation the design exists for.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_ccdu");

/// A tree to destroy, and a state directory to keep the plan in.
struct Scene {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    state: std::path::PathBuf,
}

fn scene() -> Scene {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let state = dir.path().join("state");

    std::fs::create_dir_all(root.join("logs/2026")).unwrap();
    for i in 0..6 {
        std::fs::write(root.join("logs/2026").join(format!("{i}.log")), vec![0u8; 20_000]).unwrap();
    }
    std::fs::create_dir(root.join("cache")).unwrap();
    for i in 0..4 {
        std::fs::write(root.join("cache").join(format!("blob{i}")), vec![0u8; 8_000]).unwrap();
    }
    std::fs::write(root.join("keep.txt"), b"keep me").unwrap();

    Scene { _dir: dir, root, state }
}

impl Scene {
    fn ccdu(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.env("CCDU_STATE_DIR", &self.state);
        cmd
    }

    /// Write a plan deleting `logs/` and `cache/`, and return its id.
    fn save_plan(&self) -> String {
        use ccdu_core::plan::store::Store;
        use ccdu_core::plan::{Ident, Op, Plan};

        let mut plan = Plan::new(self.root.clone());
        for name in ["logs", "cache"] {
            let path = self.root.join(name);
            plan.ops.push(Op::Delete { ident: Ident::of(&path).unwrap(), path, est_bytes: 0 });
        }
        Store::at(self.state.join("plans")).save(&plan).unwrap();
        plan.id
    }

    fn entries(&self) -> BTreeSet<String> {
        entries_under(&self.root)
    }
}

fn entries_under(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(list) = std::fs::read_dir(&dir) else { continue };
        for entry in list.flatten() {
            let path = entry.path();
            out.insert(path.strip_prefix(root).unwrap().to_string_lossy().into_owned());
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            }
        }
    }
    out
}

#[test]
fn a_run_aborted_at_each_journal_boundary_resumes_to_completion() {
    for point in
        ["before_op_begin", "after_op_begin", "mid_delete", "before_op_done", "after_op_done"]
    {
        for op in 0..2 {
            let scene = scene();
            let id = scene.save_plan();

            let killed = scene
                .ccdu()
                .args(["apply", &id, "--yes"])
                .env("CCDU_FAULT", format!("{point}:{op}"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();

            // abort() raises SIGABRT: no unwinding, no flushing, nothing tidied up.
            let aborted = !killed.success();

            let status = scene.ccdu().args(["status", &id]).output().unwrap();
            let text = String::from_utf8_lossy(&status.stdout).into_owned();
            if aborted {
                assert!(
                    text.contains("interrupted"),
                    "after aborting at {point}:{op}, status says:\n{text}"
                );
                assert!(text.contains("ccdu resume"), "status should say how to continue:\n{text}");
            }

            let resumed = scene.ccdu().args(["resume", &id]).output().unwrap();
            assert!(
                resumed.status.success(),
                "resume failed after {point}:{op}: {}",
                String::from_utf8_lossy(&resumed.stderr)
            );

            assert_eq!(
                scene.entries(),
                BTreeSet::from(["keep.txt".to_string()]),
                "resuming after an abort at {point}:{op} did not finish the job"
            );

            let status = scene.ccdu().args(["status", &id]).output().unwrap();
            let text = String::from_utf8_lossy(&status.stdout);
            assert!(text.contains("finished"), "after {point}:{op}, status says:\n{text}");
            assert!(text.contains("2 of 2 operations"), "totals should survive the crash:\n{text}");
        }
    }
}

#[test]
fn a_sigkill_part_way_through_leaves_a_resumable_run() {
    let scene = scene();
    let id = scene.save_plan();

    // SIGKILL cannot be caught, so not even the abort handler runs. Whatever is on disk is what
    // the journal's fsync put there.
    let mut child = scene
        .ccdu()
        .args(["apply", &id, "--yes"])
        .env("CCDU_FAULT", "mid_delete:0")
        .env("CCDU_HANG", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // The fault aborts almost immediately; kill whatever is left, which also covers the race
    // where the process is already gone.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();

    let resumed = scene.ccdu().args(["resume", &id]).output().unwrap();
    assert!(
        resumed.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(scene.entries(), BTreeSet::from(["keep.txt".to_string()]));
}

#[test]
fn a_dry_run_reports_without_touching_anything() {
    let scene = scene();
    let id = scene.save_plan();
    let before = scene.entries();

    let out = scene.ccdu().args(["apply", &id, "--dry-run"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("dry run"), "{text}");
    assert_eq!(scene.entries(), before, "a dry run changed the filesystem");

    // And it is still runnable afterwards: a rehearsal must not count as the performance.
    let out = scene.ccdu().args(["apply", &id, "--yes"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(scene.entries(), BTreeSet::from(["keep.txt".to_string()]));
}

#[test]
fn running_unattended_without_yes_is_refused() {
    let scene = scene();
    let id = scene.save_plan();
    let before = scene.entries();

    // No terminal on stdin, no --yes: this must not proceed on an assumption.
    let out = scene.ccdu().args(["apply", &id]).stdin(Stdio::null()).output().unwrap();

    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--yes"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(scene.entries(), before);
}

#[test]
fn a_plan_whose_target_changed_is_refused_before_anything_runs() {
    let scene = scene();
    let id = scene.save_plan();

    // Something appears inside logs/ after staging, so it is no longer what was reviewed.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(scene.root.join("logs/surprise"), b"new").unwrap();
    let before = scene.entries();

    let out = scene.ccdu().args(["apply", &id, "--yes"]).output().unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("changed since staging"), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(scene.entries(), before, "a refused plan still deleted something");
}
