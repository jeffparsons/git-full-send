# ADR-0015 — TTL-based reaping of stale streams

- Status: accepted
- Date: 2026-06-15

## Context / problem statement

[ADR-0014](0014-forgetting-a-stream.md) added the explicit `forget-stream`
command and deliberately deferred *TTL-based reaping* — automatically forgetting
streams that have gone stale — so its design wasn't pre-judged. Stable ids keep
the ref set bounded, and `forget-stream` lets an operator retire a stream by
hand, but nothing reclaims streams an operator forgot to retire. Issue #63 asks
for the complementary automatic policy, leaving four questions to settle first:

1. **Where is "age" measured?** The `code` commit's committer date is available
   but, per the issue, "reflects the synced working state, not when it was last
   pushed"; a sidecar "last-touched" marker written on each receive would be more
   honest.
2. **Opt-in vs. default-on**, and the default age if any.
3. **Client vs. server** — which side's refs are reaped.
4. **Trigger** — `listen` startup, the accept loop, or a dedicated subcommand.

## Decision drivers

- Reaping deletes an operator's data; it must be impossible to trigger by
  accident.
- Keep the receive hot path lean (ADR-0013) — don't add per-push writes.
- Preserve the model that a stream's whole footprint is "refs under one prefix"
  (ADR-0012/0014), so cleanup stays a ref operation with nothing else to keep
  consistent.
- Reuse the existing `forget-stream` deletion rather than inventing a second one.

## Decision

Add an opt-in, server-side **`reap` subcommand** (and a
`gfs_server::reap_streams` library function) that forgets every stream whose
`code` commit is older than a caller-supplied cutoff. Reaping is exactly "list
the stale streams, then `forget_stream` each", so it inherits ADR-0014's
guarantees.

1. **Age = the `code` commit's committer date.** The client synthesises a fresh
   `code` commit stamped with the current time on **every** sync (the client's
   `encode`, ADR-0009) and unconditionally advances the `code` ref, so its
   committer date already tracks "last synced". This needs no sidecar marker and
   no write on the receive path. The honesty gap the issue raises is only the
   client's clock vs. the server's receive clock — negligible for a single
   developer syncing to their own (typically NTP-synced) workstation, and the
   worst case is benign: a stream that looks falsely stale is simply re-created on
   the next sync (forgetting is safe on a live stream — ADR-0014).

2. **Opt-in, with a required cutoff.** The CLI takes a required
   `--older-than-days <N>`; there is no default age and nothing reaps implicitly.
   A `--dry-run` reports which streams would be reaped without deleting. The
   library function takes an explicit `cutoff_unix_secs` (the CLI passes
   `now - N days`), keeping it a pure, deterministically-testable function of
   `(repo, cutoff)` with no ambient clock.

3. **Server-side only.** `reap` reclaims the server's synced `code`/`extra` refs.
   A client repo's local `sent/*` delta-base pins are tiny, meaningful only mid
   sync, and already cleanable via the symmetric `forget-stream`; the age signal
   lives on the server anyway.

4. **A dedicated subcommand, operator/cron-triggered.** `reap` sits alongside
   `list-streams` and `forget-stream` as a streams-management command. It is not
   woven into `listen` startup (a long-lived listener would never re-reap) or the
   accept loop (which would add latency and complexity to the hot path); an
   operator runs it by hand or from cron/a timer.

## Consequences

- `git-full-send` gains a `reap` subcommand and `gfs-server` gains
  `reap_streams`, `ReapOutcome`/`ReapedStream`, and a `ServerError::Reap`
  variant. The `…/code` → stream-id recovery is now shared by `list_streams` and
  `reap_streams` so the ref layout stays in one place (ADR-0012).
- The server's ref set is now reclaimable by age, not just one stream at a time.
- Documented in [`docs/operating.md`](../operating.md) (a `reap` subsection) and
  back-referenced from ADR-0014's deferred non-goal.

### Non-goals (deferred / rejected)

- **A sidecar "last-touched" marker.** More honest about server-receive time, but
  it adds a write to the receive path and state outside the stream's ref prefix to
  keep consistent with `forget-stream`. Revisit only if client/server clock skew
  proves to be a real problem in practice.
- **The `code` reflog as the age signal.** It records server-side update times
  natively, but reflogs are off by default on bare repos, so it is unreliable.
- **Client-side reaping** of `sent/*` pins (covered by the manual `forget-stream`).
- **A metrics record for `reap`.** Like `list-streams`/`forget-stream`, it is a
  management command, not part of the sync/checkout data path (ADR-0013).
