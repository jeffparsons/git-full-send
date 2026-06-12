# Plan — #7 Research: transfer mechanism + pack-performance root-cause

- Source ADR: [ADR-0005 — Transfer mechanism](../docs/adr/0005-transfer-mechanism.md) (`proposed`)
- Builds on: [Research 0001](../docs/research/0001-gix-git-plumbing-vs-libgit2-capability-gap.md)
  (gix/`git`/libgit2 gap), [Research 0002](../docs/research/0002-encoding-the-sync-state-in-git.md)
  (encoding) / [ADR-0004](../docs/adr/0004-encoding-the-sync-state-in-git.md)
- Context ADRs: [0002](../docs/adr/0002-git-manipulation-strategy.md) (gix-first + shell-out),
  [0003](../docs/adr/0003-client-server-architecture.md) (server is a long-running `listen` process),
  [0006](../docs/adr/0006-transport-and-connectivity.md) (localhost-only + manual SSH tunnel),
  [0008](../docs/adr/0008-remote-worktree-disposability.md) (worktree disposable, **object store persists**)

## Objective

Resolve the two open questions ADR-0005 defers, **building on** (not re-deriving)
the prior reports:

1. **Recommend a transfer mechanism** — `git push` → server `receive-pack`/`git daemon`
   vs. a native gix receive path — with rationale, and pin down **how our
   server process ingests** the pushed objects.
2. **Root-cause the intermittent slow-transfer / pathological-pack behaviour**
   observed in the prototype, and show how the recommended mechanism + tuning
   gives **predictable** performance.

This is a **research + first-principles-reasoning** investigation (consistent
with how Reports 0001/0002 were done and with the ticket's "no code is written
in this ticket"). No benchmark harness is built; where empirical reproduction
would strengthen a conclusion, it is flagged as a follow-up, not coded here.

## What's already settled (inherited, cited, not re-litigated)

- **Mechanism feasibility (Research 0001).** gix lacks client push (#306,
  outscoped from 1.0), server receive-pack/`accept()` (#307), and new-delta
  computation + bitmaps (#306/#2531). These collapse to one boundary: **let
  `git` own pack-and-transfer**. A native gix transfer is not viable in the 0.84
  timeframe; not worth blocking to upstream now. → This report **confirms and
  formalises** that into ADR-0005's decision; it does not reopen it.
- **Encoding cooperation (Research 0002 / ADR-0004).** The encoding's only lever
  on pack shape is **delta-base availability**, fixed by **retaining the previous
  sync's tips on both ends**; the *root-cause analysis and `pack-objects` tuning*
  were explicitly handed to **this** ticket.
- **Trust boundary (ADR-0006).** localhost-only + manual SSH tunnel ⇒ no in-tool
  auth/encryption to design; the receive path only has to be safe on localhost
  behind the tunnel.

## Deliverables

1. **New report** `docs/research/0003-transfer-mechanism-and-pack-performance.md`
   (dated 2026-06-12; same house style/format as 0001/0002 — TL;DR, body,
   "Bearing on ADRs", "Sources", caveats).
2. **`docs/research/README.md`** — add the index row for report 0003.
3. **Update `docs/adr/0005-transfer-mechanism.md`** — move `proposed → accepted`;
   fill in **Decision**, **Consequences**, and a **performance explanation**
   section; replace the `⚠ Research task needed` callout with a link to report
   0003 (mirroring how ADR-0004 was finalised by Research 0002).
4. **`docs/adr/README.md`** — flip ADR-0005 status to `accepted` in the table and
   strike through its "Open research tasks" bullet (as done for 0001/0002).

## Research questions to resolve

### Part A — Transfer mechanism

- A1. Confirm `git push` → `git receive-pack` is the only feasible mechanism now
  (re-anchor on Research 0001; note revisit triggers — gix #306 push landing,
  or a measured `git`-CLI bottleneck).
- A2. **How the server ingests**, given ADR-0003's server is *our own
  long-running `listen` process* (not sshd):
  - Option (a) our `listen` process **spawns `git receive-pack <repo>` per
    connection**, wiring the tunnelled stream to its stdio (exactly what sshd /
    `git daemon` do internally) — minimal moving parts, no separate daemon.
  - Option (b) run **`git daemon --enable=receive-pack`** bound to localhost.
  - Compare on: simplicity/robustness (ADR-0005 driver), control over the
    invocation (env, hooks, `--fix-thin`, quarantine), and fit with the existing
    persistent-server design. Recommend one (leaning (a) on current reading;
    validate during research).
- A3. Client side: `git push`/`git send-pack` invocation shape over the tunnel
  (transport choice — `ext::`/`git://`/ssh-style stream), and which refs are
  advertised/pushed (the `code` + `extra` scratch refs from ADR-0004).
- A4. Server ingest mechanics: thin-pack completion (`git index-pack
  --stdin --fix-thin`), receive quarantine, `receive.unpackLimit`
  (loose vs keep-pack), and `receive.denyCurrentBranch`/scratch-ref namespace
  considerations.

### Part B — Root-cause of intermittent slow transfers ("sometimes fast, sometimes slow")

Enumerate and weigh candidate mechanisms; identify the dominant one(s) and the
conditions that flip the prototype between fast and slow:

- B1. **Delta-base availability (leading hypothesis, per Research 0002).** A thin
  push encodes a changed build output as an `OBJ_REF_DELTA` against the *previous*
  output **only if** that blob still exists on both ends **and** negotiation
  establishes it as common. Pruned scratch refs / server auto-gc / first sync /
  ref rotation ⇒ no base ⇒ whole-object send. Explain the bimodality this
  produces.
- B2. **`core.bigFileThreshold`** (default 512 MiB): objects above it are never
  deltified (and streamed). Determine whether "large-ish build outputs" plausibly
  cross it; treat as a ceiling on delta benefit and a knob, not necessarily the
  bimodal driver.
- B3. **Loose-object delta *recomputation* cost.** Our synthesised blobs are
  freshly written **loose** objects with no pre-existing delta to reuse, so
  `pack-objects` must (re)compute deltas within `--window` over large binaries —
  CPU that varies with whether a good base lands in the window (sort by
  type/size/name). Distinguish *delta reuse* (cheap) from *delta search* (costly).
- B4. **Negotiation / `have` determination** on push: how `receive-pack`'s ref
  advertisement and the client's `--thin` decision determine the common base set.
- B5. **Server-side churn:** `receive.autogc` / auto-`gc` repacking between syncs
  changing delta layout or causing latency spikes; interaction with retained tips.
- B6. **zlib recompression of already-compressed outputs** (minified/gzipped web
  assets, binaries): poor delta + zlib ratios, baseline CPU; bimodal only weakly.
- B7. Minor/ruled-out: missing bitmaps (object counts are small here), MIDX.

### Part C — Predictability recommendations

Translate the root-cause into concrete, predictable-by-design choices:

- C1. **Retain prior tips on both ends** (encoding already recommends this;
  restate as the #1 transfer lever) so a delta base is *always* available — turns
  intermittent-fast into reliably-fast.
- C2. **`pack-objects` / push tuning:** `--thin` (required for REF_DELTA against
  remote bases), `--window`/`--depth` (`pack.window`/`pack.depth`),
  `--delta-base-offset`, and the predictability trade-off raised in Research 0002:
  for the **big-files chain**, consider *whole-object sends for predictable cost*
  vs. delta-search CPU spikes — quantify the trade qualitatively.
- C3. **Server hygiene:** disable receive auto-gc during sync windows
  (`receive.autogc=false`) / control repack timing so bases aren't pruned
  mid-session; keep retained scratch refs reachable.
- C4. State the **expected steady-state behaviour** (only the changed working-tree
  delta + changed-output deltas cross the wire; predictable, bounded cost) and the
  conditions that would still cause a one-off full send (first sync, deliberate
  base reset).

## Report outline (`0003-…md`)

1. Header (date, source ADR, related, "builds on 0001/0002", gix-pin caveat).
2. **TL;DR** — recommended mechanism (one line) + the root-cause in one paragraph
   (delta-base availability is the bimodal driver; predictability = base retention
   + tuning) + the server-ingest recommendation.
3. **Transfer mechanism** — `git push` → `receive-pack`; the two server-ingest
   options and the recommendation; why native gix is deferred (revisit triggers).
4. **Root-cause of the intermittent slow transfers** — the candidate matrix
   (B1–B7), the dominant cause, and *why the prototype was bimodal*.
5. **Predictability: how the chosen mechanism + tuning fix it** (C1–C4).
6. **Bearing on ADRs** — 0005 (finalise), 0004 (consumes ref-retention),
   0002 (validates shell-out boundary), 0006 (trust boundary), 0008 (persisting
   object store enables retention).
7. **Sources** + **Caveats / unverified**.

## Sources to consult (web research encouraged)

- Git docs: `gitformat-pack` (OFS/REF deltas, thin packs, bitmaps),
  `git-pack-objects` (`--window`/`--depth`/`--thin`/`--delta-base-offset`,
  `bigFileThreshold`), `git-receive-pack` (quarantine, hooks, `receive.*`),
  `git-index-pack` (`--fix-thin`), `git-daemon` (`--enable=receive-pack`),
  `git-send-pack`/`git-push`, `git-repack`/`git-gc` (auto-gc), `git-config`
  (`pack.*`, `receive.*`, `transfer.*`, `core.bigFileThreshold`).
- Pro Git / packfile internals; credible community write-ups on slow pushes with
  large/binary files and delta-base behaviour (reuse the corroborating sources
  already cited in Research 0002 where apt).
- Re-anchor gix/feasibility claims on Research 0001 rather than re-fetching.

## Out of scope

- Writing or running any code / benchmark harness (ticket says no code).
- Re-opening the encoding decision (ADR-0004 / Research 0002) or the force-include
  configuration (ADR-0007).
- Designing in-tool auth/encryption (settled by ADR-0006).
- The remote worktree-update/checkout step (ADR-0008 / Research 0002).

## Acceptance criteria

- [ ] `docs/research/0003-transfer-mechanism-and-pack-performance.md` exists,
      matching house style, answering Parts A–C with citations.
- [ ] A clear **recommended transfer mechanism + server-ingest approach** with
      rationale.
- [ ] A clear **explanation of the performance variability** and how the choice
      yields predictable performance.
- [ ] ADR-0005 updated to `accepted` with Decision/Consequences/performance
      section and a link to report 0003; research callout removed.
- [ ] Both READMEs (research + adr) updated; ADR-0005 row/bullet reflects done.
- [ ] Explicitly references and reuses Research 0001 & 0002 rather than
      duplicating them.
