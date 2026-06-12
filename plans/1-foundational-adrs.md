# Plan: Foundational ADRs (issue #1)

## Goal

Record the foundational architectural decisions for `git-full-send` as a set of
Markdown ADRs (MADR-style) before any code is written, plus a root `CLAUDE.md`
that points contributors at the ADR process. ADRs for settled decisions are
written as **accepted**; ADRs for genuinely unresolved questions are written as
**proposed**, capturing the design constraints / drivers / options without
committing to an outcome, and flagging inline where a separate research task is
needed (to be split into its own ticket later).

## Conventions (to be established by ADR-0000)

- **Format:** MADR-style Markdown. Use the MADR sections that fit each decision
  (Status, Context & Problem Statement, Decision Drivers, Considered Options,
  Decision Outcome, Consequences / Pros & Cons) — **not** a rigid template;
  include only what is appropriate for each ADR. (Per user: "Don't need to stick
  a rigid template; include what is appropriate for each.")
- **Location:** `docs/adr/`.
- **Filenames:** `NNNN-kebab-title.md`, zero-padded sequential numbers starting
  at `0000`.
- **Status lifecycle:** `proposed` → `accepted` → `deprecated` / `superseded by ADR-NNNN`.
- **Research callouts:** unresolved items are marked inline with a clear
  `> ⚠ Research task needed: …` callout so each can later become its own issue.
- **Index:** `docs/adr/README.md` lists every ADR with number, title, and status.

## Deliverables (files to create)

```
CLAUDE.md                              # references the ADR process
docs/adr/README.md                     # ADR index
docs/adr/0000-record-architecture-decisions.md
docs/adr/0001-language-runtime-and-core-crates.md
docs/adr/0002-git-manipulation-strategy.md
docs/adr/0003-client-server-architecture.md
docs/adr/0004-encoding-the-sync-state-in-git.md
docs/adr/0005-transfer-mechanism.md
docs/adr/0006-transport-and-connectivity.md
docs/adr/0007-syncing-extra-gitignored-files.md
docs/adr/0008-remote-worktree-disposability.md
```

## Per-ADR outline

### ADR-0000 — Record architecture decisions (meta) · *accepted*
- We will capture significant decisions as MADR-style Markdown ADRs under
  `docs/adr/`, numbered sequentially, using the flexible-template convention
  above.
- Define the status lifecycle and the `⚠ Research task needed` callout
  convention.
- Note the index file and naming scheme.
- This is the only "process" ADR; `CLAUDE.md` links here.

### ADR-0001 — Language, runtime & core crates · *accepted*
- **Decision:** Rust, async on Tokio from the start.
- Core crates: `anyhow` (application-level error context), `thiserror`
  (library/typed errors), `clap` (CLI). Note the anyhow-vs-thiserror split
  (binaries vs reusable library boundaries).
- **Context:** records the concrete initial platform target — macOS client +
  Linux/EC2 remote — and that broad cross-platform support is a non-goal for
  now while we avoid gratuitous platform lock-in.

### ADR-0002 — Git manipulation strategy · *accepted*
- **Decision:** gix (gitoxide) first; shell out to git plumbing CLI where gix
  has gaps. Explicitly **no libgit2**.
- Drivers: pure-Rust/async fit, control over object construction, alignment with
  build tooling that already understands Git.
- Record that we will research libgit2's capabilities purely to identify gix
  feature gaps, and that we are open to pausing tool work to upstream fixes to
  gitoxide.
- `> ⚠ Research task needed:` gix-vs-libgit2 capability gap analysis for the
  operations this tool requires (object/tree synthesis, pack generation,
  send/receive-pack, alternate index, etc.).

### ADR-0003 — Client/server architecture · *accepted*
- **Decision:** distinct client and server roles. Server runs on the remote,
  configured with a target repo + worktree directory, binds localhost only.
- Server exposes two separate operations/subcommands: **receive sync** (accept
  transferred objects) and **update worktree** (check out the synced state),
  invoked independently — in practice a separate build-orchestration process
  triggers the worktree update when ready.
- Client never touches the user's current branch, main index, or working tree.

### ADR-0004 — Encoding the sync state in Git · *proposed*
- **Problem:** represent committed code + working-tree (staged & unstaged)
  changes + force-added normally-gitignored files as Git objects, synthesised
  without disturbing the current branch/index/worktree (scratch refs / alternate
  index permitted).
- Capture **constraints/drivers:** keep Git happy, produce efficient transfers,
  avoid pathological pack shapes, reuse digests already present in the tree.
- **Considered options** (no decision yet): the prototype's stacked commits
  (committed → working-tree commit → forced-files commit); a separate
  branch/independent commit for the "extra" files exploded into the worktree on
  the far end; alternate-index-based tree construction.
- `> ⚠ Research task needed:` determine the encoding that keeps Git happy and
  yields efficient/predictable transfers. Closely coupled with ADR-0005.

### ADR-0005 — Transfer mechanism · *proposed*
- **Problem:** move the synthesised objects client→server.
- **Considered options:** `git push` handed to `git-daemon` receive-pack on the
  server, vs a native gix smart-protocol implementation; whether the server
  hands the raw stream to git-daemon or handles it itself.
- Capture the **intermittent slow-transfer observation** from the prototype
  (sometimes fast, sometimes surprisingly slow; suspected pathological pack
  shape relative to what the server already has) as a driver to investigate.
- `> ⚠ Research task needed:` evaluate git-daemon vs native gix receive path and
  root-cause the pack/transfer performance variability.

### ADR-0006 — Transport & connectivity · *accepted*
- **Decision:** server binds localhost only; connectivity via manual SSH
  tunnelling; **no** built-in transport security/authentication initially.
- Record this as a deliberate initial-scope decision, with a note that
  first-class transport security is deferred (candidate future ADR).

### ADR-0007 — Syncing extra (normally-gitignored) files · *proposed*
- **Problem:** which normally-gitignored files we deliberately force-include
  (e.g. CPU-intensive web-client build outputs produced on the MacBook, per-user
  config) and how that set is configured, plus how those files land in the
  remote worktree.
- Drivers: large-ish files; build done on client for snappier interactive dev;
  must arrive in the remote worktree alongside the synced code.
- Tightly coupled to ADR-0004's encoding choice; cross-reference.
- `> ⚠ Research task needed:` configuration mechanism for the force-include set.

### ADR-0008 — Remote worktree disposability & sync authority · *accepted*
- **Decision:** the remote worktree is always **disposable**; the synced client
  state is authoritative and the remote checkout is a destructive overwrite —
  nothing on the remote is precious.
- Optionally we may record what was deleted/overwritten purely as
  diagnostics/debugging, never to preserve remote-side changes (nice-to-have).

## CLAUDE.md (root)

- Brief project description (sync working tree + staged + force-added gitignored
  files from client to remote via Git).
- Short "Architecture decisions" section: we record decisions as ADRs under
  `docs/adr/`; link to ADR-0000 for the process; instruct contributors to add a
  new numbered ADR for significant decisions and update the index.

## Out of scope (this issue)

- Any Rust code, crate scaffolding, or `Cargo.toml`.
- Resolving the open research questions (ADR-0004 / 0005 / 0007) — these stay
  `proposed`; the flagged research tasks become their own tickets later.
- Standalone ADRs for platform scope (folded into 0001/0003) and for future
  transport security (noted as deferred in 0006).

## Acceptance / verification

- All files above exist; every ADR has a Status line and the appropriate
  MADR sections for its decision.
- `docs/adr/README.md` lists all nine ADRs with correct numbers and statuses.
- `accepted` ADRs reflect the decisions confirmed on issue #1; `proposed` ADRs
  contain constraints/drivers/options and a clear `⚠ Research task needed`
  callout, with no forced conclusion.
- `CLAUDE.md` exists at the repo root and references the ADR process.
- Internal cross-references (0004↔0005, 0004↔0007, 0006→future security) and
  links resolve.
```
```

## Follow-up tickets to file later (not part of this issue)

- gix vs libgit2 capability gap analysis (from ADR-0002).
- Sync-state encoding research (from ADR-0004).
- Transfer mechanism + pack-performance investigation (from ADR-0005).
- Force-include configuration mechanism (from ADR-0007).
