# ADR-0006 — Transport & connectivity

- Status: accepted; amended by
  [ADR-0019](0019-authenticating-the-receive-pack-connection.md) — the receiver
  now authenticates pushes with a shared secret, so the third Decision bullet
  ("we do not build our own transport security or authentication") no longer
  holds for authentication; the tunnel remains the whole of confidentiality
- Date: 2026-06-12

## Context

The client and server communicate over a network connection to transfer Git
objects ([ADR-0005](0005-transfer-mechanism.md)). We need to decide how that
connection is established and secured. Building our own transport security
(authentication, encryption) is a significant undertaking, and developers
already have SSH access to their remote workstations.

## Decision

- The server **binds to localhost only**.
- Connectivity from the client is achieved via **manual SSH tunnelling**, set up
  by the operator.
- We do **not** build our own transport security or authentication initially —
  we lean entirely on the SSH tunnel for confidentiality and access control.

## Consequences

- There is no in-tool auth/encryption to implement or audit for now; the trust
  boundary is the SSH tunnel.
- Setup requires the operator to establish the tunnel before syncing; this is an
  accepted manual step initially.
- First-class transport security (e.g. built-in authentication so manual
  tunnelling isn't required) is **deferred** and is a candidate for a future
  ADR if and when we want to remove the manual step.
