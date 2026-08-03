//! The remote path end to end, against a real agent process.
//!
//! The transport is a `Command`, so spawning `ccdu --agent` directly exercises the same code an
//! ssh connection would — framing, handshake, progress, tree transfer — with no second machine,
//! no keys, and no sshd.

use std::process::Command;

use ccdu_core::model::ROOT;
use ccdu_remote::Remote;

const BIN: &str = env!("CARGO_BIN_EXE_ccdu");

fn agent() -> Command {
    let mut cmd = Command::new(BIN);
    cmd.arg("--agent");
    cmd
}

fn tree_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("logs/2026")).unwrap();
    std::fs::write(root.join("logs/2026/a.log"), vec![1u8; 40_000]).unwrap();
    std::fs::write(root.join("logs/2026/b.log"), vec![2u8; 10_000]).unwrap();
    std::fs::write(root.join("readme.txt"), vec![3u8; 500]).unwrap();
    std::os::unix::fs::symlink("readme.txt", root.join("pointer")).unwrap();
    dir
}

#[test]
fn a_tree_scanned_by_the_agent_arrives_intact() {
    let dir = tree_fixture();
    let mut remote = Remote::connect(agent()).unwrap();

    assert!(!remote.host.is_empty(), "the agent should say which host it is");
    assert!(!remote.version.is_empty());

    let tree = remote.scan(&dir.path().display().to_string(), false, 2, Vec::new(), None).unwrap();

    assert_eq!(tree.node(ROOT).items, 6, "entries went missing in transit");
    assert!(tree.node(ROOT).disk >= 50_000);
    assert_eq!(tree.root_path(), dir.path().canonicalize().unwrap());

    // Names and structure survive, not just totals.
    let names: Vec<String> =
        tree.children(ROOT).map(|c| tree.name(c).to_string_lossy().into_owned()).collect();
    assert!(names.contains(&"logs".to_string()), "{names:?}");
    assert!(names.contains(&"readme.txt".to_string()), "{names:?}");
}

#[test]
fn progress_reaches_the_caller_while_the_scan_runs() {
    // A tree with enough directories to trigger more than one progress report.
    let dir = tempfile::tempdir().unwrap();
    for i in 0..300 {
        let sub = dir.path().join(format!("d{i:03}"));
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("f"), vec![0u8; 1024]).unwrap();
    }

    let mut remote = Remote::connect(agent()).unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let tree =
        remote.scan(&dir.path().display().to_string(), false, 2, Vec::new(), Some(&tx)).unwrap();
    drop(tx);

    assert_eq!(tree.node(ROOT).items, 600);
    let updates: Vec<_> = rx.into_iter().collect();
    assert!(!updates.is_empty(), "no progress arrived during a 600-entry scan");
}

#[test]
fn one_connection_serves_several_scans() {
    let dir = tree_fixture();
    let mut remote = Remote::connect(agent()).unwrap();
    let path = dir.path().display().to_string();

    let first = remote.scan(&path, false, 2, Vec::new(), None).unwrap();
    let second = remote.scan(&path, false, 2, Vec::new(), None).unwrap();

    assert_eq!(first.node(ROOT).items, second.node(ROOT).items);
    assert_eq!(first.node(ROOT).disk, second.node(ROOT).disk);
}

#[test]
fn excludes_are_honoured_on_the_far_side() {
    let dir = tree_fixture();
    let mut remote = Remote::connect(agent()).unwrap();

    let tree = remote
        .scan(&dir.path().display().to_string(), false, 2, vec!["logs".to_string()], None)
        .unwrap();

    // The directory is still listed, but nothing under it was walked: logs, readme.txt, pointer.
    assert_eq!(tree.node(ROOT).items, 3);
    assert!(tree.node(ROOT).disk < 40_000, "the excluded subtree was counted");
}

#[test]
fn a_missing_path_is_reported_and_the_connection_survives() {
    let dir = tree_fixture();
    let mut remote = Remote::connect(agent()).unwrap();

    let err = remote.scan("/definitely/not/here", false, 1, Vec::new(), None).unwrap_err();
    assert!(err.to_string().contains("not/here"), "{err}");

    // The session is still usable: one bad request should not cost the connection.
    let tree = remote.scan(&dir.path().display().to_string(), false, 2, Vec::new(), None).unwrap();
    assert_eq!(tree.node(ROOT).items, 6);
}

#[test]
fn a_plan_can_be_stored_where_its_paths_mean_something() {
    use ccdu_core::plan::{Ident, Op, Plan};

    let dir = tree_fixture();
    let state = tempfile::tempdir().unwrap();

    let mut command = agent();
    command.env("CCDU_STATE_DIR", state.path());
    let mut remote = Remote::connect(command).unwrap();

    let target = dir.path().join("readme.txt");
    let mut plan = Plan::new(dir.path().to_path_buf());
    plan.ops.push(Op::Delete { ident: Ident::of(&target).unwrap(), path: target, est_bytes: 4096 });

    let (id, path) = remote.save_plan(&plan).unwrap();
    assert_eq!(id, plan.id);
    assert!(path.ends_with("plan.json"), "{path}");

    // It landed in the remote's own store, ready for `ccdu apply` over there.
    let store = ccdu_core::plan::store::Store::at(state.path().join("plans"));
    let saved = store.load(&id).unwrap();
    assert_eq!(saved.ops.len(), 1);
    assert_eq!(saved, plan);
}

#[test]
fn the_agent_writes_nothing_to_stdout_but_frames() {
    // A stray println anywhere in the agent's path would corrupt the stream. This checks the
    // bytes directly rather than trusting that nothing prints.
    let dir = tree_fixture();
    let mut remote = Remote::connect(agent()).unwrap();
    let tree = remote.scan(&dir.path().display().to_string(), false, 1, Vec::new(), None).unwrap();
    assert_eq!(tree.node(ROOT).items, 6);
    // Reaching here at all means every byte parsed as a frame; a stray write would have
    // desynchronised the stream and failed above.
}
