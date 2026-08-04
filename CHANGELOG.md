# Changelog

Notable changes, newest first. This project follows [semantic versioning](https://semver.org);
while the major version is 0, breaking changes may land in a minor release, and the plan file and
export formats carry their own version numbers so a file from a newer ccdu is refused rather than
half-understood.

## v0.1.0 — 2026-08-04

First release.

### Scanning

- Parallel walk handing subdirectories to workers as open descriptors, so each is reached with one
  `openat` from its parent rather than by re-walking its path. A single builder thread owns the
  tree, so nothing is locked on the hot path.
- Flat arena of 48-byte nodes with names in one shared buffer, which is what makes a tree of
  millions of entries practical to hold and browse.
- Hardlinks counted once, symlinks never followed, bind-mount loops detected, unreadable entries
  flagged rather than fatal. Sizes are `st_blocks * 512` — what freeing a file actually returns.
- Agrees with `du -s --block-size=1` on the trees it has been checked against, at roughly a third
  of the wall time on eight threads.
- A long scan can be cut short and browsed as far as it got, marked as partial.

### Browsing

- ncdu-style listing with the familiar keys, a squarified treemap panel (`t`), and an info panel.
- Duplicate detection (`D`) in three stages: group by size, sample both ends, then hash in full
  whatever survives. Hardlinks excluded, and a copy that is itself hardlinked elsewhere is left out
  of the reclaimable total because removing that one name would free nothing.

### Staging and committing

- Nothing touches the disk until a commit. Staging records each entry's device, inode, size, mtime
  and kind, and that identity is re-checked immediately before the operation runs.
- Plans persist under `$XDG_STATE_HOME/ccdu/plans/`, with `ccdu plan list|show|validate|rm` and
  `ccdu apply|resume|status`. Paths serialise losslessly, including names that are not valid UTF-8.
- Validation refuses protected system paths, the scan root, paths outside the scanned tree, moves
  into their own subtree, colliding destinations, occupied destinations, insufficient free space,
  and anything whose identity has drifted since it was staged.
- Every journal record reaches disk before the syscall it describes, so a commit interrupted by
  Ctrl-C, a crash or a kill is resumable rather than ambiguous. Verified by aborting the real
  binary at every journal boundary of every operation and requiring the resumed result to match an
  uninterrupted run, plus a `SIGKILL` case.

### Moving

- Rename within a filesystem, reflink where `st_dev` differs but the filesystem does not, and
  copy → verify → unlink across filesystems, in that order.
- Cross-filesystem moves assemble under a temporary and rename into place only when complete, so a
  half-copied tree never appears at the destination and an interrupted move resumes into the same
  temporary. A short file left by a torn copy is redone rather than trusted.
- Preserves permissions, timestamps, ownership where privileged, symlinks, holes, and hardlink
  structure within the moved tree. Sockets and device nodes are refused rather than silently
  dropped and then deleted.
- `--verify=hash` compares blake3 digests; the default checks lengths.

### Other machines

- `ccdu ssh://host/path` runs `ccdu --agent` over your own ssh. The remote scans, and the
  connection stays open so staging and committing happen where the files are — the plan and journal
  end up on the machine doing the work, so a dropped connection leaves a run that `ccdu resume`
  finishes there.
- Falls back to `ncdu -o-` on hosts without ccdu; such a tree is read-only and says so.

### Exports

- ccdu's own compact format and ncdu's JSON, both directions, with the format detected from the
  file's first bytes so it works on a pipe. Checked against `ncdu 1.19` rather than against its
  documentation.
- Both readers treat input as untrusted: truncated files, out-of-range indices and implausible
  headers produce errors rather than panics or allocation failures.

### Configuration

- Optional TOML for protected paths, default scan options and move headroom. A file that does not
  parse is an error rather than a silent fallback, because a typo in `protect` would otherwise
  leave a directory unprotected while you believed it was safe.

### Platforms

Linux and macOS, x86-64 and arm64. Released binaries are built natively on each target and carry
no build-machine paths.
