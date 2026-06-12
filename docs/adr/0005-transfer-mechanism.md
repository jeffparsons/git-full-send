# ADR-0005 — Transfer mechanism

- Status: accepted
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

## Considered options

1. **`git push` → server `git receive-pack`.** The server accepts the localhost
   connection and hands the stream to `git receive-pack`. Maximum reuse of
   battle-tested Git machinery (delta compression, thin-pack completion, object
   quarantine, fsck, atomic ref updates).
2. **Native gix smart-protocol implementation.** If gitoxide has — or can
   feasibly gain — enough of the send/receive-pack protocol, the server handles
   the transfer itself. More control, fewer moving parts at runtime, but depends
   on gix capabilities ([ADR-0002](0002-git-manipulation-strategy.md)).

## Decision

Adopt **Option 1**: the client runs `git push` and the server ingests with
**`git receive-pack`**; `git` owns pack generation and ingest.

- **Server ingest.** The long-running `listen` process
  ([ADR-0003](0003-client-server-architecture.md)) **spawns `git receive-pack
  <repo>` per connection** and wires the tunnelled stream to its stdio — the same
  hand-off `sshd` and `git daemon` perform internally. We keep a single listener
  (ours), full control of the invocation (target repo, environment, restricting
  writable refs to the `refs/git-full-send/*` namespace, `receive.*` tuning), and
  avoid running a separate `git daemon --enable=receive-pack` — the one `git
  daemon` service its own manual flags as "dangerous" (anonymous push).
- **Client.** A stock `git push` / `git send-pack` advertising and pushing the two
  scratch refs ([ADR-0004](0004-encoding-the-sync-state-in-git.md)) in one
  exchange, wired to the SSH tunnel ([ADR-0006](0006-transport-and-connectivity.md))
  as a raw receive-pack stream (e.g. via the `ext::` transport). No custom wire
  protocol.
- **Native gix transfer is deferred, not rejected.** It is currently blocked on
  three simultaneous gix gaps — client push (#306, outscoped from 1.0), server
  `accept()` (#307), and new-delta computation (#306/#2531). Revisit if gix push
  lands (a native client send becomes a drop-in for the `git push` shell-out) or
  if the `git`-CLI transfer is ever measured as a real bottleneck (unlikely;
  `git` is the performance reference).

This confirms [Research 0001](../research/0001-gix-git-plumbing-vs-libgit2-capability-gap.md)'s
steer and the gix-first-with-shell-out posture of
[ADR-0002](0002-git-manipulation-strategy.md): synthesise objects natively in gix,
hand the whole pack-and-transfer leg to one `git` child process per side.

## Performance: the intermittent slow transfers, explained

In an earlier prototype, transferring changed build outputs was **sometimes
surprisingly slow and sometimes as fast as expected**, with pathological pack
shapes the leading suspicion. The root cause is that **delta encoding is bimodal**:
a changed output is sent as a small delta **iff** the previous version of that
blob is still present on **both** ends *and* established as a common base by push
negotiation. When that base is available a `--thin` push sends a tiny
`OBJ_REF_DELTA`; when it has been pruned (scratch ref deleted, server auto-gc, or
the first sync of a chain) the **whole object** is sent and `git pack-objects`
also burns CPU on a futile delta search. Base present ⇒ fast; base absent ⇒ slow,
with nothing in between — exactly the observed symptom. (`core.bigFileThreshold`,
freshly-loose-object delta recomputation, and already-compressed payloads modulate
the cost but do not create the on/off split.)

**Predictable performance** therefore comes from guaranteeing the base is always
available:

- **Retain the previous sync's tips on both ends** (the
  [ADR-0004](0004-encoding-the-sync-state-in-git.md) lever; cheap because
  [ADR-0008](0008-remote-worktree-disposability.md) keeps the object store) so
  every push has a delta base and a negotiation common-base.
- **Push `--thin`** so retention actually shows up as a small delta on the wire.
- **Keep the receive side from pruning bases mid-session** (`receive.autogc=false`
  during sync windows; run maintenance deliberately, outside the hot path).
- **Match delta policy to the payload**: for the volatile big-files chain, prefer
  a predictable whole-object send over a variable delta search where the payload
  won't delta well anyway; keep delta defaults for the code chain.

After an unavoidable one-off first sync, each subsequent sync then moves only the
changed bytes, deltified, at bounded and predictable cost.

## Consequences

- The transfer leg is a single `git` subprocess on each side; gix's role stays
  object synthesis ([Research 0001](../research/0001-gix-git-plumbing-vs-libgit2-capability-gap.md)).
- The server's `listen` process forks `git receive-pack` per connection and must
  confine writable refs to the `refs/git-full-send/*` namespace.
- Predictability depends on **ref retention on both ends** and controlled
  server-side gc; operators/tooling must not prune the retained tips between syncs.
- The SSH tunnel ([ADR-0006](0006-transport-and-connectivity.md)) remains the
  trust boundary; the receive-pack stream carries no auth of its own.

## Status

Accepted. Full analysis in
[Research 0003 — Transfer mechanism & pack-performance root-cause](../research/0003-transfer-mechanism-and-pack-performance.md)
(2026-06-12), which also closes the root-cause investigation flagged here and in
[ADR-0004](0004-encoding-the-sync-state-in-git.md).
