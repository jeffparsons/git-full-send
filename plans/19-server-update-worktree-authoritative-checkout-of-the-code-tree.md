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

### Mechanism: throwaway index → force checkout → prune (shell out to `git`)

The reassembly is a `git` plumbing pipeline, consistent with ADR-0002 /
Research 0001 (index population and worktree checkout are the gix capability gap;
`git checkout-index` is named directly in ADR-0004) and with the existing server
code, which already shells out to `git receive-pack`. Using a **fresh throwaway
index** each run (rather than a retained worktree index) is what makes the
overwrite unconditional.

Given the resolved git dir, the `code` tree, and the worktree path, with
`GIT_INDEX_FILE` pointed at a temp index for every step:

1. **Resolve** `refs/git-full-send/code` → its tree. A missing ref is a clean
   error before any worktree mutation (`ServerError::MissingCodeRef`).
2. **Populate the index** from the tree:
   `git --git-dir=<dir> read-tree <tree>`.
3. **Force-checkout** every entry into the worktree:
   `git --git-dir=<dir> --work-tree=<wt> checkout-index -a -f`. `-f`
   overwrites unconditionally, so a remote-side edit is stomped **even when the
   blob is unchanged across syncs** — verified during prep on a loopback repo
   (an edited `keep.txt` whose committed content never changed was overwritten
   back to the synced content).
4. **Prune** anything the synced tree no longer contains:
   `git --git-dir=<dir> --work-tree=<wt> clean -fdx`. Against the throwaway
   index, "untracked" == "not in the synced tree", so stale files
   (`-f`), stale directories (`-d`), and stale ignored files (`-x`) all go.
   Exact match for a code-only checkout means ignored files that aren't in the
   tree are stale too; the later `extra` ticket re-introduces the
   deliberately-force-included ignored set as a separate overlay.

This **full overwrite + prune**, rather than a two-tree `read-tree -m -u` keyed
on a retained index, is the deliberate choice: the acceptance criterion "pre-
existing remote-local edits are stomped" includes edits to files unchanged
*between syncs*, which a merge keyed on the previous index would skip. Verified
in prep: with this pipeline, `keep.txt` (remote-edited, blob unchanged) is
restored, `stale.txt` (absent from the tree) is removed, and `gone.txt`/`new`
land correctly.

Why a throwaway index and not a persisted one: nothing here needs to remember a
prior checkout — the tree is the whole truth, the force-checkout rewrites every
path, and the prune is computed against the tree itself. Keeping state would add
a failure mode (a stale or divergent index) for no benefit.

The worktree root is created (`create_dir_all`) if missing; `checkout-index`
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
  gix; a missing ref → `MissingCodeRef`), create a temp index
  (`tempfile`), then run the three `git` steps with `GIT_INDEX_FILE` set,
  surfacing a non-zero exit (with captured stderr) as `ServerError`.

A small private helper runs a `git` step and converts a non-zero status into the
error variant, draining stderr to the message (mirroring `handle_connection`'s
stderr handling).

#### Error variants (`ServerError`)

Add, in the existing `thiserror` style:

- `MissingCodeRef` — the repo has no `refs/git-full-send/code` to check out
  (nothing has been synced yet).
- `CreateWorktree(#[source] std::io::Error)` — the worktree dir could not be
  created.
- `Worktree { step: &'static str, stderr: String }` — a `git` step
  (`read-tree` / `checkout-index` / `clean`) exited non-zero. `step` names which.
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
removed from the worktree and the surviving files match — exercising stale
removal across two syncs, not just against pre-seeded bait.

A tiny local helper walks the worktree dir into a `BTreeSet<String>` of relative
paths for the exact-match comparison. The existing `temp_repo_is_a_git_repository`
and prior transfer tests are unchanged.

## Documentation

Add a short **ADR-0011 — Worktree reassembly mechanics** (status `accepted`),
recording the concrete refinement of ADR-0004/0008: a throwaway index +
`read-tree` → `checkout-index -a -f` → `clean -fdx`, chosen as a **full
overwrite + prune** specifically so edits to blobs unchanged between syncs are
still stomped (why not `read-tree -m -u`), with no retained worktree index. Note
`clean -fdx`'s `-x` is correct for a code-only exact-match checkout and will be
revisited when the `extra` overlay re-introduces force-included ignored files.
Keep it brief and update `docs/adr/README.md` — it refines ADR-0004, mirroring
how #18 added ADR-0010.

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
- **Bare vs non-bare repo.** Resolving the git dir via `gix::discover` (as `bind`
  does) keeps `--git-dir` correct for both; tests use the bare server repo.
- **Empty / unborn `code`.** A repo that has never received a sync has no
  `refs/git-full-send/code`; that is `MissingCodeRef`, surfaced before any
  worktree mutation, not a panic.
