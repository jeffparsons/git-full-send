# Follow-up: Prune the force-include walk to skip directories that cannot contain a match

> Deferred from #20. This file exists so a follow-up issue can be opened whose
> only job is to convert it into a real GitHub issue.

## Context

Force-include selection (`crates/client/src/select.rs`) walks the working tree
and matches each path against the gix-ignore allow-list. To find force-included
files that live under normally-ignored directories (e.g. build outputs under
`dist/`/`target/`), the walk descends into ignored trees rather than pruning them
the way Git / `gix-dir` do.

The consequence is that an unrelated large ignored tree (e.g. `node_modules`) is
traversed every sync even when nothing inside it is selected. Research 0004
explicitly accepts this O(N·M)-once-per-sync cost over a curated list, so this is
a performance refinement, not a correctness issue.

## Goal

Avoid descending into a directory when **nothing inside it could possibly match
an include rule**.

## Why it wasn't done in #20

There's no *obvious, low-risk* way at the gix 0.84 pin: a correct prune has to
introspect each positive pattern's anchoring (literal leading segment vs.
unanchored / leading `**`) and reason about `**` and `!` carve-out interactions.
Getting it wrong silently **drops files** from the sync — a correctness bug — so
it was deliberately deferred rather than rushed into the selection path.

## Sketch of an approach

- Derive, from the positive (include) patterns, their **anchored literal
  directory prefixes** (e.g. `dist/`, `web-client/dist/`, `target/release/**` →
  prefixes `dist`, `web-client/dist`, `target/release`).
- Descend into a directory only if it is itself included, **or** it lies on/under
  one of those prefixes.
- Any **unanchored** pattern (no literal leading segment, or a leading `**` / bare
  basename like `*.wasm`) can match anywhere and therefore forces a full walk —
  detect this and fall back to the current exhaustive behaviour for safety.
- Pin behaviour with tests: anchored-only pattern sets prune `node_modules`;
  unanchored patterns still find matches anywhere.

## Acceptance

- Anchored include patterns no longer traverse unrelated ignored trees.
- Unanchored patterns retain today's exhaustive (correct) behaviour.
- No file that the current exhaustive walk selects is ever dropped by the prune.
