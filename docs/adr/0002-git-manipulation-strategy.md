# ADR-0002 — Git manipulation strategy

- Status: accepted
- Date: 2026-06-12

## Context

The whole tool is built around constructing and moving Git objects: synthesising
trees and commits from working-tree state, generating packs, and sending and
receiving them. We need a way to drive Git from Rust that gives us precise
control over object construction and transfer, fits an async codebase, and lets
us reuse digests already present in a Git tree.

There are three broad options for driving Git from Rust:

1. **gix (gitoxide)** — a pure-Rust Git implementation.
2. **Shelling out to the `git` plumbing CLI** — invoking `git` subprocesses.
3. **libgit2** (via the `git2` bindings) — C library with Rust bindings.

## Decision

- **gix-first:** prefer gitoxide for Git operations.
- **Shell out to the `git` plumbing CLI** where gix has gaps or where the
  plumbing command is simply the most direct path to correct behaviour.
- **No libgit2.** We will not depend on libgit2 / `git2`.

We are open to **pausing tool work to upstream fixes to gitoxide** when we hit a
gap that is better closed in gix than worked around here.

## Decision drivers

- Pure-Rust, async-friendly fit with the rest of the stack ([ADR-0001](0001-language-runtime-and-core-crates.md)).
- Fine-grained control over object/tree synthesis and pack generation, which is
  central to [ADR-0004](0004-encoding-the-sync-state-in-git.md) and
  [ADR-0005](0005-transfer-mechanism.md).
- Avoiding a C dependency (libgit2) and its build/FFI characteristics.
- The `git` CLI is already a reasonable assumed dependency on both client and
  remote, so shelling out is a low-cost fallback.

## Research

Even though we will not use libgit2, understanding what it can do is useful for
gap analysis against gix — libgit2 is a mature reference for the operations this
tool needs.

The gap analysis is done:
[Research 0001 — gix / `git` plumbing CLI vs libgit2 capability gap analysis](../research/0001-gix-git-plumbing-vs-libgit2-capability-gap.md)
(pinned to gix 0.84.0, 2026-06-12). Summary of findings:

- **Object / tree / commit synthesis** — native in gix; no gap, no shell-out.
- **Alternate index construction** — *not* native in gix (`gix-index` can derive
  an index from a tree but cannot stage entries or write-tree-from-index, gix
  #293). Avoidable: gix's tree `Editor` builds trees directly without an index.
  Only a forced `git` shell-out if we adopt an index-centric encoding.
- **Pack generation** — partial in gix: it emits packs and reuses existing
  deltas but **cannot compute new deltas and has no bitmaps** (gix #306/#2531).
  For wire packs this forces `git pack-objects` / `git push` to avoid
  pathological whole-object pack sizes.
- **send / receive-pack** — **missing in gix on both sides**: no client push
  (gix #306, explicitly outscoped from 1.0 per #470) and no server
  receive-pack/`accept()` (gix #307). libgit2 has client push but also no server
  side. Forces `git push` → `git receive-pack` / `git daemon`.

This **confirms the gix-first + shell-out posture**: synthesise objects natively
in gix, and shell out to `git` for the pack-and-transfer leg — a single
`git push` → `git receive-pack` exchange covers the pack-delta, push, and
server-receive gaps at once. The report recommends **not** pausing tool work to
upstream anything now: the high-value gaps (delta compression, push, server) are
large and gitoxide has deliberately sequenced them post-1.0; revisit if/when gix
push (#306) lands or a native transfer becomes a project goal.

## Consequences

- The codebase mixes native gix calls with `git` subprocess invocations; we keep
  that boundary explicit and revisit shell-outs as gix matures.
- Some work may flow upstream to gitoxide rather than staying in this repo.
