# Plan — #29: Replace the libc fd-fiddling in the client push transport

## Goal

Remove the direct `libc` dependency and **all `unsafe`** from the client's
`fd::` transport wiring (`crates/client/src/push.rs`), keeping the lib-friendly
in-process socket attachment. This is **Option A** from the pre-plan discussion:
`std::os::fd::OwnedFd` for RAII duplication + `rustix` for the one thing std
can't express (clearing `FD_CLOEXEC`). Document **Option C** (the `ext::`
"micro-utility" connector) as the future direction for a purely standalone
binary.

Approved on the issue: *"let's do A with a note somewhere that for a purely
standalone binary version of this we could instead do the 'micro-utility'
version. (And we could in future support both code paths behind feature gates,
and require the embedder to expose their own version of the micro utility,
basically just by wrapping something we export from the library.)"*

## Background — why libc is here today

`crates/client/src/push.rs` attaches a connected `TcpStream` to `git push`'s
`fd::<in>,<out>` transport. Because `git push` uses its own stdin/stdout (so
`fd::0,1` wedges it — see ADR-0010), the transport must land on *numbered* fds
≥ 3 that the child inherits across `exec`. std can duplicate an fd but offers no
stable API to **clear `FD_CLOEXEC`**, so the code drops to libc:

- `dup_inheritable()` → `libc::fcntl(fd, F_DUPFD, 3)` (dup to ≥ 3, CLOEXEC cleared)
- `libc::close()` ×2 — manual cleanup of those dups (error path + `Drop`)

`libc` is used **nowhere else** in the workspace (rust-side); the server side
already attaches its socket with pure std via `Stdio::from(OwnedFd::from(sock))`.

## Approach (Option A)

Replace the manual dup + CLOEXEC dance with:

1. **Duplicate via std RAII.** `sock.as_fd().try_clone_to_owned()` →
   `OwnedFd`. This dups to the lowest free fd (≥ 3 in practice, since 0/1/2 are
   stdio) and closes itself on drop — deleting the manual `libc::close` calls
   and the hand-rolled `Drop`. Note std's `try_clone` sets `FD_CLOEXEC`.
2. **Clear `FD_CLOEXEC` via rustix.** `rustix::io::fcntl_setfd(&fd,
   rustix::io::FdFlags::empty())` on each `OwnedFd`, so the child inherits it
   across `exec`. (Exact call confirmed at build time; the cloexec-clearing
   `fcntl_setfd` + empty `FdFlags` is the documented safe path — we explicitly
   do *not* want `fcntl_dupfd_cloexec`, which would re-set the flag.)
3. **Pass the raw numbers, keep the `OwnedFd`s alive across `spawn`, drop
   after.** Build `fd::<in>,<out>` from `.as_raw_fd()` on the two `OwnedFd`s,
   then `drop` them (and the socket) immediately after `spawn` so the parent's
   copies close and only the child's inherited fds remain — same lifecycle as
   today, now expressed with `OwnedFd` instead of raw `i32` + manual close.

No `unsafe`, no direct `libc`. The transport stays in-process: one `git`
subprocess, no helper binary, fully usable from `gfs-client` as a library.

### Why not pure std

There is no stable std API to clear `FD_CLOEXEC` or to hand a child an fd on a
chosen number, so *some* syscall wrapper is unavoidable for the in-process
shape. `rustix` is the idiomatic, widely-used, `unsafe`-free choice. Honest
caveat (already surfaced in pre-plan): on macOS `rustix` uses a libc backend
internally, so `libc` may remain a *transitive* dependency there — this change
removes our **direct** dep and all our **`unsafe`**, which is the stated goal.
Truly-zero-libc is only achievable via Option C, which we're deferring.

## Changes

### 1. `crates/client/src/push.rs`
- Rewrite `TransportFds` to hold two `OwnedFd`s (or restructure to a small
  helper returning `(OwnedFd, OwnedFd)`); delete the manual `Drop`/`close` and
  the `dup_inheritable` libc function.
- Replace the dup with `as_fd().try_clone_to_owned()` + rustix CLOEXEC clear.
- Adjust the `spawn` closure: derive the `fd::` arg from `as_raw_fd()` before
  spawn, drop the `OwnedFd`s + `sock` after spawn (as now).
- Update the module-level doc comment and inline comments that currently say
  `fcntl(F_DUPFD)` / "`F_DUPFD` clears `FD_CLOEXEC`" to describe the std
  `try_clone` + rustix `fcntl_setfd` wiring. Drop the `use ... AsRawFd` /
  add `AsFd` imports as needed.

### 2. Cargo manifests
- `crates/client/Cargo.toml`: remove `libc.workspace = true`; add
  `rustix = { workspace = true, features = ["fs"] }` (or whichever feature
  gates `fcntl_setfd`/`FdFlags` — confirmed at build time; likely the default
  `std` + `fs`).
- Root `Cargo.toml` `[workspace.dependencies]`: remove the `libc` line; add a
  pinned `rustix = "<latest 1.x>"`. Keep the alphabetical-ish ordering and the
  explanatory comment style.
- `Cargo.lock` updates via the build.

### 3. ADR-0010 (`docs/adr/0010-receive-pack-transport-wiring.md`)
- Update the "Socket attachment" decision + "Consequences" to reflect the new
  wiring: client reserves inheritable dups via `std::os::fd::OwnedFd`
  (`try_clone_to_owned`) with `FD_CLOEXEC` cleared via `rustix`, rather than
  `libc::fcntl(F_DUPFD)`. Remove the "socket fds via `std::os::fd` /
  `libc::fcntl`" phrasing.
- Add a short note (the "somewhere" the user asked for) recording **Option C**
  as the future direction for a **purely standalone binary**: an `ext::`
  connector "micro-utility" subcommand that bridges stdin/stdout ↔ socket,
  eliminating fd-passing entirely and any libc — at the cost of needing a real
  binary on a known path (awkward for the library embedding case). Capture the
  user's forward-looking idea: support **both** code paths behind **feature
  gates**, with the embedder exposing **their own** micro-utility that wraps
  something we export from `gfs-client`. ADR-0010 already names the
  `ext::`-with-connector shape as the natural fit for the eventual SSH
  transport, so this slots in as a cross-reference rather than a new concept.
- Leave a brief pointer comment in `push.rs` to the ADR note so the code's
  reasoning is discoverable.

## Testing / verification
- `cargo build` and `cargo clippy --all-targets` clean (clippy will confirm the
  `unsafe` is gone and no `libc` remains).
- `cargo test -p gfs-client` — the loopback integration tests in
  `crates/client/tests/transfer.rs` (`push_lands_code_ref_and_objects`,
  `retains_pushed_tip_on_the_client`, `second_sync_advances_the_server`, the
  namespace-rejection and update-worktree tests) exercise the **real** push
  transport end to end: if the rewritten fd wiring failed to hand the socket to
  `git push`, these would hang or fail. They are the primary correctness gate.
- `cargo test` across the workspace stays green.
- Confirm `libc` no longer appears as a direct dependency:
  `cargo tree -p gfs-client -i libc` shows only transitive paths (or none),
  and `grep -rn "libc" crates/` finds no rust-side usage.

## Out of scope
- Implementing Option C / the `ext::` connector subcommand, feature-gating the
  two code paths, or the embedder-exposed micro-utility — documented as future
  work in ADR-0010, not built here.
- Any change to the server-side attachment (already pure std).

## Risks / notes
- **Transitive libc on macOS** via rustix's backend: acceptable per the
  approved scope (goal is removing our direct dep + `unsafe`). Called out in the
  ADR so it isn't mistaken for a clean libc-free tree.
- **rustix feature/API name**: the precise module/feature for
  `fcntl_setfd`/`FdFlags` will be pinned during implementation; the build +
  integration tests confirm correctness immediately.
