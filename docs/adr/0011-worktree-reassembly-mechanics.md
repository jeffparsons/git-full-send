# ADR-0011 — Worktree reassembly mechanics

- Status: accepted
- Date: 2026-06-13

## Context

[ADR-0008](0008-remote-worktree-disposability.md) makes the remote worktree
disposable: `update-worktree` is an authoritative, destructive overwrite that
makes the worktree match the synced `code` tree exactly.
[ADR-0004](0004-encoding-the-sync-state-in-git.md) sketches the reassembly as
"check the `code` tree out, then explode `extra` over it (`git checkout-index`)".
This ADR pins the concrete mechanics for the `code` tree (the `extra` overlay is
a later ticket).

Two forces pull in different directions:

- **Correctness.** The overwrite must be exact: remote-side edits are stomped
  **even when the edited file's blob is unchanged between syncs**, files dropped
  between syncs are removed, and untracked remote additions are removed.
- **Efficiency on a large repository.** A code tree can be large while a typical
  sync changes only a few files, so the update should do work proportional to
  the sync *delta*, not the whole tree.

A naïve "rewrite everything" pipeline (a throwaway index +
`git checkout-index -a -f`, then prune) is exactly correct but fails the second
force: measured on a 200-file loopback repo, an update that changed one file
rewrote all 200 — O(repo-size) worktree writes every run.

## Decision

Reassemble with a **persistent per-worktree index** and Git's stat cache — the
same machinery `git reset --hard` / `git checkout` use — shelling out to `git`
(index population and worktree checkout are the gitoxide capability gap, see
[ADR-0002](0002-git-manipulation-strategy.md) /
[Research 0001](../research/0001-gix-git-plumbing-vs-libgit2-capability-gap.md)).

Per update, with `GIT_INDEX_FILE` pointed at the worktree's persistent index and
`--git-dir` / `--work-tree` set:

1. Resolve `refs/git-full-send/code` to its tree (a missing ref fails cleanly
   before any worktree mutation).
2. `git read-tree --reset -u <tree>` — reset the index and worktree to the tree.
   `--reset` discards worktree-local changes (so remote edits are stomped) and is
   keyed on the worktree's stat, not on a prior-tree diff; `-u` updates the
   worktree and removes files dropped between syncs. The index's stat cache means
   only changed paths are re-hashed and re-written.
3. `git clean -fdx` — prune untracked leftovers (remote-added files `-f`,
   directories `-d`, ignored files `-x`).

### Why this point in the design space

- **Not throwaway index + `checkout-index -a -f`:** that rewrites the whole
  worktree every run (the O(repo-size) cost above). The persistent index is
  precisely the state that lets Git skip unchanged files.
- **Not `read-tree -m -u` (the merge form):** keyed on the prior tree, it skips
  any path whose blob is unchanged between syncs and so would *not* revert a
  remote edit to such a file. `--reset` reverts it.

### The persistent index is pure cache

It records what was last checked out, stored under the git dir keyed by worktree
path, and never inside the worktree (`clean -fdx` would delete it there). If it
is missing or stale (first run, or it was deleted), `read-tree --reset -u` simply
has no stat shortcut and does a one-time full rewrite — still producing an exact
match — then is incremental again. So it needs no integrity tracking.

## Consequences

- Worktree updates do O(changed-files) writes; the only O(repo-size) cost is an
  `lstat` scan, the same cost `git status` pays — fine on large repos.
- The server keeps a small per-worktree index under the git dir. Losing it costs
  one full rewrite, never a wrong result.
- `clean -fdx`'s `-x` is correct for this code-only exact-match checkout; it will
  be revisited when the [ADR-0007](0007-syncing-extra-gitignored-files.md)
  `extra` overlay re-introduces deliberately force-included ignored files.
