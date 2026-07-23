# ADR-0016 — `clean` spares gitignored files it didn't deliver

- Status: accepted
- Date: 2026-07-23
- Amends: [ADR-0011](0011-worktree-reassembly-mechanics.md) (Decision step 3 and
  the `-x` Consequences bullet)

## Context

[ADR-0011](0011-worktree-reassembly-mechanics.md) reassembles the worktree with
`read-tree --reset -u <combined-tree>` followed by `git clean -fdx`, and blesses
the `-x` on the grounds that force-included `extra` files are folded into the
combined tree and therefore index-tracked, so `-x` "prunes only genuine
remote-local junk".

That reasoning holds for files gfs *delivers*, but `-x` also deletes gitignored
files gfs never delivered. A target worktree that doubles as a live dev
environment loses its build state — `.env`, `node_modules/`, `target/`,
per-user config, caches — on every update (issue #73, verified empirically).
Those files are, by definition, the user's: gitignored and never part of any
synced tree.

## Decision

Run `git clean -fd` — **without `-x`** — as the post-`read-tree` sweep.

- `read-tree --reset -u` against the persistent per-worktree index already
  manages the entire *delivered* set. Both the `code` layer and the
  force-included `extra` layer are folded into one combined tree (ADR-0011
  step 1) and are index-tracked, so read-tree handles their updates **and their
  deletions** — a delivered file (ignored or not) dropped between syncs was in
  the prior combined index and is removed by `--reset -u`. Dropping `-x` does
  not regress that.
- `clean`'s only legitimate job is therefore sweeping untracked non-ignored
  cruft the remote side created; `-fd` still does exactly that.
- Gitignored files gfs didn't deliver are left alone: they belong to the user,
  not gfs.

A delivered-paths manifest (remove only old − new among files gfs itself wrote)
was considered in issue #73 and rejected as unnecessary: read-tree already
prunes stale delivered files, so there is nothing left for a manifest to track.

## Consequences

- A worktree can safely double as a live dev environment: updates no longer
  destroy its gitignored local state.
- Untracked *non-ignored* remote-local files are still removed each update, and
  remote-local edits to delivered files are still stomped — the ADR-0008
  authoritative-overwrite contract is unchanged for everything gfs owns.
- Gitignored cruft the remote side accumulates is no longer gfs's to clear;
  clearing it (if ever wanted) is the user's job.
