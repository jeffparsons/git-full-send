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

The resulting tree is a pure function of **(working tree) + (HEAD, for submodule
gitlinks only)**. Chosen enumeration strategy — **overlay every on-disk path** —
because it sidesteps the staged-vs-worktree subtlety: a file `git add`ed and not
further edited has worktree content == index content, which an index-vs-worktree
status diff would report as "unmodified" and we'd wrongly keep HEAD's committed
content. Reading content straight from disk for every included path is always
correct regardless of staging.

Algorithm in `encode`:

1. `let repo = gix::discover(repo_dir)?;`
2. Resolve HEAD commit: `repo.head()?` → `try_into_peeled_id()`. Handle the
   **unborn HEAD** case (fresh repo, no commits): no parent, seed tree from the
   empty tree.
3. Seed the editor from HEAD's tree so submodule **gitlinks** (`160000`,
   `EntryKind::Commit`) and any path we don't overlay survive unchanged:
   `let mut editor = repo.edit_tree(head_tree_id_or_empty)?;`
   Then explicitly **remove** every non-gitlink path in HEAD's tree that is a
   *tracked* file deleted on disk (see step 6), so deletions propagate.
4. Enumerate the **tracked** path set from the index: `repo.index()?` → iterate
   entries. Staged deletes are already absent from the index. Submodule entries
   (mode `160000`) are left to the HEAD-tree carry-through (skip overlay).
5. Enumerate the **untracked, non-ignored** path set via `repo.dirwalk(...)`
   configured to emit untracked entries while honouring `.gitignore`
   (ignored + the `.git` dir excluded). Files and symlinks only.
6. For the union of (4) ∪ (5), for each path:
   - `symlink_metadata` the on-disk file.
     - missing (a tracked file deleted on disk) → `editor.remove(path)` and skip.
     - symlink → blob content = link target bytes (`fs::read_link`),
       `EntryKind::Link` (`120000`).
     - regular file → blob content = file bytes, `EntryKind::BlobExecutable`
       (`100755`) if the owner-exec bit is set, else `EntryKind::Blob`
       (`100644`).
   - `let id = repo.write_blob(content)?;` then
     `editor.upsert(path_components, kind, id.detach())?;`
   - Mode detection is Unix-first (`std::os::unix::fs::PermissionsExt`); on
     platforms without a meaningful exec bit / symlink, fall back to the
     index-recorded mode. Note this as a documented limitation.
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
- Performance optimisation of re-hashing unchanged tracked files (correctness
  first; an index/stat fast-path can be a follow-up if it ever matters).

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
