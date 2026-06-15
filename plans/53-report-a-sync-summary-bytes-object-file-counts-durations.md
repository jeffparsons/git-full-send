# Plan — #53: Report a sync summary (bytes, object/file counts, durations)

## Goal

Give the operator a clear, human-readable summary at the end of every `sync`:
per-layer file/object counts and byte sizes for the `code` and `extra` trees, and
per-phase durations (encode / extra-encode / push / extra-push / total). The data
already exists — `sync` computes all of it for the #42 metrics record — so this is
a **reporting-surface** change, not new measurement.

## Background

#42 / ADR-0013 has landed. Today `sync` (`crates/client/src/lib.rs`):

- emits two per-phase progress lines via `tracing::info!` ("encoded code state",
  "encoded extra state");
- emits one final combined `tracing::info!` summary line (lib.rs:123-128) carrying
  `total_ms` and raw per-layer file/byte counts; and
- appends the durable JSON Lines record via `metrics::record_sync`.

ADR-0013 explicitly defers **"analysis / reporting tooling"** as a non-goal, and
notes the tracing summary line is "a convenience, not the record". The gap #53
closes: that convenience line is a structured log event (level/timestamp prefix,
raw bytes, only `total_ms` — no per-phase breakdown, no `files_removed`), not the
"clear summary line/block" an operator reads at a glance. The other operator-facing
commands (`list-streams`, `forget-stream`) already speak to the operator via
`println!` to **stdout**; the sync summary should match that.

Approved in pre-plan: a clean **stdout block** is the chosen surface.

## Design

Respect the library/CLI boundary the codebase already uses: the library returns
data, the CLI owns operator presentation (exactly how `list_streams` returns
values that `main.rs` prints). So:

1. `gfs_client::sync` returns a `SyncSummary` instead of `()`.
2. `crates/cli/src/main.rs` formats and prints the human-readable block to stdout.

This keeps `gfs-client` from assuming it owns stdout, and puts byte/duration
formatting where presentation lives.

### 1. `SyncSummary` — the returned data (`crates/client/src/lib.rs`)

Add a public struct carrying exactly what the summary shows. It mirrors the values
already gathered for the metrics record, so we lift them from the same locals:

```rust
/// Operator-facing summary of one completed `sync` (issue #53).
///
/// Carries the counts, sizes, and per-phase timings a `sync` already computes for
/// its metrics record (issue #42), returned so the caller can present them. The
/// durable JSONL record (ADR-0013) is still written independently inside `sync`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SyncSummary {
    pub stream: StreamId,
    pub code: CodeLayerStats,   // files_overlaid, bytes_overlaid, files_removed
    pub extra: ExtraLayerStats, // files, bytes
    pub timings: SyncTimings,
}
```

For the timings, expose a public, owned mirror of the (currently private)
`metrics::Timings` — `SyncTimings { total_ms, code_encode_ms, extra_encode_ms,
code_push_ms, extra_push_ms, retain_ms }`. Cleanest move: **promote
`metrics::Timings` to a public `SyncTimings`** in `lib.rs`, re-export it, and have
`metrics::record_sync` consume it (drop the duplicate private struct). `CodeLayerStats`
/ `ExtraLayerStats` are already public (`encode.rs`) and re-exported.

`sync`'s signature becomes `Result<SyncSummary, ClientError>`. It builds the
`SyncSummary` from the same `code`/`extra` outcomes and timing locals it already
has, passes the timings to `record_sync` (unchanged behaviour), and returns the
summary. **Remove the final combined `tracing::info!` summary line (lib.rs:123-128)**
— the stdout block supersedes it. Keep the two per-phase progress `info!` lines:
they are genuine live progress on stderr while the (single) summary now lives on
stdout, so the channels stay complementary (stderr progress log · stdout summary ·
JSONL durable record).

Call sites: the CLI binds the result; the `crates/client/tests/transfer.rs` callers
use `?`/`.unwrap()` and simply ignore the now-returned value — no test changes
forced (returning a value where `()` was ignored is source-compatible).

### 2. Format and print the block (`crates/cli/src/main.rs`)

The `Command::Sync` arm captures the returned `SyncSummary` and prints a block to
stdout. Proposed layout (final wording tuned during implementation):

```text
Synced stream a1b2c3d4 to host:1234 in 1.4s
  code:  3 files (+2.1 KiB), 1 removed   encode 12ms · push 0.9s
  extra: 5 files (1.3 MiB)               encode 8ms · push 0.4s
```

Helpers, kept in the CLI as presentation concerns:

- `fn human_bytes(n: u64) -> String` — B / KiB / MiB / GiB, binary (1024) units,
  one decimal above KiB (e.g. `512 B`, `2.1 KiB`, `1.3 MiB`).
- `fn human_ms(ms: f64) -> String` — `12ms` under 1000ms, else seconds with one
  decimal (`1.4s`). The metrics keep raw ms; only the display rounds.

`code`'s `files_removed` is shown only when non-zero (or `0 removed` — decide at
implementation; leaning toward always showing it for predictability). The `retain`
phase is small and internal; fold it into total rather than its own column.

Use `println!` (stdout), matching `list-streams` / `forget-stream`.

### 3. Reconcile with ADR-0013

ADR-0013 described the tracing summary line as the live-visibility convenience.
Since #53 moves that convenience to a dedicated stdout block, add a short note to
ADR-0013 (Consequences) cross-linking #53: the per-operation human summary is now
a stdout block emitted by the CLI; the per-phase `tracing` lines and the durable
JSONL record are unchanged. No new ADR — this refines an existing accepted decision
rather than making a new one.

## Verification

- **Unit tests (CLI):** `human_bytes` / `human_ms` boundary cases (0, 1023→`1023 B`,
  1024→`1.0 KiB`, MiB, 999ms vs 1000ms→`1.0s`).
- **End-to-end (`crates/cli/tests/end_to_end.rs`):** the existing round-trip test
  captures sync stdout via `run_cli`; assert the summary block appears and reflects
  the known fixture (1 overlaid code file of "hello", 1 extra file "app") — e.g.
  stdout contains the stream id, `code:` and `extra:` lines, and the byte figures.
  Extend `run_cli` (or add a `run_cli_capture`) to return stdout for the assertion;
  the metrics-JSONL assertions already in that test are unaffected.
- `cargo test --workspace`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`.

## Out of scope / non-goals

- No change to the durable JSONL metrics record (#42 owns it) or to what's measured.
- No `--quiet`/`--format=json` flag for the summary — single human block for now;
  a machine-readable stdout mode can follow if asked.
- No on-wire byte count in the client summary: ADR-0013 records that server-side;
  the client shows its encoded per-layer byte totals (consistent with the metrics).
- Server-side (`receive` / `update_worktree`) summaries are not in scope here.

## Risks

- **Double-reporting if the tracing line were kept.** Avoided by removing the final
  combined `info!` so the operator sees one summary (stdout), not two.
- **Return-type churn.** `sync` now returns a value; verified the only callers are
  `main.rs` and `transfer.rs`, all of which either bind or harmlessly ignore it.
- **Formatting bikeshed.** Exact layout/units are easy to adjust; the helpers and
  the `SyncSummary` data shape are the load-bearing parts.
