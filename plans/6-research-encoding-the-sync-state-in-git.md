# Plan — #6: Encoding the sync state in Git

Source ADR: [ADR-0004 — Encoding the sync state in Git](../docs/adr/0004-encoding-the-sync-state-in-git.md).
Related: ADR-0005 (transfer mechanism), ADR-0007 (force-include config),
ADR-0008 (remote-worktree disposability), ADR-0002 (git-manipulation strategy),
and the prior [research/0001](../docs/research/0001-gix-git-plumbing-vs-libgit2-capability-gap.md)
(gix / `git` CLI / libgit2 capability gap analysis), whose findings constrain
which encodings are cheap to build with the chosen drivers.

This is a **research-only** ticket. The deliverable is a written investigation
report plus targeted ADR updates — **no tool code or proof-of-concept Rust is
written**. The "implementation" below is research, reasoning verified against
primary sources, and document authoring.

## Goal

Choose the object/commit/tree **encoding** of the full sync state —

1. committed history already in the repo,
2. working-tree changes (staged **and** unstaged), and
3. a deliberately **force-included** set of normally-gitignored files (large-ish
   build outputs, per-user config) —

that is built **without disturbing** the user's branch, main index, or working
tree (scratch refs / an alternate index permitted), keeps Git happy, yields
**predictable pack shapes**, and reassembles cleanly into the remote worktree.

Evaluate the three options from the ADR and recommend one:

1. **Stacked commits** (prototype approach): committed code → working-tree commit
   → forced-files commit.
2. **Separate branch / independent commit** for the extra files, exploded into
   the worktree separately on the remote.
3. **Alternate-index-based tree construction** without materialising scratch
   commits on a real branch.

## Evaluation criteria (matrix columns)

Each option is scored against:

- **Non-disturbance** — does building it leave the user's branch / main index /
  working tree untouched (using only scratch refs / an alternate index)?
- **Build cost with our drivers** — can it be synthesised natively in gix, or
  does it force a `git` CLI shell-out, per research/0001? (Tree `Editor` vs
  index-centric `update-index`/`write-tree`; commit synthesis.)
- **Keeps Git happy** — well-formed objects, digests reusable across syncs,
  nothing that confuses standard tooling.
- **Pack-shape predictability** — does the encoding tend toward stable,
  delta-friendly packs against what the server already has, or invite the
  pathological shapes flagged in ADR-0005? Focus on how large/generated
  force-included blobs sit relative to real history (mixing churny generated
  files into the code lineage vs isolating them).
- **Force-included files handling** — how cleanly the extra set layers in, and
  whether it pollutes the real code history / digests.
- **Remote reassembly** — how the remote turns the transferred objects into a
  checked-out worktree with the extra files in place, given ADR-0008's
  disposable, authoritative-overwrite worktree. Number of refs/trees to check
  out; whether a second "explode" step is needed.
- **Incrementality across syncs** — re-using the previous sync's objects/digests
  so repeat syncs stay small.

## Method

- Web / doc / source research only. Primary sources, in priority order:
  - **Git internals & plumbing man pages** — the object model (`gitformat-pack`,
    `gitrepository-layout`), `git-read-tree`, `git-update-index --index-info` /
    `--cacheinfo`, `git-write-tree`, `git-mktree`, `git-commit-tree`,
    `git-pack-objects` (delta window/depth, `--thin`), `git-checkout-index`,
    `git-worktree`, `git-symbolic-ref` / scratch-ref handling, `GIT_INDEX_FILE`.
  - **Git documentation / community references** on pack delta selection and what
    produces pathological pack shapes (object ordering, delta base reuse, large
    binary churn), to ground the pack-shape criterion.
  - **gitoxide** crate surface relevant to building each encoding (`gix` tree
    `Editor`, `gix-index`, `gix-worktree`, `gix-pack`), cross-checked against the
    already-pinned findings in research/0001 (gix 0.84.0) — reused, not re-derived.
  - Prior-art scan for tools that snapshot dirty working trees into Git objects
    without touching HEAD/index (e.g. `git stash` internals — how it builds its
    index/worktree commits; `git stash create`; `GIT_INDEX_FILE` tricks;
    jj / git-branchless-style approaches) as comparative evidence for the
    encodings.
- **Verify, don't assert:** every mechanism claim is backed by a concrete
  plumbing command or gix API symbol (or by research/0001 for gix capability).
  Where a claim can't be confirmed to reasonable confidence, label it
  **Unverified** rather than guessing.
- **Pin the snapshot:** record the date (2026-06-12) and note that gix capability
  claims are inherited from research/0001's gix 0.84.0 pin.
- Keep within the scope boundaries agreed at pre-plan (below).

## Deliverables

1. **Investigation report** at
   `docs/research/0002-encoding-the-sync-state-in-git.md`, structured:
   - Front matter: title, date, source ADR + related links, one-paragraph TL;DR.
   - **The three sync-state components** — restating what must be encoded and the
     non-disturbance constraint, with how each is obtained (HEAD tree; a snapshot
     of staged+unstaged working tree; the force-include set).
   - **Options × criteria matrix** — rows = the three encoding options; columns =
     the criteria above; each cell a short verdict with a source/justification.
   - **Per-option narrative** — for each option: how it's built (the exact
     scratch-ref / alternate-index / commit mechanics and the plumbing or gix
     calls), how the force-included files layer in, how it reassembles on the
     remote, and its pack-shape / incrementality behaviour.
   - **Recommendation** — the chosen encoding and *why*, including how the
     force-included files are layered and how the remote reassembles the result,
     plus any caveats / conditions that would change the call.
   - **Bearing on ADR-0005 / ADR-0007 / ADR-0008** — short notes (not decisions;
     those remain their own tickets).
   - **Sources** — linked references.
2. **`docs/research/README.md`** — add report 0002 to the index table.
3. **ADR-0004 update** — replace the `⚠ Research task needed` callout with a
   findings summary that links to the report and records the **recommended
   encoding**, flipping status from `proposed` toward a decision where the
   research warrants it (mirroring how research/0001 let ADR-0002 become
   `accepted`).
4. **ADR README update** — adjust the "open research tasks" line for ADR-0004 to
   point at the completed report.
5. ADR-0005 / 0007 / 0008 are touched **only** with a one-line cross-reference if
   a finding directly bears on them; their own tickets stay open and intact.

## Steps

1. Nail down how each of the three sync-state components is captured without
   touching the user's branch/index/worktree (scratch ref + `GIT_INDEX_FILE`
   alternate index; gix tree `Editor`), grounding the non-disturbance criterion.
2. Research the **mechanics of each of the three options** against primary
   sources — exact plumbing commands / gix calls to build each, including how the
   force-included files are added.
3. Research the **pack-shape** dimension: what makes packs pathological, and how
   each encoding's treatment of large/generated blobs relative to real history
   affects delta reuse and predictability. Tie back to ADR-0005's observation
   without re-doing its root-cause.
4. Research the **remote reassembly** path for each option (checkout-index /
   worktree update / second explode step), consistent with ADR-0008.
5. Fold in research/0001's gix-vs-CLI verdicts for the build cost of each option
   (especially: index-centric encoding ⇒ `git` shell-out; tree `Editor` ⇒
   native).
6. Scan prior art (git stash internals, GIT_INDEX_FILE snapshotting) as
   comparative evidence; mark anything unconfirmed **Unverified**.
7. Author the report (components + matrix + per-option narrative + recommendation
   + bearing-on notes + sources).
8. Update `docs/research/README.md`.
9. Update ADR-0004 and the ADR README; make minimal ADR-0005/0007/0008
   cross-reference notes only where warranted.
10. Self-review for internal consistency (matrix vs narrative vs recommendation)
    and that every mechanism claim carries a source or is labelled Unverified.

## Out of scope

- Writing any tool code or proof-of-concept Rust.
- The **transfer mechanism** decision and root-causing the intermittent
  slow-transfer behaviour (ADR-0005's ticket) — covered here only insofar as the
  *encoding choice* drives pack shape.
- The **configuration** of *which* files are force-included (ADR-0007's ticket) —
  this report only consumes the force-include set, it doesn't design how it's
  declared.
- Re-deriving gix capability from scratch — reuse research/0001's pinned findings.

## Acceptance

- All three encoding options are classified against every criterion, with
  sources or explicit Unverified labels.
- The report lands on a clear **recommended encoding**, covering how the
  force-included files layer in and how the remote reassembles the result.
- Report persisted under `docs/research/` with an index entry; ADR-0004's
  `⚠ Research task needed` callout resolved (with status updated where warranted)
  and the ADR README updated.
