//! Plans: the staged set of changes, and everything that decides whether they are safe to run.
//!
//! A plan is a document, not an action. Building one touches nothing; only the executor does, and
//! it re-checks every [`Ident`] immediately before acting so a path swapped underneath us aborts
//! that operation instead of destroying whatever now occupies it.

mod path_repr;
pub mod store;
mod validate;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::model::Kind;

pub use validate::{default_protected, validate, Finding, Severity, ValidateOptions};

/// Bumped when the on-disk format changes incompatibly. A plan from the future is refused rather
/// than half-understood.
pub const PLAN_VERSION: u32 = 1;

/// What an entry looked like when it was staged.
///
/// Re-checked before the operation runs: if any of it has changed, the path is not the thing the
/// user reviewed, and we refuse to touch it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ident {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    #[serde(rename = "type")]
    pub kind: EntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
    Other,
}

impl From<Kind> for EntryKind {
    fn from(k: Kind) -> Self {
        match k {
            Kind::Dir => EntryKind::Dir,
            Kind::File => EntryKind::File,
            Kind::Symlink => EntryKind::Symlink,
            Kind::Other => EntryKind::Other,
        }
    }
}

impl Ident {
    /// Read the identity of `path` without following a final symlink.
    pub fn of(path: &Path) -> io::Result<Ident> {
        use rustix::fs::{statat, AtFlags, FileType, CWD};
        let st = statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
        #[allow(clippy::unnecessary_cast)] // `Stat` field widths differ across platforms.
        Ok(Ident {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            size: st.st_size.max(0) as u64,
            mtime: st.st_mtime as i64,
            kind: match FileType::from_raw_mode(st.st_mode as _) {
                FileType::Directory => EntryKind::Dir,
                FileType::RegularFile => EntryKind::File,
                FileType::Symlink => EntryKind::Symlink,
                _ => EntryKind::Other,
            },
        })
    }
}

/// What to do if a move's destination already exists by the time it runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Conflict {
    /// Refuse the operation. The only safe default: the alternative silently destroys data.
    #[default]
    Fail,
    /// Leave the source in place and mark the operation done.
    Skip,
}

/// One staged change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Op {
    Delete {
        #[serde(with = "path_repr")]
        path: PathBuf,
        ident: Ident,
        /// Disk bytes this is expected to reclaim, as measured during the scan.
        est_bytes: u64,
    },
    Move {
        #[serde(with = "path_repr")]
        src: PathBuf,
        #[serde(with = "path_repr")]
        dst: PathBuf,
        ident: Ident,
        est_bytes: u64,
        #[serde(default)]
        on_conflict: Conflict,
    },
}

impl Op {
    /// The path this operation reads from or removes.
    pub fn source(&self) -> &Path {
        match self {
            Op::Delete { path, .. } => path,
            Op::Move { src, .. } => src,
        }
    }

    /// The path this operation writes to, if any.
    pub fn destination(&self) -> Option<&Path> {
        match self {
            Op::Delete { .. } => None,
            Op::Move { dst, .. } => Some(dst),
        }
    }

    pub fn ident(&self) -> &Ident {
        match self {
            Op::Delete { ident, .. } | Op::Move { ident, .. } => ident,
        }
    }

    pub fn est_bytes(&self) -> u64 {
        match self {
            Op::Delete { est_bytes, .. } | Op::Move { est_bytes, .. } => *est_bytes,
        }
    }

    pub fn is_delete(&self) -> bool {
        matches!(self, Op::Delete { .. })
    }

    /// A one-line description for listings.
    pub fn summary(&self) -> String {
        match self {
            Op::Delete { path, .. } => format!("delete {}", path.display()),
            Op::Move { src, dst, .. } => format!("move   {} -> {}", src.display(), dst.display()),
        }
    }
}

/// A reviewable set of changes against one scanned tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub version: u32,
    pub id: String,
    /// Unix seconds.
    pub created: i64,
    /// Host the paths belong to. A plan is refused elsewhere unless forced, because an inode
    /// number means nothing on another machine.
    pub host: String,
    #[serde(with = "path_repr")]
    pub root: PathBuf,
    pub ops: Vec<Op>,
}

impl Plan {
    /// A plan against this machine's own filesystem.
    pub fn new(root: PathBuf) -> Plan {
        Plan::for_host(root, hostname())
    }

    /// A plan against `host`'s filesystem.
    ///
    /// The host is not a label: validation refuses a plan whose host is not the machine running
    /// it, because an inode number means nothing elsewhere and a path that happens to exist on
    /// both would otherwise be acted on here. Anything staged against a tree fetched from
    /// somewhere else must record where that tree came from.
    pub fn for_host(root: PathBuf, host: String) -> Plan {
        Plan {
            version: PLAN_VERSION,
            id: new_id(),
            created: now_secs(),
            host,
            root,
            ops: Vec::new(),
        }
    }

    /// Bytes the plan expects to free: deletes reclaim, and so do moves, but only if the
    /// destination is on another filesystem. Callers that know better should say so.
    pub fn delete_bytes(&self) -> u64 {
        self.ops.iter().filter(|o| o.is_delete()).map(|o| o.est_bytes()).sum()
    }

    pub fn move_bytes(&self) -> u64 {
        self.ops.iter().filter(|o| !o.is_delete()).map(|o| o.est_bytes()).sum()
    }

    /// Drop operations made redundant by another: a delete inside a deleted directory happens
    /// anyway, and staging both is a normal consequence of marking a tree and then a file in it.
    ///
    /// Returns the number removed.
    pub fn normalize(&mut self) -> usize {
        let deleted_dirs: Vec<PathBuf> = self
            .ops
            .iter()
            .filter(|o| o.is_delete() && o.ident().kind == EntryKind::Dir)
            .map(|o| o.source().to_path_buf())
            .collect();

        let before = self.ops.len();
        self.ops.retain(|op| {
            let src = op.source();
            !deleted_dirs.iter().any(|dir| src != dir && src.starts_with(dir))
        });
        before - self.ops.len()
    }
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn hostname() -> String {
    rustix::system::uname().nodename().to_string_lossy().into_owned()
}

/// A sortable, human-readable identifier: `20260803T201530-3f2a9c1e`.
///
/// Sortable so `plan list` is chronological without parsing, and readable so it can be typed.
fn new_id() -> String {
    use std::hash::{BuildHasher, RandomState};
    let secs = now_secs();
    let (y, mo, d, h, mi, s) = civil(secs);
    // `RandomState` is seeded from the OS, which is enough to keep two plans created in the same
    // second apart without pulling in a random-number dependency.
    let salt = RandomState::new().hash_one(secs) as u32;
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}-{salt:08x}")
}

fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 };
    (year, month, day, (rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(kind: EntryKind) -> Ident {
        Ident { dev: 66306, ino: 42, size: 10, mtime: 1_700_000_000, kind }
    }

    fn del(path: &str, kind: EntryKind) -> Op {
        Op::Delete { path: PathBuf::from(path), ident: ident(kind), est_bytes: 100 }
    }

    #[test]
    fn ids_are_sortable_and_distinct() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b, "two plans in the same second collided");
        assert_eq!(a.len(), 24);
        assert!(a.starts_with("20"), "{a}");
    }

    #[test]
    fn normalize_drops_entries_inside_a_deleted_directory() {
        let mut plan = Plan::new(PathBuf::from("/data"));
        plan.ops = vec![
            del("/data/cache", EntryKind::Dir),
            del("/data/cache/a.bin", EntryKind::File),
            del("/data/cache/sub", EntryKind::Dir),
            del("/data/keep.bin", EntryKind::File),
        ];
        assert_eq!(plan.normalize(), 2);
        assert_eq!(plan.ops.len(), 2);
        assert_eq!(plan.ops[0].source(), Path::new("/data/cache"));
        assert_eq!(plan.ops[1].source(), Path::new("/data/keep.bin"));
    }

    #[test]
    fn normalize_does_not_confuse_sibling_prefixes() {
        let mut plan = Plan::new(PathBuf::from("/data"));
        // "/data/cache2" starts with the string "/data/cache" but is not inside it.
        plan.ops = vec![del("/data/cache", EntryKind::Dir), del("/data/cache2", EntryKind::Dir)];
        assert_eq!(plan.normalize(), 0);
    }

    #[test]
    fn normalize_keeps_moves_out_of_deleted_trees_but_drops_ones_inside() {
        let mut plan = Plan::new(PathBuf::from("/data"));
        plan.ops = vec![
            del("/data/old", EntryKind::Dir),
            Op::Move {
                src: PathBuf::from("/data/old/keepme"),
                dst: PathBuf::from("/mnt/big/keepme"),
                ident: ident(EntryKind::File),
                est_bytes: 5,
                on_conflict: Conflict::Fail,
            },
        ];
        // Moving something out of a directory that is about to be deleted is contradictory; the
        // delete wins because it is the coarser statement of intent.
        assert_eq!(plan.normalize(), 1);
        assert!(plan.ops[0].is_delete());
    }

    #[test]
    fn totals_split_deletes_from_moves() {
        let mut plan = Plan::new(PathBuf::from("/data"));
        plan.ops = vec![
            del("/data/a", EntryKind::File),
            Op::Move {
                src: PathBuf::from("/data/b"),
                dst: PathBuf::from("/mnt/b"),
                ident: ident(EntryKind::File),
                est_bytes: 250,
                on_conflict: Conflict::Fail,
            },
        ];
        assert_eq!(plan.delete_bytes(), 100);
        assert_eq!(plan.move_bytes(), 250);
    }

    #[test]
    fn round_trips_through_json() {
        let mut plan = Plan::new(PathBuf::from("/data"));
        plan.ops = vec![
            del("/data/a", EntryKind::File),
            Op::Move {
                src: PathBuf::from("/data/b"),
                dst: PathBuf::from("/mnt/b"),
                ident: ident(EntryKind::Dir),
                est_bytes: 7,
                on_conflict: Conflict::Skip,
            },
        ];
        let json = serde_json::to_string_pretty(&plan).unwrap();
        assert!(json.contains("\"kind\": \"delete\""), "{json}");
        assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), plan);
    }

    #[test]
    fn identity_is_read_without_following_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, vec![0u8; 1234]).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(Ident::of(&target).unwrap().kind, EntryKind::File);
        assert_eq!(Ident::of(&target).unwrap().size, 1234);
        assert_eq!(Ident::of(&link).unwrap().kind, EntryKind::Symlink);
    }
}
