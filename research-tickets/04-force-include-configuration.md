# Research: force-include configuration mechanism

- Blocked by: #1 (Foundational ADRs)
- Source: [ADR-0007 — Syncing extra (normally-gitignored) files](../docs/adr/0007-syncing-extra-gitignored-files.md)

## Context

`git-full-send` deliberately syncs a set of normally-gitignored files (e.g.
locally-built web-client outputs, per-user config). We need to decide how this
force-include set is declared and how the files land in the remote worktree.

## Goal

Design the configuration mechanism for the force-include set:

- **Where** it is declared.
- **Granularity** — globs vs. explicit paths.
- **Scope** — per-project vs. per-user.
- **How** the selected files are placed into the remote worktree.

## Notes

- The on-the-wire encoding of these files is part of
  [ticket 02 (encoding the sync state)](02-encoding-sync-state.md); this ticket
  is about *what is included and how that is configured*.
