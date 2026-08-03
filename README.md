# ccdu

A terminal disk-usage analyzer that **stages** changes into a reviewable plan and **commits** them
under an append-only journal, so a commit can be paused, resumed, and survives a crash.

Think `ncdu`, but you never delete by accident — and `yadu`'s execution model without the browser.

## Status

Early development. See the milestone table below.

| # | Deliverable | State |
|---|---|---|
| M0 | Workspace skeleton, CI | done |
| M1 | Scanner, arena tree, hardlink accounting | done |
| M2 | TUI browser | done |
| M3 | Staging, plan persistence, validation | done |
| M4 | Executor: deletes, journal, pause/resume | done |
| M5 | Moves: same-fs, reflink, cross-device | done |
| M6 | Treemap panel, duplicate detection | done |
| M7 | ncdu import/export, SSH agent | done |

## Try it

```sh
ccdu /some/path              # scan, then browse
ccdu -x -t8 /                # one filesystem, 8 threads
ccdu scan --top 30 /usr      # headless: summary + largest entries
ccdu ssh://server/var        # scan on another machine, browse here
ccdu /some/path -o dump      # save the scan
ccdu -f dump                 # browse a saved scan
```

A long scan can be cut short with `q` — you drop straight into the browser with whatever was
found so far, marked `[partial scan]` so the totals are not mistaken for the whole picture.

### Keys

| | |
|---|---|
| `↑ ↓ j k`, `PgUp` `PgDn`, `Home` `End` | move |
| `⏎` `→` `l` / `←` `h` `Backspace` | open a directory / go up |
| `s` `n` `C` `M` | sort by size, name, item count, mtime (again to reverse) |
| `a` | apparent size vs disk usage |
| `g` | cycle graph: bar, percent, both, off |
| `t` / `D` | treemap panel / find duplicate files |
| `Space` | mark an entry; `d`/`m`/`u` then apply to every mark |
| `d` / `m` / `u` | stage a deletion / a move / unstage |
| `p` / `w` / `c` | review the plan / write it to the plan store / commit |
| `i` / `?` / `q` | details / keys / quit |

## Duplicates and the treemap

`t` puts a squarified treemap beside the listing — areas proportional to size, so the thing worth
deleting is the thing that looks biggest.

`D` finds files with identical contents, in three stages, each shrinking the input to the next:
group by size (free — the scan already knows every size), hash the first and last few kilobytes
(a fixed read however large the file), then hash in full whatever survived. Hardlinks are excluded:
two names for one inode are not two copies.

```
 duplicates   2 groups  14.0 MiB reclaimable
 3 copies of 6.0 MiB  — 12.0 MiB reclaimable
   keep work/video-again.mp4
        media/video.mp4
        backup/video-copy.mp4
```

`A` stages every copy in a group except the first. It never stages the whole group: a bulk action
that could remove the last copy is not a labour saver. A copy that is itself hardlinked elsewhere
says so and is left out of the reclaimable total, because removing that one name frees nothing.

```sh
ccdu dupes /some/path --min-size 1048576 --top 5
```

## Staging

Nothing you do in the browser touches the disk. `d` and `m` record what to do and what the entry
looked like at the time — device, inode, size, mtime — and `p` shows the result with every problem
attributed to the operation that caused it:

```
 plan  2 operations  reclaims 8.0 MiB  moves 3.0 MiB
 M    3.0 MiB  /data/cache
              → /mnt/big/cache
 D    8.0 MiB  /data/logs
```

`w` writes the plan to `$XDG_STATE_HOME/ccdu/plans/<id>/plan.json` (override the location with
`CCDU_STATE_DIR`). From there:

```sh
ccdu plan list                 # newest first
ccdu plan show <id>
ccdu plan validate <id>        # exits non-zero if anything blocks a commit
ccdu plan rm <id>              # removes the plan, never the files it names
```

Validation refuses protected system directories and the scan root, operations on paths outside the
scanned tree, moves into their own subtree, two operations writing the same destination, a
destination that already exists, a destination filesystem without room, and — the one that matters
most — any entry whose identity no longer matches what was staged:

```
error  #0  changed since staging: modified at 2026-08-03 20:29:54 (was 2026-08-03 20:29:51)
```

## Committing

```sh
ccdu apply <id> --dry-run    # check everything, change nothing, journal nothing
ccdu apply <id>              # asks for confirmation first
ccdu resume <id>             # continue a paused or interrupted run
ccdu status <id>             # how far it got
```

Ctrl-C **pauses** rather than kills, leaving a run you can resume instead of a state you have to
work out. So does a crash — the two are the same thing to the recovery path:

```
$ ccdu apply 20260803T212000-cafe0002 --yes
  #0  delete /data/bulk
^C
pausing; run `ccdu resume 20260803T212000-cafe0002` to continue
paused after 0 operations, 67.0 MiB reclaimed

$ ccdu resume 20260803T212000-cafe0002
  #0  delete /data/bulk
  #0  done, 50.9 MiB reclaimed

1 operations done, 117.9 MiB reclaimed
```

The total covers both attempts, because the resumed run can only measure what was left to remove.

Every record reaches disk before the syscall it describes, so the journal can claim work that never
happened but can never omit work that did — and since each operation re-checks reality and is
idempotent, a premature claim costs one wasted check rather than a file. Deletion runs through
`*at` syscalls on a directory descriptor the executor opened itself, so a path swapped mid-run
cannot redirect it, and an unwritable directory is detected before its contents are gone rather
than after.

In the browser, `c` opens the plan, and `c` again from there asks for confirmation on its own
screen — the review step is the safety, so committing is never one keystroke from browsing. A
commit running in the TUI can be paused with `p` and continued later with `ccdu resume`. Once it
has run, the listing describes a disk that no longer exists, and says so rather than showing
numbers that are quietly wrong.

## Moving

Three paths, cheapest first:

| | |
|---|---|
| **Rename** | Same filesystem: one syscall, atomic, consumes nothing |
| **Reflink** | Different `st_dev`, same filesystem — btrfs subvolumes, where `rename` gives `EXDEV` but the data can still be shared rather than duplicated |
| **Copy, verify, unlink** | Different filesystems. In that order, always |

The source is the only copy of the data until the destination is durable and checked, so it is the
last thing to go. A cross-filesystem move assembles the destination under `.ccdu-part-<n>-<name>`
and renames it into place only once complete, so a half-copied tree never appears at the
destination path and an interrupted move resumes into the same temporary instead of starting over.
A file already there at the right length is skipped; a short one is a torn copy and is redone
rather than trusted.

Copies preserve permissions, timestamps, ownership where privileged, symlinks (recreated, not
followed), and holes — a 64 MiB sparse file arrives sparse. Files hardlinked to each other inside
the tree arrive as one inode with two names rather than two copies. A socket or device node is
refused outright: ccdu will not claim to have copied something it cannot reproduce and then delete
the original.

`--verify=hash` re-reads both sides and compares blake3 digests; the default checks lengths, which
is what an interrupted copy gets wrong.

## Running the tests

```sh
cargo test
CCDU_TEST_OTHER_FS=/dev/shm/ccdu-tests cargo test   # also exercises cross-filesystem moves
```

The cross-filesystem tests need a directory on a second filesystem. Without one they print that
they were skipped rather than passing vacuously.

Sizes are `st_blocks * 512` (what freeing the file actually returns) unless you press `a`, and
hardlinked files are counted once. `ccdu` matches `du -s --block-size=1` on the trees it has been
checked against, at roughly a third of the wall time on 8 threads.

## Saving, sharing, and other machines

A scan can be written out and read back, in ccdu's own format or ncdu's:

```sh
ccdu /usr -o usr.ccdu                      # exact and compact
ccdu /usr -o - --format ncdu-json | zstd   # readable by `ncdu -f`
ccdu -f usr.ccdu                           # either format; `-` reads stdin
```

The format is worked out from the file's first bytes, so neither naming conventions nor a flag are
needed, and it works on a pipe. Both readers treat their input as untrusted: a file claiming a
node's parent lives at index four billion gets an error, not a panic somewhere much later.

Interoperability is checked against the real thing rather than against assumptions — `ncdu 1.19`
and `ccdu` report the same 696.7 MiB and 50 838 items for `/usr/share` in both directions.

`ccdu ssh://host/path` runs `ccdu --agent` over ssh: the remote walks the filesystem, where the
files are, and sends back the finished tree. If that host has no ccdu, it falls back to
`ncdu -o-` — a host with ncdu on it is the common case, not a failure.

```
$ ccdu ssh://server/var/log
no ccdu agent on server (the remote said nothing; ccdu may not be installed or on its PATH); trying ncdu
```

Use `--remote-ccdu /path/to/ccdu` when it is installed somewhere ssh's non-interactive `PATH` does
not reach. A tree fetched from another machine is browsable but not stageable — its paths describe
a filesystem this process cannot safely act on, and it says so rather than failing later.

## Layout

- `crates/ccdu-core` — scanner, tree model, duplicate engine, plan, journal, executor. No UI deps.
- `crates/ccdu-tui` — ratatui frontend.
- `crates/ccdu-remote` — SSH agent protocol (both ends).
- `crates/ccdu` — the `ccdu` binary.

## License

Apache-2.0
