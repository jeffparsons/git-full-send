# Plan — #52: Property test for the force-include prune invariant

## Goal

Add a `proptest`-based property test that proves the documented correctness
property of the walk-pruning in `crates/client/src/select.rs` (lines 56–57): the
prune is a pure over-approximation — it changes only *which directories are
entered*, never *which files are selected*. The test generates random directory
trees and pattern sets, runs both the pruned walk and an exhaustive
(prune-disabled) walk, and asserts the selected sets are identical.

## Background

`select_in` builds a `gix::ignore::Search` from the include files, derives a
`PruneInfo` via `build_prune_info(&search)`, then runs `Walk::run`. The walk
skips a directory unless `state || self.prune.can_contain_match(rel)`. The whole
point of the prune is performance; correctness requires that the selected file
set is exactly what an exhaustive walk would produce.

Two `#[cfg(test)]` seams already exist and make this cheap:
- `Walk.entered` records every directory the walk descended into.
- `select_recording` already constructs a `Walk` by hand and returns
  `(selected, entered)`.

Crucially, **the exhaustive walk needs no production-code change**: a
`PruneInfo { any_unanchored: true, prefixes: vec![] }` makes
`can_contain_match` short-circuit to `true` for every directory, so the walk
descends everything except `.git` — exactly the exhaustive walk.

## Changes

### 1. Add `proptest` as a dev-dependency

- `Cargo.toml` (workspace): add `proptest = "1"` (resolve exact latest patch at
  implementation time) under `[workspace.dependencies]`, alphabetically placed,
  with a short comment noting it backs the prune-invariant property test (#52).
- `crates/client/Cargo.toml`: add `proptest.workspace = true` under
  `[dev-dependencies]`.

Dev-only: no impact on the shipped binaries' build or audit surface. Approved in
pre-plan.

### 2. Test-only helper to run the walk with a chosen prune

In the `#[cfg(test)] mod tests` block of `select.rs`, factor a helper that runs a
`Walk` against a caller-supplied `PruneInfo` and returns both outputs:

```rust
/// Run the walk under `prune` and return (sorted/deduped selection, entered dirs).
fn walk_under(root: &Path, user: Option<&Path>, prune: &PruneInfo)
    -> (Vec<String>, Vec<String>)
```

Refactor the existing `select_recording` to delegate to `walk_under` with the
real `build_prune_info(&search)` so there's a single walk-construction site (no
behaviour change to the existing tests).

The exhaustive walk is then `walk_under(root, user, &EXHAUSTIVE)` where
`EXHAUSTIVE = PruneInfo { any_unanchored: true, prefixes: vec![] }`.

### 3. Generators

Use a **small fixed segment alphabet** so generated patterns and tree paths
actually collide (random unique names would make almost every pattern match
nothing, giving a vacuous test). Proposed alphabet: `["a", "b", "dist", "app",
"x.wasm", "y.txt"]` (mix of dir-ish and file-ish names; extensions let `*.wasm`
hit).

**Tree generator.** Produce a `Vec<(path, is_dir)>` describing entries to
materialise: bounded depth (≤ 4) and breadth, a mix of files and directories,
some empty directories. Materialise under a fresh `tempfile::tempdir()`:
`create_dir_all` for dirs and for file parents, `fs::write(.., b"x")` for files.
Keep names from the alphabet so paths are reproducible and matchable.

**Pattern-set generator.** Produce a `Vec<String>` of include lines mixing every
anchoring class the prune distinguishes:
- anchored multi-segment: `a/dist/`, `a/b/app`, `a/dist/**`
- root-anchored: `/dist/`, `/a/b`
- unanchored basename / basename-dir: `dist/`, `app`, `*.wasm`
- leading `**`: `**/app`
- negative carve-outs: `!a/dist/y.txt`, `!*.wasm`

Join with `\n` and write to `root/PROJECT_INCLUDE_FILE`. (Single project layer is
enough to exercise the prune; the user-layer last-match-wins semantics are
already covered by hand-written tests and don't interact with pruning.)

### 4. The property test(s)

```rust
proptest! {
    #![proptest_config(ProptestConfig { cases: 256, .. })]
    #[test]
    fn prune_never_changes_selection(tree in tree_strategy(), patterns in patterns_strategy()) {
        // materialise tree + include file under a tempdir
        let (sel_pruned,  entered_pruned)  = walk_under(root, None, &build_prune_info(&search));
        let (sel_exhaust, entered_exhaust) = walk_under(root, None, &EXHAUSTIVE);

        // Primary invariant: selection is identical.
        prop_assert_eq!(&sel_pruned, &sel_exhaust);

        // Bonus: prune only ever *skips* dirs — entered_pruned ⊆ entered_exhaust.
        // (Guards against a vacuous pass where pruning did nothing.)
        let exhaustive_set: BTreeSet<_> = entered_exhaust.iter().collect();
        prop_assert!(entered_pruned.iter().all(|d| exhaustive_set.contains(d)));
    }
}
```

Notes:
- `search`/`build_prune_info` are reused from the parent module (tests already
  use them, e.g. `select_recording`, `build_prune_info_flags_*`).
- Selection vectors are sorted+deduped inside `walk_under`, so `assert_eq` is a
  set comparison.
- `cases: 256` is a starting point — enough coverage without making the suite
  slow (each case does real tempdir I/O). Adjust down if runtime is a concern.

### 5. Verification

- `cargo test -p gfs-client` — new property test passes alongside existing ones.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.
- Sanity-check the bonus assertion is non-vacuous: confirm at least some
  generated cases actually prune (e.g. via a temporary `eprintln`, removed before
  commit), so we know the test exercises the prune rather than always falling
  back to the exhaustive path.

## Out of scope / non-goals

- No production-code changes to `select.rs` walk logic — the test consumes
  existing `#[cfg(test)]` seams only.
- Not exercising the per-user include layer in the property test (covered by
  existing example tests; orthogonal to pruning).
- No `proptest-regressions` persistence policy change beyond whatever the crate
  does by default; if a regression file is generated it will be committed so the
  failing seed is reproducible.

## Risks

- **Vacuous pass.** Mitigated by the small shared alphabet (patterns actually
  match) and the `entered_pruned ⊆ entered_exhaustive` subset assertion plus the
  manual non-vacuity sanity check in verification.
- **Flaky/slow I/O.** Each case writes a small tree to a tempdir; bounded
  depth/breadth and a modest `cases` count keep this fast and deterministic.
