# git-full-send

A tool for syncing a developer's Git working state — committed code, working-tree
(staged & unstaged) changes, and a deliberately force-included set of normally
gitignored files (e.g. locally-built artifacts and per-user config) — from a
client machine to a remote workstation, using Git to move the data.

## Architecture decisions

Significant decisions are recorded as ADRs under [`docs/adr/`](docs/adr/). Start
with [ADR-0000](docs/adr/0000-record-architecture-decisions.md), which describes
the ADR process and conventions, and see [`docs/adr/README.md`](docs/adr/README.md)
for the index.

When making a significant architectural decision, add a new numbered ADR
(`docs/adr/NNNN-kebab-title.md`) and update the index. Decisions that aren't
settled yet are recorded as `proposed` ADRs that capture the constraints and
options, with `⚠ Research task needed` callouts for follow-up work.
