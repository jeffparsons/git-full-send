# Plan — #47: Bound server connection concurrency and add graceful shutdown

## Goal

Make the `listen` server resilient under load and able to stop cleanly:

1. **Bound concurrency** — never run more than a configured number of
   `git receive-pack` handlers at once, so a burst of connections can't exhaust
   threads.
2. **Graceful shutdown** — on SIGTERM/SIGINT, stop accepting, drain in-flight
   connections, and return cleanly so the `hooks` `TempDir` is dropped (it is
   currently unreachable because the accept loop only ends on a listener error).
3. **Per-connection timeout** — a stuck client can't pin a slot indefinitely.
4. Keep the existing transport tests passing.

Approved approach: **(B) async accept loop (tokio)** — the runtime is already
present, and it gives clean, non-polling shutdown/backpressure. Approved
defaults: `--max-connections` default **16**; `--connection-timeout` default
**300s**; both surfaced as `gfs_common` constants.

## Design

### `crates/common/src/lib.rs`
Add two constants next to `DEFAULT_LISTEN_ADDR`:
- `DEFAULT_MAX_CONNECTIONS: usize = 16`
- `DEFAULT_CONNECTION_TIMEOUT_SECS: u64 = 300`

Each with rustdoc explaining the bound and that it's overridable via the
corresponding `listen` flag.

### `crates/server/Cargo.toml`
Extend the tokio feature set from `["rt"]` to add `net` (tokio `TcpListener`),
`signal` (Unix signal streams), `sync` (`Semaphore`), `time` (per-connection
timeout / `enable_all` on the sync wrapper's runtime), and `macros`
(`tokio::select!`).

### `crates/server/src/lib.rs`

**Config.** Introduce a small `ListenConfig { max_connections: usize,
connection_timeout: Duration }` with a `Default` impl sourcing the new
`gfs_common` constants. Public, so the CLI can construct it from flags.

**`bind` — unchanged.** Keeps returning a `Listener` wrapping a
`std::net::TcpListener`, so `local_addr` and the tests work without an ambient
runtime.

**Accept loop → async.** Replace the body of `serve` with an async core. Sketch:

- `pub async fn serve_async(listener: Listener, config: ListenConfig, shutdown: impl Future<Output = ()>) -> Result<(), ServerError>`
  - Destructure `Listener`; keep the `hooks` `TempDir` binding alive until the
    end (explicit `drop(hooks)` after the drain, with the existing comment
    updated to say shutdown now reaches it).
  - `listener.set_nonblocking(true)?` then `tokio::net::TcpListener::from_std(listener)?`.
  - `let sem = Arc::new(Semaphore::new(config.max_connections));`
  - `let mut handlers = JoinSet::new();`
  - Loop with `tokio::select!` (biased toward shutdown) over:
    - the `shutdown` future → break out of the accept loop;
    - acquiring a semaphore permit *then* `listener.accept()` — structured so
      that **(a)** we only accept when a slot is free (backpressure) and **(b)**
      shutdown stays responsive while parked at the cap. Each accepted socket is
      moved, with its owned permit, into `handlers.spawn(...)`.
  - The per-connection task runs the existing blocking `handle_connection` via
    `tokio::task::spawn_blocking`, passing `config.connection_timeout`; the owned
    permit is dropped when the task finishes, freeing the slot.
  - **Drain:** after the loop breaks, stop accepting and
    `while handlers.join_next().await.is_some() {}` to let in-flight handlers
    finish, then `drop(hooks)` and return `Ok(())`.

- `pub fn serve(listener: Listener) -> Result<(), ServerError>` — **keeps its
  current signature** so the transport tests are untouched. Becomes a thin sync
  wrapper: build a `tokio::runtime::Builder::new_current_thread().enable_all()`
  runtime and `block_on(serve_async(listener, ListenConfig::default(), std::future::pending()))`.
  (Only ever called outside a runtime — from the tests — so a nested runtime is
  fine. `listen` calls `serve_async` directly, not this.)

**Testability seam.** Factor the bounded-and-drained accept loop into a helper
parameterised by the per-connection action (e.g. a closure
`FnMut(TcpStream, OwnedSemaphorePermit)` that returns a future spawned into the
`JoinSet`). `serve_async` supplies the real `spawn_blocking(handle_connection)`
action; a unit test supplies a fast stub that records peak concurrency. This is
what makes the concurrency cap deterministically testable (see Testing).

**Per-connection timeout (inside `handle_connection`).** `spawn_blocking` tasks
are not cancellable, so wrapping the task in `tokio::time::timeout` would not
actually unstick a wedged handler. Instead enforce the deadline *inside*
`handle_connection`, which owns the socket and the child:
- `handle_connection` gains a `timeout: Duration` parameter.
- Spawn a watchdog thread that waits up to `timeout` on a completion signal
  (e.g. `mpsc::Receiver::recv_timeout`, or a `Condvar`). On timeout it logs and
  calls `sock.shutdown(Shutdown::Both)` — the same mechanism the normal path
  already uses at the end — which forces both pump threads to EOF/`EPIPE`, makes
  `git receive-pack` see EOF on stdin / a broken stdout and exit, so
  `child.wait()` returns and the handler unwinds. On normal completion the
  handler signals the watchdog to stand down before it returns.
- The resulting record/log path is unchanged; a timed-out connection simply
  reports as a failed receive (non-zero/aborted), which the metrics record
  already tolerates.

**`listen` → wires signals.**
`pub async fn listen(addr: SocketAddr, repo: PathBuf, config: ListenConfig) -> Result<(), ServerError>`:
- `let listener = bind(addr, repo)?;`
- Build the shutdown future from signals:
  `tokio::signal::unix::signal(SignalKind::terminate())` for SIGTERM and
  `tokio::signal::ctrl_c()` for SIGINT, combined with `tokio::select!`, the first
  to fire resolving the future (log which signal triggered shutdown).
- `serve_async(listener, config, shutdown).await`.
- No more outer `spawn_blocking` wrapping the whole serve — the accept loop is
  async now; the blocking work is per-connection via `spawn_blocking`. (The
  `ServerError::Join` variant is still produced by `spawn_blocking` join
  failures in the per-connection tasks.)

### `crates/cli/src/main.rs`
Add to `ListenArgs`:
- `--max-connections <N>` (`usize`, `default_value_t` from
  `gfs_common::DEFAULT_MAX_CONNECTIONS`).
- `--connection-timeout <SECS>` (`u64` seconds, `default_value_t` from
  `gfs_common::DEFAULT_CONNECTION_TIMEOUT_SECS`).

Construct `ListenConfig` from them and pass to
`gfs_server::listen(args.addr, args.repo, config)`.

## Testing

- **Existing transport tests** (`crates/client/tests/transfer.rs`,
  `crates/cli/tests/end_to_end.rs`) call `serve(listener)` on a `std::thread`
  unchanged — covered by keeping that signature/behaviour. (Acceptance #4.)
- **Graceful shutdown / drain** (new unit test in `server`): bind an ephemeral
  listener, run `serve_async` with a `ListenConfig::default()` and a shutdown
  future the test controls (e.g. `tokio::sync::Notify`/`oneshot`). Capture the
  hooks dir path beforehand; fire shutdown; assert `serve_async` returns
  `Ok(())` and the hooks `TempDir` has been removed (proving the drain path
  reaches `drop(hooks)`). Optionally drive one quick connection first to assert
  it's drained. (Acceptance #2.)
- **Concurrency cap** (new unit test, via the testability seam): drive the
  accept-loop helper with a stub handler that increments a shared
  `AtomicUsize` on entry, sleeps briefly, decrements on exit, and tracks the
  peak. Open more connections than `max_connections` and assert the observed
  peak never exceeds the cap. (Acceptance #1.)
- **CLI flags present**: extend the existing `listen --help` assertion in
  `end_to_end.rs` to check `--max-connections` and `--connection-timeout`
  appear.
- **Per-connection timeout** (best-effort): a test that opens a raw TCP
  connection and never speaks the protocol, with a short `connection_timeout`,
  and asserts the handler returns within roughly the timeout window rather than
  hanging. Marked best-effort if it proves timing-flaky in CI; the watchdog
  logic is otherwise covered by construction. (Acceptance #3.)

Run `cargo test --workspace`, `cargo clippy --workspace --all-targets`, and
`cargo fmt --check` before handing off.

## Notes / decisions for review

- **ADR?** This refines the server's concurrency/shutdown *mechanics* rather
  than setting a new architectural direction (the ADRs cover transport,
  encoding, etc.). I propose **no new ADR**, documenting the model in the
  `listen`/`serve_async` rustdoc instead. Tell me if you'd prefer a short ADR.
- **`serve` stays sync** purely to keep the transport tests untouched; `listen`
  uses `serve_async` directly. If you'd rather drop the sync wrapper and update
  the two tests to use a runtime, say so.

## Out of scope
- Transport security / auth (ADR-0006 defers this).
- Windows support (Unix-only transport, ADR-0006).
- Any change to the receive-pack wiring or metrics format.
