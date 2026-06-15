# ADR-0014 — Forgetting a stream

- Status: accepted
- Date: 2026-06-15

## Context / problem statement

[ADR-0012](0012-namespacing-managed-refs-per-stream.md) namespaces every managed
ref under a **stream** (`refs/git-full-send/streams/<id>/…`) and deliberately
deferred *cleanup* as a non-goal: stable ids keep the ref set bounded, but
nothing removes a stream once you are done with it. Its `code` and `extra` refs
persist on the server, and the client's scratch `code`/`extra` refs and `sent/*`
delta-base pins persist locally, forever. Issue #48 (raised by the #40 audit)
asks for an explicit way to retire a stream.

## Decision drivers

- A stream's entire footprint is refs under one well-defined prefix — removal
  should be exactly "delete those refs", nothing more clever.
- The client and the server each hold a stream's refs; one mechanism should clean
  up either side rather than two divergent commands.
- Stay built from the shared `gfs_common` ref-layout builders so neither side
  hard-codes the strings (ADR-0012).
- Don't overreach into state that isn't a stream's to own (worktrees, config).

## Decision

Add a **`forget-stream --repo --stream-id`** command (and a
`gfs_server::forget_stream` library function) that deletes every ref under
`gfs_common::stream_prefix(stream)` — `refs/git-full-send/streams/<id>/` — in the
target repo, in one ref transaction, returning the count removed.

- **Prefix-scoped, trailing-slash-bounded.** A new `gfs_common::stream_prefix`
  builder assembles the prefix from `STREAMS_PREFIX` so the layout stays in one
  place (ADR-0012). The trailing slash bounds it at a path segment, so forgetting
  `foo` never touches `foobar`. Enumeration and deletion go through `gix`,
  matching `list_streams` and the client's ref edits.

- **Symmetric across both ends.** The same command cleans up whichever repo it is
  pointed at: against the **server** repo it drops the synced `code`/`extra`
  refs (so the stream leaves `list_streams`); against a **client** repo it drops
  that repo's local refs for the stream — the scratch `code`/`extra` refs a sync
  pushes from and the `sent/*` delta-base pins. No separate client command.

- **Idempotent.** Forgetting a stream with no matching refs (never synced, or
  already forgotten) removes nothing and succeeds, rather than erroring — so it is
  safe to run speculatively and to re-run.

- **Safe to run on a live stream.** Deletion only orphans refs; a subsequent
  `sync` re-creates them from scratch (that first push simply has no delta base).

## Consequences

- `git-full-send` gains a `forget-stream` subcommand and `gfs-common` a
  `stream_prefix` builder; `gfs-server` gains `forget_stream` and a
  `ServerError::ForgetStream` variant, sitting alongside `list_streams` as the
  streams-management surface.
- The ref set a server holds is now operator-reclaimable, not just bounded.
- Documented in [`docs/operating.md`](../operating.md) (a `forget-stream` server
  subsection and a client-side "Retiring a stream" note).

### Non-goals (deferred)

- **Reaping the per-worktree index dir.** `<git-dir>/git-full-send/worktrees/<key>`
  is keyed by a hash of the canonical *worktree path*, not the stream id — a
  stream and a worktree are orthogonal (ADR-0012), so an index dir cannot be
  reliably associated back to a stream. The worktree is disposable anyway
  ([ADR-0008](0008-remote-worktree-disposability.md)); operators remove worktree
  directories themselves. `forget-stream` touches refs only.

- **Auto-unsetting the client's `git-full-send.stream-id` config.** Dropping the
  refs does not clear the repo's default-stream config key; that is left as a
  documented manual `git config --unset` so forgetting refs never silently
  changes which stream a bare `sync` would use. An `--unset-config` flag can be
  added later if it proves worth it.

- **TTL-based reaping.** Automatically forgetting streams whose `code` ref is
  older than a configurable age is a complementary policy, split into its own
  follow-up so its design (where the age is measured, opt-in vs. default, client
  vs. server) is not pre-judged here. **Settled in
  [ADR-0015](0015-ttl-based-reaping-of-stale-streams.md)** (issue #63): an opt-in,
  server-side `reap` command that forgets streams whose `code` committer date is
  older than a required cutoff.
