# ADR-0007 — Syncing extra (normally-gitignored) files

- Status: accepted
- Date: 2026-06-12

## Context / problem statement

Part of the point of `git-full-send` is to sync files that are **normally
gitignored** but that we deliberately want on the remote. The motivating cases:

- **Web-client build outputs** produced on the MacBook. The build is very CPU
  intensive, so we run it on the developer's powerful laptop for snappier
  interactive development rather than on the remote — but the outputs still need
  to be present on the remote for the rest of the build/run flow.
- **Per-user config files** that we want synced.

Assume there will be other good reasons to sync large-ish, normally-unversioned
files too. We need to decide how the force-include set is **configured** and how
those files **land in the remote worktree**.

## Decision drivers

- The set of extra files is project- and user-specific, and may be large-ish.
- These files would otherwise be gitignored, so the mechanism must override
  ignore rules for an explicit, controlled set rather than syncing all ignored
  files indiscriminately.
- The files must end up in the right place in the remote worktree alongside the
  synced code.

## Relationship to other decisions

How these files are encoded for transfer is part of
[ADR-0004](0004-encoding-the-sync-state-in-git.md) (e.g. stacked in a commit, on
a branch of their own, or exploded out separately on the remote). This ADR is
about **what is included and how that is configured**, not the on-the-wire
encoding.

## Decision

Declare the force-include set as **gitignore-syntax glob patterns** across two
layers, and land the selected files as a **same-path overlay** on the remote:

- **Where.** A **committed, project-level pattern file at the repo root** (shared,
  version-controlled — the natural home for "the build outputs this project
  produces"), plus an **optional per-user pattern file** outside the repo
  (mirroring Git's `core.excludesFile`) for personal config. This is Git's own
  in-tree + per-user split.
- **Granularity.** **Globs, not explicit path lists** — the full gitignore
  vocabulary (anchoring, `**`, character classes, `!` carve-outs), so a whole
  volatile build-output tree is one durable line rather than a manifest that rots.
  We accept full glob expressiveness (sparse-checkout's "non-cone" shape) because
  selection runs **once per sync** over a small curated list, so the O(N·M) cost
  that made Git restrict sparse-checkout to cone mode does not apply.
- **Scope.** Two layers evaluated **`[project, then user]` with last-match-wins**
  (Git's convention). Both layers may *add* includes; because the user layer is
  evaluated last, a per-user `!` can *carve out* a project include, giving the
  operator final say on their own machine.
- **How placed.** Keep
  [ADR-0004](0004-encoding-the-sync-state-in-git.md)/[Research 0002](../research/0002-encoding-the-sync-state-in-git.md)'s
  overlay, with **identity path-mapping**: each file lands at its **same
  repo-relative path** on the remote (no `--prefix` remapping), because build/run
  tooling expects the outputs exactly where they were produced.

Two mechanics underpin this:

- **Selection is gix-native** at the current pin — `gix-ignore` parses the pattern
  files, `gix-glob` matches, `gix-dir` walks/classifies the worktree — so
  enumerating the set needs **no `git` shell-out**, consistent with
  [ADR-0002](0002-git-manipulation-strategy.md). The matched blobs feed the gix
  `Editor` that builds the `extra` tree.
- The set is an **independent allow-list matched against the working-tree
  filesystem**, *not* `!` negations layered on the project's real `.gitignore`.
  This sidesteps Git's "cannot re-include a file under an excluded parent
  directory" limitation, which would otherwise bite constantly (build outputs
  typically live under an ignored `dist/`/`target/`).

The polarity is inverted from `.gitignore` (here a bare pattern *includes* and `!`
*carves out*); the file is named/documented as an **include / allow-list** to
avoid the confusion that got non-cone sparse-checkout deprecated.

The full analysis — prior-art survey, the four questions, and the gix capability
check — is in
[Research 0004 — Force-include configuration mechanism](../research/0004-force-include-configuration-mechanism.md)
(2026-06-12).

## Consequences

- The project ships its force-include patterns in a committed file that rides
  along in the `code` tree automatically; the per-user file stays out of the repo
  and is read on the client only (it drives selection and need not travel).
- The selected files become the `extra` tree
  ([ADR-0004](0004-encoding-the-sync-state-in-git.md)) and are exploded over the
  `code` checkout at their original paths.
- Because the set is volatile, the remote update must **remove** force-included
  files from a prior sync that are no longer selected — latitude provided by
  [ADR-0008](0008-remote-worktree-disposability.md)'s disposable/authoritative
  worktree; the exact removal mechanism is ADR-0004/0008's reassembly detail.
- The exact pattern-file name/location and any future folding into a central
  project config are low-stakes, revisable details left to implementation.
