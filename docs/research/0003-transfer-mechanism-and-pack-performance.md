# Research 0003 — Transfer mechanism & pack-performance root-cause

- Date: 2026-06-12
- Source ADR: [ADR-0005 — Transfer mechanism](../adr/0005-transfer-mechanism.md)
- Related: [ADR-0004](../adr/0004-encoding-the-sync-state-in-git.md) (encoding),
  [ADR-0003](../adr/0003-client-server-architecture.md) (client/server roles),
  [ADR-0006](../adr/0006-transport-and-connectivity.md) (localhost + SSH tunnel),
  [ADR-0008](../adr/0008-remote-worktree-disposability.md) (worktree disposable,
  object store persists), [ADR-0002](../adr/0002-git-manipulation-strategy.md)
  (gix-first + shell-out)
- Builds on [Research 0001](0001-gix-git-plumbing-vs-libgit2-capability-gap.md)
  (gix / `git` CLI / libgit2 gap, pinned to gix 0.84.0) and
  [Research 0002](0002-encoding-the-sync-state-in-git.md) (encoding the sync
  state). gix-capability claims are **inherited from Research 0001's 0.84.0
  pin**, not re-derived; re-check `crate-status.md` before relying on them.

## TL;DR

**Mechanism.** Use `git push` on the client → **`git receive-pack` on the
server**, with `git` owning pack generation and ingest. This is the only option
feasible today (Research 0001: gix has no client push, no server `accept()`, and
cannot compute new deltas) and it is also the one we'd pick on the merits — it
hands us delta compression, thin-pack completion, object quarantine, fsck and
atomic ref updates for free. A **native gix receive path is deferred**, not
chosen-against permanently (revisit triggers below).

**How our server ingests.** ADR-0003 already gives us a long-running `listen`
process that owns the localhost socket. The clean fit is for that process to
**spawn `git receive-pack <repo>` per connection and wire the tunnelled stream to
its stdio** — exactly what `sshd` and `git daemon` do internally — rather than
standing up a separate `git daemon --enable=receive-pack`. We keep one listener
(ours), full control of the invocation (target repo, env, ref-namespace
restriction, `receive.*`), and avoid `git daemon`'s anonymous-push service, which
the manual itself flags as "dangerous".

**Root-cause of the intermittent slow transfers.** The prototype's "sometimes
fast, sometimes slow" behaviour is **bimodal because delta encoding is bimodal**:
a changed build output is sent as a small delta **iff** the previous version of
that blob is (a) still present on **both** ends and (b) established as a common
base by push negotiation. When that base is available, a thin push sends a tiny
`OBJ_REF_DELTA`; when it has been pruned (scratch ref deleted, server auto-gc,
first sync after a reset), the **whole object** is sent *and* `git pack-objects`
burns CPU doing a (futile) delta search over a large binary. There is no smooth
middle — base present ⇒ fast, base absent ⇒ slow — which is precisely the
symptom. Secondary contributors (`core.bigFileThreshold`, freshly-loose-object
delta *recomputation*, already-compressed payloads, server auto-gc churn) modulate
the cost but do not create the on/off split.

**Predictability** follows directly: **guarantee the base is always present.**
Retain the previous sync's tips on both ends (the ADR-0004 lever; cheap because
ADR-0008 keeps the object store), push `--thin`, keep the receive side from
pruning bases mid-session (`receive.autogc=false` during sync windows), and for
the big-files chain prefer a **predictable whole-object send** over a variable
delta-search where the payload won't delta well anyway. That converts an
intermittent base-hit into a reliable one and turns the bimodal curve into a
predictable "only the changed bytes, deltified, cross the wire".

## 1. Transfer mechanism

### 1.1 `git push` → `git receive-pack` is the only feasible option now

Research 0001 settled the feasibility question and this report does not reopen it,
only formalises it. The native-gix smart-protocol option (ADR-0005 option 2) is
blocked on **three** simultaneous gix gaps — client push/send-pack (#306,
explicitly outscoped from 1.0 per #470), server receive-pack/`accept()` (#307),
and new-delta computation + bitmaps (#306/#2531) — none of which is close to
landing. The `git push` → `git receive-pack` option (ADR-0005 option 1) sidesteps
all three at once and is the route Research 0001 already steers toward.

Beyond feasibility, it is the right call on the decision drivers in ADR-0005
("reuse of Git's battle-tested machinery", "keep the server side simple and
robust"): `git receive-pack` gives us, for free, delta-compressed/thin pack
receipt, **object quarantine** ("objects … are placed into a temporary
'quarantine' directory … and migrated into the main object store only after the
`pre-receive` hook has completed. If the push fails … the temporary directory is
removed entirely" — [git-receive-pack](https://git-scm.com/docs/git-receive-pack)),
connectivity/fsck checks, and atomic ref updates. Re-implementing any of that
natively would be strictly worse.

**Revisit triggers** (carried from Research 0001): reconsider a native gix
transfer if gix push (#306) lands — at which point a native client send becomes a
drop-in replacement for the `git push` shell-out — or if we ever *measure* the
`git`-CLI transfer as a real bottleneck (unlikely; `git` is the performance
reference here). The server side would additionally need gix #307, which is not
started. Until then there is nothing to upstream that would change this decision.

### 1.2 How the server ingests — our `listen` process spawns `receive-pack`

ADR-0003 already specifies the server as a **long-running `listen` process** that
owns the localhost socket, with worktree-update decoupled as a separate step.
That shapes the ingest design: we do **not** need a general-purpose Git server, we
need to get a received pack into a repository's object store and move two scratch
refs. Two ways to do that with stock `git`:

- **(a) Our process spawns `git receive-pack <repo>` per connection** *(recommended)*.
  On accepting a tunnelled connection, the `listen` process forks `git
  receive-pack /path/to/repo` and connects the socket to its stdin/stdout. This is
  exactly the hand-off `sshd` performs for `ssh host git-receive-pack 'repo'` and
  that `git daemon` performs internally for its `receive-pack` service. The bytes
  on the wire are precisely the receive-pack stream — no extra framing to
  implement. We retain full control of the invocation: which repository, the
  environment, restricting writable refs to the `refs/git-full-send/*` namespace
  (ADR-0004), and `receive.*` tuning (§3). gix's role stays "synthesise objects"
  (Research 0001); the transfer leg is one `git` child process.

- **(b) `git daemon --enable=receive-pack` bound to localhost.** Functionally
  fine behind the ADR-0006 trust boundary (localhost-only + SSH tunnel), and the
  client could then use a stock `git push git://localhost:PORT/repo` URL. But it
  means running a **second** long-lived listener alongside our own `listen`
  process — duplicating the role ADR-0003 assigns to us — and enabling the one
  `git daemon` service the manual explicitly warns about: receive-pack "allow[s]
  anonymous push … there is *no* authentication in the protocol … This is solely
  meant for a closed LAN setting where everybody is friendly … **This is a
  dangerous command**" ([git-daemon](https://git-scm.com/docs/git-daemon)). The
  tunnel makes that acceptable, but it buys us nothing over (a) and adds a moving
  part.

**Recommend (a).** It is the minimal, battle-tested path and the natural fit for
the persistent-server design we already have.

### 1.3 Client side

The client side is a stock `git push` / `git send-pack` that advertises and pushes
the two scratch refs from ADR-0004 (`refs/git-full-send/code`,
`refs/git-full-send/extra`) in **one exchange** (push packs multiple refs
together). Because the transport is a raw bidirectional stream over a
manually-established SSH tunnel (ADR-0006), the simplest wiring keeps **both ends
as plain `git` processes glued to the tunnel**:

- With server option (a), use `git push` over the **`ext::` transport** (or
  equivalently `git send-pack` on a connected stream): the client-side command
  just connects stdio to the local tunnel endpoint, and the server pipes that
  straight into `git receive-pack`. No `git://` framing is needed — the stream is
  the receive-pack protocol end-to-end, mirroring `ssh host git-receive-pack`.
- With server option (b), the client uses a `git://localhost:PORT/repo` URL and
  `git daemon` does the service dispatch.

The exact wiring is an implementation detail for the build ticket; the research
conclusion is that **no custom wire protocol is warranted** — reuse `git`'s
send-pack/receive-pack stream over the tunnel.

## 2. Root-cause of the intermittent slow transfers

ADR-0005 records that transferring changed build outputs was "sometimes
surprisingly slow and sometimes as fast as expected", with pathological pack
shapes the leading suspicion. The key feature to explain is not "it is sometimes
slow" but that it is **bimodal** — two distinct regimes, not a noisy continuum.

### 2.1 The dominant cause: delta-base availability is bimodal

Git's wire size for a changed blob is dominated by whether it can be encoded as a
**delta against a base the receiver already has**. On push this works via two
mechanisms acting together:

1. **Negotiation.** `git receive-pack` advertises the server's current refs;
   `git send-pack` walks from the pushed refs and excludes everything reachable
   from those advertised refs, so the set of "objects the receiver already has" is
   exactly *what the retained scratch refs make reachable on the server*.
2. **Thin packs.** With `--thin`, `pack-objects` may "omit … the common objects
   between a sender and a receiver" and encode a new object as an `OBJ_REF_DELTA`
   whose base is **outside the pack** — i.e. a base the receiver is known to hold
   ([git-pack-objects](https://git-scm.com/docs/git-pack-objects),
   [gitformat-pack](https://git-scm.com/docs/gitformat-pack)). The server
   completes the thin pack on receipt ("after adding any missing delta bases" —
   [receive.unpackLimit](https://git-scm.com/docs/git-config), via `git index-pack
   --fix-thin`).

So a changed build output costs **one small delta** *iff* the previous version of
that blob is still present on both ends **and** surfaced as common by
negotiation. The moment that base is gone the encoding flips to **whole object**:

- The base is pruned when the previous scratch ref is deleted/rotated, when the
  server's `git maintenance run --auto` (run by default after every receive —
  [receive.autogc](https://git-scm.com/docs/git-config)) garbage-collects an
  unreferenced prior blob, or simply on the **first** sync of a chain.
- With no base, `pack-objects` sends the object whole *and* may spend CPU
  attempting deltas it cannot use — see §2.3.

This produces exactly two regimes — **base present ⇒ small delta, fast**; **base
absent ⇒ whole object, slow** — with nothing in between. That on/off character is
the signature of the prototype's symptom, and it is why Research 0002 named
delta-base availability (not commit topology or encoding) as the real lever. The
"pathological pack shape" suspicion in ADR-0005 is correct in spirit: the
pathology is a pack that re-sends whole blobs the server effectively already had,
because the base was not retained/negotiated.

### 2.2 `core.bigFileThreshold` — a ceiling, not the switch

Objects larger than `core.bigFileThreshold` (**default 512 MiB**) are "Stored
deflated in packfiles, without attempting delta compression" and "treated as if
they were labeled `binary`", and are streamed on write
([core.bigFileThreshold](https://git-scm.com/docs/git-config);
[git-pack-objects](https://git-scm.com/docs/git-pack-objects): "delta compression
is not used on objects larger than the `core.bigFileThreshold`"). For any output
above the threshold, delta encoding is **off unconditionally** — such a file is
always sent whole, predictably, regardless of base availability.

This matters two ways. First, it is a **knob**: if some build outputs are large
and known not to delta usefully, raising/lowering the threshold makes their cost
*predictable-whole* rather than subject to a variable delta search. Second, it is
*not* the bimodal driver unless outputs actually straddle 512 MiB — "large-ish"
outputs below the threshold are still in the delta-eligible regime where §2.1's
on/off behaviour dominates. Worth verifying the real output sizes against the
threshold during implementation, but the default is high enough that most web
build artifacts sit below it.

### 2.3 Freshly-loose objects force a delta *search*, not delta *reuse*

A subtle CPU contributor specific to this tool: the blobs we transfer are
**freshly synthesised loose objects** (gix `write_blob`, Research 0001), so they
carry **no pre-existing delta to reuse**. `pack-objects` distinguishes:

- **Delta reuse** — forwarding a delta already present in a source pack (cheap;
  what gix can also do, Research 0001). Not available for our just-written loose
  blobs.
- **Delta search** — computing new deltas: objects are "internally sorted by type,
  size and optionally names and compared against the other objects within
  `--window`" (default window 10, depth 50 —
  [git-pack-objects](https://git-scm.com/docs/git-pack-objects),
  [pack-heuristics](https://git-scm.com/docs/pack-heuristics)).

So every push pays a delta *search* over the changed large blobs. Whether that
search **finds** a usable base depends on whether a good candidate lands in the
window — itself a function of the size/name sort heuristic. When a base is
present and lands in-window, the search succeeds cheaply; when the base is absent
(§2.1) the search runs and **fails**, paying CPU for nothing before sending the
object whole. This amplifies the slow regime's cost (CPU on top of bytes) and is
corroborated by community reports of large/binary pushes pinning CPU in delta
resolution
([repo-discuss: "Pushing files with large delta … increases CPU usage"](https://groups.google.com/g/repo-discuss/c/zQ5aAxq0Ufg),
[Git, Compression, and Deltas](https://gist.github.com/matthewmccullough/2695758)).

### 2.4 Already-compressed payloads and server-side churn

Two further modulators, neither bimodal on its own:

- **Already-compressed outputs** (minified/gzipped JS/CSS, images, binaries)
  delta and zlib-recompress poorly: even with a base, the delta may be large, and
  the per-push zlib pass is wasted work. This raises the *floor* cost and narrows
  the fast/slow gap but does not create the split.
- **Server auto-gc churn.** `git maintenance run --auto` after each receive
  ([receive.autogc](https://git-scm.com/docs/git-config)) can repack and, if a
  prior blob is no longer referenced, **prune the very base** a future push needs —
  feeding back into §2.1 — or introduce a latency spike on an otherwise fast sync.
  `receive.unpackLimit`/`transfer.unpackLimit` (default 100 — [git-config](https://git-scm.com/docs/git-config))
  also decides loose-vs-keep-pack on receipt; our pushes are small in object
  *count*, so they unpack to loose by default, which makes those objects ordinary
  gc candidates unless a ref keeps them alive.

### 2.5 Ruled out / minor

- **Missing reachability bitmaps** (gix gap, Research 0001) accelerate *object
  counting* for large histories; here the object count per sync is tiny, so this
  is negligible — and `git` (not gix) owns the pack anyway.
- **Commit/tree topology** — Research 0002 already showed the bytes that move are
  the changed blobs, identical across encodings; arrangement changes only a few
  tiny objects. Not a factor in the slowdown.

### 2.6 Candidate matrix

Legend: **■ bimodal driver** · **□ cost modulator** · **· negligible here**.

| Candidate | Bimodal? | Effect on transfer | Lever |
| --- | --- | --- | --- |
| Delta-base availability + negotiation (§2.1) | ■ | base present → small delta; absent → whole object | **ref retention**, `--thin` |
| Freshly-loose delta *search* (§2.3) | □ (amplifies slow regime) | CPU per push; wasted when no base | `--window`/`--depth`, base retention |
| `core.bigFileThreshold` (§2.2) | · (a ceiling) | above 512 MiB always whole | threshold; treat as predictable-whole |
| Already-compressed payloads (§2.4) | · | raises floor; poor delta/zlib | accept whole-object for big chain |
| Server auto-gc / unpackLimit churn (§2.4) | □ (can *cause* §2.1) | prunes bases; latency spikes | `receive.autogc=false` in sync window |
| Missing bitmaps / topology (§2.5) | · | negligible at this object count | — |

## 3. Predictability: how the chosen mechanism + tuning fix it

The fix is to **remove the variance at its source — guarantee the delta base is
always present and negotiated** — and then tune `pack-objects` so the remaining
cost is bounded and known.

1. **Retain the previous sync's tips on both ends** *(the #1 lever)*. Keep
   `refs/git-full-send/code@{prev}` and `…/extra@{prev}` reachable on client and
   server between syncs. ADR-0004 already recommends this and ADR-0008 makes it
   cheap: only the **worktree** is disposable; the **object store persists**, so
   retained tips and their (large) blobs survive as delta bases and as
   negotiation common-base. This is what converts §2.1's intermittent base-hit
   into a **reliable** one. Option 2's dedicated `extra` chain (ADR-0004) makes
   the retention explicit per chain.
2. **Push `--thin`.** Required for the `OBJ_REF_DELTA`-against-remote-base
   encoding that retention enables; the server completes the pack with `index-pack
   --fix-thin` on receipt. Without `--thin`, retention buys nothing on the wire.
3. **Keep the receive side from pruning bases mid-session.** Set
   `receive.autogc=false` (or otherwise control maintenance timing) on the server
   repo during sync windows so a post-receive gc cannot prune the base the next
   push needs (§2.4). Run maintenance deliberately, outside the hot path, and
   never on objects still referenced by a retained tip.
4. **Tune the delta search to the payload, and accept predictable whole-object
   sends for the big-files chain.** For the volatile large build outputs, a
   variable delta *search* that often fails to beat the floor (§2.3–2.4) trades
   worst-case CPU spikes for marginal byte savings. Where outputs don't delta well
   (already-compressed/binary), prefer **predictable whole-object cost** — e.g. via
   `core.bigFileThreshold` / the `delta` attribute — over an intermittent search.
   For the code chain (text, deltas well), keep the defaults (`--window=10
   --depth=50`) and `--delta-base-offset` for a few percent smaller packs.

**Expected steady state.** After the first sync of a chain (an unavoidable
one-off full send), each subsequent sync moves **only the changed working-tree
delta and the changed-output deltas**, deltified against the retained prior tip,
at predictable, bounded cost. The only situations that legitimately fall back to a
full send become explicit and understood: the first sync, or a deliberate
base reset — never the previously-mysterious "sometimes slow".

## 4. Bearing on ADRs

- **ADR-0005 (source).** Resolve the `⚠ Research task needed` callout and move
  `proposed → accepted`. **Decision:** `git push` → `git receive-pack`, with the
  server's `listen` process spawning `receive-pack` per connection (§1.2(a)); `git`
  owns pack generation and ingest; native gix transfer deferred with the §1.1
  revisit triggers. **Performance:** the intermittent slowness is delta-base
  availability flipping on/off (§2); predictability comes from base retention +
  `--thin` + controlled server gc + payload-appropriate delta policy (§3).
- **ADR-0004 / Research 0002 (encoding).** This report *consumes* the
  ref-retention recommendation as the central predictability lever and confirms
  the encoding's job is only to make a base reliably available; nothing in ADR-0004
  is reopened.
- **ADR-0002 (git strategy).** Reinforces the gix-first + shell-out boundary:
  synthesise objects in gix, hand the entire pack-and-transfer leg to one `git`
  child process per side.
- **ADR-0003 (client/server).** The server's `listen` role is the connection
  acceptor that forks `receive-pack`; worktree-update stays the separate step
  (ADR-0008 / Research 0002).
- **ADR-0006 (transport).** The localhost-only + SSH-tunnel trust boundary is what
  makes the unauthenticated receive-pack stream safe and lets us avoid building
  any in-tool auth; it also makes `git daemon`'s anonymous-push risk moot — but we
  still prefer spawning `receive-pack` ourselves (§1.2).
- **ADR-0008 (disposable worktree).** The persisting object store is the
  precondition for ref retention (§3.1); only the worktree is thrown away.

## 5. Caveats / unverified

- This is a **reasoning + documentation** investigation, not an empirical
  reproduction (the ticket scopes out code). The delta-base-availability
  root-cause is strongly supported by Git's documented mechanics and corroborating
  community reports, but the prototype's exact slow instances were not re-run. A
  small **follow-up benchmark** — sync a changed build output with vs. without the
  prior tip retained, measuring pack size and `pack-objects` CPU — would confirm
  the bimodal split directly and is recommended before locking in the tuning
  numbers. Filed as a candidate follow-up rather than done here.
- **Real output sizes vs. `core.bigFileThreshold` (512 MiB)** were not measured;
  whether any force-included outputs cross the threshold (§2.2) should be checked
  against the actual `extra` set (ADR-0007) when configuring the tool.
- gix-capability claims are inherited from Research 0001's gix 0.84.0 pin;
  gitoxide moves fast — re-check `crate-status.md` and #306/#307 before relying on
  a gap being open (it would only *widen* the set of options, not change today's
  recommendation).

## Sources

Primary documentation (fetched 2026-06-12):

- [git-receive-pack](https://git-scm.com/docs/git-receive-pack) — server end of
  push; object **quarantine** and migrate-on-success; pre/post-receive hooks;
  `receive.*`.
- [git-daemon](https://git-scm.com/docs/git-daemon) — service enabling
  (`--enable=receive-pack`), localhost `--listen`, and the explicit
  anonymous-push **security warning** ("This is a dangerous command").
- [git-pack-objects](https://git-scm.com/docs/git-pack-objects) — delta selection
  (sort by type/size/name, `--window` 10 / `--depth` 50), `--thin`,
  `--delta-base-offset`, `--reuse-delta`/`--reuse-object`, and "delta compression
  is not used on objects larger than `core.bigFileThreshold`".
- [gitformat-pack](https://git-scm.com/docs/gitformat-pack) — `OBJ_OFS_DELTA` vs
  `OBJ_REF_DELTA`, thin-pack external bases.
- [pack-heuristics](https://git-scm.com/docs/pack-heuristics) — the size/name
  windowing heuristic behind whether a good base lands in-window.
- git-config (source `Documentation/config/{core,receive,transfer}.adoc`) —
  `core.bigFileThreshold` (**default 512 MiB**, no delta, streamed, treated as
  binary); `receive.unpackLimit` (below limit → loose, at/above → keep pack
  "after adding any missing delta bases"); `receive.autogc` (default runs `git
  maintenance run --auto` after receive); `transfer.unpackLimit` (**default 100**).
  <https://git-scm.com/docs/git-config>
- [Research 0001](0001-gix-git-plumbing-vs-libgit2-capability-gap.md) — gix lacks
  push (#306), server `accept()` (#307), new-delta + bitmaps (#306/#2531); `git`
  owns pack-and-transfer.
- [Research 0002](0002-encoding-the-sync-state-in-git.md) — encoding cooperates via
  ref retention; root-cause + `pack-objects` tuning handed to this ticket.

Corroborating community reports:

- [Git, Compression, and Deltas — an explanation](https://gist.github.com/matthewmccullough/2695758)
  — loose objects are stored non-delta; delta work happens at pack time.
- [repo-discuss: pushing files with large delta increases CPU usage](https://groups.google.com/g/repo-discuss/c/zQ5aAxq0Ufg)
  — large/binary pushes pinning CPU in delta resolution.
