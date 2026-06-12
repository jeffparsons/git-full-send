# ADR-0008 — Remote worktree disposability & sync authority

- Status: accepted
- Date: 2026-06-12

## Context

When the server updates its worktree ([ADR-0003](0003-client-server-architecture.md))
to match the synced state, there is a question of how to treat whatever is
already in that worktree — files the client deleted, local modifications made on
the remote, etc. Getting this wrong would mean either failing to faithfully
reproduce the client's state or trying to preserve remote-side state that nobody
asked us to keep.

## Decision

The **remote worktree is always disposable.** The synced client state is
authoritative, and updating the remote worktree is an **authoritative,
destructive overwrite**: nothing on the remote is precious, and anything there
may be stomped over to match the client.

Concretely: the worktree update brings the remote into line with the synced
state without trying to preserve remote-side changes, deletions, or additions.

## Optional diagnostics (nice-to-have)

We *may* record what had to be deleted or overwritten on the remote — purely for
diagnostics / debugging. This is explicitly **not** a mechanism for preserving
remote-side changes; it is only to make it easier to understand what a sync did.

## Consequences

- The worktree-update logic can be simple and unconditional — make the remote
  match the synced state — without merge/conflict handling against remote-local
  edits.
- Operators must treat the remote worktree as throwaway and never store anything
  there they aren't willing to lose.
