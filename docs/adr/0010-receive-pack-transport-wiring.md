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

- **Server** hands `git receive-pack` the accepted socket as its **stdin and
  stdout** (a `try_clone` for the second fd) — exactly what `git daemon` does
  internally. `receive-pack` drives the protocol over fd 0/1 happily.
- **Client** must *not* put the transport on fd 0/1: `git push` uses its own
  stdin/stdout, and `fd::0,1` wedges it before the protocol starts. Instead the
  client reserves two inheritable dups of the connected socket in the parent
  (`fcntl(F_DUPFD)`, which clears `FD_CLOEXEC`) and passes their numbers as
  `git push --thin fd::<in>,<out>`, leaving `git`'s own stdio free. Reserving in
  the parent (vs. `dup2` in a `pre_exec` hook) keeps the transport fds clear of
  the descriptors `Command` uses to wire up the child's stdio.

Both invocations set `-c protocol.fd.allow=always`, since `git` blocks the
`fd::`/`ext::` transports by default. The `ext::`-with-connector shape (e.g.
`ext::ssh … git-receive-pack`) remains the natural fit for the eventual
SSH-tunnel ergonomics and is a drop-in alternative to the in-process attachment.

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
- The attachment is Unix-only (socket fds via `std::os::fd` / `libc::fcntl`);
  the tool is Unix-first, consistent with the rest of the codebase.
- The server process spawns one `git receive-pack` per connection and must keep
  the hooks directory alive for its lifetime.

## Status

Accepted. Supersedes nothing; refines [ADR-0005](0005-transfer-mechanism.md).

[`gfs_common::REF_NAMESPACE`]: ../../crates/common/src/lib.rs
