<!-- What changed, and why. The "why" is what review spends its time on. -->

## What this changes

## Why

## Checks

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets --all-features`
- [ ] `cargo test` — and with `CCDU_TEST_OTHER_FS` set, if this touches moves
- [ ] Anything destructive has a test proving it *refuses* when it should
