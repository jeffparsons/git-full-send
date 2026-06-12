# Research: gix / git plumbing vs libgit2 capability gap analysis

- Blocked by: #1 (Foundational ADRs)
- Source: [ADR-0002 — Git manipulation strategy](../docs/adr/0002-git-manipulation-strategy.md)

## Context

We've decided on gix-first + shelling out to the `git` plumbing CLI, and
explicitly **no libgit2**. Even so, libgit2 is a mature reference worth
understanding for gap analysis.

## Goal

Capability gap analysis of **gix (gitoxide)** and the **`git` plumbing CLI**
versus **libgit2** for the operations this tool requires:

- Object / tree synthesis
- Alternate index construction
- Pack generation
- send/receive-pack

## Deliverables

- For each required operation: is it available natively in gix, only via shelling
  out to `git`, or missing in both?
- Identify gaps that would force shelling out, and which gaps are worth closing
  **upstream in gitoxide** (we're open to pausing tool work to upstream fixes).
