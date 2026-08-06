# ADR-0020 — Curating the ref advertisement with `receive.hideRefs`

- Status: accepted
- Date: 2026-08-06
- Refines: [ADR-0010](0010-receive-pack-transport-wiring.md) (the `receive-pack`
  invocation), [ADR-0018](0018-liveness-and-repo-health-surfaces.md) (`doctor`'s
  ref-count remedy)

## Context

A server repo that is the workstation's own clone can hold tens of thousands of
refs — on the repo measured for issue #79, 28,709 of them, 28,696 being
`refs/remotes/origin/*`. Every `receive-pack` connection advertises all of them
before a byte of the developer's data moves: ~3.1 MB per connection, and a sync
makes two connections. Over a real (non-loopback) link that advertisement was
measured at up to 1.1 s of pure latency per connection.

The obvious fix — we spawn `receive-pack` ourselves (ADR-0010), so we can pass
`-c receive.hideRefs=…` and collapse the advertisement — was held back by a
delta-base worry: the advertised refs are also what the client's `--thin` push
deltas against (ADR-0005, Research 0003). Hiding the wrong refs could trade a
megabytes-sized advertisement for a megabytes-sized pack.

Issue #79's proposed pattern (hide everything except `refs/heads/*` and
`refs/git-full-send/*`) assumed the ref bulk is PR-style refs while
`refs/heads/*` carries the delta bases. Measurement against the real repo showed
the premise is backwards for the workstation-clone shape:

- **Steady state** (retained `sent/*` pin): pack bytes are *identical* across
  every hiding variant, because the client deltas against its own retained pin,
  not the advertisement. Hiding is pure win: ~6.3 MB and ~2.3 s saved per sync.
- **Cold** (no gfs refs on the server): pack size is governed by how *fresh* the
  best advertised ref is, not how many are advertised. On a workstation clone,
  `refs/heads/*` is whatever was last checked out (12 days stale on the measured
  repo) and `refs/remotes/origin/*` is the only thing tracking anything current.
  Against a freshly fetched clone, the issue's pattern pushed a **14.0 MB** pack
  where the un-hidden baseline pushed **195 bytes** — the pattern hides exactly
  the refs that carry the usable delta base.
- Advertising one *current* ref alongside the issue's pattern
  (`!refs/remotes/origin/master`) gave the best of both: a 196-byte pack **and**
  a ~1 KB advertisement — a cold sync in 1.0 s versus baseline's 1.6 s / 6.4 MB
  of advertisement, and versus 16.8 s / 13.6 MB either way on an
  eleven-days-stale clone.

So the decision is not *whether* to hide (that is settled) but how the unhidden
**anchor set** is derived.

### Deriving the anchor: options considered

1. **The N most recent refs by committer date.** Self-tuning, but measured at
   ~350 ms per derivation on the 29k-ref repo (no commit-graph) — a real cost on
   every connection — and worse, the freshest refs on a busy monorepo are
   just-pushed agent/scratch branches. A delta base is only usable if the
   *client* also has the commit, and the most recently pushed refs are precisely
   the ones a client that last fetched an hour ago is missing. The remote's
   default branch is the one ref every fetching clone is guaranteed to share.
2. **A fixed configurable ref name.** Degrades silently: a configured branch
   that stops being fetched, or is renamed, quietly reverts cold syncs to
   full-pack size.
3. **The default branch of each remote, resolved at connection time.**
   `refs/remotes/<remote>/HEAD` names it and resolves in ~4 ms even on the
   29k-ref repo. It is exactly the ref that measured as the perfect base, it is
   fresh on any clone that fetches at all, and it needs no configuration. Its
   only weakness is that `refs/remotes/<remote>/HEAD` exists only on cloned (or
   `git remote set-head`) repos — `git fetch` alone never creates it.

## Decision

`listen` passes a curated `receive.hideRefs` to every `receive-pack` it spawns,
**on by default**:

```text
-c receive.hideRefs=refs/
-c receive.hideRefs=!refs/git-full-send/
-c receive.hideRefs=!refs/heads/
-c receive.hideRefs=!<anchor>          (one per derived anchor)
```

The anchor set is **derived per connection** (option 3):

- `refs/git-full-send/` — the stream refs, so a client that has lost its local
  `sent/*` pin can still delta against its previous pushes, and other streams
  from the same client remain visible bases.
- `refs/heads/` — the repo's own branches. Cheap (a handful of refs on both the
  workstation-clone and dedicated-repo shapes), and on a dedicated sync repo
  they can be the only refs there are.
- For each configured remote: the symref target of `refs/remotes/<remote>/HEAD`
  (e.g. `refs/remotes/origin/master`); if that symref does not exist, whichever
  of `refs/remotes/<remote>/main` / `refs/remotes/<remote>/master` exists.
  A remote where none of these resolve contributes no anchor — `doctor` reports
  that, rather than the connection path guessing harder.

Derivation is a few ref lookups per connection (single-digit milliseconds), so
there is no cache to invalidate and a `git fetch` or `git remote set-head`
between syncs is picked up immediately.

Escape hatches, because the anchor derivation is a heuristic about which refs a
client can delta against:

- `--no-hide-refs` restores the full advertisement.
- `--advertise-ref <prefix>` (repeatable) appends extra unhide patterns for
  repos whose useful base lives somewhere unusual.

`doctor` gains an `anchors` check: it reports the derived anchor set, warns when
a remote contributes no anchor (remedy: `git remote set-head <remote> --auto`),
and warns when the freshest anchor's commit is old enough that a cold sync will
pay for it (remedy: fetch). The existing ref-count check's first-line remedy
becomes this hide (on by default, so the remedy is mostly "already handled");
the dedicated-repo-with-`alternates` shape remains as the second-line remedy for
what hiding cannot fix (e.g. the cost non-gfs tooling pays to enumerate refs).

## Consequences

- Steady-state syncs stop paying the advertisement entirely (measured: 3.7 s →
  1.3 s per sync on the real repo and link) with byte-identical packs.
- Cold syncs are governed by anchor freshness. Fresh anchor: near-free
  (10.8 KB total wire, measured). Stale anchor: no worse than the stale-clone
  baseline (the +1.0 MB pack the hide costs there is bought back sixfold by the
  −6.3 MB advertisement). The failure mode — no derivable anchor at all — is
  surfaced by `doctor` rather than silently eaten.
- The `-c` values append after the repo's own config, and `receive.hideRefs` is
  last-match-wins, so our unhide patterns override an operator's own hiding
  where they overlap. An operator who needs gfs to advertise *less* than the
  anchor set uses `--no-hide-refs` plus repo config rather than fighting the
  appended values.
- Hidden refs remain fully usable server-side: `index-pack --fix-thin` completes
  thin packs from the whole object store, and the `pre-receive` namespace hook
  (ADR-0010) is untouched. Hiding changes only what the *client* can see and
  delta against.
- A library embedder gets the same default (`ListenConfig::hide_refs`), and can
  turn it off or extend the advertised set programmatically.
