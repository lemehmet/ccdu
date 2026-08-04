# Contributing to ccdu

Thanks for your interest. ccdu deletes and moves people's files, which shapes
most of the decisions below.

## Dev setup

Requirements: Rust 1.88 or newer. Nothing else — no C toolchain beyond what
`blake3` needs, and no system libraries.

```sh
cargo build
cargo test
cargo clippy --all-targets --all-features
cargo fmt --all
```

Some tests need a second filesystem and will say so if they do not get one:

```sh
mkdir -p /dev/shm/ccdu-test                          # Linux
CCDU_TEST_OTHER_FS=/dev/shm/ccdu-test cargo test
```

Without it the cross-filesystem move tests print that they were **skipped**
rather than passing vacuously. CI supplies one on both platforms and fails if it
turns out not to be a second filesystem after all.

## The checks

All of these run in CI on Linux and macOS, with warnings denied:

- `cargo fmt --all --check`
- `cargo clippy --all-targets --all-features`
- `cargo test --all-features`, with a second filesystem
- `cargo build --workspace` on the minimum supported Rust version

## What the review will actually ask about

**Anything destructive needs a test that proves the failure mode.** Not just
that the happy path works — that the operation *refuses* when it should. If you
touch `exec/`, `plan/validate.rs`, or the identity checks, expect to be asked
where the test is that fails without your change. The fault-injection harness in
`crates/ccdu-core/src/exec/tests.rs` interrupts a commit at every journal
boundary and demands the resumed result match an uninterrupted one; new
operations belong in it.

**Errors beat silence.** A malformed config, a plan from a newer version, a
truncated export, a name we cannot decode — these are reported, not worked
around. The rule of thumb: if a user could end up believing something is
protected when it is not, that path must fail loudly.

**Say what you cannot do.** ccdu refuses to move sockets and device nodes rather
than pretend it copied them and then delete the original. Where a platform lacks
a call, the operation is refused, not approximated.

**Dependencies are argued for, not added.** The current list is `rustix` for
syscalls, `ratatui` and `crossterm` for the interface, `clap`, `serde`,
`crossbeam-channel`, `blake3`, `toml`, `anyhow`, `thiserror`, and `libc`. If the
standard library or `rustix` can do it, it should.

**Comments explain the decision, not the code.** Why this ordering, why this is
refused, what breaks if it changes. The code already says what it does.

## Architecture in one paragraph

`ccdu [path]` walks the tree in parallel (`ccdu-core::scan`) into an arena of
48-byte nodes, browsed with a ratatui frontend (`ccdu-tui`) where entries are
staged into a plan (`ccdu-core::plan`, persisted under the XDG or macOS state
directory). Committing runs the journaled executor (`ccdu-core::exec`), which
writes each record to disk before the syscall it describes, re-checks every
entry's recorded identity through `*at` calls on descriptors it opened itself,
and is resumable after any interruption. Cross-filesystem moves copy into a
temporary, verify, and only then unlink the source. `ccdu ssh://host/path` runs
the same engine on another machine over a framed stdio protocol
(`ccdu-remote`), with the plan and journal kept where the files are.

## Pull requests

1. Branch off `main`; do not push to `main` directly.
2. Keep them focused, and include tests for new behaviour.
3. Make the checks above pass locally first.
4. Explain what you decided and why in the description — that is the part
   review spends its time on.

## Reporting bugs

Include the ccdu version, the platform and filesystem, and what you expected.
For anything involving a commit that went wrong, the journal
(`~/.local/state/ccdu/plans/<id>/journal.jsonl`) is the most useful thing you can
attach; it records what was attempted and in what order. Read it before you post
it — it contains the paths it operated on.
