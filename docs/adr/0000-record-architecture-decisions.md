# ADR-0000 — Record architecture decisions

- Status: accepted
- Date: 2026-06-12

## Context

`git-full-send` is starting from scratch and several foundational decisions
shape everything that follows — the language, how we drive Git, the client/server
shape, and how working state is transferred. We want these decisions, and the
reasoning behind them, to be discoverable and durable rather than living only in
issue threads or in people's heads. Some decisions are already settled; others
are genuinely open and need research before we commit.

## Decision

We record significant architectural decisions as **Markdown ADRs** (MADR-style)
under `docs/adr/`.

Conventions:

- **Filenames:** `NNNN-kebab-title.md`, with zero-padded sequential numbers
  starting at `0000`. This document is `0000`.
- **Template:** MADR-style, but **not rigid**. Each ADR includes only the
  sections that are appropriate for it — typically some of: Status, Context /
  Problem Statement, Decision Drivers, Considered Options, Decision (Outcome),
  and Consequences. Don't pad an ADR with empty sections.
- **Status lifecycle:** `proposed` → `accepted` → `deprecated` or
  `superseded by ADR-NNNN`. An ADR for a decision we have *not* yet made stays
  `proposed` and records the constraints, drivers, and options on the table
  without forcing a conclusion.
- **Research callouts:** where an ADR depends on work we haven't done yet, mark
  it inline with a blockquote callout:

  > ⚠ Research task needed: …

  Each such callout is a candidate for its own tracking issue.
- **Index:** `docs/adr/README.md` lists every ADR with its number, title, and
  status. Keep it in sync when adding or changing an ADR.

## Consequences

- Contributors add a new numbered ADR for each significant decision and update
  the index. Superseding a decision means adding a new ADR and marking the old
  one `superseded by ADR-NNNN` rather than editing history.
- The root `CLAUDE.md` points here so the process is easy to find.
