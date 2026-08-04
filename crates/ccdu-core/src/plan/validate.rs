//! Deciding whether a plan is safe to run.
//!
//! Validation is deliberately paranoid and deliberately cheap to re-run: it is called every time
//! the plan view is opened, before a commit, and by `ccdu plan validate`. Everything it reports is
//! attributed to an operation so the user can see which line is the problem.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rustix::fs::{statvfs, Access, AtFlags, CWD};

use super::{Conflict, EntryKind, Ident, Op, Plan, PLAN_VERSION};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Blocks the commit.
    Error,
    /// Worth reading before committing, but not a reason to stop.
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    /// Index into [`Plan::ops`], or `None` for something about the plan as a whole.
    pub op: Option<usize>,
    pub message: String,
}

impl Finding {
    fn error(op: impl Into<Option<usize>>, message: impl Into<String>) -> Finding {
        Finding { severity: Severity::Error, op: op.into(), message: message.into() }
    }

    fn warn(op: impl Into<Option<usize>>, message: impl Into<String>) -> Finding {
        Finding { severity: Severity::Warning, op: op.into(), message: message.into() }
    }
}

pub struct ValidateOptions {
    /// Permit operations on paths outside the scanned tree.
    pub allow_outside_root: bool,
    /// Paths that may never be operated on, matched exactly. Their contents are fair game; the
    /// directories themselves are not.
    pub protected: Vec<PathBuf>,
    /// Fraction of a destination filesystem to leave free after a move.
    pub headroom: f64,
    /// Skip checks that touch the filesystem. Used by tests and by callers inspecting a plan for
    /// a host they are not on.
    pub offline: bool,
}

impl Default for ValidateOptions {
    fn default() -> Self {
        ValidateOptions {
            allow_outside_root: false,
            protected: default_protected(),
            headroom: 0.02,
            offline: false,
        }
    }
}

/// Directories that exist to hold a running system. Deleting or moving any of them is never what
/// somebody meant, and the cost of being wrong is the machine.
pub fn default_protected() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = [
        "/",
        "/bin",
        "/boot",
        "/dev",
        "/etc",
        "/home",
        "/lib",
        "/lib32",
        "/lib64",
        "/libx32",
        "/opt",
        "/proc",
        "/root",
        "/run",
        "/sbin",
        "/srv",
        "/sys",
        "/tmp",
        "/usr",
        "/var",
        "/System",
        "/Applications",
        "/Library",
        "/Users",
        "/private",
        "/Volumes",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();

    if let Some(home) = std::env::var_os("HOME") {
        // The home directory itself, not its contents.
        paths.push(PathBuf::from(home));
    }
    paths
}

/// Check a plan. An empty result means it is safe to run.
pub fn validate(plan: &Plan, opts: &ValidateOptions) -> Vec<Finding> {
    let mut out = Vec::new();

    if plan.version > PLAN_VERSION {
        out.push(Finding::error(
            None,
            format!(
                "plan format v{} is newer than this build understands (v{PLAN_VERSION})",
                plan.version
            ),
        ));
        // Nothing below can be trusted about a format we do not know.
        return out;
    }
    if plan.ops.is_empty() {
        out.push(Finding::warn(None, "plan is empty"));
    }
    if !opts.offline {
        let here = rustix::system::uname().nodename().to_string_lossy().into_owned();
        if plan.host != here {
            out.push(Finding::error(
                None,
                format!(
                    "plan was made on {:?} but this is {here:?}; inode identities do not carry \
                     across machines",
                    plan.host
                ),
            ));
        }
    }

    check_shape(plan, opts, &mut out);
    if !opts.offline {
        check_filesystem(plan, opts, &mut out);
    }

    out.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.op.cmp(&b.op)));
    out
}

/// Checks that need only the plan itself: overlaps, collisions, and forbidden targets.
fn check_shape(plan: &Plan, opts: &ValidateOptions, out: &mut Vec<Finding>) {
    let deleted_dirs: Vec<&Path> = plan
        .ops
        .iter()
        .filter(|o| o.is_delete() && o.ident().kind == EntryKind::Dir)
        .map(|o| o.source())
        .collect();

    let mut sources: HashMap<&Path, usize> = HashMap::new();
    let mut destinations: HashMap<&Path, usize> = HashMap::new();

    for (i, op) in plan.ops.iter().enumerate() {
        let src = op.source();

        if let Some(first) = sources.insert(src, i) {
            out.push(Finding::error(i, format!("same path is already operated on by #{first}")));
        }
        if opts.protected.iter().any(|p| p == src) {
            out.push(Finding::error(i, format!("{} is a protected path", src.display())));
        }
        if src == plan.root {
            out.push(Finding::error(i, "refusing to operate on the scan root itself"));
        } else if !opts.allow_outside_root && !src.starts_with(&plan.root) {
            out.push(Finding::error(
                i,
                format!("{} is outside the scanned tree {}", src.display(), plan.root.display()),
            ));
        }
        if let Some(dir) = deleted_dirs.iter().find(|d| is_inside(src, d)) {
            let severity = if op.is_delete() { Severity::Warning } else { Severity::Error };
            let message = if op.is_delete() {
                format!("redundant: already covered by the deletion of {}", dir.display())
            } else {
                format!("cannot move out of {}, which this plan deletes", dir.display())
            };
            out.push(Finding { severity, op: Some(i), message });
        }

        let Op::Move { src, dst, .. } = op else { continue };

        if dst == src {
            out.push(Finding::error(i, "destination is the source"));
        }
        if is_inside(dst, src) {
            out.push(Finding::error(i, "destination is inside the directory being moved"));
        }
        if opts.protected.iter().any(|p| p == dst) {
            out.push(Finding::error(i, format!("{} is a protected path", dst.display())));
        }
        if let Some(first) = destinations.insert(dst, i) {
            out.push(Finding::error(i, format!("#{first} already writes to this destination")));
        }
        if let Some(dir) = deleted_dirs.iter().find(|d| is_inside(dst, d)) {
            out.push(Finding::error(
                i,
                format!("destination is inside {}, which this plan deletes", dir.display()),
            ));
        }
    }
}

/// Checks that need to look at the disk: does the source still match, is the destination clear,
/// is there room, and are the parent directories writable.
fn check_filesystem(plan: &Plan, opts: &ValidateOptions, out: &mut Vec<Finding>) {
    // Bytes each destination filesystem is being asked to absorb, keyed by device.
    let mut demand: HashMap<u64, (u64, PathBuf)> = HashMap::new();

    for (i, op) in plan.ops.iter().enumerate() {
        let src = op.source();
        match Ident::of(src) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                out.push(Finding::error(i, format!("{} no longer exists", src.display())));
                continue;
            }
            Err(e) => {
                out.push(Finding::error(i, format!("cannot stat {}: {e}", src.display())));
                continue;
            }
            Ok(now) if &now != op.ident() => {
                out.push(Finding::error(i, changed_message(op.ident(), &now)));
                continue;
            }
            Ok(_) => {}
        }

        // Removing or renaming an entry is a write to its parent, not to the entry itself.
        if let Some(parent) = src.parent() {
            if !writable(parent) {
                out.push(Finding::error(i, format!("no write permission on {}", parent.display())));
            }
        }

        let Op::Move { dst, est_bytes, on_conflict, .. } = op else { continue };

        match Ident::of(dst) {
            Ok(_) if *on_conflict == Conflict::Fail => {
                out.push(Finding::error(i, format!("{} already exists", dst.display())));
                continue;
            }
            Ok(_) => {
                out.push(Finding::warn(i, format!("{} exists; will be skipped", dst.display())))
            }
            Err(_) => {}
        }

        let Some(parent) = dst.parent() else { continue };
        let parent_id = match Ident::of(parent) {
            Ok(id) => id,
            Err(_) => {
                out.push(Finding::error(
                    i,
                    format!("destination directory {} does not exist", parent.display()),
                ));
                continue;
            }
        };
        if parent_id.kind != EntryKind::Dir {
            out.push(Finding::error(i, format!("{} is not a directory", parent.display())));
            continue;
        }
        if !writable(parent) {
            out.push(Finding::error(i, format!("no write permission on {}", parent.display())));
        }

        // A move within one filesystem is a rename: instant, and it consumes nothing.
        if parent_id.dev == op.ident().dev {
            continue;
        }
        out.push(Finding::warn(
            i,
            "crosses filesystems: will be copied and verified, then the source removed",
        ));
        let slot = demand.entry(parent_id.dev).or_insert((0, parent.to_path_buf()));
        slot.0 = slot.0.saturating_add(*est_bytes);
    }

    for (needed, dir) in demand.into_values() {
        let Some(available) = available_bytes(&dir) else { continue };
        let reserve = (available as f64 * opts.headroom) as u64;
        if needed + reserve > available {
            out.push(Finding::error(
                None,
                format!(
                    "{} needs {} but {} has {} free ({}% reserved)",
                    dir.display(),
                    crate::format::human_size(needed),
                    dir.display(),
                    crate::format::human_size(available),
                    (opts.headroom * 100.0).round() as u64,
                ),
            ));
        }
    }
}

/// Say which part of the identity moved, so the user can judge whether they still mean it. A bare
/// "it changed" leaves them re-deriving what we already know.
fn changed_message(was: &Ident, now: &Ident) -> String {
    if was.ino != now.ino || was.dev != now.dev {
        "changed since staging: a different file now occupies this path".to_string()
    } else if was.kind != now.kind {
        format!("changed since staging: now a {:?}", now.kind).to_lowercase()
    } else if was.size != now.size {
        format!(
            "changed since staging: size {} -> {}",
            crate::format::human_size(was.size),
            crate::format::human_size(now.size)
        )
    } else {
        // For a directory this is the usual signal: something was added or removed inside it.
        format!(
            "changed since staging: modified at {} (was {})",
            crate::format::format_time_secs(now.mtime),
            crate::format::format_time_secs(was.mtime)
        )
    }
}

/// True when `path` is strictly inside `dir`, comparing whole components so `/a/bc` is not
/// mistaken for something inside `/a/b`.
fn is_inside(path: &Path, dir: &Path) -> bool {
    path != dir && path.starts_with(dir)
}

fn writable(path: &Path) -> bool {
    rustix::fs::accessat(CWD, path, Access::WRITE_OK, AtFlags::empty()).is_ok()
}

fn available_bytes(path: &Path) -> Option<u64> {
    let st = statvfs(path).ok()?;
    Some(st.f_bavail.saturating_mul(st.f_frsize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Conflict, Ident, Op, Plan};
    use std::fs;

    fn offline() -> ValidateOptions {
        ValidateOptions { offline: true, ..Default::default() }
    }

    fn ident(kind: EntryKind) -> Ident {
        Ident { dev: 1, ino: 2, size: 3, mtime: 4, kind }
    }

    fn plan_with(ops: Vec<Op>) -> Plan {
        let mut plan = Plan::new(PathBuf::from("/data"));
        plan.ops = ops;
        plan
    }

    fn del(path: &str, kind: EntryKind) -> Op {
        Op::Delete { path: PathBuf::from(path), ident: ident(kind), est_bytes: 10 }
    }

    fn mv(src: &str, dst: &str) -> Op {
        Op::Move {
            src: PathBuf::from(src),
            dst: PathBuf::from(dst),
            ident: ident(EntryKind::Dir),
            est_bytes: 10,
            on_conflict: Conflict::Fail,
        }
    }

    fn errors(findings: &[Finding]) -> Vec<&str> {
        findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .map(|f| f.message.as_str())
            .collect()
    }

    #[test]
    fn a_plain_plan_passes() {
        let plan = plan_with(vec![del("/data/cache", EntryKind::Dir)]);
        assert!(errors(&validate(&plan, &offline())).is_empty());
    }

    #[test]
    fn protected_paths_are_refused() {
        let plan = plan_with(vec![del("/usr", EntryKind::Dir)]);
        let found = validate(&plan, &ValidateOptions { allow_outside_root: true, ..offline() });
        assert!(errors(&found).iter().any(|m| m.contains("protected")), "{found:?}");
    }

    #[test]
    fn the_scan_root_itself_is_refused() {
        let plan = plan_with(vec![del("/data", EntryKind::Dir)]);
        let found = validate(&plan, &offline());
        assert!(errors(&found).iter().any(|m| m.contains("scan root")), "{found:?}");
    }

    #[test]
    fn paths_outside_the_scanned_tree_need_permission() {
        let plan = plan_with(vec![del("/elsewhere/junk", EntryKind::File)]);
        assert!(errors(&validate(&plan, &offline())).iter().any(|m| m.contains("outside")));

        let allowed = ValidateOptions { allow_outside_root: true, ..offline() };
        assert!(errors(&validate(&plan, &allowed)).is_empty());
    }

    #[test]
    fn moving_a_directory_into_itself_is_refused() {
        let plan = plan_with(vec![mv("/data/tree", "/data/tree/inner")]);
        let found = validate(&plan, &offline());
        assert!(errors(&found).iter().any(|m| m.contains("inside the directory being moved")));
    }

    #[test]
    fn two_operations_cannot_share_a_destination() {
        let plan = plan_with(vec![mv("/data/a", "/mnt/x"), mv("/data/b", "/mnt/x")]);
        let found = validate(&plan, &offline());
        assert!(errors(&found).iter().any(|m| m.contains("already writes")), "{found:?}");
    }

    #[test]
    fn the_same_path_cannot_be_operated_on_twice() {
        let plan = plan_with(vec![del("/data/a", EntryKind::File), mv("/data/a", "/mnt/a")]);
        let found = validate(&plan, &offline());
        assert!(errors(&found).iter().any(|m| m.contains("already operated on")), "{found:?}");
    }

    #[test]
    fn nesting_warns_for_deletes_but_blocks_moves() {
        let plan = plan_with(vec![
            del("/data/tree", EntryKind::Dir),
            del("/data/tree/file", EntryKind::File),
        ]);
        let found = validate(&plan, &offline());
        assert!(errors(&found).is_empty(), "{found:?}");
        assert!(found.iter().any(|f| f.message.contains("redundant")), "{found:?}");

        let plan = plan_with(vec![del("/data/tree", EntryKind::Dir), mv("/data/tree/f", "/mnt/f")]);
        let found = validate(&plan, &offline());
        assert!(errors(&found).iter().any(|m| m.contains("cannot move out of")), "{found:?}");
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_nested() {
        let plan =
            plan_with(vec![del("/data/tree", EntryKind::Dir), del("/data/tree2", EntryKind::Dir)]);
        let found = validate(&plan, &offline());
        assert!(found.iter().all(|f| !f.message.contains("redundant")), "{found:?}");
    }

    #[test]
    fn a_newer_plan_format_is_refused_outright() {
        let mut plan = plan_with(vec![del("/data/a", EntryKind::File)]);
        plan.version = PLAN_VERSION + 1;
        let found = validate(&plan, &offline());
        assert_eq!(found.len(), 1, "no other check should run: {found:?}");
        assert!(found[0].message.contains("newer than this build"));
    }

    #[test]
    fn a_missing_source_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = Plan::new(dir.path().to_path_buf());
        plan.ops = vec![Op::Delete {
            path: dir.path().join("gone"),
            ident: ident(EntryKind::File),
            est_bytes: 1,
        }];
        let found = validate(&plan, &ValidateOptions::default());
        assert!(errors(&found).iter().any(|m| m.contains("no longer exists")), "{found:?}");
    }

    #[test]
    fn a_source_that_changed_since_staging_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, vec![0u8; 100]).unwrap();

        let mut plan = Plan::new(dir.path().to_path_buf());
        let staged = Ident::of(&file).unwrap();
        plan.ops = vec![Op::Delete { path: file.clone(), ident: staged, est_bytes: 100 }];
        assert!(errors(&validate(&plan, &ValidateOptions::default())).is_empty());

        // Same path, different file.
        fs::remove_file(&file).unwrap();
        fs::write(&file, vec![0u8; 200]).unwrap();
        let found = validate(&plan, &ValidateOptions::default());
        assert!(errors(&found).iter().any(|m| m.contains("changed since staging")), "{found:?}");
    }

    #[test]
    fn drift_says_what_actually_changed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("tree");
        fs::create_dir(&target).unwrap();

        let mut plan = Plan::new(dir.path().to_path_buf());
        plan.ops = vec![Op::Delete {
            path: target.clone(),
            ident: Ident::of(&target).unwrap(),
            est_bytes: 0,
        }];

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(target.join("new"), b"x").unwrap();

        let found = validate(&plan, &ValidateOptions::default());
        let messages = errors(&found);
        assert_eq!(messages.len(), 1, "{found:?}");
        let message = messages[0];
        assert!(message.contains("changed since staging"), "{message}");

        // Whichever detail the message reports, the two values must actually differ. The bug this
        // guards against printed "4.0 KiB -> 4.0 KiB" for a directory whose contents had changed
        // but whose size had not — which is the case on ext4, where the mtime is the only signal.
        // On APFS the directory's size does move, so the message legitimately names that instead;
        // what must never happen is reporting a change by quoting a value against itself.
        if let Some((before, after)) = message.split_once(" -> ") {
            // The value before the arrow is the tail of the sentence: take the same number of
            // words the value after it has, in the order they were written.
            let words = after.split_whitespace().count();
            let mut tail: Vec<&str> = before.rsplit(' ').take(words).collect();
            tail.reverse();
            assert_ne!(
                tail.join(" "),
                after.trim(),
                "the message reports a change by quoting a value against itself: {message}"
            );
        }
    }

    #[test]
    fn an_existing_destination_blocks_a_move() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::write(&src, b"a").unwrap();
        fs::write(&dst, b"b").unwrap();

        let mut plan = Plan::new(dir.path().to_path_buf());
        plan.ops = vec![Op::Move {
            src: src.clone(),
            dst: dst.clone(),
            ident: Ident::of(&src).unwrap(),
            est_bytes: 1,
            on_conflict: Conflict::Fail,
        }];
        let found = validate(&plan, &ValidateOptions::default());
        assert!(errors(&found).iter().any(|m| m.contains("already exists")), "{found:?}");

        // Asking to skip turns it into something to be aware of, not a blocker.
        let Op::Move { on_conflict, .. } = &mut plan.ops[0] else { unreachable!() };
        *on_conflict = Conflict::Skip;
        let found = validate(&plan, &ValidateOptions::default());
        assert!(errors(&found).is_empty(), "{found:?}");
        assert!(found.iter().any(|f| f.message.contains("will be skipped")));
    }

    #[test]
    fn a_missing_destination_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::write(&src, b"a").unwrap();

        let mut plan = Plan::new(dir.path().to_path_buf());
        plan.ops = vec![Op::Move {
            src: src.clone(),
            dst: dir.path().join("nope/deeper/dst"),
            ident: Ident::of(&src).unwrap(),
            est_bytes: 1,
            on_conflict: Conflict::Fail,
        }];
        let found =
            validate(&plan, &ValidateOptions { allow_outside_root: true, ..Default::default() });
        assert!(errors(&found).iter().any(|m| m.contains("does not exist")), "{found:?}");
    }

    #[test]
    fn a_same_filesystem_move_does_not_claim_space() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(dir.path().join("to")).unwrap();
        fs::write(&src, vec![0u8; 4096]).unwrap();

        let mut plan = Plan::new(dir.path().to_path_buf());
        plan.ops = vec![Op::Move {
            src: src.clone(),
            dst: dir.path().join("to/src"),
            ident: Ident::of(&src).unwrap(),
            // Absurdly large: if this were treated as consuming space it would fail the free
            // space check. A rename consumes nothing.
            est_bytes: u64::MAX / 2,
            on_conflict: Conflict::Fail,
        }];
        let found = validate(&plan, &ValidateOptions::default());
        assert!(errors(&found).is_empty(), "{found:?}");
        assert!(
            found.iter().all(|f| !f.message.contains("crosses filesystems")),
            "same-filesystem move should not be reported as a copy: {found:?}"
        );
    }
}
