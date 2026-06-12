# Plan: Convert ticket markdown files to GitHub issues (#3)

## Goal

Turn the four draft research tickets under `research-tickets/` into GitHub
issues (via `ghwf`), delete the source markdown, and bake the extra guidance
#3 calls for into each issue body.

## Source material

Four ticket drafts plus an index:

| # | File | Source ADR |
| --- | --- | --- |
| 01 | `research-tickets/01-gix-libgit2-gap-analysis.md` | `docs/adr/0002-git-manipulation-strategy.md` |
| 02 | `research-tickets/02-encoding-sync-state.md` | `docs/adr/0004-encoding-the-sync-state-in-git.md` |
| 03 | `research-tickets/03-transfer-mechanism-pack-performance.md` | `docs/adr/0005-transfer-mechanism.md` |
| 04 | `research-tickets/04-force-include-configuration.md` | `docs/adr/0007-syncing-extra-gitignored-files.md` |
| — | `research-tickets/README.md` | (index of the above) |

Issue #1 (Foundational ADRs) is **closed**, so the drafts' "Blocked by: #1"
note is moot — the issues are unblocked and ready to pick up.

## Steps

### 1. Create a `research` label

The repo has no suitable label. Run `gh label create research` (idempotent —
tolerate "already exists"). If creation fails for any reason, fall back to
filing the issues without a label rather than blocking.

### 2. File four issues with `ghwf create-issue --no-block`

For each ticket, `ghwf create-issue --no-block --label research --title "<title>"`
with the body on stdin. `--no-block` keeps them standalone (not blocked by #3).

To avoid stale numeric cross-links (issue numbers don't exist until creation,
and tickets reference each other), I'll reference sibling tickets in prose by
title — e.g. "(see the companion *Encoding the sync state* ticket)" — rather
than `#N`. ADRs are referenced by full GitHub blob URL. With no numeric
cross-links, filing order doesn't matter.

Each issue body is the ticket's existing **Context / Goal / Deliverables /
Notes**, with these transformations:

- **Drop** the "Blocked by: #1" line (resolved).
- **Rewrite the Source line** to a full GitHub URL:
  `https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/<file>`.
- **Rewrite cross-ticket links** (currently relative `.md` links) to prose
  references naming the companion ticket, since the sibling files are being
  deleted and issue numbers aren't a clean fit inside `create-issue` stdin.
- **Append a standard footer** to every issue:

  > ## Working this ticket
  >
  > - **Web research is allowed and explicitly encouraged.**
  > - The **deliverable is the research and the write-up — no code is written
  >   in this ticket.**
  > - Persist findings to the repo as an **investigation report** under
  >   `docs/research/` (or similar) for future reference.
  > - **Update the source ADR(s) where the findings warrant it.**

  (The exact write-up location is a suggestion for the worker, not a hard
  constraint.)

### 3. Delete the source markdown

`git rm` the four ticket files **and** `research-tickets/README.md` — i.e.
remove the whole `research-tickets/` directory. Per the pre-plan hand-off, the
directory's only purpose was to stage these drafts; the README only indexes
files that will no longer exist. (The user approved the default to delete the
whole directory.)

### 4. Verify

- `gh issue list` shows the four new issues, each with the `research` label and
  the footer.
- `research-tickets/` no longer exists in the working tree.
- No remaining references to `research-tickets/` elsewhere in the repo
  (`grep -r research-tickets`). If the foundational ADRs or other docs link to
  it, update those references.

## Out of scope

- Doing any of the research itself — that's the work of the new issues.
- Writing investigation reports or editing ADRs now — those are deliverables of
  the new issues, not of #3.

## Files changed in this PR

- Deleted: `research-tickets/01-gix-libgit2-gap-analysis.md`,
  `research-tickets/02-encoding-sync-state.md`,
  `research-tickets/03-transfer-mechanism-pack-performance.md`,
  `research-tickets/04-force-include-configuration.md`,
  `research-tickets/README.md`.
- Possibly updated: any doc that linked to `research-tickets/` (TBD by grep).

The four GitHub issues are created via `ghwf` as a side effect, not as repo
file changes.
