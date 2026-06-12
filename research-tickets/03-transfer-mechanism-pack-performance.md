# Research: transfer mechanism + pack-performance root-cause

- Blocked by: #1 (Foundational ADRs)
- Source: [ADR-0005 — Transfer mechanism](../docs/adr/0005-transfer-mechanism.md)

## Context

Synthesised Git objects must move from client to server over a localhost
(SSH-tunnelled) connection. We need to choose how they're transferred and how the
server ingests them.

## Goal

1. Evaluate **`git push` → `git-daemon` receive-pack** on the server vs. a
   **native gix receive path** (including whether gix needs upstream work).
2. **Root-cause** the intermittent slow-transfer behaviour observed in the
   prototype — transfers of changed build outputs were sometimes surprisingly
   slow and sometimes fast, with **pathological pack shapes** (poor delta reuse
   relative to what the server already has) the leading suspicion.

## Deliverables

- A recommended transfer mechanism with rationale.
- An explanation of the performance variability and how the chosen mechanism
  gives predictable performance.

## Notes

- Tightly coupled to [ticket 02 (encoding the sync state)](02-encoding-sync-state.md).
