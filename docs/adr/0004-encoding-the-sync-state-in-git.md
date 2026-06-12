# ADR-0004 — Encoding the sync state in Git

- Status: proposed
- Date: 2026-06-12

## Context / problem statement

The client must represent the full sync state as Git objects so it can be
transferred efficiently ([ADR-0005](0005-transfer-mechanism.md)) and checked out
on the remote. The sync state is more than the committed code:

1. The committed history already in the repository.
2. Working-tree changes — both staged and unstaged.
3. A set of **normally-gitignored files that we deliberately force-include** —
   e.g. CPU-intensive web-client build outputs produced on the MacBook, and
   per-user config files (see
   [ADR-0007](0007-syncing-extra-gitignored-files.md)). Some of these are
   large-ish.

This must happen **without disturbing** the user's current branch, main index,
or working tree. Scratch refs and an alternate index are permitted.

## Decision drivers

- Must not touch the user's branch / main index / working tree.
- Keep Git happy: produce well-formed objects that Git tooling (including our
  build planning) understands and whose digests can be reused.
- Efficient, predictable transfers — avoid pathological pack shapes that blow up
  transfer size or time (this is tightly coupled to
  [ADR-0005](0005-transfer-mechanism.md), where intermittent slow transfers were
  observed in the prototype).
- Handle the "extra" force-included files cleanly alongside the real history.

## Considered options (no decision yet)

1. **Stacked commits (the original prototype approach).** Start from the already
   committed code, add a commit with all working-tree changes on top, then
   another commit on top with the force-added, otherwise-gitignored files.
   Simple and linear, but mixes generated/large files into the same commit
   lineage as real code.
2. **Separate branch / independent commit for the extra files**, exploded out
   into the working tree separately on the remote end. Keeps generated artifacts
   out of the code lineage, at the cost of a second thing to transfer and
   reassemble.
3. **Alternate-index-based tree construction.** Build the tree(s) via an
   alternate index without ever materialising scratch commits on a real branch.

These options interact with how the transfer is performed and with how the
remote worktree is reassembled.

## Status

Proposed. The constraints and options above are recorded; the choice is
deferred pending research.

> ⚠ Research task needed: determine the object/commit/tree encoding that keeps
> Git happy and yields efficient, predictable transfers — including how the
> force-included files are layered relative to the working-tree changes, and
> how the result is reassembled into the remote worktree. Coordinate with
> [ADR-0005](0005-transfer-mechanism.md) and
> [ADR-0007](0007-syncing-extra-gitignored-files.md).
