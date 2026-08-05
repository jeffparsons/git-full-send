# Plan — #85: Relative `--repo` makes `update-worktree`'s `clean` destroy the checkout

Found while investigating #82. With a relative `--repo`, the per-worktree
`GIT_INDEX_FILE` (derived from `gix::discover`'s relative git dir) is passed to
the `git` steps as a relative path. Git absolutizes `--git-dir` during setup
but resolves `GIT_INDEX_FILE` lazily: `read-tree` resolves it against the
invocation cwd (index found, warm, checkout correct), while `clean` chdirs
into the work tree first and resolves it *there* — no index, so the whole
tracked checkout is classified untracked and `clean -d -f` deletes it. Every
update then leaves the worktree empty and every subsequent one is a full
rewrite plus a full wipe.

Verified empirically (macOS, 34k-file loopback repo): relative invocation
costs a flat ~2.5s per "no-op" with `clean` reporting `Removing src/` every
run; the identical absolute invocation settles at ~130ms per no-op with the
ADR-0011 stat cache behaving exactly as designed. Isolated to the index path
alone: `clean -d -n` with a relative `GIT_INDEX_FILE` and *absolute*
`--git-dir`/`--work-tree` still reports `Would remove src/`; making only the
index path absolute silences it.

This also answers #82's first two "worth checking" bullets for this
environment — the persistent index *is* written back and reused, and no
narrower update is needed — but #82 stays open until the ~4s workstation
measurement is re-taken with this fix in place (whether it used relative
paths is unknown).

## Decision

**Canonicalize once, spawn absolute.** In `update_worktree_blocking`,
canonicalize the git dir right after discovery and the worktree right after
its `create_dir_all`, and use the canonical paths for everything downstream
(`GIT_INDEX_FILE` derivation, every spawned `git` step, the lock, the
report). No behavioural change for callers already passing absolute paths —
`worktree_state_dir` already canonicalizes for its hash key, so the state
directory is unchanged either way.

## Changes

### Code

- `crates/server/src/lib.rs`, `update_worktree_blocking`:
  - canonicalize `git_dir` after `gix::discover` (failure maps to
    `NotARepo`, matching the discovery error);
  - canonicalize `worktree` after `std::fs::create_dir_all` (failure maps to
    `CreateWorktree`, matching `worktree_state_dir`'s own canonicalize), and
    shadow the parameter so every later use — index path, lock, `git` steps,
    measurement, report — sees the canonical path.

### Tests (`crates/cli/tests/end_to_end.rs`)

Driving the real binary with a controlled subprocess cwd (safe to do
per-child, unlike changing the test process's own cwd):

- `update_worktree_with_relative_paths_keeps_the_checkout`: server repo and
  worktree as sibling directories under one parent; sync; run
  `update-worktree --repo <relative> --worktree <relative>` with the child's
  cwd at the parent, twice. Assert after each run that the worktree matches
  the synced union exactly (run 1 catches the wipe on current code), and from
  the second run's `--json` record that `clean.removed == 0` and
  `changed.vs_index.to_write == 0` — pinning both the correctness fix and the
  restored no-op stat-cache behaviour.

## Validation

- `cargo test --workspace`, `cargo clippy --workspace --all-targets`,
  `cargo fmt --check`.
- Re-run the 34k-file loopback benchmark with relative paths and confirm the
  no-op settles at the absolute-path cost with `clean removed: 0`.
