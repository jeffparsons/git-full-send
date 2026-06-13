# ADR-0004 — Encoding the sync state in Git

- Status: accepted
- Date: 2026-06-12

## Context / problem statement

The client must represent the full sync state as Git objects so it can be
transferred efficiently ([ADR-0005](0005-transfer-mechanism.md)) and checked out
on the remote. The sync state is more than the committed code:

1. The committed history already in the repository.
2. Working-tree changes — both staged and unstaged.
3. A set of **normally-gitignored files that we deliberately force-include** —
   e.g. CPU-intensive web-client build outputs produced on the MacBook, and
   per-user config files (see
   [ADR-0007](0007-syncing-extra-gitignored-files.md)). Some of these are
   large-ish.

This must happen **without disturbing** the user's current branch, main index,
or working tree. Scratch refs and an alternate index are permitted.

## Decision drivers

- Must not touch the user's branch / main index / working tree.
- Keep Git happy: produce well-formed objects that Git tooling (including our
  build planning) understands and whose digests can be reused.
- Efficient, predictable transfers — avoid pathological pack shapes that blow up
  transfer size or time (this is tightly coupled to
  [ADR-0005](0005-transfer-mechanism.md), where intermittent slow transfers were
  observed in the prototype).
- Handle the "extra" force-included files cleanly alongside the real history.

## Considered options

1. **Stacked commits (the original prototype approach).** Start from the already
   committed code, add a commit with all working-tree changes on top, then
   another commit on top with the force-added, otherwise-gitignored files.
   Simple and linear, but mixes generated/large files into the same commit
   lineage as real code.
2. **Separate branch / independent commit for the extra files**, exploded out
   into the working tree separately on the remote end. Keeps generated artifacts
   out of the code lineage, at the cost of a second thing to transfer and
   reassemble.
3. **Alternate-index-based tree construction.** Build the tree(s) via an
   alternate index without ever materialising scratch commits on a real branch.

These options interact with how the transfer is performed and with how the
remote worktree is reassembled.

> **Note (ADR-0012):** the `refs/git-full-send/code` and `refs/git-full-send/extra`
> names used as examples below are now namespaced per *stream* —
> `refs/git-full-send/streams/<stream-id>/code` (and `…/extra`) — so multiple
> senders can coexist on one server. See
> [ADR-0012](0012-namespacing-managed-refs-per-stream.md); the encoding decision
> here is otherwise unchanged.

## Decision

Adopt **Option 2, refined** (Separate commit/tree for the extra files):

- Capture the working tree — staged **and** unstaged changes collapsed to the
  *current on-disk contents* (the remote never needs the index/worktree split) —
  as a single tree in **one commit parented on `HEAD`**, under a scratch ref
  (e.g. `refs/git-full-send/code`). Parenting on `HEAD` lets push negotiation
  share the whole committed history with the remote so only the working-tree
  delta crosses the wire.
- Capture the force-included files as a **separate tree/commit** under its own
  scratch ref (e.g. `refs/git-full-send/extra`), parented on the **previous**
  sync's extra commit so the prior (large) build outputs are retained as delta
  bases. This keeps generated artifacts out of the code lineage and gives the
  volatile big files their own predictable delta-base chain.
- **Synthesise the trees with gix's native tree `Editor`, not a scratch index.**
  The index-centric build (Option 3's framing) is the one approach here that
  forces a `git` shell-out
  ([Research 0001](../research/0001-gix-git-plumbing-vs-libgit2-capability-gap.md)).
- **Retain the previous sync's tips on both ends.** ADR-0008 makes only the
  remote *worktree* disposable; the object store persists, so keeping the prior
  `code`/`extra` tips alive is cheap and is the highest-impact encoding lever for
  predictable pack shapes (the intermittent slow transfers in
  [ADR-0005](0005-transfer-mechanism.md) are driven by delta-base *availability*,
  not commit topology).
- **Reassemble on the remote** as an authoritative, destructive overwrite
  ([ADR-0008](0008-remote-worktree-disposability.md)): check the `code` tree into
  the disposable worktree, then explode the `extra` tree over it
  (`git checkout-index`).

All three options meet the non-disturbance constraint via the same primitives
(scratch ref + alternate index / in-memory tree builder), so that was not the
differentiator; Option 1's downside (generated files in the code lineage) and
Option 3's index-centric shell-out cost decided it.

The full analysis — including why the encoding is only a minor lever on the
ADR-0005 pack-shape concern — is in
[Research 0002 — Encoding the sync state in Git](../research/0002-encoding-the-sync-state-in-git.md)
(2026-06-12).

## Consequences

- The client synthesises two scratch refs per sync (`code`, `extra`), pushed in
  one exchange; the remote does a checkout plus one explode step.
- Retained prior tips must be kept on both ends between syncs (cheap; object
  store persists) and are what keep transfers predictable.
- The transfer mechanism and the slow-transfer root-cause remain
  [ADR-0005](0005-transfer-mechanism.md)'s decision; *which* files are
  force-included and how that is declared remain
  [ADR-0007](0007-syncing-extra-gitignored-files.md)'s.
