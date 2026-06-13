Done — all 11 selected opportunities are filed as standalone GitHub issues:

| ID | # | Title | Label |
| --- | --- | --- | --- |
| A1 | #44 | Add CI: fmt, clippy, test, and an MSRV build gate | enhancement |
| A2 | #45 | Supply-chain checks in CI (cargo-deny / cargo-audit) | enhancement |
| A3 | #46 | Add LICENSE files (MIT and Apache-2.0) | documentation |
| B1 | #47 | Bound server connection concurrency and add graceful shutdown | enhancement |
| B2 | #48 | Stream lifecycle: a `forget-stream` command (and optional TTL reaping) | enhancement |
| B3 | #49 | Lock worktree updates against concurrent runs | enhancement |
| C1 | #50 | Per-chain delta policy: split the `extra` and `code` pushes | enhancement |
| C2 | #51 | Transfer benchmark harness for the delta-base design | enhancement |
| C3 | #52 | Property test for the force-include prune invariant | enhancement |
| D1 | #53 | Report a sync summary (bytes, object/file counts, durations) | enhancement |
| F1 | #54 | Fix two stale docs (server test docstring; operating.md performance note) | documentation |

All are standalone (`--no-block`) and carry the intended label. #53 (D1) is scoped to the reporting surface and cross-links #42, with the "fold into #42" note preserved. E1, E2, F2 were left out per your selection.

The PR adds only `plans/40-full-audit-and-review.md` (the audit + selection + the issue bodies as filed); the issues were created via `ghwf` as a side effect. PR #43 title/body updated to match.
