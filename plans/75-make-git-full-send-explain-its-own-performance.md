# Plan — #75: make `git-full-send` explain its own performance

`git-full-send` was wired into Stile's `dev` CLI as an alternative to Unison and
adds ~6s to every delegating command. Finding out *where* those 6s went took an
afternoon of shelling into the workstation, reading `tracing` output, timing
subcommands by hand, and counting refs with `git for-each-ref | wc -l`. Every
number involved was available at the point where the cost was incurred, and none
of it was recorded.

The goal is **not** to make it faster. The goal is that the next person who
finds it slow can point at the cause in one command, and that an integrator can
capture the same numbers programmatically.

## The observations to answer

Measured against a ~34,000-file monorepo worktree; server repo is the
workstation's own non-bare clone (`--repo` and `--worktree` the same path),
2.4 GB of objects, **28,709 refs**.

1. A no-op `update-worktree` costs 4s, essentially all in `read-tree`
   (`total_ms=4083 resolve_ms=8 read_tree_ms=4008 clean_ms=67`), and nothing in
   the record distinguishes "lots of real work" from "slow for no reason".
2. Each `git push` connection carries a ~3.1 MB ref advertisement, which the
   per-receive `bytes_out` lumps in with pack data — the advertisement looked
   like real transfer until someone counted refs by hand.
3. An orchestrator's readiness probe (connect, close) is logged as
   `WARN receive-pack exited non-zero status=…13… / received git push
   success=false`. A healthy probe rendered as a broken push.
4. A broken `objects/info/alternates` made every `git` invocation print
   `error: unable to normalize alternate object path: …`; gfs passed it through
   silently.
5. `dev` scrapes `sync`'s human stdout block because there is no machine-readable
   alternative, and the server's JSONL sink is a file on the *other* machine.

## Decisions taken with the operator

Four calls were made before planning (all four are recorded in ADR-0017/0018):

- **Count on-wire bytes on both ends**, which means interposing a socketpair
  between `git push` and the TCP socket on the client, mirroring the server's
  existing counting pump. Accepted with the risk understood (this is the
  transport of #44's splice deadlock and #57's fd leak).
- **Harvest `GIT_TRACE2_EVENT`** from the `git` children to decompose
  `read_tree_ms` from the inside, parsed strictly best-effort.
- **Curated `receive.hideRefs` is out of scope**: `doctor` names the ref count
  as the cause and suggests the remedies; the fix itself gets its own issue,
  because hiding refs also hides the client's thin-pack delta bases.
- **Delivered as four PRs** in dependency order off one plan.

## Design

### 1. One record, three surfaces (ADR-0013's separation, kept)

ADR-0013 established three complementary surfaces: `tracing` progress on stderr ·
a human summary block on stdout · the durable JSONL sink. That stays. What
changes is that the **JSONL record becomes a first-class output**, not only a
side-effect: the same struct the sink receives is what an operation returns to
the CLI, and `--json` prints it to stdout instead of the human block.

So the duplication between the client's private `SyncRecord` and its public
`SyncSummary` (the same numbers, spelled twice) collapses into one `Serialize`
struct, and the server's `update_worktree` starts returning its record instead of
discarding it.

Records gain a `schema` integer alongside `kind`/`ts_unix_ms`/`tool_version`,
because integrators are now invited to parse them and `tool_version` is `0.0.0`
for everything. This change is `schema: 2`; pre-`schema` lines are 1.

Fields are regrouped by *layer* (`code`, `extra`) and by *concern* (`encode`,
`wire`) rather than the flat `code_encode_ms`/`extra_push_ms` naming, so that
adding a number to a phase does not add a top-level field.

### 2. Making a phase timing self-explaining

**`update-worktree` / `read-tree`.** Four independent signals, each cheap:

- *Was the index warm?* `git` reports it for free: with `GIT_TRACE2_EVENT`, a run
  that read an existing index emits `data index read/cache_nr`; a cold run emits
  no read event at all. We also stat the index file for its size, and take
  `write/cache_nr` for the entry count afterwards.
- *A split inside `read-tree`.* The same trace2 stream gives
  `index:do_read_index`, `unpack_trees:traverse_trees`,
  `unpack_trees:unpack_trees` (the outer region — so file-writing is
  outer − traverse) and `index:do_write_index`. Verified against git 2.52.
- *How many paths actually differed.* `git diff-index --cached --name-status
  <tree>` before the checkout counts tree-vs-index differences without a single
  `lstat`. Its own cost is timed and recorded, so the measurement never hides
  inside the thing it measures.
- *Is this a strict no-op?* The combined tree id of each successful checkout is
  persisted in the per-worktree state dir. If the target tree equals the last one
  checked out, the tree side of the work is definitionally zero, and a large
  `read_tree_ms` is visibly *not* explained by work done.

`clean`'s output is already captured, so counting its `Removing …` lines is free.

The two genuinely expensive measurements — how many paths differ from what is on
*disk* (an `lstat` per index entry) and the worktree's file count (a full walk) —
go behind `--measure-worktree`, and the docs say so.

**Client `encode`.** Split into index load · status walk · hashing · tree write ·
commit+ref, and record index entries, status items considered, files hashed and
bytes hashed. "Files hashed" is the honest count: `gix`'s `index_as_worktree`
applies git's stat shortcut, so files *stat'd* is the index entry count and files
*hashed* is what the delta actually forced us to read.

**Extra selection walk.** `select.rs` already tracks entered directories under
`cfg(test)`; promote that to always-on counters (directories entered, pruned,
paths considered, paths selected) and add the count of unanchored patterns. The
walk already warns that an unanchored pattern forces an exhaustive walk; this
makes the *cost* of that warning visible.

### 3. Separating protocol overhead from payload

The receive-pack exchange is pkt-line framed, and both interesting boundaries are
the **first flush-pkt** in each direction:

- server → client: ref advertisement, `0000`, then (later) the report-status;
- client → server: ref-update commands, `0000`, then the raw pack.

So one shared, direction-agnostic counter in `gfs-common` — feed it the bytes,
get back `{pre_flush_bytes, pre_flush_pkts, post_flush_bytes}` — splits both
directions on both ends, and `pre_flush_pkts` on the server's outbound side *is*
the advertised ref count. It is a state machine over the 4-byte length headers,
so chunk boundaries don't matter.

The server already pumps both directions through counting threads (ADR-0013), so
it only swaps the counter. The client currently hands `git push` two dups of the
real socket, so it gains a socketpair: `git` gets dups of one end, two pump
threads move bytes between the other end and the TCP socket. The pumps reuse the
same explicit read/write loop as the server's — deliberately *not*
`std::io::copy`, whose splice fast path deadlocked this exchange in #44 — and the
`pre_exec` `FD_CLOEXEC` handling of #57 is unchanged in shape, just applied to
the socketpair dups.

### 4. A healthy probe is not a failure

Classify each connection's outcome instead of reducing it to `success: bool`:

| outcome | condition | log |
| --- | --- | --- |
| `updated` | exited 0, refs accepted | `info` |
| `no_op` | exited 0, no ref commands (a flush-only probe) | `debug` |
| `probe` | no ref commands received at all, however it ended (incl. SIGPIPE) | `debug` |
| `rejected` | the namespace hook declined a ref | `warn` |
| `failed` | anything else | `warn` |

That silences the observed `WARN … status=…13… stderr=` / `success=false` pair
for a connect-and-close probe: no commands arrived, so nothing was being pushed,
so nothing failed.

And it should not be necessary to fake a push at all, so `probe` becomes a
command: connect, read the advertisement, send a flush-pkt, exit. Verified that
`git receive-pack` exits **0** on a flush-only conversation, so this is a real,
clean protocol exchange rather than an abort. It also answers observation 2 from
the client side, on demand: `probe` reports the advertisement's size and ref
count without touching the push path.

### 5. Noticing that the repo itself is the problem

`doctor --repo <path> [--worktree <path>]` runs the checks that predictably hurt
and that the operator can act on: ref count and the advertisement bytes that
implies (estimated from the ref names — the wire cost of a ref is
`46 + len(name)`), broken or unreachable `alternates` entries, pack/loose object
layout, `receive.autogc`, whether the target worktree is the repo's own working
tree (the case measured above), the per-worktree index's state, and unanchored
include patterns. Each check reports `ok`/`warn`/`error` with a remedy; an
`error` exits non-zero so an orchestrator can gate on it.

The two cheap, high-value checks — ref count and broken alternates — also run
once at `listen` startup, because the operator who most needs them is the one who
did not think to run `doctor`.

### 6. Aggregation

`metrics --repo <path> [--kind K] [--last N]` reads the sink and prints count and
p50/p95/max per numeric field, so `docs/operating.md` can stop suggesting
hand-written `jq`. It flattens records to dotted keys generically rather than
knowing each record's shape, so it survives a schema change.

## Delivery

Four PRs, each independently useful, in dependency order.

**PR 1 — one record shape, and `--json`** (this plan + ADRs land here)
- `gfs_common::metrics`: a shared `Envelope` (`kind`, `schema`, `ts_unix_ms`,
  `tool_version`) flattened into each record; `SCHEMA_VERSION`.
- Client: `SyncSummary` *becomes* the record (`Serialize`, nested by layer);
  delete the parallel `SyncRecord`.
- Server: `UpdateWorktreeReport` made public and returned from
  `update_worktree`; the sink write moves to the same value.
- CLI: `--json` on `sync` and `update-worktree`, printing the record and
  suppressing the human block; a human summary block for `update-worktree`,
  which had none.
- Docs: ADR-0017, ADR-0018, README index, `docs/operating.md`.

**PR 2 — decompose the phase timings**
- `gfs_common::trace2`: run a `git` child with `GIT_TRACE2_EVENT` to a temp file
  and harvest region durations + data counters, best-effort.
- Server: index warm/cold + entries + bytes, tree-vs-index changed paths (timed),
  last-checked-out-tree marker, `clean` removals, the read-tree split, and
  `--measure-worktree` for the expensive pair.
- Client: encode sub-phases and hashing counts; selection walk counters.

**PR 3 — protocol overhead vs payload, and probing**
- `gfs_common::pktline`: the split counter, unit-tested across chunk boundaries.
- Server: split both directions, record `refs_advertised`, classify outcomes, fix
  the log levels.
- Client: the socketpair interposer; per-push wire stats in the record.
- CLI: `probe`.

**PR 4 — doctor and aggregation**
- `gfs_server::doctor` + the `doctor` command; the cheap subset at `listen`
  startup.
- `metrics` command.
- `docs/operating.md`: a "why is it slow" section replacing the `jq` snippet.

## Verification

A synthetic repro, built by a test-support helper rather than a real workstation:
a repo with tens of thousands of refs and a large worktree.

1. `update-worktree` twice with no changes between: the second run's record shows
   `tree_unchanged: true` and zero changed paths against a large `read_tree_ms`.
2. Ref-advertisement bytes are reported separately, track the ref count, and
   collapse against a few-ref repo carrying the same objects.
3. Connect to `listen` and disconnect immediately: no `WARN`. Same for `probe`.
4. `objects/info/alternates` pointing at a missing path: `doctor` reports it, and
   `listen` says so at startup.
5. `--json` parses, and carries every number the human summary shows plus the new
   ones.

Automated equivalents of each land as tests; the ref-scale numbers (1) and (2)
also get a manual run recorded in the PR, since a 28k-ref fixture is too slow for
CI.

## Out of scope (follow-ups)

- **Curated `receive.hideRefs`** — the actual fix for observation 2, and the
  single biggest available win, but it hides the delta bases `--thin` negotiates
  against, so it needs the benchmark harness of #51 rather than a one-line
  change. Filed separately.
- **Failure-path client metrics** and **sink rotation** remain ADR-0013 non-goals.
