//! Running a plan.
//!
//! Three properties matter here, in this order:
//!
//! 1. **Nothing is destroyed that the user did not review.** Every operation re-checks the identity
//!    recorded at staging time, and acts through `*at` syscalls on a directory descriptor it opened
//!    itself, so a path swapped underneath us cannot redirect a deletion.
//! 2. **An interrupted run is resumable, not ambiguous.** The journal is written ahead of the
//!    syscall it describes, and every operation is idempotent, so replaying is always safe.
//! 3. **Stopping is cheap.** Pause is checked between operations and between entries inside a large
//!    tree, so a commit can be interrupted without waiting for it to finish.

pub mod journal;
pub mod moves;

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::Sender;
use rustix::fd::OwnedFd;
use rustix::fs::{
    accessat, openat, statat, unlinkat, Access, AtFlags, Dir, FileType, Mode, OFlags, CWD,
};
use rustix::io::Errno;

use crate::plan::{EntryKind, Ident, Op, Plan};
use journal::{Event, Journal};
pub use moves::Verify;

const DIR_FLAGS: OFlags =
    OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

/// Deepest tree we will descend into while deleting. Each level costs a descriptor; a tree deeper
/// than this is more likely to be a bug than a directory.
const MAX_DEPTH: usize = 512;

/// Shared stop switch. Pausing is voluntary; recovery treats it exactly like a crash.
#[derive(Debug, Default)]
pub struct Control {
    pause: AtomicBool,
}

impl Control {
    pub fn new() -> Control {
        Control::default()
    }

    pub fn pause(&self) {
        self.pause.store(true, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::Relaxed)
    }
}

/// Points at which a test can interrupt the executor. Named for what has and has not happened yet,
/// because that is the distinction recovery has to get right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultPoint {
    /// Nothing recorded, nothing done.
    BeforeOpBegin,
    /// Intent recorded, nothing done.
    AfterOpBegin,
    /// Part of a tree removed, no completion recorded.
    MidDelete,
    /// Part of a tree copied, nothing published.
    MidCopy,
    /// The copy is published at its destination; the original is still there.
    BeforeSourceRemoval,
    /// Everything done, completion not yet recorded.
    BeforeOpDone,
    /// Everything done and recorded.
    AfterOpDone,
}

/// Called at each [`FaultPoint`]; returning an error stops the run there.
/// The lifetime is explicit so a hook may borrow, which is how tests drive the executor with a
/// closure that touches their own state.
pub type FaultFn<'a> = dyn Fn(FaultPoint, usize) -> io::Result<()> + Sync + 'a;
pub type Fault<'a> = &'a FaultFn<'a>;

#[derive(Default)]
pub struct ExecOptions<'a> {
    /// Check everything, change nothing.
    pub dry_run: bool,
    /// How hard a copied file is checked before its original is removed.
    pub verify: Verify,
    pub fault: Option<Fault<'a>>,
}

impl ExecOptions<'_> {
    fn fault(&self, point: FaultPoint, op: usize) -> io::Result<()> {
        match self.fault {
            Some(hook) => hook(point, op),
            None => Ok(()),
        }
    }
}

/// Progress, for a frontend that wants to show it.
#[derive(Clone, Debug)]
pub enum ExecEvent {
    Started {
        index: usize,
        summary: String,
    },
    Finished {
        index: usize,
        freed: u64,
    },
    Failed {
        index: usize,
        error: String,
    },
    /// Already completed by an earlier run.
    AlreadyDone {
        index: usize,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub done: usize,
    pub failed: usize,
    pub skipped: usize,
    /// Disk bytes actually reclaimed, counted from what was unlinked.
    pub freed: u64,
    pub paused: bool,
}

impl Outcome {
    pub fn is_complete(&self) -> bool {
        !self.paused && self.failed == 0
    }
}

/// What a plan directory's journal says about the last run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    NotStarted,
    /// Started and neither finished nor paused: a crash, or a kill.
    Interrupted,
    Paused,
    Finished,
}

pub fn state(dir: &Path) -> io::Result<RunState> {
    let records = journal::read_dir(dir)?;
    Ok(match records.last().map(|r| &r.event) {
        None => RunState::NotStarted,
        Some(Event::RunEnd { .. }) => RunState::Finished,
        Some(Event::Paused { .. }) => RunState::Paused,
        Some(_) => RunState::Interrupted,
    })
}

/// Totals recovered from a journal, so a resumed run can report the whole commit rather than just
/// its own share of it.
fn replay(dir: &Path) -> io::Result<(HashSet<usize>, HashSet<usize>, Outcome)> {
    let records = journal::read_dir(dir)?;
    let mut terminal = HashSet::new();
    let mut begun = HashSet::new();
    let mut totals = Outcome::default();

    for record in &records {
        match &record.event {
            Event::OpBegin { op } => {
                begun.insert(*op);
            }
            Event::OpDone { op, freed } => {
                terminal.insert(*op);
                totals.done += 1;
                totals.freed += freed;
            }
            Event::OpFailed { op, .. } => {
                terminal.insert(*op);
                totals.failed += 1;
            }
            Event::OpSkipped { op, .. } => {
                terminal.insert(*op);
                totals.skipped += 1;
            }
            // Work an interrupted operation had already done. The resumed run only removes what
            // is left, so adding this is the difference between an honest total and one that
            // understates the commit by whatever the first attempt got through.
            Event::Paused { freed, .. } => totals.freed += freed,
            _ => {}
        }
    }
    // Operations that began but never reached a terminal state were in flight when the run
    // stopped. They get resumed, with a relaxed identity check: any drift is our own doing.
    let in_flight: HashSet<usize> = begun.difference(&terminal).copied().collect();
    Ok((terminal, in_flight, totals))
}

/// Run `plan`, journaling into `dir`. Safe to call again on the same directory: completed
/// operations are recognised and skipped.
pub fn execute(
    plan: &Plan,
    dir: &Path,
    opts: &ExecOptions,
    control: &Control,
    progress: Option<&Sender<ExecEvent>>,
) -> io::Result<Outcome> {
    let (terminal, in_flight, mut totals) = replay(dir)?;

    // A dry run writes nothing — not even to the journal. Recording completions for work that did
    // not happen would mark the plan as executed, and the real run afterwards would skip
    // everything: a rehearsal must not count as the performance.
    let mut journal = if opts.dry_run { None } else { Some(Journal::open(dir)?) };
    record(
        &mut journal,
        Event::RunBegin { plan: plan.id.clone(), pid: std::process::id(), ops: plan.ops.len() },
    )?;

    for index in execution_order(plan) {
        if terminal.contains(&index) {
            send(progress, ExecEvent::AlreadyDone { index });
            continue;
        }
        if control.is_paused() {
            record(&mut journal, Event::Paused { at: index, freed: 0 })?;
            totals.paused = true;
            return Ok(totals);
        }

        let op = &plan.ops[index];
        opts.fault(FaultPoint::BeforeOpBegin, index)?;
        record(&mut journal, Event::OpBegin { op: index })?;
        opts.fault(FaultPoint::AfterOpBegin, index)?;
        send(progress, ExecEvent::Started { index, summary: op.summary() });

        let strict = !in_flight.contains(&index);
        match run_op(op, strict, opts, control, index) {
            Ok(Step::Done(freed)) => {
                opts.fault(FaultPoint::BeforeOpDone, index)?;
                record(&mut journal, Event::OpDone { op: index, freed })?;
                opts.fault(FaultPoint::AfterOpDone, index)?;
                totals.done += 1;
                totals.freed += freed;
                send(progress, ExecEvent::Finished { index, freed });
            }
            Ok(Step::Paused(freed)) => {
                // No completion record: the operation is unfinished and will be redone. What it
                // did reclaim is recorded on the pause instead, because the resumed run can only
                // measure what is left — those entries are already gone. Without this the commit
                // reports whatever the last attempt happened to remove.
                record(&mut journal, Event::Paused { at: index, freed })?;
                totals.freed += freed;
                totals.paused = true;
                return Ok(totals);
            }
            // A simulated crash leaves the journal exactly as a real one would and stops the run.
            Err(OpError::Fault(e)) => return Err(e),
            Err(OpError::Failed(e)) => {
                let error = e.to_string();
                record(&mut journal, Event::OpFailed { op: index, error: error.clone() })?;
                totals.failed += 1;
                send(progress, ExecEvent::Failed { index, error });
            }
        }
    }

    record(
        &mut journal,
        Event::RunEnd {
            done: totals.done,
            failed: totals.failed,
            skipped: totals.skipped,
            freed: totals.freed,
        },
    )?;
    Ok(totals)
}

/// Append to the journal if there is one. A dry run has none.
fn record(journal: &mut Option<Journal>, event: Event) -> io::Result<()> {
    match journal {
        Some(journal) => journal.append(event).map(|_| ()),
        None => Ok(()),
    }
}

/// Moves before deletions, and within each, deepest paths first.
///
/// Moves go first because they preserve data and deletions destroy it: if a run stops half way,
/// what survives should be everything that was going to be kept. Depth ordering means a deletion
/// nested inside another runs before the parent sweeps it away, so the freed bytes are attributed
/// to the operation that actually removed them.
fn execution_order(plan: &Plan) -> Vec<usize> {
    let mut order: Vec<usize> = (0..plan.ops.len()).collect();
    order.sort_by_key(|&i| {
        let op = &plan.ops[i];
        (op.is_delete(), std::cmp::Reverse(op.source().components().count()))
    });
    order
}

fn send(progress: Option<&Sender<ExecEvent>>, event: ExecEvent) {
    if let Some(tx) = progress {
        tx.send(event).ok();
    }
}

enum Step {
    Done(u64),
    Paused(u64),
}

/// An injected fault is a simulated crash, not an operation that failed: it must propagate out of
/// the run untouched rather than be recorded as a failure the way a real error would be.
pub(crate) enum OpError {
    Fault(io::Error),
    Failed(io::Error),
}

impl From<io::Error> for OpError {
    fn from(e: io::Error) -> OpError {
        OpError::Failed(e)
    }
}

fn run_op(
    op: &Op,
    strict: bool,
    opts: &ExecOptions,
    control: &Control,
    index: usize,
) -> Result<Step, OpError> {
    let (path, ident, est_bytes) = match op {
        Op::Delete { path, ident, est_bytes } => (path, ident, est_bytes),
        Op::Move { src, dst, ident, est_bytes, on_conflict } => {
            return moves::run_move(
                &moves::MoveRequest {
                    src,
                    dst,
                    ident,
                    est_bytes: *est_bytes,
                    on_conflict: *on_conflict,
                    strict,
                    index,
                },
                opts,
                control,
            )
        }
    };

    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::other(format!("{} has no parent directory", path.display())).into());
    };
    let name = CString::new(name.as_encoded_bytes())
        .map_err(|_| io::Error::other("entry name contains a NUL"))?;

    // Everything from here happens relative to this descriptor. Resolving the parent follows
    // symlinks in intermediate components, which it must — `/var/run` is a symlink on many
    // systems — but once it is open, nothing can substitute a different directory underneath us,
    // and the identity check below settles what the final component is.
    let parent_fd = match openat(CWD, parent, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(Step::Done(0)),
        Err(e) => {
            return Err(
                io::Error::other(format!("opening {}: {}", parent.display(), errno(e))).into()
            )
        }
    };

    let stat = match statat(&parent_fd, name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        // Already gone. A resumed run reaches this constantly, and it is exactly what we wanted.
        Err(Errno::NOENT) => return Ok(Step::Done(0)),
        Err(e) => return Err(io::Error::other(format!("{}: {}", path.display(), errno(e))).into()),
    };

    let now = ident_of(&stat);
    if !matches(ident, &now, strict) {
        return Err(io::Error::other(format!(
            "{} is not what was staged: {}",
            path.display(),
            difference(ident, &now)
        ))
        .into());
    }

    if opts.dry_run {
        return Ok(Step::Done(*est_bytes));
    }

    if now.kind == EntryKind::Dir {
        remove_tree(&parent_fd, name.as_c_str(), blocks(&stat), opts, control, index)
    } else {
        unlinkat(&parent_fd, name.as_c_str(), AtFlags::empty())
            .map_err(|e| io::Error::other(format!("{}: {}", path.display(), errno(e))))?;
        opts.fault(FaultPoint::MidDelete, index).map_err(OpError::Fault)?;
        Ok(Step::Done(blocks(&stat)))
    }
}

/// Does the entry still match what was staged?
///
/// Device and inode must always agree: that is what makes it the same object rather than a
/// different file at the same path. Size and mtime are only enforced for an operation that has not
/// started, because once we begin emptying a directory we are the reason its mtime moved.
fn matches(staged: &Ident, now: &Ident, strict: bool) -> bool {
    if staged.dev != now.dev || staged.ino != now.ino || staged.kind != now.kind {
        return false;
    }
    !strict || (staged.size == now.size && staged.mtime == now.mtime)
}

fn difference(staged: &Ident, now: &Ident) -> String {
    if staged.ino != now.ino || staged.dev != now.dev {
        "a different file now occupies this path".to_string()
    } else if staged.kind != now.kind {
        format!("now a {:?}", now.kind).to_lowercase()
    } else {
        "it has been modified since it was staged".to_string()
    }
}

struct Child {
    name: CString,
    is_dir: bool,
    blocks: u64,
}

struct Frame {
    fd: OwnedFd,
    /// This directory's name within its own parent.
    name: CString,
    blocks: u64,
    children: Vec<Child>,
    pos: usize,
}

enum Next {
    Descend(Frame),
    Unlinked(u64),
    Pop,
}

/// Remove a directory and everything under it, deepest entry first.
///
/// Iterative rather than recursive: a pathological tree should fail the depth check, not overflow
/// the stack of a process that is in the middle of deleting things.
fn remove_tree(
    parent_fd: &OwnedFd,
    name: &CStr,
    blocks: u64,
    opts: &ExecOptions,
    control: &Control,
    index: usize,
) -> Result<Step, OpError> {
    let mut freed = 0u64;
    let mut announced = false;
    let mut stack = vec![read_frame(parent_fd, name.to_owned(), blocks)?];

    while !stack.is_empty() {
        if control.is_paused() {
            return Ok(Step::Paused(freed));
        }

        let next = {
            let top = stack.last_mut().expect("checked above");
            if top.pos < top.children.len() {
                let child = &top.children[top.pos];
                let (child_name, is_dir, child_blocks) =
                    (child.name.clone(), child.is_dir, child.blocks);
                top.pos += 1;

                if is_dir {
                    Next::Descend(read_frame(&top.fd, child_name, child_blocks)?)
                } else {
                    match unlinkat(&top.fd, child_name.as_c_str(), AtFlags::empty()) {
                        // Something else removed it first; the goal was that it be gone.
                        Ok(()) | Err(Errno::NOENT) => Next::Unlinked(child_blocks),
                        Err(e) => {
                            return Err(io::Error::other(format!(
                                "removing {:?}: {}",
                                child_name,
                                errno(e)
                            ))
                            .into())
                        }
                    }
                }
            } else {
                Next::Pop
            }
        };

        match next {
            Next::Descend(frame) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(io::Error::other(format!(
                        "directory nesting deeper than {MAX_DEPTH} levels"
                    ))
                    .into());
                }
                stack.push(frame);
            }
            Next::Unlinked(bytes) => {
                freed += bytes;
                if !announced {
                    announced = true;
                    opts.fault(FaultPoint::MidDelete, index).map_err(OpError::Fault)?;
                }
            }
            Next::Pop => {
                let frame = stack.pop().expect("checked above");
                drop(frame.fd);
                let dir = stack.last().map(|f| &f.fd).unwrap_or(parent_fd);
                match unlinkat(dir, frame.name.as_c_str(), AtFlags::REMOVEDIR) {
                    Ok(()) | Err(Errno::NOENT) => freed += frame.blocks,
                    Err(e) => {
                        return Err(io::Error::other(format!(
                            "removing directory {:?}: {}",
                            frame.name,
                            errno(e)
                        ))
                        .into())
                    }
                }
            }
        }
    }
    Ok(Step::Done(freed))
}

/// Open a directory and list what has to come out of it.
///
/// The write check is what stops a removal from destroying half a tree and then failing: entries
/// are unlinked from their parent, so a directory we cannot write to will refuse the operation
/// anyway — better to find that out before its children are gone than after. It is a preflight,
/// not a guarantee: a read-only mount or an immutable flag still surfaces later, as a failure of
/// the operation rather than of the whole run.
fn read_frame(dir_fd: &OwnedFd, name: CString, blocks: u64) -> io::Result<Frame> {
    let fd = openat(dir_fd, name.as_c_str(), DIR_FLAGS, Mode::empty())
        .map_err(|e| io::Error::other(format!("opening {name:?}: {}", errno(e))))?;

    if accessat(&fd, ".", Access::WRITE_OK, AtFlags::empty()).is_err() {
        return Err(io::Error::other(format!("no write permission on {name:?}")));
    }

    let mut children = Vec::new();
    let mut dir = Dir::read_from(&fd)
        .map_err(|e| io::Error::other(format!("reading {name:?}: {}", errno(e))))?;
    while let Some(entry) = dir.read() {
        let entry =
            entry.map_err(|e| io::Error::other(format!("reading {name:?}: {}", errno(e))))?;
        let child = entry.file_name();
        if child == c"." || child == c".." {
            continue;
        }
        // `st_blocks` has to come from a stat regardless, so the directory hint is not worth
        // consulting separately.
        let stat = match statat(&fd, child, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => continue,
            Err(e) => return Err(io::Error::other(format!("stat {child:?}: {}", errno(e)))),
        };
        let is_dir = ident_of(&stat).kind == EntryKind::Dir;
        children.push(Child { name: child.to_owned(), is_dir, blocks: blocks_of(&stat) });
    }

    Ok(Frame { fd, name, blocks, children, pos: 0 })
}

#[allow(clippy::unnecessary_cast)] // `Stat` field widths differ across platforms.
fn ident_of(stat: &rustix::fs::Stat) -> Ident {
    Ident {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
        size: stat.st_size.max(0) as u64,
        mtime: stat.st_mtime as i64,
        kind: match FileType::from_raw_mode(stat.st_mode as _) {
            FileType::Directory => EntryKind::Dir,
            FileType::RegularFile => EntryKind::File,
            FileType::Symlink => EntryKind::Symlink,
            _ => EntryKind::Other,
        },
    }
}

#[allow(clippy::unnecessary_cast)]
fn blocks_of(stat: &rustix::fs::Stat) -> u64 {
    (stat.st_blocks as u64).saturating_mul(512)
}

fn blocks(stat: &rustix::fs::Stat) -> u64 {
    blocks_of(stat)
}

/// Remove a tree without checking for a pause.
///
/// Used once a move has published its copy: at that point the original is redundant, and stopping
/// half way would leave two copies of the data and an operation that looks unfinished.
pub(super) fn remove_tree_uninterruptible(
    parent_fd: &OwnedFd,
    name: &CStr,
    blocks: u64,
) -> Result<u64, OpError> {
    let opts = ExecOptions::default();
    let control = Control::new();
    match remove_tree(parent_fd, name, blocks, &opts, &control, usize::MAX)? {
        Step::Done(freed) | Step::Paused(freed) => Ok(freed),
    }
}

fn errno(e: Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

#[cfg(test)]
mod move_tests;
#[cfg(test)]
mod tests;
