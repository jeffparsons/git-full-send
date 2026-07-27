# ADR-0018 — Liveness and repo-health surfaces

- Status: accepted
- Date: 2026-07-27
- Relates to: [ADR-0017](0017-making-operation-cost-self-explaining.md)

## Context

Two failures of the tool to say what it knows, both found while integrating
`git-full-send` into an orchestrator (issue #75).

**A readiness probe reads as a broken push.** An orchestrator must know when
`listen` is accepting before it pushes, and the only way to ask is to connect.
Connecting and closing produces:

```text
WARN receive-pack exited non-zero status=ExitStatus(unix_wait_status(13)) stderr=
INFO received git push success=false bytes_in=0 bytes_out=274 refs=0
```

Exit status 13 is `receive-pack` taking SIGPIPE when the prober hangs up. That is
a healthy liveness check rendered as a failure, once or twice per invocation,
actively misleading anyone reading the log for a real problem. The server has
everything it needs to know better: **no ref-update commands ever arrived**, so
nothing was being pushed and nothing failed.

**A misconfigured repo is passed through silently.** The server repo had an
`objects/info/alternates` entry pointing at a path that no longer existed. Every
`git` invocation printed `error: unable to normalize alternate object path: …`
and carried on; `git-full-send` neither noticed nor said anything. The same class
of problem covers the 28,709 refs that made every connection carry a 3.1 MB
advertisement: knowable, actionable, and never reported.

## Decision drivers

- **A log level is a claim.** `warn` must mean "a human should look at this", or
  it stops meaning anything.
- **Don't make integrators fake protocol exchanges** to ask a simple question.
- **Report what the operator can act on**, with the remedy, not raw facts.
- Stay within the existing surfaces (ADR-0013): stderr log · stdout human
  summary · JSONL record.

## Decision

### Classify the outcome of a connection

Replace the per-receive `success: bool` with an explicit outcome, derived from
what the exchange actually contained rather than only from the exit status:

| outcome | condition | log level |
| --- | --- | --- |
| `updated` | exited 0, the namespace hook accepted refs | `info` |
| `no_op` | exited 0, no ref-update commands (a flush-only conversation) | `debug` |
| `probe` | no ref-update commands arrived at all, however it ended — including a SIGPIPE hang-up | `debug` |
| `rejected` | the namespace hook declined a ref | `warn` |
| `failed` | anything else | `warn` |

"No ref-update commands arrived" is exactly the pre-flush pkt count of the
inbound stream (ADR-0017) being zero, so the classification falls out of the byte
accounting already being done. `success` stays in the record as a derived
convenience; `outcome` is the field to read.

### Make liveness a first-class question

`probe --remote HOST:PORT` connects, reads the ref advertisement, sends a
flush-pkt, and exits — a complete, well-formed conversation that `git
receive-pack` terminates with **exit 0**. No orchestrator needs to fake a push,
and the server logs a clean `no_op` rather than a corpse.

It doubles as the client-side answer to "why is every connection expensive":
`probe` reports the advertisement's byte size and ref count, measured on the
wire, without touching the push path.

### `doctor`: report the repo problems that predictably hurt

`doctor --repo <path> [--worktree <path>]` runs the checks whose failures the
operator can actually act on, each reporting `ok`/`warn`/`error` **with a
remedy**:

- **ref count**, and the advertisement bytes it implies — estimated from the ref
  names, since a ref costs `4` (length header) `+ 40` (oid) `+ 1 + len(name) + 1`
  on the wire — warning once that cost dominates a small push;
- **`alternates`** entries that are missing or unreadable;
- **object/pack layout**: pack count and loose-object pressure;
- **`receive.autogc`**, which the receive window relies on being off;
- **whether the target worktree is the repo's own working tree**, and whether the
  two share an object store — the configuration that produced the measurements
  behind ADR-0017;
- **the per-worktree index**: present, its size and entry count;
- **unanchored force-include patterns**, which force an exhaustive worktree walk
  (ADR-0007).

An `error` exits non-zero so an orchestrator can gate on it; a `warn` does not.
`--json` emits the checks structurally, like every other command (ADR-0017).

The two cheapest, highest-value checks — ref count and broken `alternates` — also
run once at `listen` startup, because the operator who most needs them is the one
who did not think to run `doctor`.

## Consequences

- A connect-and-close probe is now invisible at default log level, and `warn`
  regains its meaning. A genuine failed push still warns.
- Reading `success` alone under-reports: a `probe` is `success: false` and
  perfectly healthy. Consumers should read `outcome`; the record is `schema: 2`
  (ADR-0017), so the change is announced.
- `listen` startup does one pass over the repo's refs. On the 28,709-ref repo
  that is tens of milliseconds, once per process.
- `doctor` reports the ref-count problem but does not fix it. The obvious fix —
  a curated `receive.hideRefs`, since we already spawn `receive-pack` ourselves —
  is deliberately *not* taken here: the advertised refs are also the delta bases
  the client's `--thin` push negotiates against (ADR-0005), so hiding them trades
  a smaller advertisement for a fatter pack. It needs the benchmark harness of
  issue #51 and is filed as its own issue.

### Alternatives considered

- **A dedicated health endpoint or a second port.** Rejected: more surface, more
  configuration, and a port that answers when `receive-pack` would not is a worse
  liveness signal than the real thing.
- **Suppressing the warning by pattern-matching exit status 13.** Rejected as
  treating the symptom: the classification we want is "was anything actually
  pushed", which is also the right basis for `rejected` and `no_op`.
- **Running `doctor`'s checks before every sync.** Rejected: the checks are
  cheap, not free, and most of what they find changes on the timescale of repo
  configuration, not of a sync. The two that are nearly free run at `listen`
  startup instead.
