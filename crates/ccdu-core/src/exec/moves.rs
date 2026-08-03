//! Moving.
//!
//! Three paths, cheapest first:
//!
//! 1. **Rename.** Same filesystem: instant, atomic, consumes nothing.
//! 2. **Reflink.** Different `st_dev` but the same underlying filesystem — btrfs subvolumes are the
//!    common case, where `rename` fails with `EXDEV` but the data can still be shared rather than
//!    duplicated.
//! 3. **Copy, verify, then unlink the source.** In that order, always. The source is the only copy
//!    of the data until the destination is durable and checked, so it is the last thing to go.
//!
//! A cross-filesystem move assembles the destination under a temporary name and renames it into
//! place only once it is complete. A half-copied tree therefore never appears at the destination
//! path, and an interrupted move resumes into the same temporary rather than starting again.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io;
use std::path::{Path, PathBuf};

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{
    fchmod, fchown, fsync, ftruncate, futimens, linkat, mkdirat, openat, readlinkat,
    renameat, seek, statat, symlinkat, unlinkat, AtFlags, Dir, FileType, Gid, Mode, OFlags,
    SeekFrom, Stat, Timestamps, Uid, CWD,
};
use rustix::io::{read, write, Errno};

use super::{
    blocks_of, errno, ident_of, Control, ExecOptions, FaultPoint, OpError, Step, DIR_FLAGS,
};
use crate::plan::{Conflict, EntryKind, Ident};

/// How thoroughly a copied file is checked before the original is removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Verify {
    /// Length must match. Catches truncation, which is what an interrupted copy produces.
    #[default]
    Size,
    /// Read both copies back and compare digests. Slower by a full re-read of both sides.
    Hash,
}

/// Chunk size for the read/write fallback, and the interval at which a pause is noticed.
const CHUNK: usize = 8 * 1024 * 1024;

/// Deepest tree a move will descend into. Mirrors the deletion limit.
const MAX_DEPTH: usize = 512;

pub(super) struct MoveRequest<'a> {
    pub src: &'a Path,
    pub dst: &'a Path,
    pub ident: &'a Ident,
    pub est_bytes: u64,
    pub on_conflict: Conflict,
    /// False for an operation being resumed, whose own progress explains any drift.
    pub strict: bool,
    pub index: usize,
}

pub(super) fn run_move(
    req: &MoveRequest,
    opts: &ExecOptions,
    control: &Control,
) -> Result<Step, OpError> {
    let (Some(src_parent), Some(src_name)) = (req.src.parent(), req.src.file_name()) else {
        return Err(io::Error::other(format!("{} has no parent", req.src.display())).into());
    };
    let (Some(dst_parent), Some(dst_name)) = (req.dst.parent(), req.dst.file_name()) else {
        return Err(io::Error::other(format!("{} has no parent", req.dst.display())).into());
    };
    let src_name = cstring(src_name)?;
    let dst_name = cstring(dst_name)?;

    let dst_dir = match openat(CWD, dst_parent, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            return Err(io::Error::other(format!(
                "opening destination directory {}: {}",
                dst_parent.display(),
                errno(e)
            ))
            .into())
        }
    };

    let src_dir = match openat(CWD, src_parent, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return already_moved(&dst_dir, &dst_name, req),
        Err(e) => {
            return Err(
                io::Error::other(format!("opening {}: {}", src_parent.display(), errno(e))).into()
            )
        }
    };

    let src_stat = match statat(&src_dir, src_name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        // The source is gone. Either we finished and were interrupted before recording it, or
        // something else took it; the destination decides which.
        Err(Errno::NOENT) => return already_moved(&dst_dir, &dst_name, req),
        Err(e) => {
            return Err(io::Error::other(format!("{}: {}", req.src.display(), errno(e))).into())
        }
    };

    let now = ident_of(&src_stat);
    if !super::matches(req.ident, &now, req.strict) {
        return Err(io::Error::other(format!(
            "{} is not what was staged: {}",
            req.src.display(),
            super::difference(req.ident, &now)
        ))
        .into());
    }

    match statat(&dst_dir, dst_name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
        // Source and destination both present, on an operation we know we started: the copy was
        // published and we were interrupted before reclaiming the original. Finish that.
        Ok(_) if !req.strict => {
            return remove_source(req, &src_dir, &src_name, &src_stat);
        }
        Ok(_) if req.on_conflict == Conflict::Skip => return Ok(Step::Done(0)),
        Ok(_) => {
            return Err(io::Error::other(format!("{} already exists", req.dst.display())).into())
        }
        Err(Errno::NOENT) => {}
        Err(e) => {
            return Err(io::Error::other(format!("{}: {}", req.dst.display(), errno(e))).into())
        }
    }

    if opts.dry_run {
        return Ok(Step::Done(req.est_bytes));
    }

    // The cheap path: one filesystem, one syscall, nothing copied.
    match rename_into_place(&src_dir, &src_name, &dst_dir, &dst_name) {
        Ok(()) => return Ok(Step::Done(0)),
        // Not one filesystem after all — or not one the kernel will rename across.
        Err(Errno::XDEV) => {}
        Err(e) => {
            return Err(io::Error::other(format!(
                "renaming {} to {}: {}",
                req.src.display(),
                req.dst.display(),
                errno(e)
            ))
            .into())
        }
    }

    copy_then_remove(req, opts, control, &src_dir, &src_name, &src_stat, &dst_dir, &dst_name)
}

/// The source is missing. If the destination is there, an earlier attempt finished the job.
fn already_moved(dst_dir: &OwnedFd, dst_name: &CStr, req: &MoveRequest) -> Result<Step, OpError> {
    match statat(dst_dir, dst_name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(Step::Done(0)),
        _ => Err(io::Error::other(format!(
            "{} is gone and nothing arrived at {}",
            req.src.display(),
            req.dst.display()
        ))
        .into()),
    }
}

fn rename_into_place(
    src_dir: &OwnedFd,
    src_name: &CStr,
    dst_dir: &OwnedFd,
    dst_name: &CStr,
) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{renameat_with, RenameFlags};
        // NOREPLACE closes the window between checking that the destination is free and taking it.
        match renameat_with(src_dir, src_name, dst_dir, dst_name, RenameFlags::NOREPLACE) {
            // Filesystems that do not implement the flag fall back to the plain call, which is
            // still safe: the destination was checked a moment ago.
            Err(Errno::INVAL) | Err(Errno::NOSYS) | Err(Errno::OPNOTSUPP) => {}
            other => return other,
        }
    }
    renameat(src_dir, src_name, dst_dir, dst_name)
}

/// Copy to a temporary beside the destination, rename it into place, then remove the source.
#[allow(clippy::too_many_arguments)]
fn copy_then_remove(
    req: &MoveRequest,
    opts: &ExecOptions,
    control: &Control,
    src_dir: &OwnedFd,
    src_name: &CStr,
    src_stat: &Stat,
    dst_dir: &OwnedFd,
    dst_name: &CStr,
) -> Result<Step, OpError> {
    // Deterministic, so a resumed run finds the same partial without consulting the journal.
    let temp = temp_name(req.index, dst_name)?;

    let copied = match ident_of(src_stat).kind {
        EntryKind::Dir => {
            copy_tree(req, opts, control, src_dir, src_name, src_stat, dst_dir, &temp)?
        }
        EntryKind::File => {
            copy_regular(req, opts, control, src_dir, src_name, src_stat, dst_dir, &temp)?
        }
        EntryKind::Symlink => {
            copy_symlink(src_dir, src_name, dst_dir, &temp)?;
            Step::Done(0)
        }
        EntryKind::Other => {
            return Err(io::Error::other(format!(
                "{} is a special file; ccdu will not pretend to have copied it",
                req.src.display()
            ))
            .into())
        }
    };
    if let Step::Paused(bytes) = copied {
        // Nothing is removed and nothing is renamed: the temporary stays for the resumed run.
        return Ok(Step::Paused(bytes));
    }

    opts.fault(FaultPoint::BeforeSourceRemoval, req.index).map_err(OpError::Fault)?;

    // Only now is the destination allowed to exist under its real name.
    renameat(dst_dir, temp.as_c_str(), dst_dir, dst_name)
        .map_err(|e| io::Error::other(format!("publishing {}: {}", req.dst.display(), errno(e))))?;
    fsync(dst_dir).ok();

    remove_source(req, src_dir, src_name, src_stat)
}

/// Reclaim the original once the copy is safely in place.
///
/// Deliberately not interruptible: the destination already holds the data, so stopping half way
/// would leave two copies and a plan that looks unfinished. It is idempotent, so a crash here
/// costs nothing but a second pass.
fn remove_source(
    req: &MoveRequest,
    src_dir: &OwnedFd,
    src_name: &CStr,
    src_stat: &Stat,
) -> Result<Step, OpError> {
    let freed = match ident_of(src_stat).kind {
        EntryKind::Dir => {
            super::remove_tree_uninterruptible(src_dir, src_name, blocks_of(src_stat))?
        }
        _ => {
            let freed = blocks_of(src_stat);
            unlinkat(src_dir, src_name, AtFlags::empty()).map_err(|e| {
                io::Error::other(format!("removing {}: {}", req.src.display(), errno(e)))
            })?;
            freed
        }
    };
    Ok(Step::Done(freed))
}

fn temp_name(index: usize, dst_name: &CStr) -> Result<CString, OpError> {
    let mut bytes = format!(".ccdu-part-{index}-").into_bytes();
    bytes.extend_from_slice(dst_name.to_bytes());
    CString::new(bytes).map_err(|_| io::Error::other("destination name contains a NUL").into())
}

struct CopyFrame {
    src: OwnedFd,
    dst: OwnedFd,
    /// Absolute path of this directory in the temporary tree, for hardlink targets.
    dst_path: PathBuf,
    children: Vec<CString>,
    pos: usize,
}

/// Copy a whole directory tree into `temp` under `dst_dir`.
#[allow(clippy::too_many_arguments)]
fn copy_tree(
    req: &MoveRequest,
    opts: &ExecOptions,
    control: &Control,
    src_dir: &OwnedFd,
    src_name: &CStr,
    src_stat: &Stat,
    dst_dir: &OwnedFd,
    temp: &CStr,
) -> Result<Step, OpError> {
    let root_src = openat(src_dir, src_name, DIR_FLAGS, Mode::empty())
        .map_err(|e| io::Error::other(format!("opening {}: {}", req.src.display(), errno(e))))?;
    let root_dst = ensure_dir(dst_dir, temp, mode_of(src_stat))?;

    let mut links: HashMap<(u64, u64), PathBuf> = HashMap::new();
    let mut copied_bytes = 0u64;
    let mut announced = false;

    let temp_root = req
        .dst
        .parent()
        .unwrap_or(Path::new("/"))
        .join(String::from_utf8_lossy(temp.to_bytes()).into_owned());
    let mut stack = vec![CopyFrame {
        children: list(&root_src)?,
        src: root_src,
        dst: root_dst,
        dst_path: temp_root,
        pos: 0,
    }];

    while !stack.is_empty() {
        if control.is_paused() {
            return Ok(Step::Paused(copied_bytes));
        }

        let top = stack.last_mut().expect("checked above");
        if top.pos >= top.children.len() {
            let frame = stack.pop().expect("checked above");
            // Timestamps last: creating entries inside a directory moves its mtime.
            copy_times(&frame.dst, &stat_fd(&frame.src)?);
            continue;
        }

        let name = top.children[top.pos].clone();
        top.pos += 1;

        let stat = match statat(&top.src, name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            // Vanished while we walked; there is nothing to move.
            Err(Errno::NOENT) => continue,
            Err(e) => return Err(io::Error::other(format!("stat {name:?}: {}", errno(e))).into()),
        };

        // Decided while `top` is borrowed, applied after it is released.
        let descend = match ident_of(&stat).kind {
            EntryKind::Dir => {
                let child_src = openat(&top.src, name.as_c_str(), DIR_FLAGS, Mode::empty())
                    .map_err(|e| io::Error::other(format!("opening {name:?}: {}", errno(e))))?;
                let child_dst = ensure_dir(&top.dst, name.as_c_str(), mode_of(&stat))?;
                let dst_path = top.dst_path.join(os_str(&name));
                Some(CopyFrame {
                    children: list(&child_src)?,
                    src: child_src,
                    dst: child_dst,
                    dst_path,
                    pos: 0,
                })
            }
            EntryKind::Symlink => {
                copy_symlink(&top.src, name.as_c_str(), &top.dst, name.as_c_str())?;
                None
            }
            EntryKind::File => {
                // A file with more than one link inside this tree must stay one file on the other
                // side; copying it twice would silently double its size.
                let key = (ident_of(&stat).dev, ident_of(&stat).ino);
                let mut linked = false;
                if stat.st_nlink > 1 {
                    if let Some(first) = links.get(&key) {
                        link_to(first, &top.dst, name.as_c_str())?;
                        linked = true;
                    } else {
                        links.insert(key, top.dst_path.join(os_str(&name)));
                    }
                }
                if !linked {
                    copied_bytes += copy_file(
                        &top.src,
                        name.as_c_str(),
                        &stat,
                        &top.dst,
                        name.as_c_str(),
                        opts,
                        control,
                    )?;
                    if !announced {
                        announced = true;
                        opts.fault(FaultPoint::MidCopy, req.index).map_err(OpError::Fault)?;
                    }
                }
                None
            }
            EntryKind::Other => {
                if recreate_fifo(&top.dst, name.as_c_str(), &stat) {
                    None
                } else {
                    // Sockets and device nodes cannot be honestly reproduced, and a move that
                    // silently dropped one would then delete the original.
                    return Err(io::Error::other(format!(
                        "{name:?} is a special file; ccdu will not pretend to have copied it"
                    ))
                    .into());
                }
            }
        };

        if let Some(frame) = descend {
            if stack.len() >= MAX_DEPTH {
                return Err(io::Error::other(format!(
                    "directory nesting deeper than {MAX_DEPTH} levels"
                ))
                .into());
            }
            stack.push(frame);
        }
    }

    Ok(Step::Done(copied_bytes))
}

#[allow(clippy::too_many_arguments)]
fn copy_regular(
    req: &MoveRequest,
    opts: &ExecOptions,
    control: &Control,
    src_dir: &OwnedFd,
    src_name: &CStr,
    src_stat: &Stat,
    dst_dir: &OwnedFd,
    temp: &CStr,
) -> Result<Step, OpError> {
    let copied = copy_file(src_dir, src_name, src_stat, dst_dir, temp, opts, control)?;
    if control.is_paused() {
        return Ok(Step::Paused(copied));
    }
    opts.fault(FaultPoint::MidCopy, req.index).map_err(OpError::Fault)?;
    Ok(Step::Done(copied))
}

/// Copy one regular file, preserving holes, permissions and timestamps.
fn copy_file(
    src_dir: &OwnedFd,
    src_name: &CStr,
    src_stat: &Stat,
    dst_dir: &OwnedFd,
    dst_name: &CStr,
    opts: &ExecOptions,
    control: &Control,
) -> Result<u64, OpError> {
    let size = src_stat.st_size.max(0) as u64;

    // A file already sitting there at the right length is one an earlier attempt finished. A
    // short one is a torn copy: start it again rather than guess where it stopped.
    if let Ok(existing) = statat(dst_dir, dst_name, AtFlags::SYMLINK_NOFOLLOW) {
        if existing.st_size.max(0) as u64 == size {
            return Ok(0);
        }
        unlinkat(dst_dir, dst_name, AtFlags::empty()).ok();
    }

    let src = openat(
        src_dir,
        src_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io::Error::other(format!("opening {src_name:?}: {}", errno(e))))?;
    // Read-write, not write-only: hash verification has to read the copy back, and a check that
    // cannot open what it is checking is not a check.
    let dst = openat(
        dst_dir,
        dst_name,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        mode_of(src_stat),
    )
    .map_err(|e| io::Error::other(format!("creating {dst_name:?}: {}", errno(e))))?;

    // Reflink where the filesystem allows it: instant, and it consumes no extra space. This is
    // reachable because two btrfs subvolumes have different `st_dev` but share a filesystem.
    let cloned = reflink(&dst, &src);
    if !cloned {
        copy_contents(&src, &dst, size, control)?;
    }

    fchmod(&dst, mode_of(src_stat)).ok();
    // Ownership only sticks when privileged; losing it is not a reason to fail a move.
    let _ =
        fchown(&dst, Some(Uid::from_raw(src_stat.st_uid)), Some(Gid::from_raw(src_stat.st_gid)));
    copy_times(&dst, src_stat);
    fsync(&dst).map_err(|e| io::Error::other(format!("flushing {dst_name:?}: {}", errno(e))))?;

    verify(&src, &dst, size, opts, dst_name)?;
    Ok(size)
}

/// Copy the data, skipping holes so a sparse file stays sparse.
fn copy_contents(
    src: &OwnedFd,
    dst: &OwnedFd,
    size: u64,
    control: &Control,
) -> Result<(), OpError> {
    // The full length is set up front: the tail of a sparse file may be a hole with no data after
    // it, which no amount of copying would produce.
    ftruncate(dst, size)
        .map_err(|e| io::Error::other(format!("sizing destination: {}", errno(e))))?;

    let mut offset = 0u64;
    let mut buffer = vec![0u8; CHUNK];

    while offset < size {
        if control.is_paused() {
            return Ok(());
        }

        // Find the next region that actually holds data. Filesystems without the extension report
        // the whole file as data, which is correct if pessimistic.
        let data_start = match seek(src, SeekFrom::Data(offset)) {
            Ok(pos) => pos,
            Err(Errno::NXIO) => break, // only holes left
            Err(_) => offset,
        };
        let data_end = match seek(src, SeekFrom::Hole(data_start)) {
            Ok(pos) => pos.min(size),
            Err(_) => size,
        };
        if data_end <= data_start {
            break;
        }

        offset = data_start;
        while offset < data_end {
            if control.is_paused() {
                return Ok(());
            }
            let want = ((data_end - offset) as usize).min(CHUNK);
            let moved = copy_chunk(src, dst, offset, want, &mut buffer)?;
            if moved == 0 {
                return Err(io::Error::other("source ended earlier than its size claimed").into());
            }
            offset += moved as u64;
        }
    }
    Ok(())
}

/// One chunk, by whichever mechanism the kernel offers.
fn copy_chunk(
    src: &OwnedFd,
    dst: &OwnedFd,
    offset: u64,
    len: usize,
    buffer: &mut [u8],
) -> Result<usize, OpError> {
    #[cfg(target_os = "linux")]
    {
        let mut off_in = offset;
        let mut off_out = offset;
        match rustix::fs::copy_file_range(src, Some(&mut off_in), dst, Some(&mut off_out), len) {
            Ok(moved) => return Ok(moved),
            // Older kernels refuse across filesystems, and some filesystems refuse entirely.
            Err(Errno::XDEV) | Err(Errno::NOSYS) | Err(Errno::INVAL) | Err(Errno::OPNOTSUPP) => {}
            Err(e) => return Err(io::Error::other(format!("copying: {}", errno(e))).into()),
        }
    }

    seek(src, SeekFrom::Start(offset))
        .map_err(|e| io::Error::other(format!("seeking source: {}", errno(e))))?;
    seek(dst, SeekFrom::Start(offset))
        .map_err(|e| io::Error::other(format!("seeking destination: {}", errno(e))))?;

    let got = read(src, &mut buffer[..len])
        .map_err(|e| io::Error::other(format!("reading: {}", errno(e))))?;
    let mut written = 0;
    while written < got {
        let n = write(dst, &buffer[written..got])
            .map_err(|e| io::Error::other(format!("writing: {}", errno(e))))?;
        if n == 0 {
            return Err(io::Error::other("destination accepted no more data").into());
        }
        written += n;
    }
    Ok(got)
}

/// Share the data instead of duplicating it, where the filesystem supports it. Failure is not an
/// error: it just means the bytes have to be copied.
#[cfg(all(target_os = "linux", not(any(target_arch = "sparc", target_arch = "sparc64"))))]
fn reflink(dst: &OwnedFd, src: &OwnedFd) -> bool {
    rustix::fs::ioctl_ficlone(dst, src).is_ok()
}

#[cfg(not(all(target_os = "linux", not(any(target_arch = "sparc", target_arch = "sparc64")))))]
fn reflink(_dst: &OwnedFd, _src: &OwnedFd) -> bool {
    false
}

/// Confirm the copy is what the source is, before the source stops existing.
fn verify(
    src: &OwnedFd,
    dst: &OwnedFd,
    size: u64,
    opts: &ExecOptions,
    name: &CStr,
) -> Result<(), OpError> {
    let written = stat_fd(dst)?.st_size.max(0) as u64;
    if written != size {
        return Err(io::Error::other(format!(
            "{name:?} copied as {written} bytes but the source is {size}"
        ))
        .into());
    }
    if opts.verify == Verify::Hash && digest(src)? != digest(dst)? {
        return Err(io::Error::other(format!("{name:?} does not match the source")).into());
    }
    Ok(())
}

fn digest(fd: &OwnedFd) -> Result<[u8; 32], OpError> {
    seek(fd, SeekFrom::Start(0))
        .map_err(|e| io::Error::other(format!("rewinding: {}", errno(e))))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let got = read(fd, &mut buffer)
            .map_err(|e| io::Error::other(format!("reading: {}", errno(e))))?;
        if got == 0 {
            break;
        }
        hasher.update(&buffer[..got]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn copy_symlink(
    src_dir: &OwnedFd,
    src_name: &CStr,
    dst_dir: &OwnedFd,
    dst_name: &CStr,
) -> Result<(), OpError> {
    let target = readlinkat(src_dir, src_name, Vec::new())
        .map_err(|e| io::Error::other(format!("reading link {src_name:?}: {}", errno(e))))?;
    match symlinkat(target.as_c_str(), dst_dir, dst_name) {
        // Left by an earlier attempt.
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(e) => {
            Err(io::Error::other(format!("recreating link {dst_name:?}: {}", errno(e))).into())
        }
    }
}

fn link_to(existing: &Path, dst_dir: &OwnedFd, name: &CStr) -> Result<(), OpError> {
    match linkat(CWD, existing, dst_dir, name, AtFlags::empty()) {
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(e) => Err(io::Error::other(format!("linking {name:?}: {}", errno(e))).into()),
    }
}

/// Recreate a named pipe beside its original, where the platform offers a way to do it relative
/// to a directory descriptor.
///
/// macOS has no `mkfifoat`, and reconstructing the full path mid-copy would reintroduce exactly
/// the path-resolution race the rest of this module avoids. A pipe carries no data, but a move
/// that silently dropped one and then deleted the original would still be a move that lost
/// something, so the caller refuses instead.
#[cfg(target_os = "linux")]
fn recreate_fifo(parent: &OwnedFd, name: &CStr, stat: &Stat) -> bool {
    if FileType::from_raw_mode(stat.st_mode as _) != FileType::Fifo {
        return false;
    }
    rustix::fs::mkfifoat(parent, name, mode_of(stat)).is_ok()
}

#[cfg(not(target_os = "linux"))]
fn recreate_fifo(_parent: &OwnedFd, _name: &CStr, _stat: &Stat) -> bool {
    false
}

fn ensure_dir(parent: &OwnedFd, name: &CStr, mode: Mode) -> Result<OwnedFd, OpError> {
    match mkdirat(parent, name, mode) {
        // Already there from an interrupted attempt: reuse it and carry on.
        Ok(()) | Err(Errno::EXIST) => {}
        Err(e) => return Err(io::Error::other(format!("creating {name:?}: {}", errno(e))).into()),
    }
    openat(parent, name, DIR_FLAGS, Mode::empty())
        .map_err(|e| io::Error::other(format!("opening {name:?}: {}", errno(e))).into())
}

fn list(dir: &OwnedFd) -> Result<Vec<CString>, OpError> {
    let mut out = Vec::new();
    let mut handle =
        Dir::read_from(dir).map_err(|e| io::Error::other(format!("reading: {}", errno(e))))?;
    while let Some(entry) = handle.read() {
        let entry = entry.map_err(|e| io::Error::other(format!("reading: {}", errno(e))))?;
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        out.push(name.to_owned());
    }
    Ok(out)
}

fn stat_fd(fd: &OwnedFd) -> Result<Stat, OpError> {
    rustix::fs::fstat(fd.as_fd())
        .map_err(|e| io::Error::other(format!("stat: {}", errno(e))).into())
}

fn copy_times(fd: &OwnedFd, from: &Stat) {
    #[allow(clippy::unnecessary_cast)]
    let times = Timestamps {
        last_access: rustix::fs::Timespec {
            tv_sec: from.st_atime as _,
            tv_nsec: from.st_atime_nsec as _,
        },
        last_modification: rustix::fs::Timespec {
            tv_sec: from.st_mtime as _,
            tv_nsec: from.st_mtime_nsec as _,
        },
    };
    let _ = futimens(fd, &times);
}

#[allow(clippy::unnecessary_cast)]
fn mode_of(stat: &Stat) -> Mode {
    Mode::from_raw_mode(stat.st_mode as _)
}

fn os_str(name: &CString) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(name.to_bytes().to_vec())
}

fn cstring(name: &std::ffi::OsStr) -> Result<CString, OpError> {
    CString::new(name.as_encoded_bytes())
        .map_err(|_| io::Error::other("entry name contains a NUL").into())
}
