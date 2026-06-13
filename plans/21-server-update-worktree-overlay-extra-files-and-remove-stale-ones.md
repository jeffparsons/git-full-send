# Plan — #21 Server update-worktree: overlay `extra` files and remove stale ones

## Goal

Complete the force-include round-trip on the **server** side. After the existing
`code` checkout, `gfs_server::update_worktree` must also:

1. **Overlay the `extra` tree** (`refs/git-full-send/streams/<id>/extra`) over the
   worktree at **identity paths** — each file lands at its same repo-relative
   path, no `--prefix` remapping (ADR-0007).
2. **Remove force-included files** carried over from a prior sync that are no
   longer in the current `extra` set, without disturbing `code`-tree files
   (ADR-0007 / ADR-0008).

The client already encodes + pushes the `extra` ref alongside `code` every sync
(#20), so **no client changes are needed**.

## Decision locked at pre-plan (approved 👍): combined-tree overlay

Rather than checking out `code` and then separately `checkout-index`-ing the
`extra` tree on top (plus a manual prior-vs-current diff to delete dropped
paths), build **one combined tree** server-side and run the *existing*
single-index pipeline (ADR-0011) against it:

1. Resolve the `code` tree **and** the `extra` tree. A missing `extra` ref is
   treated as the **empty tree** (defensive; in practice the client always pushes
   `extra` alongside `code`).
2. **Overlay `extra` onto `code`** with a gix `Editor` (extra wins on any path
   collision) and write a **combined tree** — mirroring how the client builds
   trees in `encode.rs`.
3. `git read-tree --reset -u <combined>` + `git clean -fdx`, **unchanged**,
   against the per-worktree index.

Why this is correct and preferred:

- **Stale removal falls out for free.** Last sync's combined index contained the
  prior `extra` files; the new combined tree does not, so `read-tree --reset -u`
  deletes the dropped ones — the exact mechanism already proven for dropped
  `code` files (`update_worktree_removes_files_dropped_between_syncs`). No manual
  bookkeeping.
- **`code` files are untouched.** `extra` paths are gitignored and don't collide
  with tracked `code` paths; the overlay only *adds* paths.
- **`clean -fdx` stays as-is and stays safe.** The `extra` files are now
  index-tracked (they're in the combined index), so `clean` won't remove them;
  `-x` still prunes genuine remote-local junk. This resolves the ADR-0011 `-x`
  follow-up **without dropping `-x`**.
- **Efficient.** The stat cache keeps unchanged large build artifacts in place
  instead of churning (delete + rewrite) them every sync.

## Key technical points (to verify during implementation)

- **`gix` is already a server dependency** (`gix::discover` is used in
  `update_worktree_blocking`). The combined-tree build needs object read +
  `tree-editor`. Confirm the server's `gix` feature set includes `tree-editor`
  (the client relies on it via `repo.edit_tree(...)`); add the feature to
  `crates/server/Cargo.toml` only if it turns out gated.
- **Overlay via `Editor`:** seed `repo.edit_tree(code_tree_id)`, then walk the
  `extra` tree recursively and `upsert(path, kind, id)` each blob/leaf so extra
  wins on collision. Prefer iterating the `extra` tree with gix
  (`Tree::traverse()` / a recorder) and upserting at full repo-relative paths.
  Write the combined tree, return its id as a hex string for `read-tree`.
- **Empty `extra` tree** ⇒ combined tree == code tree; the pipeline then removes
  any prior `extra` files via `--reset -u`. Confirm the empty-tree path doesn't
  error in the `Editor`.
- The existing `resolve_code_tree` resolves `code` first so a never-synced stream
  fails cleanly *before* any worktree mutation — keep that ordering; resolve
  `extra` after `code`.

## Architecture / changes

### 1. `crates/server/src/lib.rs` — `update_worktree_blocking`

- After `resolve_code_tree`, resolve the `extra` tree id (new helper, see below).
- Build the combined tree id via a new gix helper `overlay_extra_onto_code`
  (or inline if small): load `code` tree into an `Editor`, upsert all `extra`
  entries, write, return the combined tree's hex id.
- Feed the **combined** tree id (not the bare `code` tree) into the existing
  `read-tree --reset -u` step. The `clean -fdx` step is unchanged.
- Keep the doc-comment accurate: update it to describe the overlay step.

### 2. `crates/server/src/lib.rs` — `extra` tree resolution

- Add `resolve_extra_tree(git_dir, stream) -> Result<TreeId-or-empty, ServerError>`.
  Unlike `resolve_code_tree`, a **missing `extra` ref is not an error** — return
  the empty tree id (or an `Option` the caller maps to empty). Use gix to peel
  `extra_ref(stream)` to a tree, or `git rev-parse --verify --quiet`
  `<extra_ref>^{tree}` and substitute the empty-tree id on absence.
- Decide code shape: doing the resolve + overlay entirely in gix (peel both refs
  to trees, build combined tree, hand the hex id to the CLI `read-tree`) keeps
  object manipulation in gix per ADR-0002 and avoids an extra shell-out.

### 3. Errors

- Add `ServerError` variant(s) as needed for combined-tree build failures
  (e.g. `BuildTree` / `ResolveExtra`), mirroring the existing `RunGit` /
  `Worktree` style. Reuse existing variants where they fit.

### 4. Docs — ADRs

- **ADR-0011:** replace the "code-only" framing — step 1 now resolves both `code`
  and `extra` and overlays them into a combined tree; update the `clean -fdx`
  `-x` consequence note to record that the overlay keeps `extra` files
  index-tracked, so `-x` stays correct (the deferred revisit is now resolved).
- **ADR-0007:** tighten the "remote update must remove no-longer-selected files"
  consequence to point at the combined-tree `--reset -u` mechanism as the
  realisation of the removal.

## Testing

Integration test(s) in `crates/client/tests/transfer.rs` (the existing loopback
harness — `start_server`, `worktree_files`, `tree_paths`, test-support helpers):

1. **Overlay at identity paths.** Set up a repo with committed `code` files and
   a force-included gitignored file (via the project include pattern file); sync;
   `update_worktree`; assert the `extra` file lands at its original repo-relative
   path **and** all `code` files are present and correct.
2. **Stale removal across syncs.** From that state, drop one extra file from the
   selection (remove it / change the include pattern), sync again, `update_worktree`;
   assert the dropped extra file is **gone** from the worktree while the remaining
   `extra` files and **all `code` files are unaffected**.
3. Consider a focused assertion that a remote-local edit to a `code` file is
   still stomped (regression guard that the overlay didn't weaken the `--reset`
   semantics) — only if cheap to fold into the above.

Reuse `crates/test-support` for repo/file/commit helpers and the existing
include-file plumbing exercised by `crates/client/tests/extra.rs`.

## Sequencing

1. Add `resolve_extra_tree` + combined-tree overlay helper; wire into
   `update_worktree_blocking`. Build green.
2. Add error variants as needed.
3. Write the integration test(s); iterate to green.
4. Update ADR-0011 and ADR-0007.
5. `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt --check` all green.

## Acceptance (from the issue)

- `extra` files land at their identity paths over the `code` checkout. ✓ test 1
- Force-included files dropped since the last sync are removed; `code`-tree files
  unaffected. ✓ test 2
- Full force-include round-trip integration test passes. ✓
- Build / test / clippy / fmt green. ✓ sequencing step 5

## Out of scope

- Optional remote-diff diagnostics (ADR-0008 nice-to-have).
- Per-chain delta tuning for the volatile `extra` chain (ADR-0005 follow-up,
  noted in #20's plan).
