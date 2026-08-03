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
| M2 | TUI browser | |
| M3 | Staging, plan persistence, validation | |
| M4 | Executor: deletes, journal, pause/resume | |
| M5 | Moves: same-fs, reflink, cross-device | |
| M6 | Treemap panel, duplicate detection | |
| M7 | ncdu import/export, SSH agent | |

## Try it

```sh
cargo run --release -- /some/path          # summary + largest entries
cargo run --release -- -x -t8 --top 30 /   # one filesystem, 8 threads
```

Sizes are `st_blocks * 512` (what freeing the file actually returns) unless you pass `-a`, and
hardlinked files are counted once. `ccdu` matches `du -s --block-size=1` on the trees it has been
checked against, at roughly a third of the wall time on 8 threads.

## Layout

- `crates/ccdu-core` — scanner, tree model, duplicate engine, plan, journal, executor. No UI deps.
- `crates/ccdu-tui` — ratatui frontend.
- `crates/ccdu-remote` — SSH agent protocol (both ends).
- `crates/ccdu` — the `ccdu` binary.

## License

Apache-2.0
