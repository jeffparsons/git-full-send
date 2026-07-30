# Architecture Decision Records

This directory holds the architecture decision records (ADRs) for
`git-full-send`. See [ADR-0000](0000-record-architecture-decisions.md) for the
process and conventions.

Statuses: `proposed` (constraints/options recorded, decision deferred),
`accepted`, `deprecated`, `superseded by ADR-NNNN`.

| ADR | Title | Status |
| --- | --- | --- |
| [0000](0000-record-architecture-decisions.md) | Record architecture decisions | accepted |
| [0001](0001-language-runtime-and-core-crates.md) | Language, runtime & core crates | accepted |
| [0002](0002-git-manipulation-strategy.md) | Git manipulation strategy | accepted |
| [0003](0003-client-server-architecture.md) | Client/server architecture | accepted |
| [0004](0004-encoding-the-sync-state-in-git.md) | Encoding the sync state in Git | accepted |
| [0005](0005-transfer-mechanism.md) | Transfer mechanism | accepted |
| [0006](0006-transport-and-connectivity.md) | Transport & connectivity | accepted |
| [0007](0007-syncing-extra-gitignored-files.md) | Syncing extra (normally-gitignored) files | accepted |
| [0008](0008-remote-worktree-disposability.md) | Remote worktree disposability & sync authority | accepted |
| [0009](0009-working-tree-fidelity-for-the-code-commit.md) | Working-tree fidelity for the `code` commit | accepted |
| [0010](0010-receive-pack-transport-wiring.md) | `receive-pack` transport wiring | accepted |
| [0011](0011-worktree-reassembly-mechanics.md) | Worktree reassembly mechanics | accepted |
| [0012](0012-namespacing-managed-refs-per-stream.md) | Namespacing managed refs per stream | accepted |
| [0013](0013-recording-operation-metrics.md) | Recording operation metrics | accepted |
| [0014](0014-forgetting-a-stream.md) | Forgetting a stream | accepted |
| [0015](0015-ttl-based-reaping-of-stale-streams.md) | TTL-based reaping of stale streams | accepted |
| [0016](0016-clean-spares-undelivered-gitignored-files.md) | `clean` spares gitignored files it didn't deliver | accepted |
| [0017](0017-making-operation-cost-self-explaining.md) | Making operation cost self-explaining | accepted |
| [0018](0018-liveness-and-repo-health-surfaces.md) | Liveness and repo-health surfaces | accepted |
| [0019](0019-authenticating-the-receive-pack-connection.md) | Authenticating the `receive-pack` connection | accepted |

## Open research tasks

The `proposed` ADRs above flag research that will be split into their own
tickets:

- ~~gix / `git` plumbing vs. libgit2 capability gap analysis ([ADR-0002](0002-git-manipulation-strategy.md)).~~
  **Done** — see [Research 0001](../research/0001-gix-git-plumbing-vs-libgit2-capability-gap.md).
- ~~Sync-state encoding in Git ([ADR-0004](0004-encoding-the-sync-state-in-git.md)).~~
  **Done** — see [Research 0002](../research/0002-encoding-the-sync-state-in-git.md).
- ~~Transfer mechanism + pack-performance root-cause ([ADR-0005](0005-transfer-mechanism.md)).~~
  **Done** — see [Research 0003](../research/0003-transfer-mechanism-and-pack-performance.md).
- ~~Force-include configuration mechanism ([ADR-0007](0007-syncing-extra-gitignored-files.md)).~~
  **Done** — see [Research 0004](../research/0004-force-include-configuration-mechanism.md).
