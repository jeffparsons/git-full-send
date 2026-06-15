# Plan — #54: Fix two stale docs (server test docstring; operating.md performance note)

## Goal

Two docs drifted from the current code (identified in the #40 audit). Bring both
back in line with current behaviour, with no code or test changes:

1. `crates/server/tests/integration.rs` — the module docstring still claims the
   tests "do not exercise any server logic yet (it is stubbed with `todo!()`)".
   The server is fully implemented (`listen`, `update_worktree`, `list_streams`).
2. `docs/operating.md` §"Performance note" — describes the pre-#39 walk that
   "descends every non-`.git` directory … is still traversed even when nothing in
   it is selected." #39 added a prune that skips subtrees that cannot contain a
   match, so this is outdated.

## Background

- **Server tests.** `crates/server/tests/integration.rs` currently holds one
  test (`temp_repo_is_a_git_repository`) that exercises the temp-git-repo
  harness, not server logic. The stale half of the docstring is the parenthetical
  "(it is stubbed with `todo!()`)" — the server is no longer stubbed. The
  accurate half is that these tests establish the harness real tests build on.
  The fix is to drop the stubbed-server claim while keeping the harness framing,
  rather than overstate what the single current test covers.
- **Performance note.** The authoritative description of the prune already lives
  in the `crates/client/src/select.rs` module doc, §"Performance — pruning the
  walk" (lines ~46–58): we skip a directory unless it is already inside an
  included subtree *or* some include pattern could still match beneath it; the
  test is derived from each positive pattern's *anchoring*. An **anchored**
  pattern (leading `/` or interior `/`, e.g. `/dist/`, `web-client/dist/`) has a
  literal directory prefix, so directories off that prefix are pruned. An
  **unanchored** pattern (bare basename or `basename/` like `*.wasm`, `dist/`, or
  a leading `**`/wildcard) can match at any depth and forces the full exhaustive
  walk (and is warned about). `docs/operating.md` should summarise this for the
  operator audience and point at the code/ADR for detail.

## Changes

### 1. `crates/server/tests/integration.rs` — module docstring

Rewrite lines 1–4 to drop the stubbed-server claim. Proposed:

```rust
//! Integration tests for the server crate.
//!
//! These currently cover the temp-git-repo test harness that the server's
//! integration tests build on, rather than the server operations themselves
//! (`listen`, `update_worktree`, `list_streams`).
```

Keeps the honest scope (the present test checks the harness) without the false
"stubbed with `todo!()`" assertion. No test bodies change.

### 2. `docs/operating.md` §"Performance note" — rewrite

Replace the current paragraph (lines ~211–216) with a description of the prune
and its residual caveat. Proposed:

```markdown
### Performance note

The selection walk prunes itself: a directory is entered only if it is already
inside a selected subtree, or an include pattern could still match beneath it.
Patterns with a literal directory prefix — anchored by a leading `/` or an
interior `/` (e.g. `/dist/`, `web-client/dist/`) — let the walk skip unrelated
trees (a large ignored `node_modules` is never descended when nothing in it is
selected). The prune is a deliberate over-approximation: it never skips a
directory the exhaustive walk would have selected from.

The residual caveat is the **unanchored** pattern: a bare basename or
`basename/` (e.g. `*.wasm`, `dist/`), or one starting with `**`/a wildcard, can
match at any depth, so it forces the full exhaustive walk and emits a warning
(such a pattern is usually an accidental include). Keep the include set curated
and prefer anchored patterns. See `crates/client/src/select.rs` and
[ADR-0007](adr/0007-syncing-extra-gitignored-files.md) for detail.
```

(Final wording polished at implementation time; substance as above.)

## Out of scope / scope note on the acceptance grep

The acceptance criterion asks that "no other references to the stubbed-server or
pre-prune walk remain (`grep` to confirm)." A repo-wide grep turns up further
hits, but they all live under `plans/` — point-in-time plan records, including
`plans/40-full-audit-and-review.md` (the audit that filed this issue) and the
boilerplate/stub plans (#9, #18, #19). These are historical documents describing
state at the time they were written; they are intentionally left untouched. The
two live docs above are the only places that assert current behaviour, and after
these edits there are no remaining live references to the stubbed server or the
pre-prune walk.

## Verification

- `grep -rn 'stubbed\|todo!()' crates/ docs/` — no live references to a stubbed
  server remain (only `plans/` history).
- `grep -rn 'descends every\|still traversed' docs/` — returns nothing.
- `cargo test -p server` — the integration test still compiles and passes
  (docstring-only change, but confirms nothing broke).
- Read both rewritten passages to confirm they match current behaviour and read
  cleanly.

No code, test logic, or behavioural changes; docs only.
