# Plan — #17: Client sync, encode the `code` commit (working-tree state)

## Goal

Give `gfs-client` the first building block of `sync`: synthesise the
developer's current code state — committed history **plus** working-tree changes
(staged & unstaged) — into a single Git commit written under the scratch ref
`refs/git-full-send/code`, **without touching** the user's branch, index, or
working tree. No transfer; the ticket stops once the `code` ref exists locally.

Per the pre-plan discussion (issue #17), the `code` tree captures **full
working-tree fidelity**: current on-disk contents of tracked files **and**
untracked-but-not-gitignored files, with deletions respected. Gitignored
force-includes stay out of scope (the separate `extra` ticket).

## Design

### Module layout

- New module `crates/client/src/encode.rs`, `pub use`d from `lib.rs`.
- Public surface:
  - `pub fn encode(repo_dir: &Path) -> Result<EncodeOutcome, EncodeError>` —
    pure, synchronous gix work; returns the new `code` commit id (plus the ref
    name) so callers/tests can inspect it.
  - `pub struct EncodeOutcome { pub commit: gix::ObjectId }` (ref name is a
    module const, exposed as `pub const CODE_REF: &str = "refs/git-full-send/code"`).
  - `#[derive(Debug, Error)] #[non_exhaustive] pub enum EncodeError { … }`
    wrapping the gix error types we touch (discover/open, head resolve, index,
    dirwalk, blob/tree/commit write, ref edit, io).
- `ClientError` gains `#[error(transparent)] Encode(#[from] EncodeError)`.
- `sync()` keeps its async signature but takes the repo location and, for now,
  just runs `encode` and `tracing::info!`s the resulting commit (no transfer).
  Signature becomes `pub async fn sync(repo_dir: PathBuf) -> Result<(), ClientError>`.

### CLI / config (locate the repo, default cwd)

- `Command::Sync` gains an optional `--repo <PATH>` arg (clap) defaulting to
  `std::env::current_dir()`. `main.rs` passes it to `gfs_client::sync`.
- Repo discovery inside `encode` uses `gix::discover(repo_dir)` so running from
  a subdirectory of the worktree still works.

### Tree synthesis (gix tree `Editor`, no scratch index, no `git` shell-out)

**Leverage git's index instead of re-hashing the worktree.** The whole point of
the index is that it caches, per tracked path, the blob OID *and* the stat
(size/mtime/inode) used to decide — cheaply — whether the worktree copy is still
that blob. So the base of our tree is the **index itself** (already the staged
state, with OIDs known and zero hashing), and we hash only the files git's own
stat-based status considers dirty, plus untracked files (genuinely new content,
no cached OID exists). Unchanged tracked files are never read or hashed.

This also dissolves the staging subtlety that an earlier draft worried about: the
index *is* the staged state, so a file `git add`ed and not re-edited is already
correct in the base with no work; a staged-then-edited file is reported
*Modified* and re-hashed to its on-disk content; a deleted file is reported
*Removed*. The result still equals "current on-disk contents".

Crucially, we operate on an **in-memory** index snapshot and **never write it
back** — gix's `status` does not persist the index, so the user's `.git/index`,
branch, and worktree stay untouched.

Algorithm in `encode`:

1. `let repo = gix::discover(repo_dir)?;`
2. Resolve HEAD commit (`repo.head()?` → peeled id) for the commit parent; handle
   **unborn HEAD** (fresh repo, no commits) → no parent.
3. **Base tree = the index.** `let index = repo.index()?;` (snapshot, never
   written). Seed `let mut editor = repo.edit_tree(EMPTY_TREE)?;` and `upsert`
   every index entry from its existing `entry.id` + mode (no hashing, no IO):
   regular/exec blobs, symlinks (`Link`), submodule gitlinks (`Commit`, `160000`)
   straight from the index. (Optimisation noted below: seeding from HEAD's tree
   and applying only the index-vs-HEAD staged diff would make even this step
   proportional to the number of staged changes; deferred — the upserts are cheap
   relative to hashing and transfer.)
4. **Overlay the worktree with a single index-vs-worktree status pass** —
   `repo.status(progress)?` configured with rename tracking **off** and the
   dirwalk set to emit untracked files but **not** ignored entries (and not the
   `.git` dir), then `.into_index_worktree_iter(...)`. For each `Item`:
   - `Modification { status: Removed, rela_path, .. }` → `editor.remove(rela_path)`.
   - `Modification { status: <content/type change>, rela_path, .. }` → read the
     on-disk file, `repo.write_blob(bytes)`, `editor.upsert(rela_path, kind, id)`
     with the on-disk mode. Only files git's stat check already flagged dirty
     reach this arm, so hashing is bounded by the actual worktree delta.
   - `DirectoryContents { entry, .. }` (an untracked, non-ignored path) → read
     on-disk file, `write_blob`, `upsert`.
   - `Rewrite` is not expected (rename tracking disabled); if encountered, treat
     as its delete+add components.
5. On-disk mode detection (for the overlay arms) is Unix-first
   (`std::os::unix::fs::PermissionsExt`): symlink → `Link` with `read_link`
   bytes as content; regular file → `BlobExecutable` if the owner-exec bit is
   set else `Blob`. On platforms without a meaningful exec bit / symlink, fall
   back to the index-recorded mode. Documented limitation.
6. Unchanged tracked files are never emitted by step 4, so they keep the index's
   blob OID from step 3 — **zero IO/hashing**, exactly what the index is for.
7. `let tree_id = editor.write()?;`
8. Build the commit object by hand and write it **without moving any ref**:
   - `author = committer = SignatureRef { name: "git-full-send", email:
     "git-full-send@localhost", time: <now> }` (fixed scratch identity; this is
     a synthetic artifact, not user-facing history). `now` via
     `std::time::SystemTime` → `gix::date::Time`.
   - `parents = HEAD commit id` (empty for unborn HEAD).
   - `message = "git-full-send: working-tree snapshot"`.
   - `let commit_id = repo.write_object(&commit)?;`
9. Force-update the scratch ref (create-or-overwrite) with an explicit
   transaction — **not** `commit_as`, whose `ExistingMustMatch`/`MustExistAndMatch`
   precondition is tied to the first parent and rejects both first-run creation
   and a scratch ref pointing at a previous synthetic commit:
   ```rust
   repo.edit_reference(RefEdit {
       change: Change::Update {
           log: LogChange { mode: RefLog::AndReference, force_create_reflog: false,
                            message: "git-full-send: encode code state".into() },
           expected: PreviousValue::Any,      // create or force-overwrite
           new: Target::Object(commit_id.detach()),
       },
       name: CODE_REF.try_into()?,
       deref: false,
   })?;
   ```
10. Return `EncodeOutcome { commit: commit_id.detach() }`.

Non-disturbance is structural: the index is only *read* (`repo.index()`),
trees/blobs are built in-memory via the `Editor`, the commit is written via
`write_object`, and the **only** ref touched is `refs/git-full-send/code`. The
user's branch, `HEAD`, index file, and working tree are never written.

## Test-support helpers

Extend `crates/test-support/src/lib.rs` (keep the shell-out-to-`git` style
already established) with small helpers used by the integration tests:

- `git(repo: &Path, args: &[&str]) -> Output` (assert success, return output).
- `write_file(repo: &Path, rel: &str, contents: &[u8])` (creates parent dirs).
- `commit_all(repo: &Path, message: &str)` (`git add -A && git commit`).
- Configure a deterministic identity + `init.defaultBranch` in `init_temp_repo`
  so commits work in CI with no global git config.

## Integration tests (`crates/client/tests/`)

A `git2`-free, gix-free harness — set up state with the `git` CLI, run
`gfs_client::encode`, then assert against the produced ref using both gix (read
the `code` tree) and the `git` CLI (prove non-disturbance).

### Main scenario — `code` tree equals intended on-disk state

Build one temp repo exercising every case at once:
- committed-and-unchanged file → committed content present;
- tracked file modified but **unstaged** → new content;
- tracked file `git add`ed **then further edited** → latest on-disk content;
- tracked file `git add`ed and **not** re-edited (worktree == index ≠ HEAD) →
  staged content (guards the staged-only subtlety);
- tracked file **deleted** on disk → absent from tree;
- **untracked, non-ignored** file → present;
- **gitignored** file (+ a `.gitignore`) → absent;
- nested subdirectory paths → correct tree nesting;
- **executable** bit preserved (`100755`);
- **symlink** preserved (`120000`) — Unix-gated test.

Assertion: resolve `refs/git-full-send/code`, walk its tree recursively into a
`BTreeMap<path, (mode, bytes)>`, compare to the expected map.

### Non-disturbance proof

Capture before and after `encode`:
- `git rev-parse HEAD` and the current branch ref → unchanged;
- `git status --porcelain=v2 --branch` → byte-identical (proves index **and**
  working tree untouched);
- the working tree's file set/contents (spot-check) → unchanged.
After: assert `refs/git-full-send/code` now resolves to the returned commit and
its parent is the original `HEAD`.

### Edge-case test — unborn HEAD

Fresh `git init` with an untracked file, no commits → `encode` produces a
parentless `code` commit whose tree contains the file.

Replace the placeholder `temp_repo_is_a_git_repository` token test (or keep it;
it still passes) — the real coverage lives in the new tests.

## Documentation

Per `CLAUDE.md` (record significant decisions as ADRs), add a short
`docs/adr/0009-working-tree-fidelity-for-the-code-commit.md` (status `accepted`)
capturing the behavioural decision settled in this ticket: the `code` tree =
full on-disk working-tree fidelity (tracked **and** untracked-non-ignored),
deletions via absence, modes preserved (exec/symlink), submodule gitlinks
carried from `HEAD` unchanged, gitignored files excluded (deferred to the
`extra` ticket). Update `docs/adr/README.md` index. Keep it brief; it refines
ADR-0004 rather than overturning it.

## Quality gates (acceptance)

- `cargo build`, `cargo test`, `cargo clippy --all-targets -- -D warnings`,
  `cargo fmt --check` all green.
- Encoding a dirty temp repo yields a `refs/git-full-send/code` commit whose
  tree equals the intended on-disk contents; HEAD/index/worktree provably
  unchanged.

## Out of scope (unchanged from ticket)

- The `extra` (force-include) commit and any gitignored-file inclusion.
- Any push/transfer or server-side work.
- Seeding the base tree from HEAD + index-vs-HEAD staged diff (to make the
  base-tree build proportional to staged changes rather than repo size). The
  index already gives us the no-re-hash property; this is a further constant-
  factor optimisation, deferred until profiling says it matters.

## Risks / notes

- **gix `dirwalk` emission options.** The precise options to list
  untracked-non-ignored files (and to keep `.git` and ignored entries out) will
  be pinned during implementation against the 0.84 `dirwalk` API; the *semantics*
  above (tracked from the index, untracked-non-ignored from dirwalk, content &
  mode from disk, gitlinks from HEAD) are what the tests lock in, so the exact
  mechanism can flex without changing observable behaviour.
- **Submodules** have no ADR and are an edge case; carrying HEAD's gitlink
  unchanged is the conservative default. Not explicitly tested beyond not
  corrupting an existing gitlink.
- **Windows** exec-bit/symlink fidelity is best-effort (falls back to
  index-recorded mode); the dev tool is Unix-first.
