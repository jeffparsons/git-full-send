# ADR-0010 — `receive-pack` transport wiring

- Status: accepted
- Date: 2026-06-13

## Context

[ADR-0005](0005-transfer-mechanism.md) settled the *mechanism*: the client runs
`git push --thin` and the server ingests with `git receive-pack`, over a
localhost connection ([ADR-0006](0006-transport-and-connectivity.md)). It left
the concrete wiring — how the socket is attached to each `git` process, how the
writable-ref namespace is enforced, and how the client retains a delta base — as
an implementation detail for the build ticket (#18). This ADR records the
choices made there, refining ADR-0005 rather than overturning it.

## Decision

### Socket attachment: raw receive-pack stream, but asymmetric fds

The wire is the raw receive-pack stream end to end (no `git daemon`/`git://`
framing). Each side attaches the TCP socket to its `git` child differently,
because the two commands treat their fds differently:

- **Server** runs `git receive-pack` with **piped** stdin/stdout and copies the
  bytes between the socket and those pipes with two pump threads, so each
  direction's byte count can be recorded in the metrics (issue #42). (The
  original wiring handed `receive-pack` the accepted socket directly as its
  stdin/stdout — exactly what `git daemon` does — which is simpler but gives no
  seam to count bytes.) The pumps copy with an explicit read/write loop rather
  than `std::io::copy`: on Linux that function takes a `splice`/`sendfile`
  zero-copy fast path between the socket and the pipe that **deadlocked** the
  bidirectional `receive-pack` exchange (issue #44) — `unpack-objects` was
  starved of pack bytes — while macOS, lacking that path, was unaffected. A
  plain buffered loop behaves identically on both platforms and yields the count
  for free.
- **Client** must *not* put the transport on fd 0/1: `git push` uses its own
  stdin/stdout, and `fd::0,1` wedges it before the protocol starts. Instead the
  client reserves two dups of the connected socket in the parent
  (`OwnedFd::try_clone_to_owned`) and passes their numbers as
  `git push --thin fd::<in>,<out>`, leaving `git`'s own stdio free. Reserving in
  the parent (vs. `dup2` in a `pre_exec` hook) keeps the transport fds clear of
  the descriptors `Command` uses to wire up the child's stdio. The dups keep
  `FD_CLOEXEC` in the parent; `git` would not inherit them across `exec`, so the
  client clears the flag **only in the forked child**, via a `pre_exec` hook
  (`rustix` `fcntl(F_SETFD)`, async-signal-safe) registered on the inner `std`
  command and run just before `exec`. `fork` copies the fd table, so the intended
  `git push` still inherits the dups while no unrelated concurrent `spawn` can —
  the previous design cleared `FD_CLOEXEC` in the parent, leaving a window where
  any concurrent `spawn` inherited the connection socket (#57).

Both invocations set `-c protocol.fd.allow=always`, since `git` blocks the
`fd::`/`ext::` transports by default. The `ext::`-with-connector shape (e.g.
`ext::ssh … git-receive-pack`) remains the natural fit for the eventual
SSH-tunnel ergonomics and is a drop-in alternative to the in-process attachment.

The in-process attachment keeps `gfs-client` usable as a pure library (one
`git` subprocess, no helper binary to locate). Its only cost is the descriptor
bookkeeping above: `std` can duplicate an fd but cannot clear `FD_CLOEXEC` or
hand a child an fd on a chosen number, so the client leans on `rustix` for the
flag flip (`#29` removed an earlier hand-rolled `libc::fcntl(F_DUPFD)` + `unsafe`
in favour of `OwnedFd` + `rustix`, dropping the direct `libc` dependency).
`rustix` may still pull `libc` transitively on some targets, so this removes our
direct dep, not necessarily `libc`, from the tree. The flag flip now happens
inside a `pre_exec` hook (#57), which reintroduces one small `unsafe` block —
registering the hook — but the flip itself is still a safe `rustix` call, and the
hook body is async-signal-safe (a fixed pair of `fcntl(F_SETFD)` calls, no
allocation).

A **purely standalone binary** could go further and drop fd-passing entirely via
the same `ext::`-with-connector shape: a tiny "micro-utility" subcommand that
connects the socket and shunts bytes between its own stdin/stdout and the
socket, which `git` wires up with ordinary pipes — no numbered fds, no
`FD_CLOEXEC`, no `rustix`. We don't do this now because it needs a real binary on
a known path, which the **library** embedding case can't assume. A future build
could support **both** paths behind feature gates, with the embedder exposing
**their own** connector that wraps a helper exported from `gfs-client` — getting
the standalone path's zero-syscall-crate cleanliness without forcing it on
library consumers.

### Namespace confinement: `pre-receive` hook via `core.hooksPath`

The server confines writable refs to the `refs/git-full-send/*` namespace
([`gfs_common::REF_NAMESPACE`]) with a `pre-receive` hook that rejects any ref
outside it. The hook is materialised into a gfs-managed directory and selected
with `-c core.hooksPath=…`, so the target repo's own `hooks/` are never touched.
This is preferred over `GIT_NAMESPACE`, which would *relocate* the received refs
under `refs/namespaces/…` rather than land them where the worktree-update step
([ADR-0008](0008-remote-worktree-disposability.md)) expects them.

### Predictability levers (from [Research 0003](../research/0003-transfer-mechanism-and-pack-performance.md))

- The server runs `receive-pack` with `-c receive.autogc=false`, so a
  post-receive gc cannot prune a delta base mid-session.
- The client retains the last-confirmed-pushed `code` tip under
  `refs/git-full-send/sent/code` (within the namespace), advancing it only after
  a push succeeds, so the prior objects survive locally as the `--thin` base and
  a failed push leaves the base pointing at what the server actually has.
- The scratch refs are **force**-pushed (`+code:code`): each `code` commit is
  parented on `HEAD`, not the previous tip ([ADR-0004](0004-encoding-the-sync-state-in-git.md)),
  so successive pushes are deliberately non-fast-forward.

## Consequences

- The transfer leg stays a single `git` subprocess per side; `gix`'s role
  remains object synthesis. No custom wire protocol.
- The attachment is Unix-only (socket fds via `std::os::fd`, with `rustix`
  clearing `FD_CLOEXEC` in a `pre_exec` hook); the tool is Unix-first, consistent
  with the rest of the codebase.
- The server process spawns one `git receive-pack` per connection and must keep
  the hooks directory alive for its lifetime.

## Status

Accepted. Supersedes nothing; refines [ADR-0005](0005-transfer-mechanism.md).

[`gfs_common::REF_NAMESPACE`]: ../../crates/common/src/lib.rs
