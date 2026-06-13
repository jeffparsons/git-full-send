# Plan — #20 Client sync: force-include selection and the `extra` commit

## Goal

Extend the client `sync` so that, **alongside** the existing `code` commit, it:

1. **Selects** the force-included, normally-gitignored files via a gix-native
   allow-list — no `git` shell-out (ADR-0007, Research 0004).
2. **Builds the `extra` tree** with the gix `Editor` and writes a commit
   **parented on the previous sync's `extra` tip** (rootless on first sync),
   under a new per-stream `extra` ref (ADR-0004).
3. **Pushes it alongside `code` in the same exchange** and **retains** the
   pushed `extra` tip locally as the next delta base (ADR-0005).

Out of scope (next ticket): exploding `extra` onto the remote worktree and
stale-file removal. **The server needs no changes** — its `pre-receive` hook
already accepts any ref under `refs/git-full-send/`, so the new `extra` ref lands
untouched.

## Decisions locked at pre-plan (approved 👍)

- **Project pattern file:** `.git-full-send-include` at the repo root (committed;
  rides along in the `code` tree automatically). Read from the working tree.
- **Per-user pattern file:** `$XDG_CONFIG_HOME/git-full-send/include`, falling
  back to `~/.config/git-full-send/include`. Read on the client only; never
  travels. A missing file is an empty layer (not an error).
- **New refs** in `gfs-common`: `extra_ref(stream)` → `…/streams/<id>/extra`, and
  a retained-tip ref `…/streams/<id>/sent/extra`. The `extra` commit parents on
  the retained `sent/extra` tip (what the server is known to have), mirroring the
  `code`/`sent` retention model so a failed push never leaves us parenting on a
  commit the server lacks.
- **One push exchange:** extend the push to send `+code` and `+extra` in a single
  `git push`. Per-chain whole-object delta tuning for the volatile `extra` chain
  (ADR-0005) is deferred as a follow-up refinement; the single `--thin` exchange
  is kept for now.
- **Empty selection:** still produce an `extra` commit (empty tree if nothing
  selected) so the chain and the push stay uniform across syncs.

## Key technical findings (verified against the gix 0.84 pin)

- `gix::ignore` (`gix-ignore` 0.21.1) and `gix::glob` (`gix-glob` 0.26.1) are
  **already available** with the client's current `gix` features (defaults pull
  `extras → excludes`; `tree-editor` is already on). **No `Cargo.toml` change is
  needed** for selection. (Confirm during implementation; add `excludes` to the
  feature list only if the re-exports turn out gated.)
- `gix_ignore::Search` realises **last-match-wins with `!` negation**:
  - `add_patterns_buffer(bytes, source, root=None, Ignore::default())` appends a
    pattern list parsed from a buffer; `root = None` makes patterns repo-root
    relative (top-level `.gitignore` semantics — what we want).
  - `Search::pattern_matching_relative_path(rel, is_dir, Case::Sensitive)`
    returns `Option<Match>`; matching iterates pattern **lists in reverse** and
    patterns within a list in reverse → the last-added list and last matching
    pattern win. `Match::pattern.is_negative()` is the `!` flag.
  - Adding the **project** buffer first, then the **user** buffer, is provably
    equivalent to flat-concatenated last-match-wins (because user patterns are
    always last in flat order), so it implements `[project, then user]`
    last-match-wins exactly while keeping per-source attribution.
- **gix-dir cannot drive this walk.** Its recursion into an *ignored* directory
  is gated on a positive **pathspec** (`Status::Ignored::can_recurse` returns
  `true` only under `for_deletion` repo-finding modes or a non-ignoring
  `pathspec_match`). Our allow-list is inverted gitignore syntax, not pathspecs,
  and build outputs live under ignored `dist/`/`target/`. So we walk the
  filesystem ourselves and apply the gix-ignore matcher — precisely how
  Research 0004 frames selection ("an independent allow-list matched against the
  working-tree filesystem, independent of Git's ignore tree").

## Architecture / changes

### 1. `crates/common/src/lib.rs` — refs

Add alongside `code_ref`/`sent_ref`:

```rust
/// `…/streams/<id>/extra` — the force-included (normally-gitignored) tree.
pub fn extra_ref(stream: &StreamId) -> String { format!("{STREAMS_PREFIX}{}/extra", stream.as_str()) }

/// `…/streams/<id>/sent/extra` — retained last-pushed `extra` tip / delta base.
pub fn sent_extra_ref(stream: &StreamId) -> String { format!("{STREAMS_PREFIX}{}/sent/extra", stream.as_str()) }
```

Extend the existing namespace/round-trip unit tests to cover both.

### 2. `crates/client/src/select.rs` — new module (the allow-list)

Public surface: `pub fn select_extra_paths(repo: &gix::Repository, workdir: &Path) -> Result<Vec<BString>, SelectError>` returning repo-relative paths of the
force-included files (sorted, deterministic).

Steps:

1. **Load patterns** into one `gix_ignore::Search`:
   - Project: read `<workdir>/.git-full-send-include` if present;
     `search.add_patterns_buffer(&bytes, project_path, None, Ignore::default())`.
   - User: resolve `$XDG_CONFIG_HOME/git-full-send/include` (fall back to
     `$HOME/.config/git-full-send/include`); if present, add as a second buffer.
   - Neither present ⇒ empty `Search` ⇒ empty selection (caller still writes an
     empty `extra` tree).
2. **Walk the working tree** with our own recursion (std `read_dir`), carrying an
   inherited tri-state (`Included` / `Excluded`), starting `Excluded`:
   - **Skip `.git`** (and any nested `.git`) outright.
   - For a **directory** at `rel`: evaluate
     `search.pattern_matching_relative_path(rel, Some(true), Sensitive)`. A
     non-negative match → `Included`; a negative (`!`) match → `Excluded`; no
     match → inherit. **Always descend** (except `.git`) so deeper patterns and
     `!` carve-outs under an included parent still apply — the inversion is what
     lets us re-include/carve-out under a normally-ignored parent without hitting
     Git's "can't re-include under an excluded parent" trap.
   - For a **file or symlink** at `rel`: evaluate with `Some(false)`. A
     non-negative match → select; a negative match → skip; no match → select iff
     inherited state is `Included` (so a directory pattern like `dist/` pulls its
     whole subtree, while `!dist/secret` carves a file back out).
   - This dir-then-leaf evaluation reproduces standard gitignore directory
     semantics (a trailing-slash `dir/` pattern decides the subtree at the
     directory level), which a per-leaf-only match would miss.
   - Non-regular, non-symlink entries (FIFO/socket) are skipped, mirroring
     `encode::overlay_from_disk`.
3. Return the selected paths.

**Known limitation (documented, not fixed now):** the walk descends every
non-`.git` directory that isn't carved out, so an unrelated large ignored tree
(e.g. `node_modules`) is traversed even when nothing in it is selected. Research
0004 explicitly accepts O(N·M) once-per-sync over a curated list. A future
optimisation can prune by deriving anchored directory prefixes from the positive
patterns (descend only those trees; `**`/unanchored patterns still force a full
walk). Flag in a code comment; out of scope here.

`SelectError`: I/O errors reading the worktree / pattern files, surfaced with the
offending path (same shape as `EncodeError::ReadWorktree`).

### 3. `crates/client/src/encode.rs` — build the `extra` commit

Add `pub fn encode_extra(repo_dir: &Path, stream: &StreamId) -> Result<EncodeOutcome, EncodeError>` (or a shared `EncodeExtraOutcome` with `{ commit, extra_ref }`):

- Discover repo + workdir (reuse the existing guards).
- `let paths = select::select_extra_paths(&repo, &workdir)?;`
- Seed `repo.edit_tree(empty_tree)`; for each selected path, reuse the existing
  `overlay_from_disk` helper (write blob, upsert with disk-derived mode/symlink
  handling). **Refactor `overlay_from_disk` to be shared** by both `encode` and
  `encode_extra` (it already does exactly the read-blob-and-upsert work).
- **Parent** = the retained `sent/extra` tip resolved from
  `gfs_common::sent_extra_ref(stream)` if it exists, else none (rootless first
  sync). Resolve via `repo.try_find_reference(&sent_extra_ref)?` → peel to commit
  id. (Deliberately the *sent* tip, not the local `extra` ref, so the parent is
  always something the server already has.)
- Write the commit with the same synthetic identity/message convention (e.g.
  message `git-full-send: extra (force-included) snapshot`).
- Force-update `extra_ref(stream)` via the existing `update_ref`-style raw
  transaction (`PreviousValue::Any`). Generalise `update_code_ref` into a small
  `update_ref(repo, name, id, msg)` reused by both.

Return the commit id + ref name.

### 4. `crates/client/src/push.rs` — push both refs in one exchange + retain extra

- Generalise `push_ref` to **`push_refs(repo_dir, remote, ref_names: &[&str])`**
  building one `+src:dst` refspec per ref and passing them all to a single
  `git push --thin … fd::…` invocation (transport/fd handling unchanged). Keep a
  thin `push_ref` wrapper if convenient for the existing test seam, or update the
  one caller/test.
- Generalise `retain_pushed_tip` to take the target ref name (or add
  `retain_pushed_extra_tip`) so it can pin `sent_extra_ref(stream)` to the pushed
  `extra` commit, mirroring the existing `sent/code` retention. Advance **both**
  `sent/code` and `sent/extra` only **after** the single push succeeds.
- Note in the module doc the deferred per-chain delta policy (ADR-0005:
  whole-object preferred for the volatile chain) so the trade-off is recorded at
  the seam.

### 5. `crates/client/src/lib.rs` — wire into `sync`

- `pub use encode::{… , encode_extra}` and `gfs_common::{extra_ref, sent_extra_ref}` as needed.
- In `sync`: after `encode`, call `encode_extra`; push `[&code_ref, &extra_ref]`
  in one `push_refs`; then retain both tips. Update the `tracing` lines to log the
  `extra` commit too.

## Tests (`crates/client/tests/`)

Follow the existing `integration.rs` style: build the temp repo with the `git`
CLI and assert on results via the `git` CLI (independent of the implementation's
own `gix`). Likely a new `extra.rs` integration test file (mirrors `encode`
tests) plus unit tests in `select.rs`.

- **Selection — directory include + carve-out + per-user layer.** Temp repo with
  a committed `.gitignore` ignoring `dist/`, on-disk `dist/app.js`,
  `dist/app.wasm`, `dist/secret.txt`, and an untracked-but-not-ignored source
  file. Project `.git-full-send-include` = `dist/` and `!dist/secret.txt`. Point
  the per-user file (via an env/path override hook used only in tests) at a buffer
  that carves out one more path or adds one — assert the per-user `!` overrides
  the project include. Assert the `extra` tree (`git ls-tree -r`) contains exactly
  the expected paths at their identity repo-relative locations, and that
  `dist/secret.txt` is absent. Assert **no `git` shell-out happens for selection**
  (selection is pure gix; the test just checks the resulting tree).
- **Re-inclusion under an ignored parent works** (the trap we sidestep): a file
  under an ignored `target/` is selected by a `target/**`-style include.
- **Chaining.** Run the `extra` build twice (changing a selected file between
  runs) and assert the second `extra` commit's parent is the first
  (`git rev-list --parents` / `git cat-file commit`). First run is rootless
  (no parents).
- **Empty selection.** No pattern files ⇒ an `extra` commit with an empty tree is
  still produced (uniform chain).
- **Push-alongside (integration).** Extend the existing client↔server transfer
  test so a `sync` lands **both** `code` and `extra` refs on the bare server repo
  in one exchange, and the local `sent/extra` retention ref is advanced.
- `gfs-common` unit tests: `extra_ref`/`sent_extra_ref` shapes.

**Test seam for the per-user path:** add a small internal override (e.g. an env
var like `GIT_FULL_SEND_USER_INCLUDE`, or a parameterised internal entry point)
so tests can supply a per-user file without touching the developer's real
`~/.config`. Decide the exact mechanism during implementation; keep it internal
and documented.

## Validation

- `cargo build --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Risks / open points (carry into implementation)

- **Per-chain delta policy** (whole-object for `extra`) is deferred — recorded in
  the push module doc; revisit if `extra` transfers prove slow.
- **Walk performance** on large unrelated ignored trees — documented limitation
  with a concrete future optimisation (anchored-prefix pruning).
- **gix feature gating** for `gix::ignore` — expected already-on; verify and add
  `excludes` to the client's `gix` features only if needed.
- **Per-user file location override** for tests — mechanism to be finalised in
  implementation.

## Out of scope

- Remote explode of `extra` over the `code` checkout and stale-file removal
  (next ticket; ADR-0008).
- Any server-side change (none required).
