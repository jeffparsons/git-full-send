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

> ⚠ Research task needed: capability gap analysis of gix (and the `git` plumbing
> CLI) versus libgit2 for the operations this tool requires — object/tree
> synthesis, alternate index construction, pack generation, and
> send/receive-pack. Identify gaps that would force shelling out, and which gaps
> are worth closing upstream in gitoxide.

## Consequences

- The codebase mixes native gix calls with `git` subprocess invocations; we keep
  that boundary explicit and revisit shell-outs as gix matures.
- Some work may flow upstream to gitoxide rather than staying in this repo.
