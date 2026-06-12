# ADR-0005 — Transfer mechanism

- Status: proposed
- Date: 2026-06-12

## Context / problem statement

Once the client has synthesised the sync state as Git objects
([ADR-0004](0004-encoding-the-sync-state-in-git.md)), those objects have to move
from the client to the server over a localhost connection (manually SSH-tunnelled
— see [ADR-0006](0006-transport-and-connectivity.md)). We need to decide how the
objects are packed and transferred, and how the server ingests them into its
repository.

## Decision drivers

- Efficient, predictable transfer of potentially large-ish payloads (build
  outputs).
- Reuse of Git's existing pack/transfer machinery vs. control over the wire
  format.
- Keep the server side simple and robust.

## Considered options (no decision yet)

1. **`git push` → `git-daemon` receive-pack on the server.** The server accepts
   the connection (localhost) and hands the stream off to `git-daemon` to run
   receive-pack. Maximum reuse of battle-tested Git machinery.
2. **Native gix smart-protocol implementation.** If gitoxide has — or can
   feasibly gain — enough of the send/receive-pack protocol, the server handles
   the transfer itself without `git-daemon`. More control, fewer moving parts at
   runtime, but depends on gix capabilities
   ([ADR-0002](0002-git-manipulation-strategy.md)).

## Known observation: intermittent slow transfers

In an earlier prototype, transferring changed build outputs to the remote was
**sometimes surprisingly slow and sometimes as fast as expected**, and the cause
was never pinned down. A leading suspicion is **pathological pack shapes** —
packs that end up structured poorly relative to what the server already has,
defeating delta reuse. Whatever transfer mechanism we choose must let us
understand and control this.

## Status

Proposed. Options and the performance concern are recorded; the choice is
deferred pending research.

> ⚠ Research task needed: evaluate `git-daemon` receive-pack vs. a native gix
> receive path (including whether gix needs upstream work), and **root-cause the
> intermittent slow-transfer / pathological-pack behaviour** so the chosen
> mechanism gives predictable performance. Coordinate with
> [ADR-0004](0004-encoding-the-sync-state-in-git.md).
