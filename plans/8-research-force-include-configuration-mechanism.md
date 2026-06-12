# Plan — #8 Research: force-include configuration mechanism

Source ADR: [ADR-0007 — Syncing extra (normally-gitignored) files](../docs/adr/0007-syncing-extra-gitignored-files.md)

## Goal

Design the configuration mechanism for the force-include set and record the
decision. This is a **research-and-write-up ticket — no production code.** The
concrete outputs are:

1. A new investigation report `docs/research/0004-force-include-configuration-mechanism.md`.
2. An update to **ADR-0007** that resolves its `⚠ Research task needed` callout,
   moves it off bare `proposed`, and cross-links the report and the affected
   ADRs (0004, 0008).
3. An entry for report 0004 in `docs/research/README.md`.

## What is already settled (do not re-open)

- **ADR-0004 / Research 0002** decided *how the force-included files travel and
  land*: captured as a **separate `extra` tree/commit** on its own retained
  delta-base chain, reassembled on the remote as an **authoritative overlay**
  (`checkout-index` the `code` tree, then explode `extra` over it). Research 0002
  explicitly defers to this ticket: *which* files are included and *what paths
  they map to*.
- **ADR-0008**: remote worktree is disposable / overwrite-authoritative, so the
  overlay needs no merge logic.
- **ADR-0002 / Research 0001**: gix-first-with-shell-out posture.

This ticket owns the **selection + declaration** half: where the set is
declared, granularity, scope, and the path-mapping into the worktree.

## Questions the report must answer (from the ADR-0007 callout)

1. **Where it is declared.** Committed-in-repo project config vs. per-user file
   vs. both; the file format and location (dedicated include file, gitignore-style
   negation file, a section in a project config such as TOML). Decide a default.
2. **Granularity.** Globs vs. explicit paths; and if globs, the pattern semantics
   — anchoring, recursion (`**`), negation/re-exclusion, ordering/precedence, and
   how directories expand.
3. **Scope.** Per-project vs. per-user, and **how the two layers compose** (e.g.
   project declares build outputs; user adds personal config) — precedence and
   merge order between layers.
4. **How selected files are placed** into the remote worktree. Confirm/refine the
   Research-0002 overlay; pin down path-mapping (same relative path vs. fixed
   subdir prefix), behaviour for client-side deletions of force-included files,
   and the mechanics of overriding gitignore during selection.

## Approach

### Step 1 — Survey prior art (web research)

Pattern-language and include/exclude designs in comparable tools, focusing on
what semantics they chose and why:

- **Git's own**: `.gitignore` negation (`!`) semantics and limits (can't
  re-include a file if a parent dir is excluded), `git add -f`, `git check-ignore`,
  `core.excludesFile` (per-user) vs. `.gitignore` (per-project) vs.
  `.git/info/exclude` (per-clone) — the canonical three-layer scope model.
- **Sparse-checkout** cone vs. non-cone pattern semantics (a Git feature that is
  *exactly* "select a subset of paths to materialise").
- **`.gitattributes`** pattern matching + per-user vs. in-tree layering (another
  Git precedent for layered pattern config).
- **Sync/up-tooling**: rsync `--filter`/include-exclude ordering, Mutagen sync
  ignore/VCS-ignore handling, Syncthing ignore patterns, `.dockerignore`,
  git-lfs `track` attribute patterns, devcontainer config.

Pull out the recurring design axes (anchoring, `**`, negation, first-match vs.
last-match precedence, layering across scopes) and note which choices cause the
fewest surprises.

### Step 2 — Selection/enumeration mechanics under our stack

- How the force-include set is *materialised* into the `extra` tree: walk the
  declared patterns against the working tree, read blobs, build the tree with
  gix's `Editor` (consistent with Research 0002).
- Capability check: can we evaluate the patterns and walk natively (gix
  `gix-ignore` / `gix-glob` / `gix-dir`, or `gix status`/dirwalk), or do we shell
  out (`git ls-files`, `git check-ignore`)? Record this as a capability finding,
  pinned/dated like Research 0001, not a hard guarantee.
- Path-mapping: confirm force-included files keep their repo-relative paths in the
  `extra` tree (so the overlay lands them in the same place on the remote);
  consider whether any prefix remapping is ever needed.

### Step 3 — Recommend a design

Make a concrete recommendation across all four questions with rationale and the
rejected alternatives. Expected shape to evaluate (and argue for/against):

- A **committed project-level include file** (globs, gitignore-style syntax incl.
  negation) for shared things like build outputs, **plus an optional per-user
  layer** for personal config — mirroring git's project/user/clone scope model.
- Globs (not just explicit paths) for ergonomics, with a clearly specified
  precedence when layers and negations interact.
- Same-relative-path overlay into the worktree, deletions handled by the
  authoritative checkout.

Call out interactions with ADR-0004 (the `extra` tree it feeds) and ADR-0008
(deletion/overwrite authority).

### Step 4 — Write deliverables

- Write `docs/research/0004-...md` following the structure/voice of Research
  0002/0003 (TL;DR, options matrix where useful, per-option narrative, bearing on
  other ADRs, sources).
- Update **ADR-0007**: replace the callout with the decision, set status, add
  Consequences, cross-link.
- Add the row to `docs/research/README.md`.

## Out of scope

- Any production code / implementation.
- Re-deciding the encoding, transfer, or reassembly mechanics (ADR-0004/0005/0008).

## Acceptance

- Report 0004 exists and answers all four ADR-0007 questions with cited sources.
- ADR-0007 callout resolved, status updated, cross-linked.
- `docs/research/README.md` index updated.
