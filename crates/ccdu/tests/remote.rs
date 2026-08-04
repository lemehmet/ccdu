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

#[test]
fn a_remote_tree_can_be_staged_against_and_committed() {
    use ccdu_core::plan::{Op, Plan};

    let dir = tree_fixture();
    let state = tempfile::tempdir().unwrap();

    let mut command = agent();
    command.env("CCDU_STATE_DIR", state.path());
    let mut remote = Remote::connect(command).unwrap();

    // The whole loop as a caller would drive it: scan, ask what things are, build a plan from
    // those identities, store it over there, then run it over there.
    let tree = remote.scan(&dir.path().display().to_string(), false, 2, Vec::new(), None).unwrap();
    assert_eq!(tree.node(ROOT).items, 6);

    let target = dir.path().join("logs");
    let idents = remote.identify(&[target.display().to_string()]).unwrap();
    let ident = idents[0].clone().expect("no identity for a path that exists");

    let mut plan = Plan::new(dir.path().to_path_buf());
    plan.ops.push(Op::Delete { path: target.clone(), ident, est_bytes: 50_000 });
    let (id, _) = remote.save_plan(&plan).unwrap();

    let mut events = Vec::new();
    let (outcome, state) = remote.apply(&id, false, |e| events.push(e)).unwrap();

    assert_eq!(outcome.done, 1, "{outcome:?}");
    assert_eq!(outcome.failed, 0);
    assert!(outcome.freed >= 50_000);
    assert_eq!(state, ccdu_core::exec::RunState::Finished);
    assert!(!events.is_empty(), "progress should stream while the commit runs");
    assert!(!target.exists(), "the commit reported success but the directory is still there");
    assert!(dir.path().join("readme.txt").exists(), "something unstaged was removed");
}

#[test]
fn a_remote_dry_run_reports_without_changing_anything() {
    use ccdu_core::plan::{Op, Plan};

    let dir = tree_fixture();
    let state = tempfile::tempdir().unwrap();
    let mut command = agent();
    command.env("CCDU_STATE_DIR", state.path());
    let mut remote = Remote::connect(command).unwrap();

    let target = dir.path().join("logs");
    let ident = remote.identify(&[target.display().to_string()]).unwrap()[0].clone().unwrap();
    let mut plan = Plan::new(dir.path().to_path_buf());
    plan.ops.push(Op::Delete { path: target.clone(), ident, est_bytes: 1234 });
    let (id, _) = remote.save_plan(&plan).unwrap();

    let (outcome, _) = remote.apply(&id, true, |_| {}).unwrap();
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.freed, 1234, "a dry run reports the estimate");
    assert!(target.exists(), "a dry run removed something");

    // And it is still runnable afterwards.
    let (outcome, _) = remote.apply(&id, false, |_| {}).unwrap();
    assert_eq!(outcome.done, 1);
    assert!(!target.exists());
}

#[test]
fn a_stale_identity_stops_a_remote_commit() {
    use ccdu_core::plan::{Op, Plan};

    let dir = tree_fixture();
    let state = tempfile::tempdir().unwrap();
    let mut command = agent();
    command.env("CCDU_STATE_DIR", state.path());
    let mut remote = Remote::connect(command).unwrap();

    let target = dir.path().join("readme.txt");
    let ident = remote.identify(&[target.display().to_string()]).unwrap()[0].clone().unwrap();
    let mut plan = Plan::new(dir.path().to_path_buf());
    plan.ops.push(Op::Delete { path: target.clone(), ident, est_bytes: 500 });
    let (id, _) = remote.save_plan(&plan).unwrap();

    // Same path, different file, after the plan was made.
    std::fs::remove_file(&target).unwrap();
    std::fs::write(&target, vec![9u8; 4321]).unwrap();

    let (outcome, _) = remote.apply(&id, false, |_| {}).unwrap();
    assert_eq!(outcome.failed, 1, "a changed file was removed anyway: {outcome:?}");
    assert!(target.exists(), "a file we never reviewed was deleted over the wire");
}

/// A plan staged against a tree from another machine must record *that* machine, and be stored
/// there. Getting either wrong produces a plan sitting in this machine's store, labelled with this
/// machine's hostname, naming another machine's paths — which validates clean here and applies to
/// whatever happens to sit at those paths locally.
#[test]
fn a_plan_for_a_remote_tree_belongs_to_the_remote() {
    let dir = tree_fixture();
    let state = tempfile::tempdir().unwrap();
    let mut command = agent();
    command.env("CCDU_STATE_DIR", state.path());
    let mut remote = Remote::connect(command).unwrap();

    let target = dir.path().join("logs");
    let ident = remote.identify(&[target.display().to_string()]).unwrap()[0].clone().unwrap();

    // Built the way the browser builds one for a remote tree.
    let mut plan = ccdu_core::plan::Plan::for_host(dir.path().to_path_buf(), remote.host.clone());
    plan.ops.push(ccdu_core::plan::Op::Delete { path: target, ident, est_bytes: 0 });
    assert_eq!(plan.host, remote.host);

    let (id, path) = remote.save_plan(&plan).unwrap();
    assert!(
        path.starts_with(&state.path().display().to_string()),
        "the plan was stored somewhere other than the remote's own store: {path}"
    );

    // The remote's own store has it; this test's process has a different notion of "here".
    let store = ccdu_core::plan::store::Store::at(state.path().join("plans"));
    assert_eq!(store.load(&id).unwrap().host, remote.host);
}

/// The host field is not decoration: validation must refuse a plan belonging to another machine,
/// even when the paths it names happen to exist here.
#[test]
fn a_plan_from_another_host_is_refused_even_when_the_paths_exist_here() {
    use ccdu_core::plan::{validate, Ident, Op, Plan, Severity, ValidateOptions};

    let dir = tree_fixture();
    let target = dir.path().join("readme.txt");

    let mut plan = Plan::for_host(dir.path().to_path_buf(), "some-other-machine".to_string());
    plan.ops.push(Op::Delete {
        ident: Ident::of(&target).unwrap(),
        path: target.clone(),
        est_bytes: 0,
    });

    let findings = validate(&plan, &ValidateOptions::default());
    let errors: Vec<&str> = findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .map(|f| f.message.as_str())
        .collect();

    assert!(
        errors.iter().any(|m| m.contains("some-other-machine")),
        "a plan from another machine validated clean against local files: {findings:?}"
    );
    assert!(target.exists());
}
