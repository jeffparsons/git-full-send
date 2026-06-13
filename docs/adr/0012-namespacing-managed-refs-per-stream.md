# ADR-0012 — Namespacing managed refs per stream

- Status: accepted
- Date: 2026-06-13

## Context / problem statement

`git-full-send` parks its synced state on managed refs: the client encodes a
`code` commit and pushes it, the server checks that ref's tree out into a
worktree ([ADR-0004](0004-encoding-the-sync-state-in-git.md),
[ADR-0008](0008-remote-worktree-disposability.md)), and both ends retain the
prior tip as a delta base ([ADR-0005](0005-transfer-mechanism.md)). Until now
those refs were **global, single-instance** constants —
`refs/git-full-send/code` on the server, `refs/git-full-send/sent/code` on the
client. Two senders pushing to one server therefore clobber each other's `code`
ref.

We need more than one independent flow of synced state to coexist on a single
server (e.g. several machines, or one machine syncing several branches), and we
want the naming settled now because getting it wrong churns the encode,
transfer, and worktree-reassembly code later.

## Decision drivers

- Concurrent senders must not collide on refs.
- The delta-base retention of ADR-0005 must keep working — it only pays off when
  the *same* refs are reused across syncs.
- Zero-config for the common single-sender case; explicit control when needed.
- The unit of namespacing should not be welded to any one notion (a machine, a
  user, a branch) — callers compose those policies themselves.
- Stay within the `refs/git-full-send/` namespace the server already guards
  ([ADR-0005](0005-transfer-mechanism.md),
  [ADR-0010](0010-receive-pack-transport-wiring.md)).

## Decision

Introduce a **stream**: an independent, reusable slot of synced state, named by
a caller-chosen **stream id**. All managed refs are namespaced under it:

```text
refs/git-full-send/streams/<stream-id>/code        # synced code tip (client → server)
refs/git-full-send/streams/<stream-id>/sent/code   # client-local delta-base pin
```

(The future `extra` ref of [ADR-0004](0004-encoding-the-sync-state-in-git.md)
will live at `…/streams/<stream-id>/extra`.)

- **`StreamId` is a validated newtype** in `gfs-common`, and the ref layout is
  built only through `gfs_common::code_ref` / `sent_ref` so neither side
  hard-codes the strings. Ids are validated as Git ref paths via `gix-validate`,
  so a malformed id is rejected at the boundary.

- **Stable and reused, never single-use.** The id is held constant across syncs
  so the prior `code`/`sent` tips survive as delta bases. A fresh id per push
  would orphan the delta base every sync and litter the server with refs — it is
  explicitly *not* the model.

- **Free-form, including slashes.** Stream ids may be branch-shaped
  (`feature/foo`); validation operates on the assembled ref, and server-side
  enumeration recovers the (possibly slash-containing) id by stripping the
  `…/streams/` prefix and the trailing `/code`.

- **Default-on, zero-config, generated.** The client resolves the stream in
  priority order: an explicit id (CLI `--stream-id` / library argument), else
  the effective `git-full-send.stream-id` from Git config, else a freshly
  generated id (8 random bytes, hex) that is **persisted to the repo's local
  config** for reuse. A *generated* default — rather than a constant like
  `"default"` — means two unrelated repos pushing to one server don't collide by
  accident: the safe behaviour is the default, while a stable id keeps the delta
  base intact.

- **Stream identity is orthogonal to worktree assignment.** The server's
  `update_worktree(repo, worktree, stream_id)` checks *that* stream's `code`
  tree into *that* worktree; the mapping is the caller's policy (a dedicated
  worktree per stream, several streams taking turns in one shared worktree
  mediated externally, fan-in, …). No 1:1 stream↔worktree correspondence is
  assumed or imposed. `list_streams` enumerates the streams a server holds so an
  orchestrator can discover them.

The `receive-pack` `pre-receive` hook is **unchanged**: per-stream refs already
sit under `refs/git-full-send/`, so the existing namespace allowlist still
covers them.

## Consequences

- `gfs_common::CODE_REF` / `gfs_client::SENT_REF` constants are removed in favour
  of the `code_ref` / `sent_ref` builders; `encode`, `push`/`retain`, `sync`, and
  `update_worktree` all take a `StreamId`. The CLI gains `--stream-id` (optional
  on `sync`, defaulting via config; **required** on `update-worktree`, which has
  no repo-local default) and a `list-streams` command.
- `gfs-common` gains small dependencies (`gix-validate`, `bstr`) for ref-name
  validation; the client gains `getrandom` for the generated default.
- The set of refs on a server is bounded by the number of *streams*, not the
  number of pushes, because ids are stable and reused.

### Non-goals (deferred)

- **Cleanup / reaping of unused streams.** Because stable ids keep the ref set
  bounded, this is not urgent; an explicit "forget this stream" path and/or
  TTL-based reaping are left to a follow-up ticket.
- **Cross-stream isolation / authentication.** The transport authenticates no
  one (localhost + manual SSH tunnel, single trusted user;
  [ADR-0006](0006-transport-and-connectivity.md)). Namespacing here is
  collision-avoidance among cooperating streams, not a security boundary; a hook
  that confined a connection to a single stream's subtree would be a separate
  decision.
