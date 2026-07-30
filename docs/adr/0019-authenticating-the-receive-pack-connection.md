# ADR-0019 — Authenticating the receive-pack connection

- Status: accepted
- Date: 2026-07-30
- Amends: [ADR-0006](0006-transport-and-connectivity.md) (the "no in-tool
  authentication" decision and its deferral)

## Context

[ADR-0006](0006-transport-and-connectivity.md) deferred transport security
entirely to the SSH tunnel: bind localhost, let the operator forward the port,
build no authentication of our own. That reasoning covers the *network*, and it
still holds there. It does not cover the machine at the far end.

`listen` authenticates nothing, so anything that can reach the port can push a
stream, and [`update-worktree`](0008-remote-worktree-disposability.md) then checks
that stream out **authoritatively** over the target worktree. On a dev workstation
whose tooling runs tracked files straight out of that worktree, an unrelated local
process — another user, another SSH session, someone else's port forward — can
therefore obtain arbitrary code execution as the receiving user. Binding loopback
keeps the port off the network; it does not make it private (issue #81, raised by
a security review of a downstream integration).

Downstream callers cannot close this themselves. A caller can hardcode the bind
address and verify after checkout that the tree reproduces the plan digest it
expected, but that answers "is this the tree we planned against", not "did we send
it" — and it runs after the tooling has already executed files from the tree. It
is a correctness check, not a security control.

## Decision

A **shared secret on the wire**, presented by the client and verified by the
receiver before `git receive-pack` is spawned.

### The exchange

`receive-pack` is server-speaks-first, so authentication is a **client preamble**,
written the moment the connection is up:

```text
client → server   0027git-full-send-auth-v1 <token>\n     (one pkt-line)
server            verifies, then spawns receive-pack
server → client   ref advertisement · …                    (unchanged)
```

- Everything after the preamble is the identical raw receive-pack stream:
  [ADR-0005](0005-transfer-mechanism.md) and
  [ADR-0010](0010-receive-pack-transport-wiring.md) are untouched, and the
  preamble costs no round trip the connect had not already paid.
- The comparison is constant time in the content of the two secrets.
- A refusal is answered with a protocol **`ERR` pkt-line**, which `git push`
  recognises during initial contact and reports as `remote error: …`. A
  misconfigured client is told what to fix rather than watching the connection
  drop.
- The preamble read is bounded by its own short deadline
  (`DEFAULT_AUTH_TIMEOUT_SECS`, 10s), independent of the per-connection budget: a
  client with no token configured sends nothing at all, and reaching the deadline
  is what turns that deadlock into the `ERR`.

### The posture is explicit

`listen` **refuses to start** unless it is given either a token (`--token-file`,
or `GIT_FULL_SEND_TOKEN`) or `--allow-anonymous`. Issue #81 proposed the opposite
— unauthenticated by default, so nothing breaks — and this ADR deliberately
departs from it: a receiver that executes what it is handed should not be able to
end up unauthenticated by *omission*. `--allow-anonymous` keeps the old behaviour
available for setups where the port genuinely cannot be reached by anything else,
and says so in the process list and the startup log.

The client stays implicitly anonymous: `sync` and `probe` present a token if one
is configured and nothing otherwise. The asymmetry is the point — it is the
receiver that is at risk.

### What this is not

Bearer-token-over-loopback, and nothing more. The tunnel still provides
confidentiality and the token is never a substitute for it; the goal is only to
stop an unrelated *local* process from pushing code the receiving machine will
run. It is not a replacement for the tunnel, does not authenticate the server to
the client, and does not survive an attacker who can already read the token file.

## Consequences

- **Breaking**: every existing `listen` invocation must add `--token-file` or
  `--allow-anonymous`. This is the intended cost of making the posture explicit.
- A token-carrying client pushing to an `--allow-anonymous` server fails with
  `protocol error: bad line length character: git-`: the server never reads a
  preamble it is not expecting, so those bytes reach `receive-pack`. Detecting it
  would mean sniffing the first inbound chunk inside the byte pump; it is
  documented in `docs/operating.md` instead.
- Version skew in the other direction (old client, token-requiring server) is
  clean: the client presents nothing, and the deadline turns that into the `ERR`.
- A refused connection is recorded like any other (ADR-0013), as outcome
  `unauthenticated` with an `auth_failure` of `mismatch`/`malformed`/`absent`, so
  "who is being turned away, and why" is a question the sink can answer.
- ADR-0006's deferral of first-class transport security is *partly* discharged:
  the manual tunnel is still required for confidentiality and reachability. What
  changes is that reaching the port is no longer the same thing as being allowed
  to push.
