# ADR-0017 — Making operation cost self-explaining

- Status: accepted
- Date: 2026-07-27
- Amends: [ADR-0013](0013-recording-operation-metrics.md) (what is recorded, and
  the record's status as an output rather than only a side-effect),
  [ADR-0010](0010-receive-pack-transport-wiring.md) (the client's socket wiring)

## Context

[ADR-0013](0013-recording-operation-metrics.md) decided *where* per-operation
data lands (a JSON Lines sink per side) and recorded per-phase wall times plus
per-layer file/byte counts. It deliberately left "analysis / reporting tooling"
out of scope: the decision only *produces* the data.

Field use since then (issue #75) showed the data is not enough to act on. Against
a ~34,000-file monorepo worktree on a server repo with 28,709 refs:

- a **no-op** `update-worktree` recorded `read_tree_ms=4008` — a number that says
  *that* it was slow and nothing about *why*. Whether the per-worktree index was
  warm, how many paths actually needed writing (zero), and how large the index
  was are all knowable where the number is produced, and none were recorded;
- `bytes_out` per receive is one lump, so a ~3.1 MB ref advertisement was
  indistinguishable from pack data — the overhead looked like real transfer until
  someone counted refs by hand;
- an integrator (`dev`) scrapes the human stdout block, because the numbers are
  otherwise only in a file, on whichever machine ran the operation — awkward when
  a client wants the *server's* checkout numbers.

The tool records what it costs but cannot explain it. That is the gap.

## Decision drivers

- **A number must carry its own explanation.** A duration without the size of the
  work it did is not actionable; the two belong in the same record.
- **Measurement stays cheap, and its cost is visible.** Anything expensive is
  opt-in, and any measurement we do pay for is itself timed.
- **Never load-bearing** (ADR-0013). A measurement that fails degrades to a
  missing field; it never fails the operation.
- **The three surfaces stay distinct** (ADR-0013 as refined by issue #53): stderr
  progress log · stdout human summary · durable JSONL record.

## Decision

### The record is an output, not only a side-effect

The value written to the sink is the value the operation *returns*, and `--json`
prints it verbatim to stdout in place of the human block (which stays the
default). One shape, three consumers: the sink, the CLI's formatter, and an
integrator's parser.

This collapses the client's duplicated `SyncRecord`/`SyncSummary` pair into one
`Serialize` struct, and makes the server's `update_worktree` return its record
rather than discard it — which also solves the cross-machine case, since a client
driving a remote `update-worktree --json` over SSH gets the server's numbers on
stdout.

Records carry a **`schema` integer** beside `kind`/`ts_unix_ms`/`tool_version`.
Parsers are now invited, `tool_version` is `0.0.0` for every build so far, and
this change reshapes the fields; the reshaped records are `schema: 2`, and lines
without the field are 1.

Fields are grouped by layer (`code`, `extra`) and concern (`encode`, `wire`,
`index`, `changed`) rather than flattened into `code_encode_ms`-style names, so a
new number inside a phase does not add a top-level field.

### Cost is decomposed at the point it is incurred

For each phase, record the size of the work alongside its duration, choosing the
cheapest signal that is honest:

- **`git`'s own instrumentation, harvested.** Each `git` child that dominates a
  phase runs with `GIT_TRACE2_EVENT` pointed at a per-invocation temp file, and
  the region durations and data counters are parsed out afterwards. For
  `read-tree --reset -u` that yields the split the outer timing cannot see —
  index load (`index:do_read_index`), tree resolution
  (`unpack_trees:traverse_trees`), file writing (the outer
  `unpack_trees:unpack_trees` minus traversal), index write
  (`index:do_write_index`) — plus the index entry count (`read/cache_nr`,
  `write/cache_nr`) and, for free, **whether the index was warm**: a cold run
  emits no read event at all.

  Parsing is strictly best-effort and every harvested field is optional. trace2's
  event stream is a diagnostic surface, not an API: an unrecognised git version
  yields no sub-timings and the outer wall-clock numbers still stand.

- **Cheap predictions where git reports nothing.** `git` does not report how many
  paths a checkout wrote, so `update-worktree` counts tree-vs-index differences
  with `diff-index --cached` (no `lstat`) before the checkout, and times that
  measurement so its own cost is on the record.

- **A no-op is proven, not inferred.** Each successful checkout persists its
  combined tree id in the per-worktree state dir. A target tree equal to the last
  one checked out makes the tree side of the work definitionally zero — so a
  large `read_tree_ms` is visibly *not* explained by work done.

- **Expensive measurements are opt-in.** Comparing against the *worktree* (an
  `lstat` per index entry) and counting the worktree's files (a full walk) are
  proportional to the tree, so they sit behind `--measure-worktree`.

### Protocol overhead is separated from payload, on both ends

The receive-pack exchange is pkt-line framed and the interesting boundary in each
direction is the **first flush-pkt**: server → client it ends the ref
advertisement, client → server it ends the ref-update commands and begins the
raw pack. One shared counter — a state machine over the 4-byte length headers,
so chunk boundaries are irrelevant — returns
`{pre_flush_bytes, pre_flush_pkts, post_flush_bytes}` for any stream, and on the
server's outbound side `pre_flush_pkts` *is* the advertised ref count.

The server already pumps both directions through counting threads (ADR-0013), so
it only swaps in the counter. The client hands `git push` two dups of the real
socket (ADR-0010), so it gains an **interposing socketpair**: `git` gets dups of
one end; two pump threads move bytes between the other end and the TCP socket,
counting as they go.

That is a deliberate change to the most delicate code in the tool — the transport
of #44's splice deadlock and #57's `FD_CLOEXEC` leak — accepted because the
client is where a slow sync is *felt*, and telling a developer to read a JSONL
file on the far side of an SSH tunnel is not "one command". The risk is contained
by reusing the server's proven mechanics: the same explicit read/write loop
(never `std::io::copy`, whose splice fast path caused #44) and the same
`pre_exec` clearing of `FD_CLOEXEC` in the forked child only (#57), now applied
to the socketpair dups. ADR-0005's on-wire format is untouched: the bytes
observed are the identical raw stream.

## Consequences

- One localhost-bandwidth userspace copy is added to the client push, matching
  the one ADR-0013 already accepted on the server, and negligible against git's
  pack work.
- `metrics.jsonl` lines get larger and change shape at `schema: 2`. The sink has
  no rotation (ADR-0013 non-goal) and is safe to delete.
- A dependency on git's trace2 event names, held loosely: they may drift, and the
  failure mode when they do is missing sub-timings.
- The tool now spends a small, measured amount of effort measuring itself on
  every `update-worktree` (the tree-vs-index diff). The record shows what that
  cost, so the trade is auditable rather than assumed.
- A `metrics` subcommand aggregates the sink (count, p50/p95/max per numeric
  field, flattened generically so it survives a schema change), closing
  ADR-0013's deferred "analysis / reporting tooling".

### Alternatives considered

- **Server-side byte accounting only, with the client inferring.** Rejected: it
  leaves the person running `sync` unable to see their own sync's overhead, which
  is the whole complaint.
- **`GIT_TRACE_PACKET` / `GIT_TRACE=1` instead of trace2.** Rejected: text
  formats with no stability story at all, and packet tracing is expensive enough
  to distort what it measures.
- **Deriving the advertisement size from a ref count instead of measuring it.**
  Rejected as the primary source — an estimate cannot show a *changed*
  advertisement — though `doctor` does estimate it (ADR-0018) where no connection
  is in hand.
- **Recording nothing more and documenting the `jq` incantations instead.**
  Rejected: the numbers being sought (index warmth, paths written, advertisement
  bytes) do not exist in the sink at all, so no amount of querying finds them.
