# Plan — #73: `update-worktree` clean destroys gitignored files it doesn't own

`update_worktree_blocking` (`crates/server/src/lib.rs`) runs `git clean -d -f -x`
after `git read-tree --reset -u`. The `-x` removes *all* untracked files,
including gitignored ones — so a target worktree that also hosts a live dev
environment loses its build state (`.env`, `node_modules/`, `target/`, per-user
config, caches) on every update. Verified empirically in the issue.

## Decision (settled in the issue)

**Drop `-x`: run `git clean -d -f`.**

Rationale: `read-tree --reset -u` against the persistent per-worktree index
already manages the full *delivered* set. Both the `code` layer and the
force-included `extra` layer (gitignored files gfs itself delivers, e.g.
`dist/`) are folded into one combined tree (ADR-0011 step 1) and are therefore
index-tracked — read-tree handles their updates *and* deletions, so dropping
`-x` doesn't regress removal of delivered files. `clean`'s only legitimate job
is sweeping untracked non-ignored cruft the remote side created. Gitignored
files gfs didn't deliver are, by definition, the user's — not gfs's to delete.

The manifest alternative floated in the issue (track delivered paths, remove
only old − new) is unnecessary for the same reason: read-tree already prunes
stale delivered files, ignored or not. Not pursued.

## Changes

### Code

- `crates/server/src/lib.rs`:
  - The `clean` invocation (~line 940): `["clean", "-d", "-f", "-x"]` →
    `["clean", "-d", "-f"]`.
  - The `update_worktree_blocking` doc block (~lines 871–886): it says
    `clean -fdx` twice and explains why `-x` is safe for `extra` files; rewrite
    to describe `clean -fd` and note that gitignored files not delivered by gfs
    are left alone (delivered ones are index-tracked, so read-tree manages
    them).
  - The per-worktree index comment (~line 910) and the `worktree_state_dir` doc
    (~line 1089): still true (the index file would be untracked non-ignored, so
    `clean -fd` would delete it in the worktree), but reword the `clean -fdx`
    mention.
- `crates/server/src/metrics.rs` (~line 69): "The `git clean -fdx` step." →
  match the new args.

### ADR

Per ADR-0000 (new ADR rather than rewriting history):

- New **ADR-0016 — `clean` spares gitignored files it didn't deliver**
  (accepted): records dropping `-x`, the ownership rationale, and why delivered
  ignored files don't regress (index-tracked ⇒ read-tree manages them). Amends
  ADR-0011's Decision step 3 and its `-x` Consequences bullet on that point.
- ADR-0011: short amendment note in its status line pointing at ADR-0016 — not
  a rewrite.
- `docs/adr/README.md`: add the ADR-0016 index row.

### Tests (`crates/client/tests/transfer.rs`)

Following the existing `update_worktree_*` loopback harness:

- `update_worktree_leaves_undelivered_gitignored_files_alone`: sync a tree
  whose `.gitignore` ignores `ignored.txt` and `ignored-dir/`; update the
  worktree; create `ignored.txt` and `ignored-dir/cache.bin` remote-side; sync
  and update again; assert both survive.
- `update_worktree_still_removes_untracked_cruft`: same second-update flow, but
  the remote-side files are *not* ignored; assert they are removed (the `-fd`
  clean still does its job).
- Existing `update_worktree_removes_files_dropped_between_syncs` and
  `update_worktree_removes_extra_dropped_between_syncs` must keep passing —
  they prove read-tree, not `clean -x`, owns deletion of delivered files.

## Validation

- `cargo test --workspace`, `cargo clippy --workspace --all-targets`,
  `cargo fmt --check`.
