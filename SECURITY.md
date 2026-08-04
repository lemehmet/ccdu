# Security Policy

## Reporting a vulnerability

Please report security issues privately through GitHub's **"Report a
vulnerability"** (Security ▸ Advisories) on this repository, or by email to the
maintainer. Do not open a public issue for an undisclosed vulnerability. We aim
to acknowledge within a few days and will agree a fix and disclosure timeline
with you.

## Security model

ccdu is a local tool that deletes and moves files, and can be pointed at another
machine over ssh. Its posture:

- **Nothing happens until you commit.** Marking and staging touch nothing beyond
  a single `stat` per entry. The review step is the safety, and it is not
  optional: committing is unreachable from the browser without going through the
  plan view first.
- **Identity is re-checked immediately before acting.** Every operation records
  the device, inode, size, mtime and kind of its target when staged, and refuses
  if any of it has changed by the time it runs. That is what stops a path from
  being swapped between review and execution.
- **Operations run through `*at` syscalls on descriptors ccdu opened itself.**
  Once a parent directory is open, nothing substituted underneath can redirect a
  deletion. Symlinks are never followed — not when scanning, not when deleting,
  not when moving.
- **System paths are refused.** A fail-closed list covering `/`, the usual system
  directories, `$HOME` itself, and the scan root, extensible through the config
  file. Operating outside the scanned tree takes an explicit flag.
- **Nothing is destroyed before its replacement is safe.** A cross-filesystem
  move copies into a temporary, fsyncs, verifies, renames into place, and only
  then unlinks the source. ccdu will refuse to move a socket or device node
  rather than pretend it copied one and then delete the original.
- **Every commit is journaled ahead of the syscall it describes.** The journal
  can claim work that never happened but can never omit work that did, so a
  crash, a kill, or a lost connection leaves a run that is resumable rather than
  ambiguous.
- **Remote work happens on the remote.** `ccdu ssh://host/path` runs `ccdu
  --agent` over your own ssh, using your own ssh configuration, keys and
  authentication — ccdu adds no transport, no listening socket, and no
  credentials of its own. The plan and journal live on the machine holding the
  files, and the agent runs plans from its own store rather than any that arrive
  down the pipe. The protocol refuses work before a version-checked handshake.
- **No network access of any kind.** ccdu never phones home, checks for updates,
  or contacts any endpoint. The only process it ever starts is the `ssh` (or
  `ncdu`) you asked for.

## What ccdu writes, and where

Worth knowing before you share anything:

- Plans and journals go under `$XDG_STATE_HOME/ccdu/plans/` (macOS:
  `~/Library/Application Support/ccdu/plans/`). A plan file records the absolute
  paths it operates on and the hostname it was made on. A journal records what
  was attempted, in order, with those paths. Neither leaves your machine unless
  you send it somewhere.
- Exports (`ccdu -o`) contain the paths and sizes of everything scanned.
- Released binaries are built with build paths remapped, so they do not carry the
  builder's directories.

## Known limits

These are deliberate, and worth knowing:

- **There is no undo.** The review step is the safety. A committed deletion is
  gone.
- **Recursive deletion cannot be atomic.** ccdu checks each directory for write
  permission as it opens it, so the common failure is caught before its contents
  are gone — but a failure part way through a large tree can still leave it
  partly deleted. The journal records exactly how far it got.
- **A remote tree is only as trustworthy as the host.** The agent runs with your
  privileges on that machine, and the tree it sends back is what it says it is.

If you find a way around any of the above, that is exactly the report we want.
