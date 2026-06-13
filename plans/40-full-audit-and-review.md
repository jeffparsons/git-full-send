# Plan: Full audit and review (#40)

## Goal

Turn the audit's selected opportunities into **11 real GitHub issues**, one per
item. #40 is a meta/discovery issue: its deliverable is the filed issues, not a
code change. The audit menu was posted to #40 and the user selected everything
**except** E1, E2, and F2.

This mirrors how #3 and #34 converted backlog material into tracked issues: the
work product is `ghwf create-issue` calls, and this repo's PR contains only this
plan file.

## The 11 issues to file

| ID | Title | Label |
| --- | --- | --- |
| A1 | Add CI: fmt, clippy, test, and an MSRV build gate | `enhancement` |
| A2 | Supply-chain checks in CI (cargo-deny / cargo-audit) | `enhancement` |
| A3 | Add LICENSE files (MIT and Apache-2.0) | `documentation` |
| B1 | Bound server connection concurrency and add graceful shutdown | `enhancement` |
| B2 | Stream lifecycle: a `forget-stream` command (and optional TTL reaping) | `enhancement` |
| B3 | Lock worktree updates against concurrent runs | `enhancement` |
| C1 | Per-chain delta policy: split the `extra` and `code` pushes | `enhancement` |
| C2 | Transfer benchmark harness for the delta-base design | `enhancement` |
| C3 | Property test for the force-include prune invariant | `enhancement` |
| D1 | Report a sync summary (bytes, object/file counts, durations) | `enhancement` |
| F1 | Fix two stale docs (server test docstring; operating.md performance note) | `documentation` |

## Filing convention (applies to all 11)

Each is filed standalone with **`--no-block`**, exactly as #34 reasoned: these
are independent future-work items, not gated on this audit chore. (#40 will
close as soon as the issues exist, so a `blocked_by: #40` link would only get in
the way.) If a `--label` apply ever fails, fall back to filing without the label
rather than aborting.

Command shape (body on stdin):

```sh
ghwf create-issue --no-block --label <label> --title "<title>" <<'BODY'
<body>
BODY
```

Each body follows a small **Context / Proposal / Acceptance** shape, references
the concrete code/doc locations the audit found, and notes provenance
("Identified in the #40 audit.").

## Issue bodies

### A1 — Add CI: fmt, clippy, test, and an MSRV build gate  (`enhancement`)

> Identified in the #40 audit.
>
> **Context.** There is no `.github/` in the repo, so nothing automatically
> gates the PRs this project produces. The quality bar (`cargo fmt --check`,
> `cargo clippy --all-targets --all-features`, `cargo test --all` all green) is
> currently held by hand. `rust-toolchain.toml` already pins stable with
> `rustfmt` + `clippy`, and the workspace declares `rust-version = "1.85"`.
>
> **Proposal.** Add a GitHub Actions workflow that runs on PRs and pushes to
> `main`:
> - `cargo fmt --check`
> - `cargo clippy --all-targets --all-features -D warnings`
> - `cargo test --all`
> - a build (or check) on the declared MSRV (1.85) so the `rust-version` claim
>   stays honest.
>
> `git` must be on `PATH` in CI — the tests and the tool itself shell out to it
> (ADR-0002). Consider caching the cargo registry/build to keep the large `gix`
> dependency graph cheap.
>
> **Acceptance.** A workflow runs the four checks on PRs; a failing fmt/clippy/
> test/MSRV build fails the check. Green on the current tree.

### A2 — Supply-chain checks in CI (cargo-deny / cargo-audit)  (`enhancement`)

> Identified in the #40 audit.
>
> **Context.** The `gix` tree pulls in a large transitive dependency graph; we
> currently have no automated advisory, license, or duplicate-dependency
> auditing.
>
> **Proposal.** Add `cargo-deny` (advisories + license policy + bans/duplicates)
> and/or `cargo-audit`, wired into CI (ideally the same workflow as A1). Add a
> `deny.toml` encoding the allowed licenses (the workspace is `MIT OR
> Apache-2.0`; see A3) and any necessary advisory exceptions.
>
> **Acceptance.** A CI job fails on a new security advisory or a
> disallowed/unknown license in the dependency tree. `deny.toml` is committed
> and documents any deliberate exceptions.
>
> Depends loosely on **A1** (shares the workflow) and pairs with **A3** (license
> policy).

### A3 — Add LICENSE files (MIT and Apache-2.0)  (`documentation`)

> Identified in the #40 audit.
>
> **Context.** `Cargo.toml` declares `license = "MIT OR Apache-2.0"`, but there
> are no `LICENSE-*`/`COPYING` files in the tree, so the actual license text is
> missing.
>
> **Proposal.** Add the standard `LICENSE-MIT` and `LICENSE-APACHE` files (the
> conventional Rust dual-license pair), with the correct copyright line, and
> reference them from the README. This also gives A2's license policy something
> concrete to point at.
>
> **Acceptance.** Both license files exist at the repo root and match the
> `Cargo.toml` declaration; the README links them.

### B1 — Bound server connection concurrency and add graceful shutdown  (`enhancement`)

> Identified in the #40 audit.
>
> **Context.** `serve()` accepts connections in a loop and does an unbounded
> `std::thread::spawn` per connection (`crates/server/src/lib.rs:156-169`), with
> no concurrency cap, per-connection timeout, or signal handling. The accept
> loop only ends if the listener itself errors, so there is no clean way to stop
> a running `listen`.
>
> **Proposal.**
> - Cap the number of in-flight `git receive-pack` handlers (e.g. a bounded pool
>   or a semaphore), so a burst of connections can't exhaust threads.
> - Handle SIGTERM/SIGINT so `listen` drains in-flight connections and exits
>   cleanly (the `hooks` TempDir is already dropped at the end of `serve`; make
>   that path reachable on shutdown).
> - Consider a per-connection timeout so a stuck client can't pin a slot.
>
> **Acceptance.** `listen` refuses to exceed the configured concurrency; a
> shutdown signal stops the accept loop and returns without leaking the hooks
> dir. Existing transport tests still pass.

### B2 — Stream lifecycle: a `forget-stream` command (and optional TTL reaping)  (`enhancement`)

> Identified in the #40 audit.
>
> **Context.** ADR-0012 explicitly defers stream cleanup as a non-goal: stable
> ids keep the ref set bounded, but there is no way to remove a stream that is no
> longer wanted. Its `code`, `extra`, and `sent/*` refs persist on the server (and
> the client's `sent/*` refs persist locally) forever.
>
> **Proposal.** Add a server-side `forget-stream --repo --stream-id` command that
> deletes the stream's refs under `refs/git-full-send/streams/<id>/…` (and ideally
> the per-worktree index dir under `<git-dir>/git-full-send/worktrees/…` if it can
> be associated). Optionally consider TTL-based reaping as a follow-up. Mind the
> client-side `sent/*` retention refs too — document or provide a client-side
> counterpart.
>
> **Acceptance.** `forget-stream` removes a stream's refs so it no longer appears
> in `list-streams`; documented in `docs/operating.md`. TTL reaping may be split
> into its own follow-up.

### B3 — Lock worktree updates against concurrent runs  (`enhancement`)

> Identified in the #40 audit.
>
> **Context.** `update_worktree` (`crates/server/src/lib.rs`) drives
> `git read-tree --reset -u` then `clean -fdx` against a per-worktree index, but
> nothing guards two concurrent `update-worktree` runs targeting the same
> worktree, or a run racing the index's assumptions. Fine for the single-user
> MVP, but a foot-gun once a build orchestrator triggers checkouts.
>
> **Proposal.** Take a per-worktree advisory lock (e.g. a lockfile under the
> existing `<git-dir>/git-full-send/worktrees/<key>/` directory) for the duration
> of the read-tree + clean sequence, so concurrent updates of the same worktree
> serialise (or fail fast with a clear error) rather than interleave.
>
> **Acceptance.** Two concurrent `update-worktree` calls on the same worktree do
> not interleave their git steps; the second waits or fails cleanly. Distinct
> worktrees remain independent.

### C1 — Per-chain delta policy: split the `extra` and `code` pushes  (`enhancement`)

> Identified in the #40 audit.
>
> **Context.** `push_refs` (`crates/client/src/push.rs:115-121`) documents this
> as deferred: a single `git push` applies one global delta policy, but ADR-0005
> wants `--thin` deltas for the `code` chain and a predictable whole-object send
> for the volatile `extra` chain. Today both ride one `--thin` exchange.
>
> **Proposal.** Reconcile the policy per chain — e.g. a second push for `extra`,
> or per-chain pack config — so the `code` chain keeps thin deltas while `extra`
> gets the predictable send ADR-0005 calls for. Measure the effect (pairs well
> with **C2**).
>
> **Acceptance.** `code` and `extra` can use different delta policies in a sync;
> the choice is documented against ADR-0005. No regression in the transfer tests.

### C2 — Transfer benchmark harness for the delta-base design  (`enhancement`)

> Identified in the #40 audit.
>
> **Context.** Research-0003 calls for a before/after benchmark to validate the
> ADR-0005 delta-base design with real numbers: sync a changed build output with
> vs. without the retained `sent` delta base and compare bytes on the wire. It was
> filed as a candidate follow-up rather than done in that research.
>
> **Proposal.** Add a small benchmark/harness that constructs a repo with a large
> changed artifact and reports transfer size (and/or time) with and without the
> retained delta base, so the design's payoff is measurable and regressions are
> visible. Useful input for **C1**.
>
> **Acceptance.** A runnable benchmark reports the with/without-delta-base
> transfer cost; results captured (e.g. appended to Research-0003 or a short
> notes doc).

### C3 — Property test for the force-include prune invariant  (`enhancement`)

> Identified in the #40 audit.
>
> **Context.** `crates/client/src/select.rs` documents a key correctness property
> of the walk-pruning added in #39: the prune is a deliberate over-approximation
> that **never skips a directory the exhaustive walk would have selected from**.
> Today this is covered by hand-written examples.
>
> **Proposal.** Add a property test (e.g. `proptest`) that generates random
> directory trees and pattern sets, runs both the pruned walk and an exhaustive
> (prune-disabled) walk, and asserts the selected sets are **identical** — i.e.
> pruning never changes the result, only the directories entered. The walk already
> records `entered` under `#[cfg(test)]`, which this can build on.
>
> **Acceptance.** A property test asserts pruned-vs-exhaustive selection equality
> across generated inputs; it lives alongside the existing `select.rs` tests.

### D1 — Report a sync summary (bytes, object/file counts, durations)  (`enhancement`)

> Identified in the #40 audit.
>
> **Context.** `tracing` is wired but the client emits only a few `info!` lines.
> Operators get no summary of what a sync moved. This overlaps with the already-
> open **#42 (record timing metrics)**: to avoid duplication, this issue is scoped
> to the **reporting/summary surface** — what gets shown to the operator — while
> #42 owns where/how timing is recorded.
>
> **Proposal.** Emit a concise end-of-sync summary: files/objects in the `code`
> and `extra` trees, bytes transferred (where obtainable from the push), and
> per-phase durations (encode / encode_extra / push). Reconcile with #42 so the
> two are complementary rather than overlapping.
>
> **Acceptance.** A `sync` prints a clear summary line/block; cross-linked with
> #42 and consistent with whatever timing mechanism it lands. (Fold into #42
> instead if the maintainer prefers.)

### F1 — Fix two stale docs  (`documentation`)

> Identified in the #40 audit. Two docs drifted from the current code:
>
> 1. **`crates/server/tests/integration.rs`** — the module docstring still says
>    the tests "do not exercise any server logic yet (it is stubbed with
>    `todo!()`)". The server is fully implemented now (`listen`,
>    `update_worktree`, `list_streams`); update the comment to match.
> 2. **`docs/operating.md`** §"Performance note" (currently lines ~162-167) —
>    describes the pre-#39 walk: "descends every non-`.git` directory … is still
>    traversed even when nothing in it is selected." #39 added the prune that
>    skips subtrees that cannot contain a match, so this is outdated. Rewrite it
>    to describe the prune and the residual unanchored-pattern caveat.
>
> **Acceptance.** Both texts reflect current behaviour; no other references to
> the stubbed-server or pre-prune walk remain (`grep` to confirm).

## Steps

1. **File the 11 issues** with `ghwf create-issue --no-block --label <label>
   --title "<title>"`, body from stdin, using the titles/labels/bodies above.
   Capture each returned issue number.
2. **Verify** with `gh issue list --state open` that all 11 exist, are
   standalone (no "blocked by #40"), and carry the intended label.
3. No source files change in this PR beyond adding this plan (ghwf commits the
   plan and opens the draft PR). The issues are created as a side effect.

## Verification

- `gh issue list --state open` shows the 11 new issues plus the pre-existing #40
  and #42.
- Spot-check a couple of bodies render correctly and reference the right
  file:line locations.
- This worktree's `cargo` checks are untouched by issue creation; a build is a
  sanity check only (this PR adds only the plan file).

## Out of scope

- **Doing any of the 11 pieces of work** — each is the deliverable of its own new
  issue, not of #40.
- **E1 (built-in transport auth/encryption), E2 (cross-stream isolation), and F2
  (CONTRIBUTING / dev-setup)** — explicitly dropped by the maintainer in the #40
  discussion.

## Files changed in this PR

- Added: `plans/40-full-audit-and-review.md` (this plan).

The 11 GitHub issues are created via `ghwf` as a side effect, not as repo file
changes.
