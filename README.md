# ccdu

[![CI](https://github.com/lemehmet/ccdu/actions/workflows/ci.yml/badge.svg)](https://github.com/lemehmet/ccdu/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

A terminal disk-usage analyzer that **stages** changes into a reviewable plan and
**commits** them under an append-only journal — so a commit can be paused,
resumed, and survives a crash. Single binary, Linux and macOS, no daemon and no
browser.

```
ccdu ~/projects
```

```
 ccdu  /home/me/projects  43.5 MiB in 8 items  [2 staged, 8.5 MiB]
    31.0 MiB  [████████████]  71.2%  archive/
 D   8.0 MiB  [███         ]  18.4%  build/
     4.0 MiB  [█           ]   9.2%  .cache/
 D 516.0 KiB  [            ]   1.2%  node_modules/
 sort: size▼  size: disk   space mark  d delete  m move  u unstage  p plan  ? help  q quit
```

Two directories marked for deletion. Nothing has happened yet — `p` reviews the
plan, `c` commits it.

## Why another one?

|  | acts on | multi-select | duplicates | resumable | remote |
|---|---|---|---|---|---|
| `du` / `df` | nothing | — | — | — | — |
| `ncdu` | deletes on keypress | no | no | no | analysis only |
| [`yadu`](https://github.com/lemehmet/yadu) | staged plan | yes | no | yes | no |
| **ccdu** | **staged plan** | **yes** | **yes** | **yes** | **scan and commit** |

Finding what is eating the disk and deciding what to do about it are different
mental modes, and tools that delete on a keypress force you to hold both at once.
ccdu separates them: sweep the tree marking things freely, review the whole set
in one place, then commit it deliberately.

The other half is that committing is not instant. Deleting a few hundred
thousand files, or moving forty gigabytes to another disk, takes long enough that
something *will* interrupt it eventually. ccdu treats that as the normal case
rather than the exception.

[`yadu`](https://github.com/lemehmet/yadu) got the model right and serves its
treemap over a local HTTP server to a browser — which is exactly wrong for the
machine where you most need this, a server you reached over ssh. ccdu keeps the
model and stays in the terminal.

## Install

Prebuilt binaries are attached to each
[release](https://github.com/lemehmet/ccdu/releases): Linux on x86-64 and arm64, macOS on Apple
Silicon. Intel Macs build from source, which needs nothing but a Rust toolchain.

```sh
cargo install --git https://github.com/lemehmet/ccdu ccdu
```

### From source

Requires Rust 1.88 or newer.

```sh
git clone https://github.com/lemehmet/ccdu
cd ccdu
cargo build --release      # target/release/ccdu
```

## Usage

```sh
ccdu                       # the current directory
ccdu /var                  # somewhere specific
ccdu -x /                  # one filesystem only
ccdu ssh://server/var/log  # another machine
```

A long scan can be cut short with `q` — you land in the browser with whatever was
found so far, marked `[partial]` so the totals are not mistaken for the whole
picture.

### Keys

|  |  |
|---|---|
| `↑ ↓ j k`, `PgUp` `PgDn`, `Home` `End` | move |
| `⏎` `→` `l` / `←` `h` `Backspace` | open a directory / go up |
| `s` `n` `C` `M` | sort by size, name, item count, mtime (again to reverse) |
| `a` | apparent size vs disk usage |
| `g` | cycle graph: bar, percent, both, off |
| `t` / `D` | treemap panel / find duplicate files |
| `Space` | mark an entry; `d` `m` `u` then apply to every mark |
| `d` / `m` / `u` | stage a deletion / a move / unstage |
| `p` / `w` / `c` | review the plan / write it out / commit |
| `i` / `?` / `q` | details / keys / quit |

Sizes are `st_blocks * 512` — what freeing the file actually gives back — unless
you press `a`. Hardlinked files are counted once, so hardlink farms report real
numbers rather than inflated ones. `ccdu` agrees with `du -s --block-size=1` on
the trees it has been checked against, at roughly a third of the wall time on
eight threads.

## Staging and committing

Nothing in the browser touches the disk. `d` and `m` record what to do and what
the entry looked like at that moment — device, inode, size, mtime — and `p` shows
the result with every problem attributed to the operation that caused it:

```
 plan  2 operations  reclaims 8.0 MiB  moves 3.0 MiB
 M    3.0 MiB  /data/cache
              → /mnt/big/cache
 D    8.0 MiB  /data/logs
```

`c` asks for confirmation on its own screen, because this is the only
irreversible thing ccdu does. There is no undo; the review step is the safety.

`w` writes the plan to `$XDG_STATE_HOME/ccdu/plans/<id>/plan.json` instead, for
running later or from a script:

```sh
ccdu plan list                 # newest first
ccdu plan show <id>
ccdu plan validate <id>        # exits non-zero if anything blocks a commit
ccdu plan rm <id>              # removes the plan, never the files it names

ccdu apply <id> --dry-run      # check everything, change nothing, journal nothing
ccdu apply <id>                # asks first
ccdu resume <id>               # continue a paused or interrupted run
ccdu status <id>               # how far it got
```

Validation refuses protected system directories and the scan root, paths outside
the scanned tree, moves into their own subtree, two operations writing the same
destination, an occupied destination, a destination filesystem without room, and
— the one that matters most — anything whose identity no longer matches what was
staged:

```
error  #0  changed since staging: modified at 2026-08-03 20:29:54 (was 2026-08-03 20:29:51)
```

### Interruptions are the normal case

Ctrl-C **pauses** rather than kills. So does a crash — to the recovery path they
are the same thing:

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

The total covers both attempts, because the resumed run can only measure what was
left to remove.

Every journal record reaches disk before the syscall it describes. The journal
can therefore claim work that never happened, but can never omit work that did —
and since every operation re-checks reality and is idempotent, a premature claim
costs one wasted check rather than a file.

## Moving

Three paths, cheapest first:

|  |  |
|---|---|
| **Rename** | Same filesystem: one syscall, atomic, consumes nothing |
| **Reflink** | Different `st_dev`, same filesystem — btrfs subvolumes, where `rename` gives `EXDEV` but the data can still be shared rather than duplicated |
| **Copy, verify, unlink** | Different filesystems. In that order, always |

The source is the only copy of the data until the destination is durable and
checked, so it is the last thing to go. A cross-filesystem move assembles the
destination under `.ccdu-part-<n>-<name>` and renames it into place only when
complete, so a half-copied tree never appears at the destination path and an
interrupted move resumes into the same temporary instead of starting over.

Copies preserve permissions, timestamps, ownership where privileged, symlinks
(recreated, not followed), and holes — a 64 MiB sparse file arrives sparse. Files
hardlinked to each other inside the tree arrive as one inode with two names. A
socket or device node is refused outright: ccdu will not claim to have copied
something it cannot reproduce and then delete the original.

`--verify=hash` re-reads both sides and compares blake3 digests; the default
checks lengths, which is what an interrupted copy gets wrong.

## Duplicates and the treemap

`t` puts a squarified treemap beside the listing — areas proportional to size, so
the thing worth deleting is the thing that looks biggest.

`D` finds files with identical contents in three stages, each shrinking the input
to the next: group by size (free, since the scan already knows every size), hash
the first and last few kilobytes (a fixed read however large the file), then hash
in full whatever survived.

```
 duplicates   2 groups  14.0 MiB reclaimable
 3 copies of 6.0 MiB  — 12.0 MiB reclaimable
   keep work/video-again.mp4
        media/video.mp4
        backup/video-copy.mp4
```

`A` stages every copy in a group except the first. It never stages the whole
group: a bulk action that could remove the last copy is not a labour saver.
Hardlinks are excluded, since two names for one inode are not two copies, and a
copy that is itself hardlinked elsewhere says so and is left out of the
reclaimable total — removing that one name would free nothing.

```sh
ccdu dupes /some/path --min-size 1048576 --top 5
```

## Another machine

```sh
ccdu ssh://server/var/log
ccdu ssh://me@server:2222/data
ccdu server:/var/log          # scp-style also works
```

This runs `ccdu --agent` on the far side over your own ssh: the remote walks the
filesystem, where the files are, and sends back the finished tree. The connection
stays open, so staging asks the remote what entries look like — only the machine
holding a file can say — and committing runs there too.

That siting is the point. The plan and its journal end up on the machine doing
the work, so a connection dropped mid-commit leaves a run that `ccdu resume`
finishes *on that host*, rather than a record stranded at the end of a pipe that
no longer exists.

If the host has no ccdu, it falls back to `ncdu -o-`, which is the common case
rather than a failure:

```
$ ccdu ssh://server/var/log
no ccdu agent on server (the remote said nothing; ccdu may not be installed or on its PATH); trying ncdu
```

A tree fetched that way is read-only, and says so. Use `--remote-ccdu
/path/to/ccdu` when it is installed somewhere ssh's non-interactive `PATH` does
not reach.

## Saving and sharing scans

```sh
ccdu /usr -o usr.ccdu                      # exact and compact
ccdu /usr -o - --format ncdu-json | zstd   # readable by `ncdu -f`
ccdu -f usr.ccdu                           # either format; `-` reads stdin
```

The format is worked out from the file's first bytes, so it needs neither a flag
nor a naming convention and works on a pipe. Both readers treat their input as
untrusted: a file claiming a node's parent lives at index four billion gets an
error, not a panic somewhere much later.

Interoperability is checked against the real tool rather than against
assumptions — `ncdu 1.19` and `ccdu` report the same 696.7 MiB and 50 838 items
for `/usr/share`, in both directions.

## Configuration

Optional. Everything has a working default, and an empty file behaves exactly
like no file.

```sh
ccdu config           # where it is read from, and what it says
ccdu config --write   # a commented file with every option at its default
```

```toml
[scan]
exclude = [".git", "node_modules"]
one_file_system = true

[safety]
# Never operated on, on top of the built-in system list. Matched exactly:
# the directory itself is refused, its contents are not.
protect = ["/srv/archive", "/home/me/photos"]
```

A file that does not parse is an error rather than a silent fallback — a typo in
`protect` would otherwise leave a directory unprotected while you believed it was
safe:

```
Error: loading configuration

Caused by:
    ~/.config/ccdu/config.toml: TOML parse error at line 2, column 1
    unknown field `protekt`, expected one of `protect`, `no_default_protection`, `headroom`
```

`CCDU_CONFIG` overrides the location, `CCDU_STATE_DIR` overrides where plans
live.

## How it works

The scan walks directories across several threads, handing subdirectories to
workers as open descriptors so each is reached with one `openat` from its parent
rather than by re-walking its path. A single builder thread owns the tree, so
nothing is locked on the hot path. Entries land in a flat arena of 48-byte nodes
with names in one shared buffer, which is what makes a tree of millions of files
practical to hold and browse.

Execution works through `*at` syscalls on directory descriptors the executor
opened itself, so nothing substituted mid-run can redirect a deletion, and every
operation re-checks the identity recorded at staging time before it acts.

The correctness argument is tested rather than asserted: a harness aborts the
real binary at every journal boundary of every operation, resumes, and demands a
tree byte-identical to a run that was never interrupted — plus a `SIGKILL` case
where not even the abort handler runs.

## Security

See [SECURITY.md](SECURITY.md) for the model, what ccdu writes and where, and the
deliberate limits. In short: nothing happens until you commit, identity is
re-checked immediately before acting, system paths are refused, and ccdu makes no
network connections of any kind — the only process it starts is the `ssh` you
asked for.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version: anything destructive
needs a test that proves it *refuses* when it should, errors beat silence, and
dependencies are argued for rather than added.

```sh
cargo test
CCDU_TEST_OTHER_FS=/dev/shm/ccdu-test cargo test   # also exercises cross-filesystem moves
```

The cross-filesystem tests need a directory on a second filesystem. Without one
they print that they were skipped rather than passing vacuously.

## Layout

- `crates/ccdu-core` — scanner, tree, duplicates, plans, journal, executor. No UI.
- `crates/ccdu-tui` — the ratatui frontend.
- `crates/ccdu-remote` — the ssh agent protocol, both ends.
- `crates/ccdu` — the `ccdu` binary.

## Changes

See [CHANGELOG.md](CHANGELOG.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
