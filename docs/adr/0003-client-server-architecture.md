# ADR-0003 — Client/server architecture

- Status: accepted
- Date: 2026-06-12

## Context

`git-full-send` moves a developer's working state from a client machine (a
powerful MacBook) to a remote workstation (Linux/EC2) where builds and
containers run. The client and the remote have distinct responsibilities, and in
practice the remote's "receive the data" and "use the data for a build" steps
happen at different times and are driven by different processes.

## Decision

The tool has distinct **client** and **server** roles.

### Server (remote)

- Runs on the remote machine, configured with the target **Git repository +
  worktree directory** it manages.
- Binds **localhost only** (see [ADR-0006](0006-transport-and-connectivity.md)).
- Exposes two separate operations / subcommands:
  - **`listen`** — a long-running server that accepts and serves **many** sync
    requests, receiving transferred objects. It runs **until explicitly shut
    down**, rather than handling a single transfer and exiting.
  - **update worktree** — checks out the synced state into the configured
    worktree. It is invoked independently of `listen`; in practice a separate
    build-orchestration process triggers it when it is ready to use the synced
    tree. This separation lets the working tree be updated on demand rather than
    on every transfer.

### Client

- Synthesises the sync state into Git objects and sends it to the server.
- **Never touches** the user's current branch, main index, or working tree (see
  [ADR-0004](0004-encoding-the-sync-state-in-git.md) for how it does this with
  scratch refs / an alternate index).

## Consequences

- Receiving data and updating the worktree are decoupled, so a build
  orchestrator can pull the latest synced state into the worktree at a moment of
  its choosing.
- The server is a persistent process with a lifecycle (start `listen`, serve,
  shut down) rather than a one-shot command.
