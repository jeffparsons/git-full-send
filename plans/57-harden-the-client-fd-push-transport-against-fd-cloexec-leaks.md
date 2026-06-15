# Plan — #57: Harden the client `fd::` push transport against `FD_CLOEXEC` leaks

## Goal

Close the latent fd-leak window in the client `git push` transport
(`crates/client/src/push.rs`). Today the two socket dups have `FD_CLOEXEC`
cleared **in the parent's fd table** before `spawn`; during that window any
unrelated `Command::spawn` on another thread forks and inherits a live copy of
the connection socket. Move the flag-clearing into the forked child (a `pre_exec`
hook, just before `exec`), so the dups are only ever inheritable inside the
intended `git push` child — never live across an unrelated spawn.

This is the change prototyped and CI-validated during #44 (compiled, passed
clippy `-D warnings`, green on Linux) before being reverted to keep that PR
focused, so the shape is known-good and small.

## Background

`push_refs` reserves two inheritable dups of the connected socket and passes
their numbers to `git` as the `fd::<in>,<out>` transport. The reservation goes
through `dup_inheritable` (push.rs:251):

```rust
fn dup_inheritable(fd: BorrowedFd<'_>) -> std::io::Result<OwnedFd> {
    let dup = fd.try_clone_to_owned()?;
    rustix::io::fcntl_setfd(&dup, rustix::io::FdFlags::empty()).map_err(std::io::Error::from)?;
    Ok(dup)
}
```

The `try_clone_to_owned()` dup is `FD_CLOEXEC` by default (so it would *not*
survive `exec`); the `rustix` call clears the flag so the dup is inherited by
`git push`. But it clears it **in the parent**, the moment the dup is created —
well before `spawn`. Any `Command::spawn` that forks on another thread in that
window inherits the now-inheritable socket. ADR-0010 and #29 already flag this
fd-fiddling as a soft spot; #44's debugging confirmed the window is real (though
not the cause of that bug).

The fix relies on `fork` semantics: the child gets a *copy* of the parent's fd
table, so clearing `FD_CLOEXEC` inside the child (after fork, before `exec`)
still lets the intended `git push` inherit the dups, while no other process can —
the parent's copies stay `FD_CLOEXEC` the whole time. `fcntl(F_SETFD)` is
async-signal-safe, so it is legal in a `pre_exec` hook.

## Changes

All in `crates/client/src/push.rs` plus the matching prose, and an ADR-0010 note.

### 1. Stop clearing `FD_CLOEXEC` in the parent

Reduce the parent-side dup to just `try_clone_to_owned()` — no `rustix` call —
and rename `dup_inheritable` to reflect that the dup is *not* yet inheritable
(e.g. `dup_socket`). Update its doc comment: the dup keeps `FD_CLOEXEC`; the
child clears it via the `pre_exec` hook (below). `TransportFds::reserve` calls
the renamed helper unchanged.

### 2. Clear `FD_CLOEXEC` in the forked child via `pre_exec`

In `push_refs`, after the command's args are set and before `.spawn()`, capture
the two raw fd numbers (Copy `i32`s — not the `OwnedFd`s) and register a
`pre_exec` hook that clears `FD_CLOEXEC` on each:

```rust
use std::os::unix::process::CommandExt; // top of file

let in_raw = transport.in_fd.as_raw_fd();
let out_raw = transport.out_fd.as_raw_fd();
// SAFETY: the closure runs in the forked child, after `std` has wired up the
// child's stdio and before `exec`. It only calls `fcntl(F_SETFD)` — async-
// signal-safe — on two fixed descriptors, with no allocation or locking.
// Clearing FD_CLOEXEC here, not in the parent, means the socket dups are
// inheritable only inside *this* child; an unrelated concurrent `spawn` can
// never inherit them (#57).
unsafe {
    command.as_std_mut().pre_exec(move || {
        for raw in [in_raw, out_raw] {
            let fd = BorrowedFd::borrow_raw(raw);
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty())
                .map_err(std::io::Error::from)?;
        }
        Ok(())
    });
}
```

Notes / why this is safe and correct:

- **tokio wiring.** `tokio::process::Command` doesn't re-expose `CommandExt`, so
  the hook is set on the inner std command via
  `unsafe { command.as_std_mut().pre_exec(..) }`. tokio's spawn is built on
  `std::process`, which runs registered `pre_exec` callbacks after fork and after
  setting up the child's stdio, just before `exec` — exactly when we want it.
- **Ownership.** The parent's `OwnedFd`s live in `transport` and are dropped only
  *after* `spawn` (existing `drop(transport)` at push.rs:216). The closure
  captures only the raw numbers, so there is no borrow/ownership conflict and the
  fds are still open at the moment the child clears the flag.
- **No fd collision.** The transport fds are reserved (and so occupied) in the
  parent before `spawn`, exactly as today; `std`'s `Stdio::piped` stderr pipe
  therefore gets different numbers and is `dup2`'d onto fd 2 in the child without
  touching the transport fds. Reservation order is unchanged.
- The structure needs a small reshuffle so the builder chain (`command.arg(..)…`)
  finishes, *then* the `pre_exec` hook is registered, *then* `command.spawn()` is
  called — rather than the current single chained `.spawn()` expression.

`rustix` stays a dependency: still used, now only inside the child hook.

### 3. Update the prose

- **push.rs module docs (§Transport, ~lines 17–24):** the dups are reserved in
  the parent and kept `FD_CLOEXEC`; their numbers are passed as `fd::<in>,<out>`;
  `FD_CLOEXEC` is cleared **only in the forked child** via a `pre_exec` hook just
  before `exec`, so the dups are never inheritable in the parent's fd table
  across an unrelated `spawn`.
- **push.rs inline comment (~lines 169–172):** keep the still-valid rationale —
  reserving the dups in the parent keeps them clear of the fds `Command` uses for
  the child's stdio — but correct the parenthetical: a `pre_exec` hook is now
  used to *clear `FD_CLOEXEC` in the child*, not to `dup2`.
- **ADR-0010 (`docs/adr/0010-receive-pack-transport-wiring.md`), Client bullet
  (~lines 36–43) and Consequences (~line 97):** record the new wiring — dups kept
  `FD_CLOEXEC` in the parent, flag cleared child-side in a `pre_exec` hook
  (async-signal-safe `fcntl`) to close the cross-`spawn` leak window (cite #57).
  Reconcile the existing "Reserving in the parent (vs. `dup2` in a `pre_exec`
  hook)" sentence, which now mis-describes the design: we *do* use a `pre_exec`
  hook, for the flag flip rather than for `dup2`. The line noting `std` "cannot
  clear `FD_CLOEXEC`" stays accurate.

## Out of scope

No change to the transport shape, the `ext::`-connector alternative, the
per-chain delta policy, or the server side. Behaviour over the wire is identical;
this only narrows *when/where* the dups are inheritable.

## Verification

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo build`
- `cargo test -p gfs-client` — exercises the push/transfer paths in
  `crates/client/tests/transfer.rs` and `crates/client/tests/delta_base_benchmark.rs`.
  A green transfer test confirms `git push` still inherits the dups (i.e. the
  child-side clear works); a broken hook would fail the push outright.
- CI runs the suite on Linux and macOS, satisfying the "stay green on both"
  acceptance criterion (the #44 prototype already went green on Linux).
- `grep -rn 'FD_CLOEXEC\|pre_exec' crates/client/src/push.rs docs/adr/0010-*.md`
  to confirm the prose matches the new wiring and no stale "clear in the parent"
  claim remains.

## Acceptance (from the issue)

- No `FD_CLOEXEC`-cleared socket dup is ever live in the parent's fd table across
  an unrelated `spawn` — the flag is cleared only in the forked child.
- Push/transfer tests stay green on Linux and macOS.
- ADR-0010 note records the wiring.
