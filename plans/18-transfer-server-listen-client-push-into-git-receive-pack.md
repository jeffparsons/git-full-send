# Plan — #18: Transfer: server `listen` + client push into `git receive-pack`

## Goal

Move the locally-synthesised `code` ref (issue #17) from client to server by
reusing Git's own transfer machinery, over a localhost TCP connection:

- **Server `listen`** (`gfs_server`): bind a localhost TCP port, accept
  connections, and for each spawn `git receive-pack <repo>` wired to the socket.
  Confine writable refs to the `refs/git-full-send/*` namespace and disable
  `receive.autogc` for the receive window. Runs until shut down.
- **Client push** (`gfs_client::sync`): after `encode`, `git push --thin` the
  `code` ref to the server over the connection (raw receive-pack stream), and
  retain the just-pushed tip locally as a delta base for the next sync.
- Minimal config/args: server listen address + target repo; client target
  endpoint.
- A loopback integration test proving the `code` ref + objects land on the
  server, that out-of-namespace refs are rejected, and that the retention ref is
  updated.

Per the pre-plan discussion (issue #18) and ADR-0005 / Research 0003, the wire is
a **raw receive-pack stream** end-to-end: both sides hand the TCP socket to their
`git` child as its stdin/stdout, so `git` owns pack generation and ingest and we
write no byte-pump bridge. Scope is the **`code` ref only** — the `extra` ref,
worktree checkout, and built-in auth/encryption stay out (later tickets /
ADR-0006).

## Design

### Transport: socket-as-stdio, raw receive-pack stream

The crux (validated locally during pre-plan): neither side hand-rolls a stream
bridge. Each hands the connected `TcpStream` to its `git` child as **stdin and
stdout** (a `try_clone()` for the second fd), and `git` speaks the receive-pack
protocol directly over the socket.

- **Server**: per accepted connection, spawn
  `git -c receive.autogc=false -c core.hooksPath=<gfs-hooks> receive-pack <repo>`
  with the socket as stdin and a clone as stdout, stderr piped to `tracing`.
- **Client**: spawn `git -c protocol.fd.allow=always push --thin fd::0,1
  <CODE_REF>:<CODE_REF>` from the repo's workdir, with the socket wired to the
  child's stdin (fd 0) and stdout (fd 1); `fd::0,1` tells Git to use those two
  fds as the transport stream.

Both sides need `-c protocol.fd.allow=always` — Git blocks the `fd::`/`ext::`
transports by default (confirmed: bare `fd::`/`ext::` fail with "transport not
allowed"). On the server side the transport is implicit (receive-pack just reads
stdin / writes stdout), so only the client passes the flag.

`TcpStream → Stdio` uses the Unix fd conversion: `Stdio::from(OwnedFd::from(sock))`
(`std::os::fd`). This is **Unix-only**; the tool is already Unix-first (`encode.rs`
gates exec-bit/symlink handling on `cfg(unix)`), so the socket-fd plumbing is
`cfg(unix)`-gated too, with a clear `compile_error!`/unimplemented path elsewhere.

*Implementation step 0 (de-risk):* confirm `git push --thin fd::0,1` works against
a spawned `git receive-pack` over a real socket pair before building the rest. If
`fd::` proves troublesome, fall back to an `ext::` connector with the **same**
socket-as-stdio wiring (the pre-plan validated `ext::<connector>` end-to-end); the
server side is unaffected either way.

### Server (`crates/server/src/lib.rs`)

Split the current stub `listen()` into a testable bind/serve pair plus an async
CLI wrapper:

- `pub struct Listener { listener: std::net::TcpListener, repo: PathBuf, hooks:
  tempfile::TempDir }` — owns the bound socket, the target repo path, and a
  gfs-managed hooks directory (kept alive for the server's lifetime).
  - `pub fn local_addr(&self) -> Result<SocketAddr, ServerError>` — so a test
    binding `127.0.0.1:0` can learn the OS-assigned port.
- `pub fn bind(addr: SocketAddr, repo: PathBuf) -> Result<Listener, ServerError>`
  — binds the TCP socket and materialises the `pre-receive` hook (below) into a
  fresh `TempDir`. Validates `repo` is a Git repository (a clear error beats a
  confusing receive-pack failure).
- `pub fn serve(listener: Listener) -> Result<(), ServerError>` — blocking accept
  loop; spawns a `std::thread` per connection running `handle_connection`. A
  failed connection is logged (`tracing::warn!`) and does not bring the loop
  down.
- `pub async fn listen(addr: SocketAddr, repo: PathBuf) -> Result<(), ServerError>`
  — CLI entry point; `tokio::task::spawn_blocking(move || serve(bind(addr,
  repo)?))`. Keeps the async signature `main.rs` expects without forcing the
  blocking accept loop onto the async executor.

`handle_connection(sock, repo, hooks_dir)`:

```rust
let out = sock.try_clone()?;
let status = Command::new("git")
    .args(["-c", "receive.autogc=false"])
    .arg("-c").arg(format!("core.hooksPath={}", hooks_dir.display()))
    .arg("receive-pack").arg(repo)
    .stdin(Stdio::from(OwnedFd::from(sock)))
    .stdout(Stdio::from(OwnedFd::from(out)))
    .stderr(Stdio::piped()) // drained to tracing
    .spawn()?...
```

A non-zero exit (e.g. the namespace hook rejecting a push) is logged but is **not**
a server error — it is the per-connection outcome; the listener keeps serving.

`update_worktree()` stays a `todo!()` stub (out of scope).

#### Namespace confinement (`refs/git-full-send/*`)

`bind` writes an executable `pre-receive` hook into the `TempDir`, and
`handle_connection` points receive-pack at it via `core.hooksPath` (so we never
touch the target repo's own `hooks/`). The hook rejects any updated ref outside
the namespace:

```sh
#!/bin/sh
while read -r old new ref; do
  case "$ref" in
    refs/git-full-send/*) ;;
    *) echo "git-full-send: refusing ref outside refs/git-full-send/: $ref" >&2
       exit 1 ;;
  esac
done
```

The namespace prefix comes from `gfs_common::REF_NAMESPACE` (templated into the
hook so the two never drift). Validated in pre-plan: pushing `refs/heads/main` is
rejected with the hook message; `refs/git-full-send/code` is accepted.

*Note on config passing:* confirm `git -c receive.autogc=false -c core.hooksPath=…
receive-pack` actually applies both (top-level `-c` propagates via
`GIT_CONFIG_PARAMETERS`). If `core.hooksPath` does not take through `-c`, fall
back to the `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` env form.

### Client (`crates/client/src/lib.rs`, new `crates/client/src/push.rs`)

`sync` gains the transfer leg after `encode`:

- Signature: `pub async fn sync(repo_dir: PathBuf, remote: String) -> Result<(),
  ClientError>` (`remote` is a `HOST:PORT` endpoint resolvable by
  `TcpStream::connect`).
- Flow:
  1. `let outcome = encode(&repo_dir)?;` — writes `refs/git-full-send/code`
     (unchanged from #17).
  2. `push::push_ref(&repo_dir, &remote, CODE_REF)?;` — connect, spawn the
     `git push --thin fd::0,1` child wired to the socket, await success.
  3. On success, retain the pushed tip: force-update
     `refs/git-full-send/sent/code` to `outcome.commit` (gix ref transaction,
     same `PreviousValue::Any` pattern as `update_code_ref`).
  4. `tracing::info!` the pushed commit + endpoint.

New module `crates/client/src/push.rs`:

- `pub(crate) fn push_ref(repo_dir: &Path, remote: &str, ref_name: &str) ->
  Result<(), PushError>` — `gix::discover` the repo to get the workdir, connect
  `TcpStream::connect(remote)`, spawn the push child from the workdir, map a
  non-zero exit (captured stderr) to `PushError::PushFailed`.
- `#[derive(Debug, Error)] #[non_exhaustive] pub enum PushError { Connect, Spawn,
  PushFailed { status, stderr }, … }`, surfaced through `ClientError` via
  `#[error(transparent)] Push(#[from] PushError)`.

#### Prior-tip retention (why `refs/git-full-send/sent/code`)

`--thin` only sends a small delta when the previous blob is present on **both**
ends and surfaced as common by negotiation (Research 0003 §2.1):

- **Server end** retains automatically — the pushed `refs/git-full-send/code`
  persists (ADR-0008: object store survives), and `receive.autogc=false` keeps
  its objects from being pruned mid-session; receive-pack advertises it, so the
  next push negotiates it as the common base.
- **Client end**: `encode` force-overwrites `refs/git-full-send/code` every sync,
  orphaning the previously-pushed commit and exposing its blobs to client-side
  gc. `refs/git-full-send/sent/code` (under the namespace) pins the
  last-confirmed-pushed tip so its objects stay present locally as the `--thin`
  delta base. It is updated **only after** a push succeeds, so a failed push
  leaves it pointing at the last state the server actually has — the correct base
  for a retry.

### CLI (`crates/cli/src/main.rs`)

- `Command::Listen` → `Listen(ListenArgs)` with `--repo <PATH>` (required; the
  target repo) and `--addr <IP:PORT>` (default `127.0.0.1:9419`). `main`:
  `gfs_server::listen(args.addr, args.repo).await?`.
- `SyncArgs` gains `--remote <HOST:PORT>` (required; the tunnelled localhost
  endpoint). `main`: `gfs_client::sync(repo, args.remote).await?`.
- `--addr` parses to `SocketAddr` via clap's `value_parser`.

### `gfs-common`

Reuse `REF_NAMESPACE`. Add a small const for the default listen port if it reads
cleanly (e.g. `DEFAULT_LISTEN_ADDR`), otherwise keep the default in the CLI arg.
No protocol-type changes needed for this ticket.

## Test-support (`crates/test-support/src/lib.rs`)

Add helpers the integration test needs, in the established shell-out-to-`git`
style:

- `init_bare_repo() -> TempDir` — `git init --bare` (the server's "remote" repo),
  with the same deterministic identity config as `init_temp_repo`.
- `git_in(git_dir, args)` or reuse the existing `git()` with explicit
  `["--git-dir", …]` args for inspecting the bare server repo.

## Integration tests (`crates/client/tests/transfer.rs`)

Loopback, `git`-CLI assertions (independent of the implementation's gix):

1. **`push_lands_code_ref_and_objects`** — build a client repo with a commit +
   dirty working-tree state; `init_bare_repo()` for the server. `bind(127.0.0.1:0,
   server_path)`, read `local_addr()`, run `serve` on a `std::thread`.
   `sync(client_path, addr)`. Assert the server's `refs/git-full-send/code`
   resolves to the client's `code` tip, and that the tree contents on the server
   equal the intended on-disk working state (recursive `ls-tree` map, mirroring
   the #17 helper).
2. **`rejects_refs_outside_namespace`** — against the same server, attempt a
   manual `git push --thin fd::0,1 HEAD:refs/heads/main` over a freshly-connected
   socket (the test drives the transport directly, as it already shells out to
   `git`); assert it fails and the server has no `refs/heads/main`.
3. **`retention_ref_updated`** — after a successful `sync`, assert the client's
   `refs/git-full-send/sent/code` resolves to the pushed `code` tip.
4. **`second_sync_succeeds`** (delta path) — sync, change a file, sync again;
   assert the server's `code` ref advances to the new tip and the new content is
   present. (Exercises the retained-base path without asserting wire size.)

Keep the existing token `temp_repo_is_a_git_repository` tests as-is.

## Documentation

Add a short ADR — `docs/adr/0010-receive-pack-transport-wiring.md` (status
`accepted`) — recording the concrete wiring decisions that refine ADR-0005:

- Raw receive-pack stream via socket-as-stdio on both sides; client uses the
  `fd::0,1` transport with `protocol.fd.allow=always` (with the `ext::`-connector
  fallback noted).
- Namespace confinement via a gfs-managed `pre-receive` hook through
  `core.hooksPath` (rather than `GIT_NAMESPACE`, which would relocate refs under
  `refs/namespaces/…`).
- `receive.autogc=false` for the receive window.
- Client-side prior-tip retention ref `refs/git-full-send/sent/code`.

Update `docs/adr/README.md` index. Keep it brief — it refines ADR-0005 rather
than overturning it (mirroring how #17 added ADR-0009).

## Quality gates (acceptance)

- `cargo build`, `cargo test`, `cargo clippy --all-targets -- -D warnings`,
  `cargo fmt --check` all green.
- `listen` serves `git receive-pack` and only accepts writes under
  `refs/git-full-send/*`.
- A loopback `sync` lands the `code` ref + objects on the server repo.
- Prior tip retained on the client (`sent/code`); `receive.autogc` disabled
  during receive.

## Out of scope (unchanged from ticket)

- The `extra` (force-include) ref and its push.
- Worktree checkout (`update_worktree` stays stubbed).
- Built-in auth/encryption — the SSH tunnel is the trust boundary (ADR-0006).
- Windows transport fidelity (socket-fd plumbing is `cfg(unix)`; dev tool is
  Unix-first).
- Deliberate server-side maintenance/gc scheduling outside the receive window
  (Research 0003 §3 follow-up); we only disable autogc during receive here.

## Risks / notes

- **`fd::0,1` for push.** Validated the equivalent `ext::<connector>` raw-stream
  path end-to-end in pre-plan; `fd::` push is documented but pinned as step 0 of
  implementation, with the `ext::` fallback ready (server unaffected).
- **`-c core.hooksPath` / `receive.autogc` propagation** to `git receive-pack`:
  confirm via `-c`; fall back to `GIT_CONFIG_*` env if needed.
- **Socket half-close.** Letting `git` own both fds (vs. a hand-rolled pump)
  sidesteps the sideband-teardown flakiness seen with the `nc` stand-in; verify
  clean shutdown on both successful and rejected pushes.
- **Connection handling.** Thread-per-connection is fine for the expected load
  (a developer's syncs); no need for tokio on the accept path. Errors per
  connection are isolated from the listener loop.
