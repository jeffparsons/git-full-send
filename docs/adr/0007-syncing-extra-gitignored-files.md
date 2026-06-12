# ADR-0007 — Syncing extra (normally-gitignored) files

- Status: proposed
- Date: 2026-06-12

## Context / problem statement

Part of the point of `git-full-send` is to sync files that are **normally
gitignored** but that we deliberately want on the remote. The motivating cases:

- **Web-client build outputs** produced on the MacBook. The build is very CPU
  intensive, so we run it on the developer's powerful laptop for snappier
  interactive development rather than on the remote — but the outputs still need
  to be present on the remote for the rest of the build/run flow.
- **Per-user config files** that we want synced.

Assume there will be other good reasons to sync large-ish, normally-unversioned
files too. We need to decide how the force-include set is **configured** and how
those files **land in the remote worktree**.

## Decision drivers

- The set of extra files is project- and user-specific, and may be large-ish.
- These files would otherwise be gitignored, so the mechanism must override
  ignore rules for an explicit, controlled set rather than syncing all ignored
  files indiscriminately.
- The files must end up in the right place in the remote worktree alongside the
  synced code.

## Relationship to other decisions

How these files are encoded for transfer is part of
[ADR-0004](0004-encoding-the-sync-state-in-git.md) (e.g. stacked in a commit, on
a branch of their own, or exploded out separately on the remote). This ADR is
about **what is included and how that is configured**, not the on-the-wire
encoding.

## Status

Proposed.

> ⚠ Research task needed: design the configuration mechanism for the
> force-include set (where it is declared, granularity — globs vs explicit
> paths, per-project vs per-user) and how the selected files are placed into the
> remote worktree. Coordinate with
> [ADR-0004](0004-encoding-the-sync-state-in-git.md).
