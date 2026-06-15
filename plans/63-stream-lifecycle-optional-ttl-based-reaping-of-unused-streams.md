# Plan — #63: TTL-based reaping of unused streams

Follow-up to #48 / ADR-0014, which added the manual `forget-stream` command and
explicitly deferred TTL-based reaping so its design wasn't pre-judged. This adds
an **opt-in, operator-triggered** way to forget streams that have gone stale.

## Decisions (settled in pre-plan, issue #63)

1. **Where "age" is measured** → the stream's **`code` commit committer date**.
   The client synthesises a fresh `code` commit stamped `now()` on every sync
   (`crates/client/src/encode.rs` → `write_synth_commit`, using
   `gix::date::Time::now_local_or_utc()`) and unconditionally advances the `code`
   ref, so the committer date already tracks "last synced". No sidecar
   "last-touched" marker, no extra write on the receive hot path (ADR-0013 keeps
   that path lean), and the stream's footprint stays "refs under one prefix"
   (ADR-0012/0014). The honesty gap is only client-clock vs. server-receive-clock,
   negligible for a single developer syncing to their own NTP-synced workstation.
2. **Policy** → **opt-in**, with a **required age threshold**. Nothing is reaped
   unless the operator names an age; no silent default-on. A `--dry-run` reports
   what *would* be reaped without deleting.
3. **Scope** → **server only** (the synced `code`/`extra`). The client's `sent/*`
   delta-base pins are tiny, meaningful only mid-sync, and already cleanable via
   the existing manual `forget-stream`; the age signal lives on the server anyway.
4. **Trigger** → a dedicated **`reap` subcommand** an operator/cron runs,
   alongside `list-streams` / `forget-stream`. Not woven into `listen` startup
   (a long-lived listener would never re-reap) or the accept loop (latency +
   complexity on the hot path).

**Reaping is exactly: list streams whose `code` commit is older than the cutoff,
then `forget_stream` each.** It reuses the existing prefix-scoped deletion, so it
inherits ADR-0014's guarantees: idempotent, and safe on a live stream (a
subsequent `sync` re-creates the refs from scratch).

## Surface

### `gfs_server::reap_streams` (library, `crates/server/src/lib.rs`)

```rust
/// One stream considered for reaping.
pub struct ReapedStream {
    pub stream: gfs_common::StreamId,
    /// The `code` commit's committer time (Unix seconds) that made it stale.
    pub committed_unix_secs: i64,
    /// Refs removed (0 in `dry_run`, where nothing is deleted).
    pub refs_removed: usize,
}

/// Outcome of a reap pass.
pub struct ReapOutcome {
    /// Streams scanned (every stream with a `code` ref).
    pub scanned: usize,
    /// Streams found stale (and, unless `dry_run`, forgotten).
    pub reaped: Vec<ReapedStream>,
    pub dry_run: bool,
}

/// Forget every stream in `repo` whose `code` commit's committer date is
/// strictly older than `cutoff_unix_secs`. With `dry_run`, report the stale
/// streams without deleting anything.
pub fn reap_streams(
    repo: &Path,
    cutoff_unix_secs: i64,
    dry_run: bool,
) -> Result<ReapOutcome, ServerError>;
```

- Takes an **explicit cutoff instant**, not a `Duration` — keeps the function a
  pure function of (repo, cutoff) with no ambient clock, so tests are
  deterministic. The CLI computes `cutoff = now - older_than`.
- Implementation, native via `gix` (mirrors `list_streams` / `forget_stream`):
  1. `gix::discover(repo)` (→ `ServerError::NotARepo` on failure).
  2. Enumerate refs under `gfs_common::STREAMS_PREFIX`; for each `…/<id>/code`
     ref, recover the id (same `strip_prefix`/`strip_suffix("/code")` logic as
     `list_streams` — factor a small shared helper so the recovery lives once).
  3. Peel the ref to its commit (`reference.peel_to_id()` → `repo.find_commit(id)`)
     and read the committer time via `commit.commit_time()` (Unix seconds; falls
     back to `commit.committer()?.time` if needed). `scanned += 1`.
  4. If `committed_unix_secs < cutoff_unix_secs`, it's stale: in `dry_run`, count
     the refs under `gfs_common::stream_prefix(stream)` for reporting; otherwise
     call `forget_stream(repo, &stream)` and record its returned count. Push a
     `ReapedStream`.
  5. Return `ReapOutcome`.
- New error variant `ServerError::Reap(Box<dyn Error + Send + Sync>)`, mirroring
  `ListStreams` / `ForgetStream`, for enumeration/peel/commit-read failures.
- Consider factoring the `…/code` → `StreamId` recovery used by both
  `list_streams` and `reap_streams` into one private helper to avoid drift.

### `reap` CLI subcommand (`crates/cli/src/main.rs`)

```
git-full-send reap --repo <PATH> --older-than-days <N> [--dry-run]
```

- `ReapArgs { repo: PathBuf, older_than_days: u64, dry_run: bool }`.
  - **`--older-than-days <N>` (required).** Days, as an integer — the natural
    operator unit, dependency-free, and consistent with the existing
    integer-`SECS` style of `--connection-timeout` / `--timeout`. This refines the
    "`--older-than <duration>`" wording from the pre-plan hand-off: a plain day
    count avoids pulling in a duration-parsing dependency (ADR-0001 keeps the
    crate set tight). Flag if a humanised `30d`/`2w` form is preferred on review.
  - **`--dry-run`** — list what would be reaped; delete nothing.
- Handler: compute `now_unix = SystemTime::now()` → seconds;
  `cutoff = now_unix - older_than_days * 86_400`; call
  `gfs_server::reap_streams(&repo, cutoff, dry_run)`; print results.
- Output (plain, matching `forget-stream`'s style):
  - none stale → `no streams older than <N> day(s); nothing to reap`.
  - dry-run → one line per stale stream (`would reap <id> (last synced <…>, <k>
    ref(s))`) and a summary (`<m> of <scanned> stream(s) would be reaped`).
  - real → `reaped <id> (<k> ref(s) removed)` per stream and a summary count.
- Add the `Reap(ReapArgs)` variant and `Command::Reap` match arm; doc-comment
  `/// Forget streams whose code is older than a cutoff (server).`

## No metrics record

`list_streams` and `forget_stream` write no ADR-0013 metrics record — they're
stream-management commands, not the sync/checkout data path. `reap` follows suit
(deliberate non-goal); the forgotten streams simply stop appearing in
`list-streams`.

## Documentation

- **`docs/operating.md`** — add `### reap — reclaim stale streams` under §2
  (server), after the `forget-stream` subsection: what it measures (the `code`
  committer date = last sync), that it's opt-in and requires `--older-than-days`,
  the `--dry-run` flag, that it's safe to run on a live stream and idempotent
  (reuses `forget-stream`), and a cron example. Cross-link the `forget-stream`
  and "Retiring a stream" sections.
- **New ADR `docs/adr/0015-ttl-based-reaping-of-stale-streams.md`** (status
  accepted, date 2026-06-15) recording the four decisions above, their drivers
  (lean receive path, footprint-is-refs model, opt-in safety), and the rejected
  alternatives (sidecar last-touched marker; `code` reflog — off by default on
  bare repos; reaping woven into `listen`/accept-loop; client-side `sent/*`
  reaping). Non-goals: sidecar marker, client reaping, metrics record.
- **`docs/adr/0014-forgetting-a-stream.md`** — update the "TTL-based reaping"
  deferred non-goal to point at ADR-0015 as the follow-up that settled it.
- **`docs/adr/README.md`** — add the ADR-0015 row to the index table.

## Tests

- **`crates/client/tests/transfer.rs`** (where server-ref management is exercised
  against real refs — next to `forget_stream_removes_a_streams_server_refs_only`,
  reusing its `ref_exists` helper):
  - A helper to seed a stream's `code` ref at a chosen committer date — build a
    commit with `GIT_COMMITTER_DATE`/`GIT_AUTHOR_DATE` set (via `std::process::
    Command` env, since `test_support::git` doesn't take env) and `update-ref`
    the `code_ref` to it (the `rejects_refs_outside_the_namespace` test already
    sets a `code` ref by hand this way).
  - `reap_forgets_only_streams_older_than_the_cutoff`: seed stream A dated well
    in the past and stream B dated recently; `reap_streams(repo, cutoff, false)`
    with a cutoff between them; assert A's refs are gone and absent from
    `list_streams`, B's remain, `scanned == 2`, `reaped == [A]`.
  - `reap_dry_run_reports_without_deleting`: same setup; `dry_run = true`; assert
    the stale stream is reported but **all** refs still exist.
  - `reap_on_empty_repo_is_a_noop`: no streams → `Ok` with `scanned == 0`,
    `reaped == []`.
  - (Optional) `reap_is_idempotent`: a second real reap with the same cutoff
    finds nothing new.
- **`crates/cli/tests/end_to_end.rs`** — a `reap` smoke test through the actual
  binary: sync a stream, hand-set its `code` ref to an old date, run
  `git-full-send reap --repo … --older-than-days 30 --dry-run` (assert the ref
  survives and the stream is named in stdout), then without `--dry-run` (assert
  the ref is gone and it leaves `list-streams`). Mirror the existing `run_cli`
  subprocess + in-process `listen` pattern.
- Keep the existing `command_line_surface_is_wired_up` smoke coverage style for
  `reap`'s arg parsing if there's an equivalent CLI-surface test.

## Out of scope / non-goals

- No sidecar "last-touched" marker (decision 1); revisit only if clock skew
  proves to be a real problem.
- No client-side reaping of `sent/*` pins (decision 3) — manual `forget-stream`
  covers it.
- No reaping triggered from `listen` startup or the accept loop (decision 4).
- No metrics record for `reap`.
- No change to `forget_stream`'s behaviour; `reap` composes it.

## Validation

- `cargo test --workspace` (new + existing tests pass).
- `cargo clippy --workspace --all-targets` and `cargo fmt --check` clean.
- Manual: `git-full-send reap --help`, a `--dry-run` then real run against a
  scratch repo with one fresh and one back-dated stream.
