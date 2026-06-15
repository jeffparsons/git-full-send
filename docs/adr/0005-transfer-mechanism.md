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
- **Client.** A stock `git push` / `git send-pack` advertising and pushing the
  scratch refs ([ADR-0004](0004-encoding-the-sync-state-in-git.md)), wired to the
  SSH tunnel ([ADR-0006](0006-transport-and-connectivity.md)) as a raw
  receive-pack stream (e.g. via the `ext::` transport). No custom wire protocol.
  The two scratch refs travel in **one exchange per chain** rather than a single
  combined push, because a `git push` applies one delta policy to its whole pack
  and the chains want different policies (see "Per-chain delta policy" below).
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
  won't delta well anyway; keep delta defaults for the code chain. See "Per-chain
  delta policy" below for how this is implemented.

After an unavoidable one-off first sync, each subsequent sync then moves only the
changed bytes, deltified, at bounded and predictable cost.

## Per-chain delta policy

A single `git push` applies **one** delta policy to its whole pack (`--thin`,
`pack.window`/`pack.depth` are per-invocation, not per-ref), so the two chains
cannot share one push and still get different policies. The client therefore
pushes **each chain in its own exchange** (issue #50):

- **`code` → `--thin`.** Thin deltas against the retained base — the code chain
  deltas well, so this is the cheap, small-on-the-wire path.
- **`extra` → `--no-thin -c pack.window=0`.** `pack.window=0` disables the delta
  search entirely for a predictable whole-object send; `--no-thin` keeps git from
  emitting thin deltas against bases outside the pack. Because the `extra` commit
  is parented on the retained `sent_extra` tip, push negotiation still excludes
  objects the server already holds, so only the *changed* objects travel — just
  whole rather than thin-deltified. This trades a futile delta search (and the
  bimodal-performance symptom above) for bounded, predictable cost on a chain of
  big build outputs that won't delta well.

Each chain's retained tip is advanced only after its own push succeeds, so the two
chains fail independently — a `code` success is not lost if the `extra` push fails.

## Consequences

- The transfer leg is a `git` subprocess on each side; gix's role stays object
  synthesis ([Research 0001](../research/0001-gix-git-plumbing-vs-libgit2-capability-gap.md)).
- A sync now runs **two** receive-pack exchanges (one per chain), so two `git
  push`/`receive-pack` subprocess pairs and two tunnel connections per sync — a
  little more latency than a single combined push, bought for the per-chain delta
  policy and predictable `extra` transfer. The per-chain push times are recorded
  separately ([ADR-0013](0013-recording-operation-metrics.md)).
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
