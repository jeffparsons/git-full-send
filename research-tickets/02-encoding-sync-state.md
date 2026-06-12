# Research: encoding the sync state in Git

- Blocked by: #1 (Foundational ADRs)
- Source: [ADR-0004 — Encoding the sync state in Git](../docs/adr/0004-encoding-the-sync-state-in-git.md)

## Context

The client must represent the full sync state as Git objects — committed history,
working-tree (staged & unstaged) changes, and a force-included set of normally
gitignored files — **without disturbing** the user's current branch, main index,
or working tree (scratch refs / an alternate index are permitted).

## Goal

Choose the object/commit/tree encoding that keeps Git happy and yields efficient,
predictable transfers, and decide how the result is reassembled into the remote
worktree.

## Options to evaluate (from the ADR)

1. Stacked commits (prototype approach): committed code → working-tree commit →
   forced-files commit.
2. Separate branch / independent commit for the extra files, exploded into the
   worktree separately on the remote end.
3. Alternate-index-based tree construction without materialising scratch commits
   on a real branch.

## Notes

- Tightly coupled to [ticket 03 (transfer mechanism)](03-transfer-mechanism-pack-performance.md)
  and [ticket 04 (force-include config)](04-force-include-configuration.md).
- A key driver is avoiding pathological pack shapes that hurt transfer
  efficiency.
