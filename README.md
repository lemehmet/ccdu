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
| M4 | Executor: deletes, journal, pause/resume | |
| M5 | Moves: same-fs, reflink, cross-device | |
| M6 | Treemap panel, duplicate detection | |
| M7 | ncdu import/export, SSH agent | |

## Try it

```sh
cargo run --release -- /some/path            # scan, then browse
cargo run --release -- -x -t8 /              # one filesystem, 8 threads
cargo run --release -- scan --top 30 /usr    # headless: summary + largest entries
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
| `Space` | mark an entry; `d`/`m`/`u` then apply to every mark |
| `d` / `m` / `u` | stage a deletion / a move / unstage |
| `p` / `w` | review the plan / write it to the plan store |
| `i` / `?` / `q` | details / keys / quit |

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

Executing a plan lands in M4; today `ccdu` only ever reads your files.

Sizes are `st_blocks * 512` (what freeing the file actually returns) unless you press `a`, and
hardlinked files are counted once. `ccdu` matches `du -s --block-size=1` on the trees it has been
checked against, at roughly a third of the wall time on 8 threads.

## Layout

- `crates/ccdu-core` — scanner, tree model, duplicate engine, plan, journal, executor. No UI deps.
- `crates/ccdu-tui` — ratatui frontend.
- `crates/ccdu-remote` — SSH agent protocol (both ends).
- `crates/ccdu` — the `ccdu` binary.

## License

Apache-2.0
