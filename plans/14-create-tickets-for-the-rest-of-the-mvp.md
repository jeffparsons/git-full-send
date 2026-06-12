# Plan — #14 Create tickets for the rest of the MVP

## Goal

File the remaining **MVP implementation tickets** as a **linear sequence** of
standalone GitHub issues, created via `ghwf create-issue --no-block` (no
dependency links — we'll tackle one at a time, top to bottom, per the issue
instructions). Each issue body cites the governing ADRs and investigation
reports so the worker has the decision context inline.

This ticket produces **GitHub issues only** — no repo file changes other than
this plan itself. The pre-plan breakdown was approved as-is (👍, no changes), so
this plan locks in that 6-ticket, walking-skeleton-first sequence.

## Source material (all already accepted / done)

- ADRs: [0001](../docs/adr/0001-language-runtime-and-core-crates.md)–[0008](../docs/adr/0008-remote-worktree-disposability.md)
  (all `accepted`).
- Investigation reports: [0001 gix-gap](../docs/research/0001-gix-git-plumbing-vs-libgit2-capability-gap.md),
  [0002 encoding](../docs/research/0002-encoding-the-sync-state-in-git.md),
  [0003 transfer/pack-perf](../docs/research/0003-transfer-mechanism-and-pack-performance.md),
  [0004 force-include](../docs/research/0004-force-include-configuration-mechanism.md).
- Existing code: the #9 boilerplate — Cargo workspace with `gfs-common` /
  `gfs-client` / `gfs-server` / `gfs-cli` (binary `git-full-send`) + `test-support`,
  and `todo!()` stubs for `sync` / `listen` / `update-worktree`.

## What's left for the MVP

Everything after #9 is *implementing* the three stubbed commands end to end:
client `sync` (encode → select → push), server `listen` (receive-pack ingest),
server `update-worktree` (authoritative checkout + extra overlay). The sequence
below is **walking-skeleton first**: tickets 1→3 get a code-only sync working end
to end, 4→5 layer force-include on, 6 hardens and documents.

## Decisions locked in pre-plan (approved 👍)

- **6 tickets**, in the order below; no merging/splitting.
- **Created directly as GitHub issues** via `ghwf create-issue --no-block`
  (standalone, no `Blocked by` links).
- **Minimal config/args folded into each command's ticket** — no speculative
  up-front config ticket.
- **Out of MVP scope** (not ticketed): built-in transport auth/encryption
  (ADR-0006 defers it), optional remote-diff diagnostics (ADR-0008 nice-to-have),
  and CI (no separate ticket requested).

## Conventions for the issue bodies

- Reference ADRs/reports by **full GitHub blob URL**
  (`https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/<file>` and
  `.../docs/research/<file>`), since issue numbers for siblings don't exist at
  filing time and absolute links render anywhere.
- **No numeric cross-links between the new issues** — refer to a sibling by name
  in prose ("the earlier *…* ticket") so filing order is the only ordering signal.
- File **in order 1→6** so the resulting issue numbers ascend with the sequence.
- Each body follows: **Context → Scope / deliverables → Relevant decisions →
  Out of scope → Acceptance**.

---

## Issue 1 — Encode the sync state: the `code` commit (client)

**Title:** `Client sync: encode the code commit (working-tree state)`

**Body:**

> ## Context
>
> First building block of the client `sync` command. The client must represent
> the developer's current code state — committed history **plus** working-tree
> changes (staged **and** unstaged) — as a Git commit it can later push, **without
> touching** the user's branch, main index, or working tree.
>
> Per the encoding decision, working-tree changes (staged + unstaged) are
> collapsed to the **current on-disk contents** (the remote never needs the
> index/worktree split) and captured as a **single tree in one commit parented on
> `HEAD`**, written under the scratch ref `refs/git-full-send/code`. Parenting on
> `HEAD` lets later push negotiation share the committed history so only the
> working-tree delta crosses the wire. The tree is synthesised with **gix's native
> tree `Editor`** — the index-centric approach is the one path that would force a
> `git` shell-out, so we avoid it.
>
> ## Scope / deliverables
>
> - Flesh out `gfs_client::sync` (or a dedicated `encode` module) to:
>   - Open the repo with `gix`, resolve `HEAD`.
>   - Walk the working tree and build a single tree reflecting current on-disk
>     contents (tracked files with their working-tree content; respect deletions),
>     using the gix tree `Editor`.
>   - Write a commit parented on `HEAD` and update `refs/git-full-send/code` to it
>     — **without** mutating the user's branch, index, or working tree.
> - Minimal client config/args needed to locate the repo (default: cwd).
> - Integration tests against a temp repo (via `test-support`): make committed +
>   staged + unstaged + deleted changes, run the encode step, assert the produced
>   `code` tree matches the on-disk state and that the user's `HEAD`/index/worktree
>   are untouched.
> - No transfer yet — this ticket stops at "the `code` ref exists locally".
>
> ## Relevant decisions
>
> - [ADR-0004 — Encoding the sync state in Git](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0004-encoding-the-sync-state-in-git.md)
> - [ADR-0003 — Client/server architecture](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0003-client-server-architecture.md)
> - [ADR-0002 — Git manipulation strategy](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0002-git-manipulation-strategy.md)
> - [Research 0002 — Encoding the sync state in Git](https://github.com/jeffparsons/git-full-send/blob/main/docs/research/0002-encoding-the-sync-state-in-git.md)
> - [Research 0001 — gix vs libgit2 capability gap](https://github.com/jeffparsons/git-full-send/blob/main/docs/research/0001-gix-git-plumbing-vs-libgit2-capability-gap.md)
>
> ## Out of scope
>
> - The `extra` (force-include) commit — its own later ticket.
> - Any push/transfer or server-side work.
>
> ## Acceptance
>
> - Running the encode step on a dirty temp repo produces a `refs/git-full-send/code`
>   commit whose tree equals the current on-disk contents.
> - The user's branch, index, and working tree are provably unchanged.
> - `cargo build` / `test` / `clippy -D warnings` / `fmt --check` green.

---

## Issue 2 — Transfer: `listen` + push → `receive-pack` ingest

**Title:** `Transfer: server listen + client push into git receive-pack`

**Body:**

> ## Context
>
> With the `code` ref synthesised locally (earlier *encode the code commit*
> ticket), move its objects to the server. The transfer reuses Git's own
> machinery: the client runs `git push --thin`; the server's long-running
> `listen` process **spawns `git receive-pack <repo>` per connection** and wires
> the connection stream to its stdio — the same hand-off `sshd`/`git daemon`
> perform internally. The server **binds localhost only**; connectivity from a
> real client is via a **manual SSH tunnel** (an operator step), so tests can run
> over plain loopback.
>
> ## Scope / deliverables
>
> - **Server `listen`** (`gfs_server::listen`): bind a localhost TCP port, accept
>   connections, and for each spawn `git receive-pack` against the configured
>   repo, piping the socket ↔ child stdio. Confine writable refs to the
>   `refs/git-full-send/*` namespace (`gfs_common::REF_NAMESPACE`) and set
>   `receive.autogc=false` for the receive window. Long-running until shut down.
> - **Client push**: extend `sync` to `git push --thin` the `code` ref to the
>   server over the connection (raw receive-pack stream, e.g. via the `ext::`
>   transport / a tunnelled localhost endpoint). **Retain the prior tip** locally
>   so subsequent pushes have a delta base.
> - Minimal config/args: server listen address/port; client target endpoint.
> - Integration test on loopback: start `listen` against a temp "remote" repo,
>   run the client push from a temp "client" repo, assert the `code` objects/ref
>   land on the server.
>
> ## Relevant decisions
>
> - [ADR-0005 — Transfer mechanism](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0005-transfer-mechanism.md)
>   (push → `receive-pack`, `--thin`, ref retention, `receive.autogc=false`, delta
>   policy per payload).
> - [ADR-0003 — Client/server architecture](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0003-client-server-architecture.md)
> - [ADR-0006 — Transport & connectivity](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0006-transport-and-connectivity.md)
> - [Research 0003 — Transfer mechanism & pack-performance root-cause](https://github.com/jeffparsons/git-full-send/blob/main/docs/research/0003-transfer-mechanism-and-pack-performance.md)
>
> ## Out of scope
>
> - The `extra` ref (pushed alongside `code` once it exists — later ticket).
> - Worktree checkout (next ticket).
> - Built-in auth/encryption — ADR-0006 leans on the SSH tunnel; not now.
>
> ## Acceptance
>
> - `listen` serves `git receive-pack` and only accepts writes under
>   `refs/git-full-send/*`.
> - A loopback push lands the `code` ref + objects on the server repo.
> - Prior tips retained on both ends; `receive.autogc` disabled during receive.
> - Build/test/clippy/fmt green.

---

## Issue 3 — `update-worktree`: authoritative checkout of `code`

**Title:** `Server update-worktree: authoritative checkout of the code tree`

**Body:**

> ## Context
>
> The remote worktree is **disposable**: updating it is an **authoritative,
> destructive overwrite** that makes the remote match the synced state, with no
> attempt to preserve remote-side edits, deletions, or additions. This ticket
> implements that for the `code` tree received in the earlier *transfer* ticket.
> Completing it gives a **working end-to-end committed + working-tree sync** (the
> walking skeleton), minus force-include.
>
> ## Scope / deliverables
>
> - `gfs_server::update_worktree`: check the `refs/git-full-send/code` tree out
>   into the configured worktree directory as an unconditional, destructive
>   overwrite (e.g. set the index from the tree + `git checkout-index` / equivalent
>   against the worktree, and clear files the synced tree no longer contains).
> - Invoked **independently** of `listen` (a build orchestrator triggers it on
>   demand). Minimal config/args: target repo + worktree dir.
> - Integration test on loopback extending the prior test: push `code`, run
>   `update-worktree`, assert the worktree exactly matches the synced tree —
>   including that files absent from the synced tree are removed and pre-existing
>   remote-local edits are stomped.
>
> ## Relevant decisions
>
> - [ADR-0008 — Remote worktree disposability & sync authority](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0008-remote-worktree-disposability.md)
> - [ADR-0003 — Client/server architecture](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0003-client-server-architecture.md)
> - [ADR-0004 — Encoding the sync state in Git](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0004-encoding-the-sync-state-in-git.md)
>   (reassembly: check out `code`, explode `extra` over it — `extra` handled in a
>   later ticket).
>
> ## Out of scope
>
> - The `extra` overlay + stale-file removal (next-but-one ticket).
> - Optional remote-diff diagnostics (ADR-0008 nice-to-have) — not in MVP.
>
> ## Acceptance
>
> - `update-worktree` makes the configured worktree exactly match the synced
>   `code` tree, destructively.
> - End-to-end loopback test (encode → push → update-worktree) passes.
> - Build/test/clippy/fmt green.

---

## Issue 4 — Force-include selection + the `extra` commit (client)

**Title:** `Client sync: force-include selection and the extra commit`

**Body:**

> ## Context
>
> `git-full-send` deliberately syncs a controlled set of **normally-gitignored**
> files (e.g. CPU-intensive web-client build outputs produced on the laptop, and
> per-user config). The set is declared as **gitignore-syntax allow-list glob
> patterns** across two layers: a **committed project-level pattern file at the
> repo root** plus an **optional per-user pattern file outside the repo**.
> Evaluated **`[project, then user]` with last-match-wins** — both layers may add
> includes, and a per-user `!` can carve out a project include. Note the inverted
> polarity vs `.gitignore`: a bare pattern **includes**, `!` **carves out**. It is
> an **independent allow-list matched against the working-tree filesystem**, not
> `!` negations on the project's real `.gitignore` (which sidesteps Git's "can't
> re-include under an excluded parent" limitation).
>
> Selection is **gix-native** (`gix-ignore` parses, `gix-glob` matches, `gix-dir`
> walks/classifies) — no `git` shell-out. The matched blobs feed a gix tree
> `Editor` that builds the **`extra` tree**, captured as a commit under
> `refs/git-full-send/extra`, **parented on the previous sync's `extra` commit**
> so prior (large) build outputs stay available as delta bases.
>
> ## Scope / deliverables
>
> - Parse the project-root pattern file + optional per-user pattern file
>   (decide/record concrete names/locations — low-stakes, revisable). Implement
>   the `[project, then user]` last-match-wins, include/`!`-carve-out semantics.
> - Enumerate matching working-tree files via `gix-ignore`/`gix-glob`/`gix-dir`.
> - Build the `extra` tree with the gix `Editor`; write a commit parented on the
>   prior `refs/git-full-send/extra` tip (or rootless on first sync) and update the
>   ref. Push it **alongside `code`** in the same exchange (extends the transfer
>   ticket's push). Prefer a predictable whole-object send for this volatile chain
>   per ADR-0005.
> - Tests: a temp repo with a project pattern file and gitignored build outputs —
>   assert the right files are selected (incl. a per-user carve-out case) and the
>   `extra` commit chains onto the previous one.
>
> ## Relevant decisions
>
> - [ADR-0007 — Syncing extra (normally-gitignored) files](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0007-syncing-extra-gitignored-files.md)
> - [ADR-0004 — Encoding the sync state in Git](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0004-encoding-the-sync-state-in-git.md)
> - [ADR-0005 — Transfer mechanism](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0005-transfer-mechanism.md)
>   (delta policy for the volatile big-files chain).
> - [Research 0004 — Force-include configuration mechanism](https://github.com/jeffparsons/git-full-send/blob/main/docs/research/0004-force-include-configuration-mechanism.md)
>
> ## Out of scope
>
> - Exploding `extra` onto the remote worktree + stale-file removal (next ticket).
>
> ## Acceptance
>
> - Patterns from both layers select the correct working-tree files (incl.
>   carve-out), with no `git` shell-out for selection.
> - An `extra` commit is produced under `refs/git-full-send/extra`, chained on the
>   prior tip, and pushed alongside `code`.
> - Build/test/clippy/fmt green.

---

## Issue 5 — Remote `extra` overlay + stale-file removal

**Title:** `Server update-worktree: overlay extra files and remove stale ones`

**Body:**

> ## Context
>
> Completes the force-include round-trip. After checking out the `code` tree
> (earlier *authoritative checkout* ticket), the remote update must **explode the
> `extra` tree over the checkout at identity paths** (each file lands at its same
> repo-relative path — build/run tooling expects outputs exactly where they were
> produced, so no `--prefix` remapping). Because the force-include set is
> **volatile**, the update must also **remove force-included files from a prior
> sync that are no longer selected**, using the latitude of the
> disposable/authoritative worktree.
>
> ## Scope / deliverables
>
> - Extend `gfs_server::update_worktree` to, after the `code` checkout, explode
>   `refs/git-full-send/extra` over the worktree at identity paths (e.g.
>   `git checkout-index` from the `extra` tree).
> - Track/remove force-included files present from a previous sync but absent from
>   the current `extra` tree (e.g. diff prior vs current `extra` tree and delete
>   the dropped paths), without harming `code`-tree files.
> - Integration test: sync with extra files → update-worktree → assert they land
>   at their original paths over the code checkout; then sync again with one extra
>   file dropped → assert it is removed from the remote worktree.
>
> ## Relevant decisions
>
> - [ADR-0007 — Syncing extra (normally-gitignored) files](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0007-syncing-extra-gitignored-files.md)
>   (same-path overlay; remove no-longer-selected files).
> - [ADR-0008 — Remote worktree disposability & sync authority](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0008-remote-worktree-disposability.md)
> - [ADR-0004 — Encoding the sync state in Git](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0004-encoding-the-sync-state-in-git.md)
>   (reassembly = check out `code`, explode `extra`).
>
> ## Out of scope
>
> - Optional remote-diff diagnostics (ADR-0008 nice-to-have) — not in MVP.
>
> ## Acceptance
>
> - `extra` files land at their identity paths over the `code` checkout.
> - Force-included files dropped since the last sync are removed from the remote
>   worktree; `code`-tree files are unaffected.
> - Full force-include round-trip integration test passes.
> - Build/test/clippy/fmt green.

---

## Issue 6 — End-to-end integration tests, CLI/config & operator docs

**Title:** `MVP hardening: end-to-end tests, CLI/config, operator docs`

**Body:**

> ## Context
>
> With all three commands implemented, harden and document the MVP. This ticket
> ties the pieces into a full round-trip integration test, finalises the CLI
> surface/config, and writes the operator-facing docs (notably the manual SSH
> tunnel ADR-0006 relies on).
>
> ## Scope / deliverables
>
> - **End-to-end integration test** on loopback: init a repo with committed code,
>   working-tree changes, and force-included files → `sync` → server receives →
>   `update-worktree` → assert the remote worktree matches the client state
>   exactly, **including extra files at identity paths and deletions** (both
>   `code`-tree and dropped force-includes).
> - **CLI/config finalisation** for `sync` / `listen` / `update-worktree`: server
>   listen address/port, target repo + worktree dir, client endpoint, and
>   force-include pattern-file paths (project + per-user). Consistent `clap` args
>   and any config-file loading the commands need.
> - **Operator docs** (README and/or `docs/`): how to set up the SSH tunnel and
>   point the client at it, how to run `listen` and `update-worktree` on the
>   remote, and how to write the force-include pattern file(s).
>
> ## Relevant decisions
>
> - All ADRs, especially
>   [ADR-0006 — Transport & connectivity](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0006-transport-and-connectivity.md)
>   (manual SSH tunnel),
>   [ADR-0003 — Client/server architecture](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0003-client-server-architecture.md),
>   and [ADR-0007 — Force-include](https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0007-syncing-extra-gitignored-files.md).
>
> ## Out of scope
>
> - Built-in transport auth/encryption (ADR-0006 defers it).
> - CI configuration (not part of this MVP unless separately requested).
>
> ## Acceptance
>
> - A single end-to-end test drives the full sync round-trip and asserts an exact
>   remote match incl. extra files and deletions.
> - All three commands have finalised, documented args/config.
> - Operator docs cover tunnel setup, running the server commands, and writing
>   force-include patterns.
> - Build/test/clippy/fmt green.

---

## Implementation steps for #14 (the prep-and-plan worker, in the implementing phase)

1. For each issue 1→6 **in order**, run
   `ghwf create-issue --no-block --title "<title>"` with the body (from the
   drafts above) on stdin. `--no-block` keeps them standalone. Filing in order
   makes the issue numbers ascend with the sequence.
2. No repo files change beyond this plan. There is no source markdown to delete
   (unlike #3) — the drafts live in this plan, not in a `*-tickets/` directory.
3. Verify with `gh issue list` (or `ghwf`) that six new open issues exist in the
   intended order, each citing its ADRs/reports, none with `Blocked by` links.

## Out of scope for #14

- Doing any of the implementation work — that's the six new issues.
- Editing ADRs, research reports, or source code.
- A separate CI ticket, transport-auth ticket, or diagnostics ticket (explicitly
  excluded from the MVP per the approved pre-plan).

## Files changed in this PR

- Added: `plans/14-create-tickets-for-the-rest-of-the-mvp.md` (this plan).

The six GitHub issues are created via `ghwf` in the implementing phase as a side
effect, not as repo file changes.
