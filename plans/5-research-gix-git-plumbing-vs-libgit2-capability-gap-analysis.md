# Plan — #5: gix / `git` plumbing vs libgit2 capability gap analysis

Source ADR: [ADR-0002 — Git manipulation strategy](../docs/adr/0002-git-manipulation-strategy.md).
Related: ADR-0004 (sync-state encoding), ADR-0005 (transfer mechanism).

This is a **research-only** ticket. The deliverable is a written gap analysis plus
targeted ADR updates — **no tool code is written**. The "implementation" below is
research, verification of findings against primary sources, and document authoring.

## Goal

For the four operations this tool needs, determine for each of the three drivers
(**gix** / **`git` plumbing CLI** / **libgit2**) whether the capability is:
**native**, **only via shelling out to `git`**, or **missing in both** (gix + CLI).
Then identify the gaps that force a shell-out and flag which are worth closing
**upstream in gitoxide**.

The four operations:

1. **Object / tree synthesis** — synthesising blobs, trees, and commits from
   working-tree state (drives ADR-0004).
2. **Alternate index construction** — building trees via an alternate index
   without disturbing the user's index / branch / worktree (ADR-0004 option 3).
3. **Pack generation** — producing packfiles, with attention to delta selection /
   pack-shape control (the ADR-0005 "pathological pack shape" / intermittent-slow
   transfer concern).
4. **send / receive-pack** — the smart protocol, informing the
   native-gix-vs-`git-daemon` transfer decision (ADR-0005).

## Method

- Web/doc/source research only. Primary sources, in priority order:
  - gitoxide repository (crate boundaries: `gix`, `gix-object`, `gix-index`,
    `gix-pack`, `gix-protocol`, `gix-transport`, `gix-worktree`, …), its READMEs,
    `CHANGELOG`s, and any roadmap / `cratesio-status` style status docs.
  - **gitoxide issue tracker** (and linked PRs / project boards) — for each gap,
    check whether the feature is already planned, has an open tracking issue, or
    is actively being worked on. Record the issue/PR number and its state
    (open / in-progress / merged-but-unreleased) so the upstream-candidates
    section reflects real upstream activity rather than a cold assessment.
  - `gix` and component-crate docs on docs.rs.
  - `git` plumbing man pages (`git-hash-object`, `git-mktree`,
    `git-commit-tree`, `git-update-index --index-info`, `git-read-tree`,
    `git-pack-objects`, `git-send-pack`, `git-receive-pack`, `git-daemon`).
  - libgit2 API reference (the `git_*` C API: `git_index`, `git_treebuilder`,
    `git_packbuilder`, `git_odb`, the `git_smart`/transport surface).
- **Pin the snapshot:** record the exact gix version(s) assessed and the date
  (today, 2026-06-12). Note that gix moves fast and the analysis is a dated
  snapshot.
- **Verify, don't assert:** every "native in gix" claim is backed by a concrete
  crate + API symbol (or a tracking issue when partial). Distinguish "exists and
  is public/stable" from "exists internally / behind a feature / planned". For
  the CLI column, name the specific plumbing command. For libgit2, name the
  specific API. Where I cannot confirm support to a reasonable confidence, label
  it **Unverified** rather than guessing.

## Deliverables

1. **Investigation report** at
   `docs/research/0001-gix-git-plumbing-vs-libgit2-capability-gap.md`, structured:
   - Front matter: title, date, pinned gix version(s), one-paragraph TL;DR.
   - **Capability matrix** — rows = the four operations (broken into concrete
     sub-capabilities where useful, e.g. blob vs tree vs commit; in-memory vs
     on-disk index; thin packs / delta reuse; protocol v2; server vs client side);
     columns = gix / `git` CLI / libgit2; each cell native / shell-out / missing
     with a source link.
   - **Per-operation narrative** — what gix offers today, where the CLI is the
     pragmatic path, what libgit2 does as the mature reference, and the resulting
     recommended approach for this tool.
   - **Gaps forcing a shell-out** — consolidated list.
   - **Upstream candidates** — which gaps are worth closing in gitoxide, with a
     rough sense of effort, **the state of any existing gitoxide issue/PR**
     (planned / in-progress / none), and which are better left as permanent
     shell-outs.
   - **Bearing on ADR-0004 / ADR-0005** — short notes (not decisions; those
     remain their own tickets).
   - **Sources** — linked references.
2. **`docs/research/README.md`** — short index mirroring the ADR README,
   listing this report.
3. **ADR-0002 update** — replace the `⚠ Research task needed` callout in the
   Research section with a findings summary that links to the report.
4. **ADR README update** — adjust the "Open research tasks" line for ADR-0002 to
   point at the completed report.
5. ADR-0004 / ADR-0005 are touched **only** if a finding directly bears on them
   (e.g. a confirmed gap forcing a shell-out); their own research tickets stay
   open and intact.

## Steps

1. Enumerate the concrete sub-capabilities under each of the four operations so
   the matrix rows are precise.
2. Research the **gix** column against primary sources; record crate + API
   symbols and version. Capture partial/feature-gated/planned states. For each
   gap or partial, search the **gitoxide issue tracker / PRs** and record whether
   it is planned, in-progress, or untracked (with issue/PR numbers and state).
3. Research the **`git` plumbing CLI** column; name the exact commands.
4. Research the **libgit2** column as the mature-reference baseline.
5. Cross-check ambiguous findings against a second source; mark anything
   unconfirmed as **Unverified**.
6. Author the report (matrix + narrative + gaps + upstream candidates + sources).
7. Write `docs/research/README.md`.
8. Update ADR-0002 and the ADR README; make minimal ADR-0004/0005 notes only
   where warranted.
9. Self-review for internal consistency (matrix vs narrative vs gap list) and
   that every capability claim carries a source.

## Out of scope

- Writing any tool code or proof-of-concept Rust.
- Making the ADR-0004 / ADR-0005 decisions (separate tickets).
- Root-causing the intermittent slow-transfer behaviour beyond noting how
  pack-generation control bears on it (that root-cause is ADR-0005's ticket).

## Acceptance

- Every required operation is classified for all three drivers with sources.
- Shell-out-forcing gaps are listed, and upstream-worthy gaps are called out.
- Report persisted under `docs/research/` with an index; ADR-0002 callout
  resolved and the ADR README updated.
</content>
</invoke>
