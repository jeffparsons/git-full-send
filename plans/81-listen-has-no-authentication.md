# Plan — #81: `listen` has no authentication

`listen` authenticates nothing. Anything that can reach the port pushes a stream,
and `update-worktree` then checks that stream out authoritatively over the target
worktree — which, on a dev workstation whose tooling runs tracked files straight
out of the worktree, is a local privilege escalation into arbitrary code execution
as the receiving user. Binding loopback (ADR-0006) keeps that off the network but
not away from another local user, another SSH session, or someone else's port
forward.

## Decision

A **shared secret on the wire**, presented by the client and verified by the
receiver *before* `git receive-pack` is spawned — so an unauthenticated peer never
reaches ref negotiation, let alone pack ingest.

Two deliberate deviations from the issue's proposal, both settled with the author:

1. **`listen` refuses to start without an explicit choice.** The issue proposed
   "no token configured keeps today's behaviour", i.e. opt-in. Instead, `listen`
   requires *either* a token *or* `--allow-anonymous`. A receiver that executes
   what it is handed should not be able to end up unauthenticated by omission.
   This is a **breaking change** to the `listen` CLI: existing invocations
   (including Stile's `dev` CLI) must add one of the two.
2. **The client stays implicitly anonymous.** No `--allow-anonymous` on `sync`:
   the asymmetry is the point, since it is the *receiver* that is at risk.

## The wire exchange

receive-pack is server-speaks-first, so authentication is a **client preamble**
written before the server says anything — the client sends it immediately on
connect, the server reads it before spawning the child, and no round trip is added
beyond the one the connect already pays:

```text
client → server   0027git-full-send-auth-v1 <token>\n     (one pkt-line)
server            verifies, then spawns receive-pack
server → client   ref advertisement · …                    (unchanged from here)
```

Everything after the preamble is the identical raw receive-pack stream — ADR-0005
and ADR-0010 are untouched.

On a failed or absent preamble the server answers with a protocol **`ERR`
pkt-line** and closes. `git push` recognises `ERR` in the initial contact
(`PACKET_READ_DIE_ON_ERR_PACKET`) and reports it as a remote error, so a
misconfigured client gets a diagnosis rather than a hang-up. The absent case needs
a deadline to reach: the auth read is bounded by
`gfs_common::DEFAULT_AUTH_TIMEOUT_SECS` (10s), independent of the much longer
per-connection timeout.

**Not handled: a token-carrying client pushing to an `--allow-anonymous` server.**
The server never reads a preamble it isn't expecting, so the bytes reach
`receive-pack`'s stdin and the push dies with `protocol error: bad line length
character: git-`. Detecting that cheaply would mean sniffing the first inbound
chunk inside the byte pump; it is documented as a troubleshooting entry instead.

## Changes

### `crates/common` — the shared piece both ends must agree on

New `auth` module (`crates/common/src/auth.rs`):

- `Token`: a validated shared secret. Redacted `Debug` (so no log line or `?err`
  can print it), no `Display`, no `PartialEq`; comparison is only available as
  `Token::matches(&self, presented: &[u8]) -> bool`, which is **constant time**
  in the content of the two secrets (length is compared separately and is not
  treated as secret — the tokens are high-entropy, and the length of the
  *presented* value is attacker-chosen anyway).
- Validation on construction: non-empty, no whitespace or control characters (it
  travels as one pkt-line), at most `MAX_TOKEN_LEN` bytes. A token shorter than
  `WEAK_TOKEN_LEN` warns rather than fails — a weak secret is the operator's call,
  an unusable one is not.
- `Token::from_file` (trailing newline trimmed) and `Token::resolve(flag)`,
  which implements the lookup once for every command: `--token-file` wins, else
  `GIT_FULL_SEND_TOKEN` (the value, inline), else `None`.
- The wire format: `AUTH_PKT_PREFIX`, `auth_pkt`, `read_auth_pkt`, `err_pkt`, and
  `AuthOutcome` (`Ok`/`Mismatch`/`Malformed`/`Absent`) so the server's decision is
  a total match rather than a pile of booleans.
- `DEFAULT_AUTH_TIMEOUT_SECS` in `lib.rs` beside the other `DEFAULT_*`.

### `crates/server`

- `Auth` (new, public): `Token(Token)` | `Anonymous`. There is no "unset" state —
  the type is what makes the choice explicit. `ListenConfig` gains
  `auth: Arc<Auth>` and so loses `Copy` (keeps `Clone`); `Default` is `Anonymous`,
  documented as *the library's* default, with the CLI refusing to pick it
  implicitly.
- `handle_connection` takes `&Auth` and, when a token is configured, runs the
  preamble exchange before `Command::spawn`. A rejected connection writes its
  `ERR`, shuts the socket down, logs at WARN with the peer address, records a
  `receive` record with the new `unauthenticated` outcome, and returns `Ok(())` —
  a rejection is the server working, not a server error.
- `Outcome::Unauthenticated` + `ReceiveRecord::unauthenticated`, which is the
  constructor for "there was no child process", so the exit fields cannot
  disagree with a status that never existed.
- One startup line either way: INFO that a token is required, WARN that
  unauthenticated pushes are accepted.

### `crates/client`

- `push_refs`/`push_ref` take `auth: Option<&Token>` and write the preamble
  straight to the `TcpStream` after connect, before the socketpair interposer and
  the `git push` spawn. It is our own write, so it is deliberately outside the
  counted stream (the `PushWire` numbers stay comparable across the change).
- `probe` takes `auth: Option<&Token>` — an authenticated server would otherwise
  reject a liveness check.
- `sync` takes `auth: Option<Token>` as a fifth argument and threads it to both
  pushes.

### `crates/cli`

- `sync`/`probe`: `--token-file <PATH>`.
- `listen`: `--token-file <PATH>` plus `--allow-anonymous`, mutually exclusive in
  clap, and *neither* is an error resolved in `main` (not by a clap arg group, so
  the message can name both remedies).
- Both sides accept `GIT_FULL_SEND_TOKEN` when no flag is given.

### Docs

- New **ADR-0019 — Authenticating the receive-pack connection** (accepted),
  amending ADR-0006's "we do not build our own auth" decision; ADR-0006 gains an
  amendment note in its status line, per ADR-0000 (never rewrite an accepted ADR's
  body). Index row in `docs/adr/README.md`.
- `docs/operating.md`: a new section on the shared secret (generating one, file
  permissions, both mismatch directions), plus the `listen` invocation in §2.
- `README.md`: the Quickstart and the Status paragraph both currently say the
  transport has no authentication at all.

### Tests

`crates/client/tests/transfer.rs` (the loopback harness) gains, via a
`start_server_with_auth` helper:

- a correct token round-trips a sync end to end;
- a wrong token, and a *missing* token, are both refused — no ref is created on
  the server, and the server records an `unauthenticated` receive;
- the client's error carries the server's `ERR` text (this is what proves `git
  push` surfaces it rather than dying of a hang-up);
- `probe` against an authenticated server works with the token and fails without;
- an `--allow-anonymous` server still accepts a tokenless client (the existing
  suite, which passes `None` throughout, is that assertion in bulk).

`crates/common/src/auth.rs` unit tests cover validation, redaction, the pkt-line
round trip, and `matches` (including length-mismatch and near-miss).

`crates/cli/tests/end_to_end.rs` covers the CLI surface: `listen` with neither
flag exits non-zero and names both remedies; a `--token-file` sync round-trips
through the real binary.
