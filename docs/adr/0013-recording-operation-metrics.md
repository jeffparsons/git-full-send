# ADR-0013 — Recording operation metrics

- Status: accepted
- Date: 2026-06-13

## Context / problem statement

`git-full-send` moves a developer's working state between machines on every sync
([ADR-0004](0004-encoding-the-sync-state-in-git.md),
[ADR-0005](0005-transfer-mechanism.md)). When a cycle feels slow, or the
force-included `extra` layer grows unexpectedly, there is currently no record to
look back on: the work happens, and the only trace is whatever `tracing` line
scrolled past. Issue #42 asks for **timing metrics, on both client and server,
recorded somewhere durable for retrospective analysis**, with relevant metadata
(e.g. the number and size of files added in each layer).

So we need to decide *where* the data lands and *in what form*, on a tool that is
Unix-first, single-trusted-user, and deliberately dependency-light
([ADR-0001](0001-language-runtime-and-core-crates.md),
[ADR-0006](0006-transport-and-connectivity.md)).

## Decision drivers

- **Durable and retrospective.** The point is to analyse *past* runs, so the
  record must outlive the process — not depend on a log scrollback the operator
  may not have captured.
- **Machine-readable.** "Analysis" means querying/aggregating, so a structured,
  greppable format beats prose log lines.
- **Self-contained.** No external metrics service or network dependency for an
  MVP that otherwise needs none.
- **Never load-bearing.** Metrics are observability; failing to record one must
  never fail the sync, receive, or checkout it describes.
- **Lives with the data it describes.** Client metrics belong to the client repo,
  server metrics to the server repo.

## Decision

Each side appends one structured **JSON Lines** record per operation to a
per-side sink:

```text
<git-dir>/git-full-send/metrics.jsonl
```

alongside the existing `git-full-send/` server state (the per-worktree indexes of
[ADR-0011](0011-worktree-reassembly-mechanics.md)). A concise `tracing` summary
line is *also* emitted per operation for live visibility, but the JSONL file is
the durable record.

- **Shared sink, per-crate record shapes.** `gfs_common::metrics` owns the
  plumbing — the path, an atomic append, and the shared envelope fields
  (`kind`, `ts_unix_ms`, `tool_version`). Each record carries a `kind` tag
  (`sync`, `receive`, `update_worktree`) so the one file is self-describing. The
  record *shapes* live in the crate that produces them.
- **Best-effort.** Writing goes through `metrics::record`, which logs a
  `tracing::warn!` on failure and swallows it. A sync never fails because its
  metrics couldn't be written.
- **Intra-process write lock.** The server handles connections on concurrent
  threads, so appends are serialised by a process-global mutex and each record is
  written in a single `write_all`, so lines never interleave.

### What is recorded

- **Client `sync`:** per-phase wall times (`code_encode`, `extra_encode`,
  `code_push`, `extra_push`, `retain`) and the total — the two pushes are timed
  separately because each chain rides its own exchange with its own delta policy
  ([ADR-0005](0005-transfer-mechanism.md)); for the **code** layer the working-tree *delta*
  (files overlaid, bytes overlaid, files removed) and for the **extra** layer the
  *full* selected set (files, bytes), each with its commit/tree id; plus stream,
  remote, timestamp, and tool version.
- **Server `receive`** (per `git receive-pack` connection): duration, exit
  status/code, on-wire `bytes_in`/`bytes_out`, and the refs the namespace hook
  accepted. Recorded even for a failed receive.
- **Server `update_worktree`:** total plus per-step times (`resolve`,
  `read_tree`, `clean`), the checked-out tree id, stream, and worktree path.

### Notable mechanics

- **Code-layer sizes are the delta, extra-layer sizes are the full set.** The
  code commit is built on the index as a base and only the index→worktree delta
  is walked ([ADR-0009](0009-working-tree-fidelity-for-the-code-commit.md)), so
  the whole-tree size is *not* cheaply available — we record what the encode
  already touches. The extra tree is re-selected in full each sync, so its file
  count and byte total are the whole layer.
- **On-wire bytes are measured server-side.** `git push` only prints the pack
  size as TTY progress, suppressed under our piped stderr, so the client records
  its encoded per-layer byte totals instead, and the authoritative on-wire size
  is counted on the server.
- **The server now pumps the receive-pack stream through two counting threads**
  rather than handing the raw socket straight to the child as its stdin/stdout.
  The bytes observed are the identical raw stream (no framing — ADR-0005 is
  unchanged; the client-side fd-passing of
  [ADR-0010](0010-receive-pack-transport-wiring.md) is untouched); only a
  localhost-bandwidth userspace copy is added, negligible against git's pack work.

## Consequences

- `gfs-common`, `gfs-client`, and `gfs-server` gain `serde`/`serde_json` for
  record serialisation (relative to ADR-0001's core set: small, ubiquitous, and
  the natural way to emit JSON; git object ids are serialised as hex strings, so
  no `gix` types need serde impls).
- A new `metrics.jsonl` file appears under each repo's git dir on first use; it is
  internal state, not part of the synced tree.
- The `pre-receive` hook now also appends accepted ref names to a per-connection
  file named by an environment variable, so the handler can report `refs_updated`
  without parsing the sideband.

### Alternatives considered

- **Structured `tracing` events only** (no on-disk format). Rejected: ephemeral
  and contingent on the operator having configured durable log capture — it does
  not satisfy "recorded somewhere for retrospective analysis". (We still emit a
  summary line, but as a convenience, not the record.)
- **An external metrics system (StatsD/Prometheus/OTLP).** Rejected for the MVP:
  it adds infrastructure, a network dependency, and configuration to a tool whose
  whole transport is a single localhost socket behind an SSH tunnel.

### Non-goals (deferred)

- **Failure-path client metrics.** `sync` records on the success path only for
  now; the server already records failed receives via exit status. Recording a
  sync that errored mid-way is a follow-up.
- **Cross-process append safety.** The process-global lock covers the realistic
  single-server-process case; an advisory file lock for two processes writing one
  repo's sink concurrently is a possible future hardening.
- **Rotation / pruning.** `metrics.jsonl` grows unbounded; a size cap or rotation
  is left to a follow-up.
- **Analysis / reporting tooling.** This decision only *produces* the data.
