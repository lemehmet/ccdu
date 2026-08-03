//! Parallel filesystem scanner.
//!
//! Worker threads read directories and stat their entries; a single builder thread owns the
//! [`Tree`] and assembles it from their results. Nothing is locked on the hot path, and the tree is
//! always internally consistent, so a frontend can render it while the scan is still running.
//!
//! Directories are handed to workers as open file descriptors wherever possible, so a subdirectory
//! is reached with one `openat` from its parent rather than by re-walking its path. Symlinks are
//! never followed — not by `openat`, not by `statat`.

use std::ffi::CStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_channel::{unbounded, Receiver, Sender};
use rustix::fd::OwnedFd;
use rustix::fs::{openat, statat, AtFlags, Dir, FileType, Mode, OFlags, Stat, CWD};
use rustix::io::Errno;

use crate::model::{flags, Kind, Meta, NodeId, Tree, ROOT};

/// How a directory is opened: read-only, no symlink traversal, closed across `exec`.
const DIR_FLAGS: OFlags =
    OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

/// Options controlling a scan.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Number of directory-reading threads.
    pub threads: usize,
    /// Do not descend into directories on a different filesystem than the root.
    pub one_file_system: bool,
    /// Entry names to skip entirely (exact match, e.g. `.git`, `node_modules`).
    pub exclude_names: Vec<Vec<u8>>,
    /// Upper bound on directory descriptors held open by the queue at once. Beyond this, pending
    /// directories fall back to being reopened by path, which is slower but cannot exhaust the
    /// process's descriptor limit on very wide trees.
    pub max_open_dirs: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        let threads =
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
        ScanOptions {
            threads,
            one_file_system: false,
            exclude_names: Vec::new(),
            max_open_dirs: 256,
        }
    }
}

/// Periodic progress, for frontends that want to show a live count.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub dirs: u64,
    pub entries: u64,
    pub disk: u64,
    pub current: PathBuf,
}

/// Scan `root`, returning the assembled tree.
///
/// `progress` receives an update every so often; if the receiver is gone, the scan continues.
pub fn scan(
    root: &Path,
    opts: &ScanOptions,
    progress: Option<&Sender<Progress>>,
) -> io::Result<Tree> {
    let root_stat = statat(CWD, root, AtFlags::SYMLINK_NOFOLLOW).map_err(errno_to_io)?;
    let root_meta = meta_from_stat(&root_stat);
    let mut tree = Tree::new(root.to_path_buf(), &root_meta);

    if root_meta.kind != Kind::Dir {
        return Ok(tree);
    }

    let ctx = Ctx {
        opts: opts.clone(),
        root_dev: root_meta.dev,
        budget: FdBudget { count: AtomicUsize::new(0), max: opts.max_open_dirs },
    };

    let (task_tx, task_rx) = unbounded::<Task>();
    let (res_tx, res_rx) = unbounded::<DirResult>();

    task_tx.send(Task { id: ROOT, handle: Handle::Path(root.to_path_buf()) }).ok();
    let mut outstanding = 1usize;

    std::thread::scope(|scope| {
        for _ in 0..opts.threads.max(1) {
            let task_rx: Receiver<Task> = task_rx.clone();
            let res_tx = res_tx.clone();
            let ctx = &ctx;
            scope.spawn(move || {
                for task in task_rx {
                    if res_tx.send(read_dir(task, ctx)).is_err() {
                        break;
                    }
                }
            });
        }
        // The builder holds the only remaining senders; dropping ours lets workers see the channel
        // close once we are done.
        drop(res_tx);
        drop(task_rx);

        let mut state = Builder::new(&ctx);
        let mut since_report = 0u32;

        while outstanding > 0 {
            let Ok(result) = res_rx.recv() else { break };
            outstanding -= 1;
            outstanding += state.absorb(&mut tree, result, &task_tx);

            since_report += 1;
            if since_report >= 64 {
                since_report = 0;
                if let Some(tx) = progress {
                    let root_node = tree.node(ROOT);
                    tx.send(Progress {
                        dirs: state.dirs,
                        entries: tree.len() as u64 - 1,
                        disk: root_node.disk,
                        current: state.last_dir.clone(),
                    })
                    .ok();
                }
            }
        }
        drop(task_tx);
    });

    tree.finish();
    Ok(tree)
}

/// Immutable per-scan state shared by every worker.
struct Ctx {
    opts: ScanOptions,
    root_dev: u64,
    budget: FdBudget,
}

/// Caps how many directory descriptors the pending queue holds at once.
struct FdBudget {
    count: AtomicUsize,
    max: usize,
}

impl FdBudget {
    fn try_acquire(&self) -> bool {
        self.count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < self.max).then_some(n + 1))
            .is_ok()
    }

    fn release(&self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
    }
}

enum Handle {
    /// An already-open descriptor for this directory.
    Fd(OwnedFd),
    /// Reopen by path: used for the root, and when the descriptor budget is exhausted.
    Path(PathBuf),
}

struct Task {
    id: NodeId,
    handle: Handle,
}

/// How the builder should reach a subdirectory's contents.
enum Descend {
    /// Not a directory, or one we deliberately do not enter.
    No,
    Fd(OwnedFd),
    /// A directory we could not keep a descriptor for; queue it by path instead.
    Reopen,
}

struct Entry {
    name: Box<[u8]>,
    meta: Meta,
    /// Could not be stat'd, or is a directory we could not open: its contents are unaccounted for.
    err: bool,
    excluded: bool,
    descend: Descend,
}

struct DirResult {
    id: NodeId,
    entries: Vec<Entry>,
    /// Reading the directory itself failed or was cut short.
    read_error: bool,
    /// This task consumed a descriptor from the budget.
    used_fd: bool,
}

/// Read one directory: stat every entry, and open the subdirectories we intend to descend into.
fn read_dir(task: Task, ctx: &Ctx) -> DirResult {
    let mut out = DirResult { id: task.id, entries: Vec::new(), read_error: false, used_fd: false };

    let fd = match task.handle {
        Handle::Fd(fd) => {
            out.used_fd = true;
            fd
        }
        Handle::Path(path) => match openat(CWD, &path, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(_) => {
                out.read_error = true;
                return out;
            }
        },
    };

    let mut dir = match Dir::read_from(&fd) {
        Ok(dir) => dir,
        Err(_) => {
            out.read_error = true;
            finish_task(&out, ctx);
            return out;
        }
    };

    while let Some(entry) = dir.read() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                out.read_error = true;
                continue;
            }
        };
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }

        let excluded = ctx.opts.exclude_names.iter().any(|e| e == name.to_bytes());

        let (meta, err) = match statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => (meta_from_stat(&st), false),
            Err(_) => (Meta::unknown(kind_of(entry.file_type())), true),
        };

        let mut descend = Descend::No;
        let mut open_failed = false;
        if meta.kind == Kind::Dir && !err && !excluded {
            let foreign = ctx.opts.one_file_system && meta.dev != ctx.root_dev;
            if !foreign {
                descend = open_child(&fd, name, ctx, &mut open_failed);
            }
        }

        out.entries.push(Entry {
            name: name.to_bytes().into(),
            meta,
            err: err || open_failed,
            excluded,
            descend,
        });
    }

    finish_task(&out, ctx);
    out
}

/// Open a subdirectory, degrading to a reopen-by-path when descriptors are scarce.
fn open_child(dir_fd: &OwnedFd, name: &CStr, ctx: &Ctx, open_failed: &mut bool) -> Descend {
    if !ctx.budget.try_acquire() {
        return Descend::Reopen;
    }
    match openat(dir_fd, name, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => Descend::Fd(fd),
        Err(Errno::MFILE) | Err(Errno::NFILE) => {
            ctx.budget.release();
            Descend::Reopen
        }
        Err(_) => {
            ctx.budget.release();
            *open_failed = true;
            Descend::No
        }
    }
}

fn finish_task(out: &DirResult, ctx: &Ctx) {
    if out.used_fd {
        ctx.budget.release();
    }
}

/// Builder-thread state: everything that must be decided globally rather than per directory.
struct Builder<'a> {
    ctx: &'a Ctx,
    /// `(dev, ino)` of every file with `nlink > 1` already counted, so its other links are free.
    hardlinks: std::collections::HashSet<(u64, u64)>,
    /// `(dev, ino)` of every directory entered, which stops bind-mount loops from being scanned
    /// forever. Symlink loops cannot occur: we never follow symlinks.
    visited_dirs: std::collections::HashSet<(u64, u64)>,
    dirs: u64,
    last_dir: PathBuf,
}

impl<'a> Builder<'a> {
    fn new(ctx: &'a Ctx) -> Self {
        Builder {
            ctx,
            hardlinks: std::collections::HashSet::new(),
            visited_dirs: std::collections::HashSet::new(),
            dirs: 0,
            last_dir: PathBuf::new(),
        }
    }

    /// Insert one directory's entries and queue any subdirectories. Returns how many new tasks
    /// were enqueued so the caller can track outstanding work.
    fn absorb(&mut self, tree: &mut Tree, result: DirResult, task_tx: &Sender<Task>) -> usize {
        self.dirs += 1;
        if result.read_error {
            tree.add_flags(result.id, flags::ERR);
            tree.errors += 1;
        }

        let mut queued = 0;
        for entry in result.entries {
            let mut extra = 0u16;
            let mut counted = !entry.err;

            if entry.err {
                extra |= flags::ERR;
                tree.errors += 1;
            }
            if entry.excluded {
                extra |= flags::EXCLUDED;
            }
            if entry.meta.kind == Kind::Dir
                && self.ctx.opts.one_file_system
                && entry.meta.dev != self.ctx.root_dev
            {
                extra |= flags::OTHER_FS;
            }
            if entry.meta.kind == Kind::File && entry.meta.nlink > 1 {
                if self.hardlinks.insert((entry.meta.dev, entry.meta.ino)) {
                    extra |= flags::HARDLINK;
                } else {
                    extra |= flags::HARDLINK_DUP;
                    counted = false;
                }
            }

            let id = tree.push_child(result.id, &entry.name, &entry.meta, extra, counted);

            let fresh = !matches!(entry.descend, Descend::No)
                && self.visited_dirs.insert((entry.meta.dev, entry.meta.ino));
            match entry.descend {
                Descend::No => {}
                Descend::Fd(fd) => {
                    if fresh {
                        task_tx.send(Task { id, handle: Handle::Fd(fd) }).ok();
                        queued += 1;
                    } else {
                        drop(fd);
                        self.ctx.budget.release();
                        tree.add_flags(id, flags::LOOP);
                    }
                }
                Descend::Reopen => {
                    if fresh {
                        let path = tree.path_of(id);
                        task_tx.send(Task { id, handle: Handle::Path(path) }).ok();
                        queued += 1;
                    } else {
                        tree.add_flags(id, flags::LOOP);
                    }
                }
            }
        }

        if self.dirs % 64 == 0 {
            self.last_dir = tree.path_of(result.id);
        }
        queued
    }
}

fn kind_of(ft: FileType) -> Kind {
    match ft {
        FileType::Directory => Kind::Dir,
        FileType::RegularFile => Kind::File,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    }
}

#[allow(clippy::unnecessary_cast)] // `Stat` field widths differ across platforms.
fn meta_from_stat(st: &Stat) -> Meta {
    let kind = kind_of(FileType::from_raw_mode(st.st_mode as _));
    Meta {
        kind,
        apparent: st.st_size.max(0) as u64,
        // `st_blocks` is in 512-byte units by definition, regardless of the filesystem's block
        // size. This is the number that reflects what freeing the file actually gives back.
        disk: (st.st_blocks as u64).saturating_mul(512),
        mtime: st.st_mtime as i64,
        dev: st.st_dev as u64,
        ino: st.st_ino as u64,
        nlink: st.st_nlink as u64,
    }
}

fn errno_to_io(e: Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs as unix_fs;

    fn opts() -> ScanOptions {
        ScanOptions { threads: 2, ..Default::default() }
    }

    /// Walk the tree collecting `relative path -> disk bytes`.
    fn collect(tree: &Tree) -> std::collections::BTreeMap<String, u64> {
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![ROOT];
        while let Some(id) = stack.pop() {
            for child in tree.children(id) {
                let rel = tree
                    .path_of(child)
                    .strip_prefix(tree.root_path())
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.insert(rel, tree.node(child).disk);
                stack.push(child);
            }
        }
        out
    }

    #[test]
    fn scans_a_nested_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/big"), vec![0u8; 8192]).unwrap();
        fs::write(root.join("a/small"), b"hi").unwrap();
        fs::create_dir(root.join("empty")).unwrap();

        let tree = scan(root, &opts(), None).unwrap();
        let found = collect(&tree);

        assert!(found.contains_key("a/b/big"));
        assert!(found.contains_key("empty"));
        assert_eq!(found["a/b/big"], 8192);
        // Every ancestor carries the file's weight.
        assert!(tree.node(ROOT).disk >= 8192);
        assert_eq!(tree.node(ROOT).items as usize, tree.len() - 1);
        assert_eq!(tree.errors, 0);
    }

    #[test]
    fn hardlinks_are_counted_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("original"), vec![0u8; 16384]).unwrap();
        fs::hard_link(root.join("original"), root.join("link1")).unwrap();
        fs::hard_link(root.join("original"), root.join("link2")).unwrap();

        let tree = scan(root, &opts(), None).unwrap();

        let total: u64 = tree.children(ROOT).map(|c| tree.node(c).disk).sum();
        assert_eq!(total, 16384, "three links to one inode must count as one file");

        let dups = tree.children(ROOT).filter(|&c| tree.node(c).has(flags::HARDLINK_DUP)).count();
        assert_eq!(dups, 2);
    }

    #[test]
    fn symlinks_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("real")).unwrap();
        fs::write(root.join("real/f"), vec![0u8; 4096]).unwrap();
        // A link back to the root: following it would recurse forever.
        unix_fs::symlink(root, root.join("real/loop")).unwrap();

        let tree = scan(root, &opts(), None).unwrap();
        let found = collect(&tree);

        assert!(found.contains_key("real/loop"));
        assert!(!found.keys().any(|k| k.starts_with("real/loop/")), "symlink was traversed");
        assert!(tree
            .children(ROOT)
            .flat_map(|c| tree.children(c))
            .any(|c| tree.node(c).has(flags::SYMLINK)));
    }

    #[test]
    fn excluded_names_are_recorded_but_not_entered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/blob"), vec![0u8; 65536]).unwrap();
        fs::write(root.join("keep"), vec![0u8; 1024]).unwrap();

        let o = ScanOptions { exclude_names: vec![b"node_modules".to_vec()], ..opts() };
        let tree = scan(root, &o, None).unwrap();
        let found = collect(&tree);

        assert!(found.contains_key("node_modules"));
        assert!(!found.contains_key("node_modules/pkg"));
        assert!(tree.node(ROOT).disk < 65536);
    }

    #[test]
    fn unreadable_directories_are_flagged_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("locked")).unwrap();
        fs::write(root.join("locked/hidden"), vec![0u8; 2048]).unwrap();
        fs::set_permissions(
            root.join("locked"),
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .unwrap();
        fs::write(root.join("visible"), vec![0u8; 512]).unwrap();

        let tree = scan(root, &opts(), None).unwrap();

        // Running as root defeats the permission bits; only assert when they actually bite.
        let locked =
            tree.children(ROOT).find(|&c| tree.name(c) == std::ffi::OsStr::new("locked")).unwrap();
        if tree.errors > 0 {
            assert!(tree.node(locked).has(flags::ERR));
        }
        assert!(collect(&tree).contains_key("visible"));

        fs::set_permissions(
            root.join("locked"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
    }

    #[test]
    fn descriptor_budget_falls_back_to_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..40 {
            let d = root.join(format!("d{i}"));
            fs::create_dir(&d).unwrap();
            fs::write(d.join("f"), vec![0u8; 1024]).unwrap();
        }

        // A budget of one forces almost every subdirectory down the reopen-by-path path.
        let o = ScanOptions { max_open_dirs: 1, threads: 4, ..Default::default() };
        let tree = scan(root, &o, None).unwrap();
        let found = collect(&tree);

        assert_eq!(found.keys().filter(|k| k.ends_with("/f")).count(), 40);
        assert_eq!(tree.errors, 0);
    }

    #[test]
    fn scanning_a_plain_file_yields_just_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lonely");
        fs::write(&path, vec![0u8; 300]).unwrap();

        let tree = scan(&path, &opts(), None).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.node(ROOT).apparent, 300);
    }
}
