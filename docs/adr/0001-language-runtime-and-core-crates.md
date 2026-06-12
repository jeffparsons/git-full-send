# ADR-0001 — Language, runtime & core crates

- Status: accepted
- Date: 2026-06-12

## Context

`git-full-send` has a client that runs on a developer's machine and a server
that runs on a remote workstation, both doing I/O-heavy work (driving Git,
moving objects over the network, reading and writing working trees). We want a
single implementation language with strong systems-level control, a good async
story, and a healthy pure-Rust ecosystem for Git tooling (see
[ADR-0002](0002-git-manipulation-strategy.md)).

## Decision

- **Language:** Rust.
- **Async runtime:** Tokio, used from the start (not retrofitted later).
- **Core crates:**
  - `clap` — command-line argument parsing for the client and server
    subcommands.
  - `anyhow` — ergonomic error propagation with context at the application /
    binary level.
  - `thiserror` — typed error enums at reusable library / module boundaries.

The `anyhow` vs `thiserror` split follows the usual convention: `thiserror` for
errors that callers may want to match on across a library boundary, `anyhow` for
top-level binary plumbing where a contextualized opaque error is enough.
Additional crates are pulled in as appropriate.

## Platform scope

The concrete initial target is a **macOS client** syncing to a **Linux (EC2)
remote**. Broad cross-platform support is a non-goal for now, but we avoid
gratuitous platform lock-in where it is cheap to stay portable.

## Consequences

- Async is pervasive; blocking work (e.g. shelling out to Git, filesystem walks)
  is handled with Tokio's facilities rather than ad-hoc threads.
- The dependency set stays small and conventional, which keeps build times and
  audit surface manageable.
