# Plan — #49 Lock worktree updates against concurrent runs

Relevant ADRs: [ADR-0008 — Remote worktree disposability & sync authority](../docs/adr/0008-remote-worktree-disposability.md),
[ADR-0011 — Worktree reassembly mechanics](../docs/adr/0011-worktree-reassembly-mechanics.md),
[ADR-0002 — Git manipulation strategy](../docs/adr/0002-git-manipulation-strategy.md).

## Goal

Serialise concurrent `update-worktree` runs that target the **same** worktree so
their `git read-tree --reset -u` → `clean -fdx` sequences cannot interleave and
corrupt the checkout. Distinct worktrees stay fully independent. Identified in
the #40 audit; harmless for the single-user MVP but a foot-gun once a build
orchestrator fires concurrent checkouts.

Because `update-worktree` is a **CLI subcommand**, two concurrent runs are
separate OS processes — an in-process mutex can't serialise them. The guard is a
**per-worktree filesystem advisory lock**.

## Decisions locked in pre-plan (approved 👍 on the issue)

- **Lock mechanism:** Rust std file locking — `File::try_lock` / `File::lock` /
  `File::unlock` (stabilised 1.89; our MSRV is 1.94, so **no new dependency**).
  On Unix this is `flock(2)` (advisory, tied to the open file description,
  **auto-released when the file closes or the process exits** — no
  stale-lock-forever mode); on Windows it's `LockFileEx`.
- **No async lock needed:** Tokio has no async file-lock API, and we don't need
  one — the git work already runs inside `spawn_blocking` (`update_worktree_blocking`),
  so a blocking lock there never touches the async executor.
- **Contention semantics:** **fail fast by default.** A second run on a busy
  worktree returns a clear "update already in progress" error and exits non-zero.
  - `--wait`: block until the holder finishes, then proceed.
  - `--wait --timeout <secs>`: poll until the deadline, then fail with a distinct
    timeout error. `--timeout` without `--wait` is a usage error.
- **Lock location:** a `lock` file in the existing per-worktree state directory
  `<git-dir>/git-full-send/worktrees/<key>/`, beside the persistent `index`
  (same `<key>` hash of the canonical worktree path). Distinct worktrees get
  distinct directories ⇒ independent locks for free. The file lives under the
  git dir, never inside the worktree, so `clean -fdx` cannot delete it (same
  reasoning as the index).
- **Critical section:** acquire the lock just before `read-tree`, hold it through
  `clean`, release after. The read-only tree resolution above it (`resolve_code_tree`
  / `resolve_extra_tree` / `overlay_extra_onto_code`) stays unlocked — it mutates
  nothing, and resolving first means a never-synced stream still fails fast with
  `MissingCodeRef` before any lock or directory work.

## Out of scope (explicitly)

- Cross-worktree or repo-global locking — the requirement is per-worktree
  isolation, and ADR-0012 keeps worktrees orthogonal to streams.
- Locking the `listen` / `receive-pack` push path — this ticket is only about
  `update-worktree` checkout serialisation.
- Recording lock-wait time in the metrics record (ADR-0013) — would change the
  `UpdateWorktreeRecord` struct; keep this change tight. A `tracing` line on
  contention/wait is enough for now.
- Any change to the `index` keying or the checkout pipeline itself.

## Design

### Lock-mode type (`crates/server/src/lib.rs`)

A small public enum describing the contention behaviour, defaulting to fail-fast:

```rust
/// How `update_worktree` reacts when another run already holds the per-worktree
/// lock.
#[derive(Debug, Clone, Copy, Default)]
pub enum LockMode {
    /// Fail immediately with [`ServerError::WorktreeBusy`] if the worktree is busy.
    #[default]
    FailFast,
    /// Wait for the lock. `None` blocks indefinitely; `Some(d)` polls until `d`
    /// elapses, then fails with [`ServerError::LockTimeout`].
    Wait { timeout: Option<Duration> },
}
```

### Acquisition helper (`crates/server/src/lib.rs`)

- `fn worktree_lock_path(git_dir, worktree) -> Result<PathBuf, ServerError>` —
  mirrors `worktree_index_path`: compute the same per-worktree `<key>` dir and
  return `dir.join("lock")`. To avoid duplicating the canonicalise+hash+`create_dir_all`
  logic, factor the shared part into `fn worktree_state_dir(git_dir, worktree)
  -> Result<PathBuf, ServerError>` and have **both** `worktree_index_path` and
  `worktree_lock_path` derive their file from it.
- `fn acquire_worktree_lock(lock_path, mode) -> Result<File, ServerError>` —
  open/create the lock file (`OpenOptions::new().read(true).write(true).create(true)`),
  then per `mode`:
  - `FailFast`: `try_lock()`. `Ok(())` ⇒ return the `File`. `Err(TryLockError::WouldBlock)`
    ⇒ `ServerError::WorktreeBusy`. `Err(TryLockError::Error(e))` ⇒ `ServerError::Lock`.
  - `Wait { timeout: None }`: `lock()` (blocks); map I/O error to `ServerError::Lock`.
  - `Wait { timeout: Some(d) }`: poll `try_lock()` on a fixed short interval
    (~100 ms via `std::thread::sleep`, fine in the blocking body) until acquired
    or the deadline passes ⇒ `ServerError::LockTimeout`.
  The returned `File` is the lock guard: keep it bound until after `clean`, then
  let it drop (the OS releases the `flock`). No explicit `unlock()` needed,
  though we can call it for clarity.

### Wiring into `update_worktree_blocking`

Between path resolution and `read-tree`:

```rust
let lock_path = worktree_lock_path(&git_dir, worktree)?;
let _lock = acquire_worktree_lock(&lock_path, mode)?;   // held to end of fn
// read-tree … clean … (unchanged)
```

`_lock` stays alive through `clean` and the metrics write, releasing on return.

### New `ServerError` variants (`crates/server/src/lib.rs`)

- `WorktreeBusy { worktree: PathBuf }` — `"another update is already in progress for worktree {worktree}"`.
- `LockTimeout { worktree: PathBuf, timeout: Duration }` — `"timed out after {timeout:?} waiting for the worktree lock on {worktree}"`.
- `Lock(#[source] std::io::Error)` — opening/locking the lock file failed (distinct
  from `CreateWorktree`). 

### Signature change + call sites

`pub async fn update_worktree(repo, worktree, stream, mode: LockMode)` gains the
`mode` parameter, threaded into `update_worktree_blocking(&repo, &worktree, &stream, mode)`.
Update the in-tree call sites to pass `LockMode::default()` (fail-fast) except
where a test exercises waiting:
- `crates/cli/src/main.rs` — maps the new flags (below).
- `crates/client/tests/transfer.rs` — ~10 existing calls pass `LockMode::default()`.

### CLI flags (`crates/cli/src/main.rs`, `UpdateWorktreeArgs`)

```rust
/// Wait for an in-progress update of the same worktree instead of failing fast.
#[arg(long)]
wait: bool,
/// With `--wait`, give up after this many seconds instead of waiting forever.
#[arg(long, value_name = "SECS", requires = "wait")]
timeout: Option<u64>,
```

`clap`'s `requires = "wait"` rejects `--timeout` without `--wait` as a usage
error. Dispatch maps to `LockMode`:
- `!wait` ⇒ `FailFast`
- `wait && timeout.is_none()` ⇒ `Wait { timeout: None }`
- `wait && Some(s)` ⇒ `Wait { timeout: Some(Duration::from_secs(s)) }`

The `update-worktree` doc-comment / `docs/operating.md` gains a short note on the
new flags and the default fail-fast behaviour.

## Tests

Integration tests in `crates/client/tests/transfer.rs` (where real synced streams
already exist via the client). They use the new **public** `worktree_lock_path`
to grab the lock deterministically — no real race, no sleeps-for-timing:

1. **Fail-fast when busy:** sync a stream, run one successful `update_worktree`
   (creates the state dir), then open `worktree_lock_path(...)` and `lock()` it
   from the test. A second `update_worktree(.., LockMode::FailFast)` returns
   `ServerError::WorktreeBusy`. (Acceptance: the second run fails cleanly rather
   than interleaving.)
2. **Timeout path:** with the lock held, `update_worktree(.., Wait { timeout:
   Some(short) })` returns `ServerError::LockTimeout` after roughly the timeout.
3. **Wait then succeed:** hold the lock, spawn the `Wait { timeout: None }` update,
   release the lock, and assert it then completes and the worktree matches the
   synced tree.
4. **Distinct worktrees independent:** hold the lock on worktree A; a fail-fast
   `update_worktree` on worktree B (same repo, same stream) succeeds — proving
   the lock is per-worktree. (Acceptance: distinct worktrees remain independent.)

Exposing `worktree_lock_path` as `pub` is a small, defensible addition (it also
lets an orchestrator locate/inspect the lock); the alternative — duplicating the
path-hashing in tests — is brittle.

## Acceptance criteria mapping

- *Two concurrent `update-worktree` calls on the same worktree do not interleave;
  the second waits or fails cleanly* → fail-fast default (test 1) + `--wait`
  (test 3) + `--wait --timeout` (test 2).
- *Distinct worktrees remain independent* → test 4.

## Definition of done

`cargo build`, `cargo test`, `cargo clippy`, `cargo fmt --check` all green;
new flags documented; the four tests above passing.
