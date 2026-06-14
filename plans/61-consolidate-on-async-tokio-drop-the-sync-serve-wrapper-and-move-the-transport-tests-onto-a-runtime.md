# Plan — #61: consolidate on async/Tokio

Drop the sync `serve()` wrapper and finish the async consolidation that #47's
review deferred: make the client push genuinely async over `tokio::process`, and
stand the transport tests' server up on the test's own Tokio runtime instead of a
dedicated OS thread running a private current-thread runtime.

## Goal

After this change there is **one** server entry point (`serve_async`) and **no**
synchronous runtime-in-a-thread shims anywhere. The client push yields during its
network exchange instead of blocking its runtime thread, and the transport tests
run client and a co-located server task cooperatively on the default
`#[tokio::test]` (current-thread) runtime.

## Background (current state)

- `gfs_server::serve_async(listener, config, shutdown)` (`crates/server/src/lib.rs:232`)
  is the real async server: bounded accept loop, per-connection `spawn_blocking` +
  timeout, drains in-flight handlers on shutdown, then drops the hooks `TempDir`.
  Production `listen()` uses it.
- `gfs_server::serve(listener)` (`crates/server/src/lib.rs:201–222`) is the sync
  shim this issue removes. Its doc-comment carries the #47-review NOTE flagging it
  as temporary. Its only callers are two test helpers:
  - `crates/client/tests/transfer.rs:24` `start_server` — `std::thread::spawn(|| serve(listener))`; client runs **in-process** as `gfs_client::sync(...).await` / `gfs_client::push_ref(...)`.
  - `crates/cli/tests/end_to_end.rs:37` `start_server` — same pattern; client runs as a **separate subprocess** via the sync `run_cli` helper (`Command::new(BIN).output()`).
- The client push path is fully blocking std: `push_refs` (`crates/client/src/push.rs:122`)
  uses `std::process::Command` + a blocking `child.wait()`, and `sync`
  (`crates/client/src/lib.rs:72`) calls it **inline** (`lib.rs:95`) — so
  `sync().await` blocks its runtime thread for the whole exchange. `push_ref`
  (`push.rs:104`) is a thin wrapper over `push_refs`.

The blocking is why a `tokio::spawn`ed co-located server would deadlock on a
current-thread runtime today: while the client is blocked in `child.wait()` (or the
test is blocked in `Command::output()`), the runtime can't poll the server task to
accept the connection.

## Changes

### 1. Server — delete the sync `serve()` wrapper

`crates/server/src/lib.rs`

- Remove `pub fn serve` (lines ~201–222) and its `// NOTE (issue raised in #47 review)` block.
- `serve_async` (and `listen`) remain the only entry points. No signature changes.
- Confirm nothing else references `serve` (only the two test helpers do; both are rewired below).

### 2. Client push — async over `tokio::process`

`crates/client/src/push.rs`

- Make `push_refs` an `async fn`. Replace `std::process::Command` with
  `tokio::process::Command`, and replace the blocking
  `child.wait()` / manual stderr read with `child.wait_with_output().await`
  (drives stderr draining and the wait concurrently, avoiding any pipe-fill
  deadlock). The fd-reservation dance is unchanged: `tokio::process::Command` is
  built on `std::process::Command`, so the reserved inheritable dups
  (`TransportFds`, CLOEXEC cleared via `rustix`) are inherited by the child
  exactly as today. Keep the `drop(transport); drop(sock)` immediately after
  `spawn()` so the parent's socket copies close once the child holds its own.
- `push_ref` (`push.rs:104`) becomes `async fn` and `push_refs(...).await`.
- Error types are unaffected: `tokio::process` returns `std::io::Error`, so
  `PushError::Spawn(std::io::Error)` and friends stay as-is. Drop the now-unused
  `use std::io::Read;` and switch `std::process::{Command, Stdio}` to
  `tokio::process::Command` (keep `Stdio` from `std::process`, which tokio reuses);
  `ExitStatus` stays.

`crates/client/src/lib.rs`

- `sync` already is `async`; change the inline calls to
  `push::push_refs(...).await` (line 95). `retain_pushed_tip` / `encode` /
  `record_sync` stay synchronous — they are local, quick, and don't need the
  server live concurrently (verified by reading `sync`).
- Re-exports at `lib.rs:33` (`pub use push::{..., push_ref, push_refs}`) are
  unchanged in name; the functions are simply async now.

`crates/cli/src/main.rs`

- The `sync` subcommand dispatch already `.await`s `gfs_client::sync`; the push
  functions becoming async is transparent there. (Verify: `main` is
  `#[tokio::main]` with `rt-multi-thread` — no change needed.)

### 3. Transport test helpers — server on the test runtime

Both `start_server` helpers (`transfer.rs:24`, `end_to_end.rs:37`):

- Replace `std::thread::spawn(move || { let _ = gfs_server::serve(listener); })`
  with `tokio::spawn(async move { let _ = gfs_server::serve_async(listener, gfs_server::ListenConfig::default(), std::future::pending::<()>()).await; });`
- Keep the helper signature `fn start_server(repo: &Path) -> SocketAddr`. It stays a
  sync fn, but is called from within `#[tokio::test]`, so `tokio::spawn` has an
  ambient runtime. The std listener is already bound (so `local_addr()` works
  immediately); `serve_async` re-homes it onto the reactor inside the task. A
  client connecting before the task reaches `accept` is fine — the kernel queues
  it in the listen backlog.
- The task is detached (fire-and-forget), matching today's leaked-thread semantics;
  the test process exits at the end of the test binary. (See "Shutdown guard" under
  Decisions for why we are **not** adding a per-test shutdown handle.)
- `ListenConfig` / `serve_async` are already `pub`; confirm `ListenConfig` is
  exported from `gfs_server` (it is — used by `listen`).

### 4. end_to_end test body — async subprocess

`crates/cli/tests/end_to_end.rs`

- Convert the `run_cli` helper (`end_to_end.rs:52`) to `async fn run_cli`, using
  `tokio::process::Command` and `.output().await`, and `.await` it at each call
  site (lines 150, 162, 207, 216, 271, 281). This keeps the current-thread test
  runtime live while the CLI subprocess runs, so the co-located server task is
  polled and accepts the connection.
- The standalone `--help` test (`end_to_end.rs:346`) does **not** start a server,
  so it cannot deadlock; convert it to async `tokio::process` too for consistency
  (small, optional — note if it adds friction).
- Add the `rt-multi-thread`? **No** — not needed; `cli` dev-deps already enable
  `rt-multi-thread` but the cooperative current-thread approach doesn't require it.
  Leave the `cli` Cargo features as they are.

### 5. transfer test body

`crates/client/tests/transfer.rs`

- The `#[tokio::test]` functions already `.await` `gfs_client::sync`; the only
  change is `gfs_client::push_ref(...)` (lines 201, 204) now returns a future, so
  add `.await` there. With `push_refs`/`push_ref` async, the in-process client
  cooperates with the co-located server on the current-thread runtime.
- `client` dev-deps tokio features are currently `["macros", "rt"]`
  (`crates/client/Cargo.toml:29`) — current-thread only, which is sufficient. No
  feature change expected.

## Decisions / trade-offs

- **Current-thread vs `multi_thread`:** with the client push and the end_to_end
  subprocess both async, neither test blocks its runtime thread, so the default
  current-thread `#[tokio::test]` runtime suffices and we keep zero dedicated
  threads. **Fallback:** if implementation surfaces a stubborn blocking dependency
  (e.g. some synchronous step inside `sync` that must overlap the server), mark
  just that test `#[tokio::test(flavor = "multi_thread")]` (adding `rt-multi-thread`
  to `client` dev-deps if needed) and call it out explicitly in the PR rather than
  reaching for it silently.

- **Shutdown guard (considered, deferred):** I floated having `start_server`
  return a guard that fires a `oneshot` shutdown on drop so each test drains the
  server and cleans up the hooks `TempDir`. On reflection I'm **deferring** it:
  doing it *properly* means awaiting the drain, which a `Drop` impl can't do
  cleanly (it would need a blocking join inside async teardown), and doing it
  *improperly* (fire-and-don't-wait in `Drop`) still changes every test's binding
  to `let (addr, _guard) = ...` (a dropped-immediately guard would kill the server)
  for little real benefit. The detached task matches today's semantics and the
  per-test hooks `TempDir` is harmless (fresh per test, OS-tmp reclaimed). The real
  shutdown/drain path stays covered by the existing
  `serve_async_shuts_down_cleanly_and_drops_the_hooks_dir` unit test. Easy to
  revisit if reviewers want it.

## Testing

- `cargo test -p gfs-client --test transfer` — in-process client + co-located
  server on current-thread runtime.
- `cargo test -p gfs-cli --test end_to_end` — CLI subprocess + co-located server.
- `cargo test --workspace` — full suite, including the server unit tests
  (`accept_loop` cap, `serve_async` shutdown/drain).
- `cargo clippy --workspace --all-targets` and `cargo fmt --check`.
- Manual sanity: the production `listen` path is unchanged; a quick local
  `git-full-send listen` + `sync` round-trip if convenient.

## Out of scope / follow-ups

- No protocol, ref-namespace, or transport-wire changes (ADR-0005/0010 untouched).
- The `--thin` per-chain delta policy deferral noted in `push.rs` is unrelated and
  stays deferred.
- Retitle the issue/PR during prep toward the broader "consolidate on async/Tokio"
  framing (the original title scoped only the `serve()` removal).
