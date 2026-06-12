# Plan — #19: Server `update-worktree`: authoritative checkout of the code tree

## Goal

Implement `gfs_server::update_worktree`: check the `refs/git-full-send/code`
tree out into a configured worktree directory as an **authoritative, destructive
overwrite** (ADR-0008). After it runs, the worktree matches the synced `code`
tree *exactly* — every remote-side edit is stomped (even to files whose blob is
unchanged between syncs), and every file the synced tree no longer contains is
removed.

This completes the walking skeleton — a working end-to-end committed +
working-tree sync (encode → push → checkout) — minus the `extra` force-include
overlay, which is a later ticket.

Per ADR-0003 the operation is invoked **independently** of `listen` (a build
orchestrator triggers it on demand) and is configured with the minimal pair of
the target **repo** + **worktree dir**. Scope is the **`code` tree only**: the
`extra` overlay and its stale-file removal, and the optional remote-diff
diagnostics (ADR-0008 nice-to-have), stay out.

## Design

### Mechanism: persistent index → `read-tree --reset -u` → prune (shell out to `git`)

The reassembly is a `git` plumbing pipeline, consistent with ADR-0002 /
Research 0001 (index population and worktree checkout are the gix capability gap)
and with the existing server code, which already shells out to `git receive-pack`.

The driving requirement is **efficiency on a large repository**: a code tree can
be huge, while a typical sync changes only a handful of files, so an
`update-worktree` must do work proportional to the *delta*, not the whole tree.
This rules out the naïve "rewrite everything" pipeline (throwaway index +
`checkout-index -a -f`), which re-writes every file in the worktree on every run.
Prep measured this directly on a 200-file loopback repo: a second update that
changed one file still rewrote all 200 (every file's mtime advanced) — O(repo)
disk writes per run.

Instead the server keeps a **persistent per-worktree index** that records what it
last checked out, and leans on Git's stat cache exactly as `git checkout` /
`git reset --hard` do. With `GIT_INDEX_FILE` pointed at that persistent index and
`--git-dir` / `--work-tree` set, and the resolved `code` tree `<T>`:

1. **Resolve** `refs/git-full-send/code` → its tree. A missing ref is a clean
   error before any worktree mutation (`ServerError::MissingCodeRef`).
2. **Reset index + worktree to the tree, discarding local changes:**
   `git --git-dir=<dir> --work-tree=<wt> read-tree --reset -u <T>`.
   - `--reset` resets the index to `<T>` and **discards** worktree-local changes
     instead of refusing on them — this is what stomps remote-side edits.
   - `-u` updates the worktree to match, and **removes files** that were in the
     prior index but are absent from `<T>` (files dropped between syncs).
   - Because the index carries a stat cache, only paths that actually differ
     (tree delta, or a file whose on-disk stat changed — i.e. a remote edit) are
     re-hashed and re-written; unchanged files cost one `lstat` and are skipped.
3. **Prune untracked additions:**
   `git --git-dir=<dir> --work-tree=<wt> clean -fdx`. `read-tree` removes
   dropped *tracked* files; `clean` removes anything never tracked — remote-added
   files (`-f`), directories (`-d`), and ignored files (`-x`). For a code-only
   exact match, ignored files not in the tree are stale too; the later `extra`
   ticket re-introduces the deliberately-force-included ignored set as a separate
   overlay (and will revisit `-x`).

Prep verified this pipeline end-to-end on the loopback repo across three syncs:
a remote edit to a file whose blob is **unchanged** between syncs is restored
(stat-dirty detection → overwrite), a file **dropped** between syncs is removed,
an **untracked** remote addition is removed, and a file unchanged across the sync
is **not** rewritten (its mtime is preserved) — i.e. exact match *and*
delta-proportional work. The O(n) `lstat` scan is the same cost `git status`
pays and is fine on large repos; the win is O(changed-files) writes.

**Why not `read-tree -m -u` (the merge form)** the pre-plan flagged: keyed on the
prior tree, it skips any path whose blob is unchanged between syncs and so would
*not* revert a remote edit to such a file. `--reset` is keyed on the worktree
state via the stat cache, so it does. **Why a persistent index, not throwaway:**
the throwaway form is what forces the O(repo) rewrite — the persistent index is
precisely the state that lets Git skip unchanged files.

**Persistent index location.** Stored under the git dir in a gfs-managed
location keyed by the worktree path (e.g.
`<git-dir>/git-full-send/worktrees/<hash>/index`), never *inside* the worktree
(or `clean -fdx` would delete it). It is pure cache: if it is missing or stale
(first run, or it was deleted), `read-tree --reset -u` simply has no stat cache
to exploit and degrades to a one-time full rewrite — still producing an exact
match — then is incremental again. Verified in prep that a deleted index still
yields an exact-match checkout. So no integrity tracking is needed.

The worktree root is created (`create_dir_all`) if missing; `read-tree -u`
creates intermediate subdirectories itself.

### Server (`crates/server/src/lib.rs`)

Replace the `update_worktree()` `todo!()` stub with the real operation, keeping
the async-wrapper-over-`spawn_blocking` shape already used by `listen` (the work
is blocking `git` shell-outs; it must not run on the async executor):

- `pub async fn update_worktree(repo: PathBuf, worktree: PathBuf) -> Result<(),
  ServerError>` — CLI entry point;
  `tokio::task::spawn_blocking(move || update_worktree_blocking(&repo, &worktree))`,
  mapping a join failure to `ServerError::Join` exactly as `listen` does.
- `fn update_worktree_blocking(repo: &Path, worktree: &Path) -> Result<(),
  ServerError>` — the pipeline above. Resolve/validate the repo with
  `gix::discover(repo)` (reusing `ServerError::NotARepo`, matching `bind`) and
  take its git dir for `--git-dir`, so the operation works against a bare server
  repo. Resolve the `code` tree (via `git rev-parse --verify <ref>^{tree}` or
  gix; a missing ref → `MissingCodeRef`), derive + `create_dir_all` the
  persistent-index dir under the git dir, create the worktree dir, then run the
  two `git` steps (`read-tree --reset -u`, `clean -fdx`) with `GIT_INDEX_FILE`
  pointed at the persistent index, surfacing a non-zero exit (with captured
  stderr) as `ServerError`.

A small private helper runs a `git` step and converts a non-zero status into the
error variant, draining stderr to the message (mirroring `handle_connection`'s
stderr handling).

#### Error variants (`ServerError`)

Add, in the existing `thiserror` style:

- `MissingCodeRef` — the repo has no `refs/git-full-send/code` to check out
  (nothing has been synced yet).
- `CreateWorktree(#[source] std::io::Error)` — the worktree dir (or the
  persistent-index dir) could not be created.
- `Worktree { step: &'static str, stderr: String }` — a `git` step
  (`read-tree` / `clean`) exited non-zero. `step` names which.
- A spawn failure reuses/extends the existing `Spawn` shape (or a sibling
  variant) so a missing `git` binary is a clear error.

### Shared constant (`gfs-common`)

The server needs to name the same ref the client writes. Today
`CODE_REF = "refs/git-full-send/code"` lives in `gfs_client::encode` and is
re-exported as `gfs_client::CODE_REF`; the server must not depend on the client.
Move the constant to `gfs_common` (it is shared protocol knowledge, exactly like
`REF_NAMESPACE`) as `gfs_common::CODE_REF`, and have `gfs_client` re-export it so
`gfs_client::CODE_REF` (used by the existing transfer tests) keeps working
unchanged. The server references `gfs_common::CODE_REF`. Single source of truth,
no drift, no test churn.

### CLI (`crates/cli/src/main.rs`)

`Command::UpdateWorktree` currently takes no args. Give it the ADR-0003 minimal
config:

- `UpdateWorktree(UpdateWorktreeArgs)` with `--repo <PATH>` (required; the
  receiving git repo) and `--worktree <PATH>` (required; the disposable checkout
  dir).
- `main`: `gfs_server::update_worktree(args.repo, args.worktree).await?`.

Update the module doc that says the subcommands "currently stub their work out".

## Integration test (`crates/client/tests/transfer.rs`)

Extend the existing loopback transfer suite (it already depends on `gfs-server`
and drives encode → push → assert-on-server, so the full
encode → push → update-worktree chain belongs here). Assertions go through the
`git` CLI / filesystem, independent of the implementation's own `gix`.

New test `update_worktree_makes_worktree_match_code`:

1. `init_bare_repo()` server + `start_server`; client repo committing
   `keep.txt = "v1"` and `drop.txt = "v1"`, plus a dirty untracked
   `new.txt = "v1"`. `sync(client, addr)` lands the `code` tree on the server.
2. Create a separate worktree temp dir pre-populated with **stomp/stale bait**:
   `keep.txt = "REMOTE-EDIT"` (a remote-side edit to a file whose synced blob is
   unchanged) and `stale.txt = "junk"` (absent from the synced tree).
3. `gfs_server::update_worktree(server_path, worktree_path).await`.
4. Assert:
   - the set of files in the worktree (recursive walk) equals
     `tree_paths(server, CODE_REF)` exactly — i.e. `{keep.txt, drop.txt,
     new.txt}`;
   - `stale.txt` is gone (file absent from the synced tree is removed);
   - `keep.txt` reads `"v1"` (the pre-existing remote-local edit is stomped).

A second test `update_worktree_removes_files_dropped_between_syncs` (small, high
value): sync a tree containing `gone.txt`, `update_worktree`, then on the client
delete `gone.txt`, sync again, `update_worktree` again, and assert `gone.txt` is
removed from the worktree and the surviving files match — exercising the
persistent-index path across two `update_worktree` runs (`read-tree --reset -u`
removing a tracked file dropped between syncs), not just removal against
pre-seeded bait. Behavioural assertions only — both runs land an exact match;
the test does not try to assert incrementality (mtime/write-count), which is a
property of Git's stat cache rather than of this code.

A tiny local helper walks the worktree dir into a `BTreeSet<String>` of relative
paths for the exact-match comparison. The existing `temp_repo_is_a_git_repository`
and prior transfer tests are unchanged.

## Documentation

Add a short **ADR-0011 — Worktree reassembly mechanics** (status `accepted`),
recording the concrete refinement of ADR-0004/0008: a **persistent per-worktree
index** + `read-tree --reset -u` → `clean -fdx`. Capture the two forces and why
they pick this point in the design space:

- **Efficiency on large repos** → reuse a persistent index so Git's stat cache
  makes the work delta-proportional (O(changed-files) writes), rather than a
  throwaway index + `checkout-index -a -f` that rewrites the whole worktree every
  run.
- **Correctness (stomp remote edits, even to unchanged-blob files)** → `--reset`
  keys on worktree stat, not on a prior-tree diff, so it reverts those edits;
  `read-tree -m -u` (merge) would skip them.

Note the persistent index is pure cache (safe to lose → degrades to a one-time
full rewrite), lives outside the worktree, and that `clean -fdx`'s `-x` is
correct for a code-only exact-match checkout and will be revisited when the
`extra` overlay re-introduces force-included ignored files. Keep it brief and
update `docs/adr/README.md` — it refines ADR-0004, mirroring how #18 added
ADR-0010.

## Quality gates (acceptance)

- `cargo build`, `cargo test`, `cargo clippy --all-targets -- -D warnings`,
  `cargo fmt --check` all green.
- `update-worktree` makes the configured worktree exactly match the synced
  `code` tree, destructively: stale files removed, pre-existing remote-local
  edits stomped.
- The end-to-end loopback test (encode → push → update-worktree) passes,
  including the stomp + stale-removal assertions.

## Out of scope (unchanged from the ticket)

- The `extra` (force-include) overlay and its stale-file removal — next-but-one
  ticket. `update_worktree` here handles `code` only.
- Optional remote-diff diagnostics (ADR-0008 nice-to-have) — not in the MVP.
- Any change to `listen` / the transfer leg (#18) — `update-worktree` is invoked
  independently.

## Risks / notes

- **`git clean -fdx` blast radius.** Confined to `--work-tree=<wt>` and never
  touches `--git-dir`, but it deletes; the operation assumes the worktree dir is
  a dedicated, disposable path (ADR-0008). Tests pin the exact-match outcome.
- **Persistent index staleness.** The index is a performance cache, not a source
  of truth — `--reset` re-stats the worktree, so a stale or missing index only
  costs a one-time full rewrite, never a wrong result (verified in prep). Keyed
  by worktree path and kept outside the worktree so `clean` can't eat it.
- **O(n) `lstat` scan.** `read-tree --reset -u` stats every path to find the
  delta — the same cost `git status` pays, fine on large repos; the optimisation
  target is *writes*, which stay O(changed-files).
- **Bare vs non-bare repo.** Resolving the git dir via `gix::discover` (as `bind`
  does) keeps `--git-dir` correct for both; tests use the bare server repo.
- **Empty / unborn `code`.** A repo that has never received a sync has no
  `refs/git-full-send/code`; that is `MissingCodeRef`, surfaced before any
  worktree mutation, not a panic.
