# Plan — #42: Record timing metrics somewhere

## Goal

Make every `git-full-send` operation — the client `sync` and the server
`listen` / `update-worktree` — emit a durable, machine-readable record of how
long it took and how much it moved, so the data can be analysed retrospectively
(e.g. "why was this sync slow?", "how big is the extra layer growing?").

Decided in the pre-plan thread:

- **Sink:** append one structured JSON object per operation (JSON Lines) to
  `<git-dir>/git-full-send/metrics.jsonl` on **each side**, alongside the
  existing `git-full-send/worktrees/` state, plus a concise `tracing` summary
  line per operation.
- Metrics are **best-effort observability**: a failure to write a record is
  logged via `tracing::warn!` and never fails the operation.

This is a new architectural surface, so it also adds an ADR.

## Background (verified)

- **Client `sync`** (`crates/client/src/lib.rs`) runs, in order: `encode` (code
  commit), `encode_extra` (extra commit), `push_refs` (one receive-pack
  exchange), then `retain_pushed_tip` ×2. Each step is a discrete, timeable call.
- `encode` (`crates/client/src/encode.rs`) overlays the index→worktree delta:
  every changed/added file goes through `overlay_from_disk` (one call per
  added/modified path), and removals go through `editor.remove`. So the code
  layer's *delta* (files + bytes actually new this sync) is countable for free at
  the point it's already doing the work — the full tree size is **not** cheaply
  available (it would require walking the whole tree, defeating the index
  shortcut), so we record the delta and label it as such.
- `encode_extra` selects the full force-include set into `paths` and overlays
  each via `overlay_from_disk`; the extra layer is the whole selected set each
  sync (file count = `paths.len()`, bytes = sum of overlaid content).
- `EncodeOutcome { commit, code_ref }` and `ExtraOutcome { commit, extra_ref }`
  are both `#[non_exhaustive]`, so adding fields is non-breaking.
- `overlay_from_disk` is shared by `encode` and `encode_extra` and currently
  returns `Result<(), EncodeError>`; it knows the byte length of the content it
  writes (file bytes, or symlink target length, or 0 for a skipped
  non-representable path).
- **Server `listen`** (`crates/server/src/lib.rs`) handles each connection on its
  own thread in `handle_connection`, which spawns `git receive-pack` with the
  **raw socket wired as the child's stdin/stdout** (`Stdio::from(OwnedFd::…)`),
  captures stderr, and `wait`s. The `pre-receive` hook (`pre_receive_hook`)
  already loops `while read -r old new ref` over every pushed ref.
- **Server `update_worktree`** runs two timeable `git` steps (`read-tree`,
  `clean`) via `run_git_step`, preceded by `resolve_code_tree` /
  `resolve_extra_tree` / `overlay_extra_onto_code`. `git_dir` is resolved up
  front via `gix::discover`.
- `gix::Repository::git_dir()` is the established way to locate the git dir
  (already used in `update_worktree_blocking`); the existing state convention is
  `<git-dir>/git-full-send/…`.
- The workspace has **no serde dependency** today (`Cargo.toml` workspace deps).
  Emitting JSON cleanly needs `serde` + `serde_json`; both are standard,
  lightweight, and warranted here (the ADR notes this addition against ADR-0001's
  core-crate rationale). Git object ids are serialized as **hex strings**
  (`id.to_string()`), so no serde impls are needed on `gix` types.
- Durations are measured with `std::time::Instant`; timestamps with
  `std::time::SystemTime` (epoch milliseconds). No date/chrono crate is added.

## The metrics sink (shared, in `gfs-common`)

Add a `metrics` module to `crates/common/src/lib.rs` (or a sibling
`crates/common/src/metrics.rs`) providing the cross-cutting plumbing; the record
*shapes* live in the crate that produces them.

- `pub fn metrics_path(git_dir: &Path) -> PathBuf` →
  `git_dir.join("git-full-send").join("metrics.jsonl")`.
- `pub fn append(git_dir: &Path, record: &impl Serialize) -> io::Result<()>`:
  create the parent dir if missing, serialize `record` to a single line, append
  `line + "\n"` to the file in one `write_all` under a **process-global
  `Mutex`** (`static SINK_LOCK: Mutex<()>` via `OnceLock` or `std::sync`), so the
  server's concurrent connection threads can't interleave lines. (Cross-*process*
  contention — two servers on one repo — is out of scope; noted in the ADR.)
- `pub fn now_unix_millis() -> u64` and a `tool_version()` returning
  `env!("CARGO_PKG_VERSION")` for the shared context fields.
- A convenience `pub fn record(git_dir, record)` (or callers inline it) that
  calls `append` and, on `Err`, emits `tracing::warn!(%error, "could not write
  metrics record")` — never propagating. Every call site is best-effort.

Add to workspace `Cargo.toml` `[workspace.dependencies]`:
`serde = { version = "1", features = ["derive"] }` and `serde_json = "1"` (pin
to current latest at implementation time), and wire `serde`/`serde_json` into
`gfs-common`, `gfs-client`, and `gfs-server` member manifests.

Every record carries a common envelope so the JSONL is self-describing:
`{ "kind": "<op>", "ts_unix_ms": …, "tool_version": "…", … }` where `kind` is one
of `sync`, `receive`, `update_worktree`.

## Client `sync` instrumentation

### Encode outcome additions (`encode.rs`)

- Change `overlay_from_disk` to return `Result<u64, EncodeError>` (bytes
  written: file length / symlink-target length / 0 for skipped).
- Extend `EncodeOutcome` with `tree: gix::ObjectId` and a `code: CodeLayerStats`
  where `CodeLayerStats { files_overlaid: usize, bytes_overlaid: u64,
  files_removed: usize }`, accumulated in the status loop (increment + sum on each
  `overlay_from_disk`; count `editor.remove` calls).
- Extend `ExtraOutcome` with `tree: gix::ObjectId` and
  `ExtraLayerStats { files: usize, bytes: u64 }` (`files = paths.len()`, `bytes`
  summed from `overlay_from_disk`).
- These structs stay `#[non_exhaustive]`; existing field access is unaffected.

### `sync` record (`lib.rs`)

Wrap each step with an `Instant`:

```
let t_total = Instant::now();
let t = Instant::now(); let code  = encode(…)?;            // code_encode_ms
let t = Instant::now(); let extra = encode_extra(…)?;      // extra_encode_ms
let t = Instant::now(); push_refs(…)?;                     // push_ms
let t = Instant::now(); retain_pushed_tip(…)?; retain…?;   // retain_ms
```

After the final step succeeds, resolve `git_dir` (one `gix::discover(&repo_dir)`)
and write a `SyncRecord` (`kind: "sync"`):

- `stream`, `remote`
- `total_ms`, `code_encode_ms`, `extra_encode_ms`, `push_ms`, `retain_ms`
- `code`: `{ files_overlaid, bytes_overlaid, files_removed, commit, tree }`
- `extra`: `{ files, bytes, commit, tree }`

Then emit one `tracing::info!` summary (`stream`, `total_ms`, code/extra file
counts + bytes). Push **pack-on-wire** bytes are *not* recorded client-side
(`git push` only prints them as TTY progress, which is suppressed under our piped
stderr — unreliable to parse); the on-wire size is captured authoritatively on
the server (`bytes_in`, below), and the client's per-layer encoded byte totals
are the client-side size story. Recording happens on the **success path** only;
failure-path metrics are noted as a follow-up (the early-return `?` paths stay as
they are, and the error is already surfaced to the operator).

## Server `listen` instrumentation

Goal per connection: duration, exit status, bytes transferred each way, and the
refs that were accepted — without abandoning the careful raw-socket transport for
correctness.

1. **Resolve `git_dir` once.** `bind` already runs `gix::discover(&repo)`; carry
   the discovered `git_dir` into `Listener` and on to `handle_connection`
   (currently it only has the repo path).
2. **Count bytes via a pump.** Replace the direct `socket-as-stdio` wiring with
   piped child stdin/stdout and two pump threads:
   - thread A: `std::io::copy(&mut socket_read, &mut child_stdin)` → `bytes_in`
   - thread B: `std::io::copy(&mut child_stdout, &mut socket_write)` → `bytes_out`
   Use two handles to the socket (`try_clone`, as the code already does for the
   stdout dup). After `child.wait()`, `socket.shutdown(Both)` to unblock the
   inbound pump, then `join` both threads to collect the counts. This observes the
   exact same bytes that flow today (raw receive-pack stream, no framing — ADR-0005
   unchanged; ADR-0010's *client-side* fd-passing is untouched) and adds only a
   localhost-bandwidth userspace copy, negligible against git's own work. **Risk
   note:** if the pump ever proves to interfere with the protocol, the fallback is
   to record duration + exit status only and drop the byte counts — the timing is
   the core requirement.
3. **Capture accepted refs cheaply.** Extend `pre_receive_hook` so that for each
   accepted ref (the in-namespace `case` arm) it echoes a marker line to stderr,
   e.g. `echo "git-full-send: accepted $ref" >&2`. `handle_connection` already
   reads the child's stderr; parse out the marker lines into `refs_updated`
   (and keep logging the rest as today). The hook's self-test
   (`hook_guards_the_shared_namespace_constant`) is extended to assert the marker.
4. **Write a `ReceiveRecord`** (`kind: "receive"`) after `wait`, regardless of
   exit status (failures are valuable): `duration_ms`, `success: bool`,
   `exit_code: Option<i32>`, `bytes_in`, `bytes_out`, `refs_updated: Vec<String>`.
   Emit a `tracing::info!`/`warn!` summary (already partly present).

## Server `update_worktree` instrumentation

In `update_worktree_blocking`, time the phases with `Instant`: `resolve_ms`
(resolve code + extra + overlay), `read_tree_ms`, `clean_ms`, `total_ms`. After
success, write an `UpdateWorktreeRecord` (`kind: "update_worktree"`):
`stream`, `worktree` (display path), the four `_ms` fields, and the resolved
`tree`. Emit a `tracing::info!` summary. `git_dir` is already in scope.

## ADR

Add `docs/adr/0013-recording-operation-metrics.md` (next free number; highest is
0012), status **accepted**:

- **Decision:** durable per-side JSON Lines at `<git-dir>/git-full-send/metrics.jsonl`,
  one record per operation, written best-effort with a process-global write lock;
  plus a concise `tracing` summary line per operation.
- **Context:** issue #42 — timing + size metadata for retrospective analysis on
  both client and server.
- **Consequences / notes:** adds `serde`/`serde_json` (relate to ADR-0001's core
  set); per-layer byte/file counts are the *delta* for `code` and the *full set*
  for `extra`; on-wire bytes are observed server-side; the pump adds a userspace
  copy to the receive path.
- **Alternatives considered:** *structured `tracing` events only* (rejected:
  ephemeral, depends on the operator's log capture, the explicit
  pre-plan-rejected option); *external metrics system / StatsD* (rejected:
  overkill for an MVP, adds infra and a network dependency).

Update `docs/adr/README.md`: add the 0013 row to the index table.

## Tests

- **common:** `metrics_path` shape; `append` round-trips a record and **appends**
  (two writes → two lines); concurrent appends from several threads produce N
  intact, individually-parseable JSON lines (exercises the lock).
- **client (`encode.rs` unit / `tests/`):** after `encode`, `EncodeOutcome.code`
  reflects the right counts/bytes for a known added + modified + removed set;
  after `encode_extra`, `ExtraOutcome.extra` matches a known force-include set
  (count + bytes). Reuse the existing temp-repo helpers.
- **client `sync` (extend `tests/` or the e2e):** after a `sync`, the client
  git dir has `git-full-send/metrics.jsonl` with one parseable `kind:"sync"`
  record whose layer counts match the fixture.
- **server (extend `crates/cli/tests/end_to_end.rs`):** after the round-trip, the
  **server** git dir has a `kind:"receive"` record (positive `bytes_in`,
  `success:true`, `refs_updated` listing the code+extra refs) and, after
  `update-worktree`, a `kind:"update_worktree"` record with non-zero `total_ms`.
  Assert the byte-counting pump didn't break the existing exact-match checkout
  assertions (they must still pass unchanged).
- **server hook:** `hook_guards_the_shared_namespace_constant` extended for the
  accepted-ref marker; a focused test that `handle_connection` parses
  `refs_updated` from a real push (covered by the e2e assertion above).
- All existing tests pass unchanged.

## Out of scope / follow-ups

- **Failure-path client metrics** (recording a `sync` that errored mid-way). The
  server already records failed receives via exit status; the client records on
  success only for now — file a follow-up if failure timing is wanted.
- **Cross-process append safety** (two servers writing one repo's metrics file
  concurrently). The process-global lock covers the realistic single-server case;
  an advisory file lock is a possible future hardening.
- **Rotation / pruning** of `metrics.jsonl` (it grows unbounded). Out of scope;
  a size cap or rotation is a follow-up.
- No analysis/reporting tooling is built here — this ticket only *produces* the
  data.

## Acceptance mapping

- *Timing recorded on both client and server* → `sync` per-phase + total;
  `receive` duration; `update_worktree` per-step + total.
- *Recorded somewhere durable for retrospective analysis* → per-side
  `git-full-send/metrics.jsonl` (JSON Lines), via the shared `gfs-common::metrics`
  sink.
- *Number and size of files added in each layer* → `code` layer delta
  (files_overlaid/bytes_overlaid/files_removed) and `extra` layer (files/bytes),
  plus server `bytes_in`/`bytes_out` on the wire.
- *Relevant metadata* → stream, remote, commit/tree ids, refs updated, exit
  status, tool version, timestamp.
- *Decision recorded per project convention* → ADR-0013 + index update.
