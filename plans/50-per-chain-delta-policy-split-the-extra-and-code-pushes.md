# Plan — #50 Per-chain delta policy: split the extra and code pushes

Relevant ADRs: [ADR-0005 — Transfer mechanism](../docs/adr/0005-transfer-mechanism.md),
[ADR-0004 — Encoding the sync state in Git](../docs/adr/0004-encoding-the-sync-state-in-git.md),
[ADR-0007 — Syncing extra gitignored files](../docs/adr/0007-syncing-extra-gitignored-files.md),
[ADR-0013 — Recording operation metrics](../docs/adr/0013-recording-operation-metrics.md).

## Goal

Give the `code` and `extra` chains **different delta policies** in a sync. ADR-0005
("Match delta policy to the payload", line 91) wants `--thin` deltas for the `code`
chain but a *predictable whole-object send* for the volatile `extra` chain (build
outputs that won't delta well, where a variable delta search just burns CPU and
produces the bimodal-performance symptom ADR-0005 diagnoses). Today both ride a
single `git push --thin` (`crates/client/src/lib.rs:95` → `push_refs`,
`crates/client/src/push.rs:122`); the deferred-work note at `push.rs:115-121` is
exactly this issue.

## The core constraint

A single `git push` emits **one** pack under **one process-global** delta policy
(`--thin`, `pack.window`/`pack.depth` are per-invocation, not per-ref). You cannot
give `code` and `extra` different policies inside one push. So the route is **two
pushes** — one per chain — each with its own policy. (Per-ref pack config was the
other option floated in the issue; it does not exist in git, so it's out.)

## Decisions locked in pre-plan (approved 👍 on the issue)

- **Two pushes, one per chain.** `code` keeps `--thin`; `extra` gets a whole-object
  send.
- **`extra` policy = `--no-thin -c pack.window=0`.** `pack.window=0` disables the
  delta search entirely (whole objects, predictable); `--no-thin` ensures no thin
  deltas against bases outside the pack. Because `extra` commits are parented on the
  retained `sent_extra_ref` tip (`encode.rs:298-303`), push negotiation still
  excludes objects the server already holds — so this sends only the *changed*
  objects, just whole rather than thin-deltified. It is **not** a full re-send.
- **Per-chain retention / failure semantics.** Retain each chain's tip immediately
  after *its own* push succeeds, so a `code` success is never lost if the later
  `extra` push fails. Each chain is independent; partial success leaves each
  `sent/*` ref pointing at what the server actually has.
- **Document against ADR-0005** (acceptance requirement): record the per-chain
  split and the consequence that the client now does **two** receive-pack exchanges
  per sync (ADR-0005 currently says "one exchange").
- **Metrics:** split the single `push_ms` into `code_push_ms` / `extra_push_ms` so
  the per-chain cost is visible (the issue's "measure the effect"); deeper
  benchmarking pairs with the transfer-benchmark issue and stays out of scope.

## Out of scope (explicitly)

- A transfer benchmark / measured before-after numbers — that's its own issue; here
  we make the policies differ and expose the per-chain timing so a benchmark *can*
  measure it.
- Tuning the exact `pack.window`/`pack.depth`/`core.bigFileThreshold` values beyond
  "disable delta for `extra`" — the ADR's lever is whole-object vs. delta, not
  fine-grained pack tuning.
- Any change to the `code` chain's policy (`--thin` stays) or to encode/checkout.
- Making the two pushes atomic across chains — they're deliberately independent.

## Design

### `DeltaPolicy` (`crates/client/src/push.rs`)

A small public enum naming the per-chain choice:

```rust
/// How a push asks `git` to deltify the objects it sends.
#[derive(Debug, Clone, Copy, Default)]
pub enum DeltaPolicy {
    /// `--thin`: send changed blobs as small deltas against a base the server
    /// already holds. The default; used for the `code` chain (ADR-0005).
    #[default]
    Thin,
    /// `--no-thin -c pack.window=0`: disable the delta search for a predictable
    /// whole-object send. Used for the volatile `extra` chain (ADR-0005), whose
    /// big build outputs don't delta well.
    WholeObject,
}
```

Mapping to argv inside `push_refs` (replacing the hard-coded `.arg("--thin")`):

- `Thin` → `push --thin`
- `WholeObject` → `push --no-thin` with `-c pack.window=0` inserted alongside the
  existing `-c protocol.fd.allow=always`.

### `push_refs` signature (`crates/client/src/push.rs`)

```rust
pub async fn push_refs(
    repo_dir: &Path,
    remote: &str,
    ref_names: &[&str],
    policy: DeltaPolicy,
) -> Result<(), PushError>
```

The body is unchanged except for building the policy flags. `push_ref` (the
single-ref test wrapper) gains the same parameter and forwards it; the seam tests
that only care about the namespace pass `DeltaPolicy::default()`.

Update the doc-comment: drop the "deferred" note (`push.rs:115-121`) and instead
describe that the caller chooses the per-chain policy, with `sync` issuing one push
per chain.

### `sync` wiring (`crates/client/src/lib.rs`)

Replace the single combined push + paired retain with one push **per chain**,
retaining each tip right after its own push so a `code` success survives an `extra`
failure:

```rust
// code chain — thin deltas (ADR-0005).
let t = Instant::now();
push::push_refs(&repo_dir, &remote, &[&code.code_ref], DeltaPolicy::Thin).await?;
let code_push_ms = elapsed_ms(t);
let t = Instant::now();
push::retain_pushed_tip(&repo_dir, &gfs_common::sent_ref(&stream), code.commit)?;
let mut retain_ms = elapsed_ms(t);

// extra chain — predictable whole-object send (ADR-0005).
let t = Instant::now();
push::push_refs(&repo_dir, &remote, &[&extra.extra_ref], DeltaPolicy::WholeObject).await?;
let extra_push_ms = elapsed_ms(t);
let t = Instant::now();
push::retain_pushed_tip(&repo_dir, &gfs_common::sent_extra_ref(&stream), extra.commit)?;
retain_ms += elapsed_ms(t);
```

The `tracing::info!` summary and the `record_sync` call take the two new timings.

`push_refs` still accepts a slice of refs, so a future caller could batch refs that
*share* a policy; `sync` happens to pass one ref per call.

### Metrics (`crates/client/src/metrics.rs`)

- `Timings`: replace `push_ms: f64` with `code_push_ms: f64` and `extra_push_ms: f64`.
- `SyncRecord`: same field swap (`push_ms` → `code_push_ms` + `extra_push_ms`).
- `record_sync`: thread the two fields through.

This is an additive-shaped change to the JSONL record; ADR-0013 treats the metrics
schema as observability-only and already best-effort, so no compatibility shim is
needed. Note the rename in the ADR-0013 record-shape description if it enumerates
fields.

### ADR-0005 update (`docs/adr/0005-transfer-mechanism.md`)

- **Client bullet (lines 47-51):** amend "in one exchange" — the client now issues
  **two** receive-pack exchanges per sync, one per chain, so each chain can carry
  its own delta policy.
- **"Match delta policy to the payload" (line 91):** note this is now *implemented*
  via the two-push split (`code` `--thin`, `extra` `--no-thin -c pack.window=0`).
- **Consequences (lines 97-105):** record the trade-off — two TCP connections and
  two `git`/`receive-pack` subprocess pairs per sync (a little more latency than one
  exchange) bought for predictable `extra` transfer; the per-chain retention/failure
  independence; and that `push_ms` is now reported per chain.

The `push.rs` module doc and `lib.rs::sync` doc-comment (which say "one exchange" /
"single `git push --thin`") get the same correction.

## Tests (`crates/client/tests/transfer.rs`)

The existing suite already asserts the functional outcome — both chains' objects
land, second syncs advance, retention refs pin the pushed tips — and must stay green
unchanged (the "no regression in the transfer tests" acceptance). Concretely:

- Fix the now-stale wording in `push_lands_extra_ref_alongside_code` ("in the same
  exchange as `code`") — the two chains now ride separate exchanges; the assertions
  (extra tree/blob land, retention ref pinned) are unchanged.
- Add **`extra_chain_second_sync_lands_changed_output`**: sync a force-included
  build output, change it, sync again, and assert the server's `extra` tree carries
  the new content and `sent_extra_ref` advanced. This exercises the whole-object
  (`WholeObject`) policy across two syncs with a retained parent — proving the new
  push path lands changed objects correctly, not just first-run.
- Add **`push_refs_whole_object_lands_objects`** (or extend the namespace seam test):
  a direct `push_refs(.., DeltaPolicy::WholeObject)` of a namespaced ref succeeds and
  the objects are walkable on the server — exercising the new arg path explicitly.

Directly asserting *on the wire* that `extra` used whole objects vs. thin deltas
needs pack-internal introspection that the black-box CLI assertions deliberately
avoid; the policy itself is covered by the code structure + ADR, while the tests
pin the observable behaviour (objects land, chains advance independently).

## Acceptance criteria mapping

- *`code` and `extra` can use different delta policies in a sync* → `DeltaPolicy`
  enum + two `push_refs` calls in `sync` (`code` `Thin`, `extra` `WholeObject`);
  exercised by `push_refs_whole_object_lands_objects` and the existing both-chains
  tests.
- *The choice is documented against ADR-0005* → ADR-0005 + `push.rs`/`sync` doc
  updates.
- *No regression in the transfer tests* → existing suite stays green; only stale
  comment wording changes, plus two added tests.

## Definition of done

`cargo build`, `cargo test`, `cargo clippy`, `cargo fmt --check` all green; ADR-0005
updated; `push_ms` split into `code_push_ms`/`extra_push_ms` in the metrics record;
the new tests passing alongside the unchanged existing transfer suite.
