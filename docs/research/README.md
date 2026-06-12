# Research reports

This directory holds investigation reports for `git-full-send` — deeper
write-ups backing the architecture decisions in [`../adr/`](../adr/). A report is
a **dated snapshot** of research; unlike an ADR it isn't a decision, and its
findings can age (especially capability surveys of fast-moving dependencies).
Where a report's findings settle or revise a decision, the relevant
[ADR](../adr/README.md) is updated to link back to it.

| Report | Title | Date | Backs |
| --- | --- | --- | --- |
| [0001](0001-gix-git-plumbing-vs-libgit2-capability-gap.md) | gix / `git` plumbing CLI vs libgit2 capability gap analysis | 2026-06-12 | [ADR-0002](../adr/0002-git-manipulation-strategy.md) |
| [0002](0002-encoding-the-sync-state-in-git.md) | Encoding the sync state in Git | 2026-06-12 | [ADR-0004](../adr/0004-encoding-the-sync-state-in-git.md) |
| [0003](0003-transfer-mechanism-and-pack-performance.md) | Transfer mechanism & pack-performance root-cause | 2026-06-12 | [ADR-0005](../adr/0005-transfer-mechanism.md) |
| [0004](0004-force-include-configuration-mechanism.md) | Force-include configuration mechanism | 2026-06-12 | [ADR-0007](../adr/0007-syncing-extra-gitignored-files.md) |
