# Plan — #39: Prune the force-include walk to skip directories that cannot contain a match

## Goal

Stop `select.rs`'s working-tree walk from descending into directories that
**cannot** contain a force-include match (e.g. a large ignored `node_modules`
tree), while preserving today's exhaustive — and correct — behaviour for any
pattern that could match anywhere. Add a warning when an include pattern is
**completely unanchored**, since that's most often an accidental "match at any
depth" the user didn't intend.

Performance refinement only: **no file the current exhaustive walk selects may
ever be dropped by the prune.**

## Background (verified)

- The walk in `crates/client/src/select.rs` (`walk_dir`, lines ~203-249)
  unconditionally recurses into every non-`.git` directory, then classifies
  files/dirs against a combined `gix::ignore::Search` allow-list.
- `gix::ignore::Search.patterns` is public: `Vec<List<Ignore>>`, each `List`
  has `patterns: Vec<Mapping>`, each `Mapping.pattern` is a public
  `gix_glob::Pattern` exposing `text: BString`, `mode: Mode`
  (`NO_SUB_DIR` / `ABSOLUTE` / `NEGATIVE` / `MUST_BE_DIR`), and
  `first_wildcard_pos: Option<usize>`. So anchoring is derivable from parsed
  state — no hand re-parsing of gitignore syntax.
- gix parse semantics (confirmed in `gix-glob-0.26.1/src/parse.rs`): leading `/`
  → `ABSOLUTE` (and stripped from `text`); trailing `/` → `MUST_BE_DIR` (stripped);
  no interior slash remaining → `NO_SUB_DIR`; `first_wildcard_pos` is the index
  of the first of `* ? [ \` in `text`.
- The codebase surfaces diagnostics via `tracing` (`tracing::warn!` used in
  `crates/server/src/lib.rs`, `info!` in `crates/client/src/lib.rs`). The CLI
  installs a `tracing_subscriber` fmt layer (default `info`), so a `warn!` from
  `select.rs` reaches the user's terminal.

## Anchoring model

For a **positive** (include) pattern, derive its *prunable literal prefix* `L`
(a list of complete path segments) or mark it **unanchored**:

1. If `NO_SUB_DIR && !ABSOLUTE` → **unanchored** (matched against basename at any
   depth: bare `dist/`, `*.wasm`, `foo`).
2. Otherwise take `lit = text[..first_wildcard_pos]` (or all of `text` if no
   wildcard). If there *was* a wildcard, drop the partial segment containing it by
   truncating at the last `/` in `lit` (e.g. `dist*/app` → `dist` → no complete
   segment). Split the result on `/`, keep non-empty segments → `L`.
3. If `L` ends up empty (leading wildcard / leading `**/`, e.g. `**/foo`,
   `/*.wasm`) → **unanchored**.

Examples: `web-client/dist/` → `["web-client","dist"]`; `target/release/**` →
`["target","release"]`; `/dist/` → `["dist"]`; bare `dist/`, `*.wasm`, `**/foo`
→ unanchored.

This deliberately follows real gitignore semantics, not the looser
`dist/ → dist` reading in the issue sketch: a bare `dist/` *can* match
`node_modules/.cache/dist/`, so it must remain a full walk. (Acknowledged by the
issue owner in the pre-plan thread.)

## Prune decision

Precompute once per `Search`, over **positive** patterns only:
- `any_unanchored: bool`
- `prefixes: Vec<Vec<BString>>` (the `L`s of anchored positives)

Negative (`!`) patterns are ignored for descent — they only ever *shrink* the
selection, so a directory with no positive match is safe to skip regardless of
carve-outs.

Descend into directory `D` (segments `d`) iff **any** of:
1. its inherited/classified state is *included* (already inside a pulled subtree —
   unchanged from today; handles `dist/` pulling its whole tree), **or**
2. `any_unanchored` (→ never prune; exhaustive fallback), **or**
3. some `L` is *path-compatible* with `d`: the shorter of the two is a segment-wise
   prefix of the longer (`L` at/under `D`, or `D` at/under `L`).

This is a safe over-approximation: it may descend slightly more than strictly
necessary but can never prune a directory the exhaustive walk would have selected
from.

## Unanchored-pattern warning

When building the prune info, collect the **positive unanchored** patterns and,
if any, emit one `tracing::warn!` naming each offending pattern (its `text`, with
a leading-`!`/trailing-`/` rendering) and, where available, the source file
(`List.source`). Message conveys: this pattern matches at **any depth**, forces a
full working-tree scan, and is often an accidental include — anchor it with a
leading `/` or a path (`web-client/dist/`) to restrict it. Negative patterns are
not warned about (carve-outs are legitimately unanchored).

Emit at most once per `select` call (dedupe identical pattern texts) to avoid
noise.

## Implementation steps

All in `crates/client/src/select.rs` unless noted.

1. **`struct PruneInfo`** holding `any_unanchored: bool` and
   `prefixes: Vec<Vec<BString>>`. Add `fn build_prune_info(search: &Search) ->
   PruneInfo` that walks `search.patterns[..].patterns[..]`, skips negatives,
   classifies each positive via the anchoring model, fills the struct, and emits
   the unanchored warning(s).
2. **`fn prunable_prefix(pattern: &gix_glob::Pattern) -> Option<Vec<BString>>`**
   — returns `None` for unanchored, else the segment list. Unit-tested directly
   against a table of pattern strings.
3. **`fn can_contain_match(info: &PruneInfo, dir_segments: &[BString]) -> bool`**
   implementing the compatibility test (`any_unanchored` short-circuits true).
4. **Thread `PruneInfo` through the walk.** Build it once in `select_in`
   alongside `load_search`. Pass `&PruneInfo` into `walk_dir`. In the `is_dir`
   branch, compute `child_state = classify(...).unwrap_or(inherited)` as today,
   then recurse only if `child_state || can_contain_match(info, &rel_segments)`.
   Derive `rel`'s segments cheaply (split the `rel` BString on `/`); keep the
   existing `rel`/state plumbing otherwise unchanged.
5. **Module docs.** Update the "Performance note" block (currently lines ~46-51)
   to describe the prune and the unanchored fallback, and add a short note that
   includes should be anchored (`/dist/` or `web-client/dist/`) to benefit.

## Tests (in the existing `#[cfg(test)] mod tests`)

- `anchored_pattern_does_not_descend_unrelated_tree`: include `web-client/dist/`;
  create a sibling `node_modules/<deep>/x` plus a real match under
  `web-client/dist/`. Assert the match is selected and `node_modules` content is
  not — and assert the prune actually fired, not just that nothing matched. Use a
  **traversal seam**: add an optional visited-dir counter/collector the walk
  records into (test-only, e.g. via a closure or a `&mut Vec<BString>` of entered
  dirs), and assert `node_modules` was never entered. (If a seam proves too
  invasive, fall back to a sentinel: a `node_modules/<deep>` directory whose only
  contents would match a *separate* unanchored pattern, asserting they're absent —
  but the explicit counter is preferred for an unambiguous prune assertion.)
- `unanchored_pattern_still_matches_anywhere`: `*.wasm` (or bare `dist/`) finds
  matches nested under an otherwise-prunable tree — exhaustive fallback intact.
- `prunable_prefix` table test: `web-client/dist/`, `target/release/**`, `/dist/`
  → expected segments; bare `dist/`, `*.wasm`, `**/foo`, `dist*/app`, `foo`
  → `None`.
- `compatibility`/`can_contain_match` unit test: `D` above, at, below, and beside
  an `L`.
- All existing tests must pass unchanged (deeply-anchored `target/release/app`,
  `!` carve-out, two-layer user includes, `.git` never descended, `*` includes
  everything).
- A test asserting the warning path is exercised for an unanchored positive
  (assert via behaviour/no panic; capturing `tracing` output is optional — if
  cheap with a test subscriber, assert the pattern text appears, else just cover
  the code path).

## Out of scope / follow-ups

- Anchored-to-root-only optimisation (e.g. `/*.wasm` matches root level only, yet
  we treat it as unanchored/full-walk). Safe but unoptimised; note as a possible
  future refinement rather than implement now.
- No change to selection semantics, pattern file format, or the two-layer
  last-match-wins evaluation.

## Acceptance mapping

- *Anchored include patterns no longer traverse unrelated ignored trees* → step 4
  + the `node_modules` traversal test.
- *Unanchored patterns retain exhaustive behaviour* → `any_unanchored`
  short-circuit + the match-anywhere test.
- *No file dropped* → conservative over-approximation; full existing-test suite
  unchanged.
- *Warn on completely unanchored includes* → step 1's `tracing::warn!`.
