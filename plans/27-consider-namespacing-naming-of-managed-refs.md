# Plan — #27: Namespace managed refs per *stream*

## Goal

Stop multiple senders from clobbering each other on a shared server by giving
each independent flow of synced state its own ref subtree, keyed by a
caller-chosen **stream id**. Thread that id through the client, the library API,
the CLI, and the server's `update_worktree`, with a zero-config default so the
common single-stream case keeps "just working". Record the scheme as an ADR.

This lands the **naming scheme + plumbing** only. Cleanup/reaping of unused
streams and any cross-stream isolation/auth are explicitly out of scope (see
[Out of scope](#out-of-scope--follow-ups)).

## Approved design (from the pre-plan discussion on #27)

- **Identifier: `stream-id`** — an independent, reusable slot of synced state,
  chosen by the caller. (Chosen over "sender id" because it's caller-chosen and
  often per-branch, not 1:1 with a sender.)
- **Ref shape:** `refs/git-full-send/streams/<stream-id>/code`; the client's
  delta-base pin mirrors per-stream at
  `refs/git-full-send/streams/<stream-id>/sent/code`. Still under the existing
  `refs/git-full-send/` namespace, so the server's `pre-receive` allowlist is
  unchanged.
- **Stable & reused, not single-use** — forced by `--thin` delta retention; a
  fresh id per push would orphan the delta base and litter refs.
- **Default-on, zero-config** — first sync generates an id and persists it in
  the repo's local git config (`git-full-send.stream-id`), reused thereafter;
  `--stream-id` / a library parameter overrides. A *generated random* default
  (not a constant like `"default"`) means two unrelated repos pushing to one
  server don't collide by accident — the safe behaviour is the default.
- **Free-form, ref-path-valid** — ids may be branch-shaped and contain slashes
  (`feature/foo`); enumeration/parsing stays slash-safe.
- **Stream identity ⊥ worktree assignment** — `update_worktree(stream_id,
  worktree)` checks *that* stream's code into *that* worktree; the mapping
  (dedicated per stream, shared & taking turns, fan-in) is the caller's policy
  and is **not** baked in. No assumption of 1:1 stream↔worktree.

## Background — current state

A single global, hard-coded code ref, no notion of a stream:

- `gfs_common::CODE_REF = "refs/git-full-send/code"` (`crates/common/src/lib.rs:25`)
  — written by the client's `encode`, read by the server's `update_worktree`.
- `gfs_client::SENT_REF = "refs/git-full-send/sent/code"`
  (`crates/client/src/push.rs:53`) — client-local delta-base pin.
- `gfs_common::REF_NAMESPACE = "refs/git-full-send/"`
  (`crates/common/src/lib.rs:17`) — the `pre-receive` allowlist
  (`crates/server/src/lib.rs:235`).
- Flow: `sync` → `encode` writes `CODE_REF` → `push_ref(repo, remote, CODE_REF)`
  force-pushes `+CODE_REF:CODE_REF` → `retain_pushed_tip` pins `SENT_REF`
  (`crates/client/src/lib.rs:45-53`). Server `update_worktree` resolves
  `CODE_REF^{tree}` and `read-tree --reset -u` + `clean -fdx`
  (`crates/server/src/lib.rs:259-326`).

Two clients pushing today overwrite each other's single `code` ref.

## The naming scheme

Add to `gfs_common` a single source of truth that builds per-stream ref names
from a validated stream id, so neither side hard-codes the layout:

```text
refs/git-full-send/streams/<stream-id>/code        # the synced code tip
refs/git-full-send/streams/<stream-id>/sent/code   # client-local delta-base pin
```

- `code_ref(stream_id) -> String` and `sent_ref(stream_id) -> String` (or a
  small `StreamRefs { code, sent }` builder) live in `gfs_common`.
- A `STREAMS_PREFIX = "refs/git-full-send/streams/"` constant, used both to
  build names and to **enumerate** streams on the server (strip the prefix, then
  strip a trailing `/code`, to recover the — possibly slash-containing — id).
- `CODE_REF`/`SENT_REF` constants are **removed** (they no longer denote a
  single ref). All call sites move to the builders.

### Stream-id validation

A `StreamId` newtype (in `gfs_common`) wrapping a validated `String`:

- Must be a valid ref *path component sequence*: reject empty, leading/trailing
  `/`, `..`, control chars, and anything `git check-ref-format
  --refname-pattern` would reject for the assembled ref. Simplest robust check:
  assemble the candidate ref and validate with `gix::refs::FullName::try_from`
  (already used in `encode`/`retain_pushed_tip`), which rejects malformed names.
- Slashes **are** allowed (branch-shaped ids), so validation is on the assembled
  full ref, not "single segment".
- A constructor error surfaces as a new `EncodeError`/`PushError`/`ServerError`
  variant and a clear CLI message.

## Changes

Ordered so each step compiles. `gfs_common` first (everything depends on it).

### 1. `crates/common/src/lib.rs`
- Remove `CODE_REF`; keep `REF_NAMESPACE`.
- Add `STREAMS_PREFIX`, the `StreamId` newtype + validation, and
  `code_ref(&StreamId)` / `sent_ref(&StreamId)` builders. (`sent` is
  client-only conceptually, but keeping both builders here keeps the layout in
  one place; the server only uses `code_ref` + the prefix for enumeration.)
- Doc-comment the layout and that it lives under `REF_NAMESPACE`.

### 2. `crates/client/src/encode.rs`
- `encode` takes the stream id: `encode(repo_dir: &Path, stream_id: &StreamId)
  -> Result<EncodeOutcome, EncodeError>`.
- `update_code_ref` writes `gfs_common::code_ref(stream_id)` instead of the
  `CODE_REF` constant.
- Drop the `pub use gfs_common::CODE_REF;` re-export. Update the module docs and
  the `code_ref_is_under_the_namespace` unit test (assert the *built* ref starts
  with `REF_NAMESPACE`).
- `EncodeOutcome` keeps `commit`; optionally carries the resolved code-ref name
  for logging (avoids rebuilding in `sync`).

### 3. `crates/client/src/push.rs`
- Remove `SENT_REF`; `retain_pushed_tip(repo_dir, stream_id, commit)` writes
  `gfs_common::sent_ref(stream_id)`.
- `push_ref` is already generic over `ref_name: &str` — keep it; `sync` passes
  the built code ref. (The `+{ref}:{ref}` refspec already namespaces correctly
  once the ref name is per-stream.)
- Update the `sent_ref_is_under_the_namespace` unit test to build the ref.

### 4. `crates/client/src/lib.rs` — `sync` + default resolution
- `sync(repo_dir, remote, stream_id: Option<StreamId>)`:
  1. **Resolve the stream id**: if `Some`, use it; else read
     `git-full-send.stream-id` from the repo's local config; else **generate**
     one, persist it, and use it.
  2. `encode(&repo_dir, &id)` → `push_ref(&repo_dir, &remote,
     &code_ref(&id))` → `retain_pushed_tip(&repo_dir, &id, commit)`.
  3. `tracing::info!(stream = %id, …)` on the existing log lines.
- **Default generation + persistence** (new small module, e.g.
  `crate::stream`):
  - Generate a short random token (e.g. 8 bytes hex) — `getrandom` is already in
    the dependency tree transitively; add it as a direct dep of `gfs-client`
    (alternative: `fastrand`, also already present — non-crypto but fine for a
    collision-avoidance id). Decide at implementation time; `getrandom` is the
    safer default.
  - Persist via `git config --local git-full-send.stream-id <token>`
    (shell-out, consistent with the existing `git push`/`receive-pack`
    shell-outs and more robust than gix's config-write path). Read via gix
    `repo.config_snapshot().string("git-full-send.stream-id")` (no shell-out on
    the hot read path) **or** symmetric `git config --get`; pick one and keep
    read/write symmetric.
  - Resolution + persistence is itself a unit-testable function taking the repo
    dir.
- Add a `ClientError`/`EncodeError`/`PushError` variant for an invalid
  explicit `--stream-id` and for config read/generate/persist failures.

### 5. `crates/server/src/lib.rs` — `update_worktree`
- `update_worktree(repo, worktree, stream_id: StreamId)` and its blocking body
  thread the id into `resolve_code_tree`, which resolves
  `format!("{}^{{tree}}", gfs_common::code_ref(&stream_id))`.
- `MissingCodeRef` error message includes the stream id (so "never synced
  *this stream*" is distinguishable).
- The `pre-receive` hook is **unchanged** — per-stream refs already sit under
  `REF_NAMESPACE`. (Note in code/ADR: the hook is a namespace guard, not a
  per-stream isolation boundary; that's out of scope.)
- **Stream discovery (small, additive):** a `list_streams(repo) ->
  Vec<StreamId>` helper enumerating refs under `STREAMS_PREFIX` (via gix ref
  iteration), so an orchestrator can find live streams. Surface it through the
  library now; a CLI `list-streams` subcommand is optional — include it if cheap,
  else note as a trivial follow-up. Enumeration must be slash-safe (strip prefix
  + trailing `/code`).

### 6. `crates/cli/src/main.rs`
- `SyncArgs`: add `--stream-id <ID>` (optional). Parse into `StreamId` (clap
  `value_parser` or parse in the handler) and pass `Option<StreamId>` to `sync`.
- `UpdateWorktreeArgs`: add `--stream-id <ID>` (**required** — the server must
  be told which stream to check out; there's no repo-local default on the server
  side). Pass to `update_worktree`.
- Optionally add a `Streams`/`list-streams` subcommand wired to `list_streams`.
- Update the doc comments on the args/subcommands.

### 7. ADR
- Add **ADR-0012 — Namespacing managed refs per stream** (status `accepted`):
  the `refs/git-full-send/streams/<id>/…` layout, stream id semantics
  (caller-chosen, stable/reused, free-form/ref-valid), the zero-config
  generated-and-persisted default, the stream⊥worktree orthogonality, and the
  explicit non-goals (cleanup, isolation/auth). Cross-reference ADR-0004
  (encoding), ADR-0005 (transfer/delta-base retention), ADR-0008
  (worktree authority), ADR-0010 (receive-pack wiring).
- Update `docs/adr/README.md` index. Touch the `e.g. refs/git-full-send/code`
  wording in ADR-0004 with a forward-reference to ADR-0012 (don't rewrite
  accepted history; a pointer is enough). The future `extra` ref (ADR-0004) will
  naturally live at `streams/<id>/extra` — note this so the later ticket inherits
  the layout.

### 8. Tests (`crates/client/tests/{transfer,integration}.rs`)
- Replace `use gfs_client::{CODE_REF, SENT_REF}` and the hard-coded
  `"refs/git-full-send/code"` strings with built per-stream refs for a fixed
  test stream id.
- Update `encode`/`sync`/`update_worktree`/`retain_pushed_tip` call sites for the
  new signatures.
- The `push_ref` namespace-rejection test (`transfer.rs:126-133`) still holds
  (a `refs/heads/main` push is rejected) — just build the accepted ref per
  stream.
- **New coverage:**
  - Two different stream ids pushed to one server produce two independent
    `code` refs that don't clobber; `update_worktree` for each yields that
    stream's tree.
  - Zero-config default: `sync` with no id generates + persists
    `git-full-send.stream-id`, and a second `sync` reuses it (same ref, delta
    base retained).
  - `--stream-id`/explicit override beats the stored default.
  - A slash-containing stream id (`feature/x`) round-trips through encode →
    push → `update_worktree`.
  - Invalid stream id is rejected with a clear error.
  - `list_streams` returns the pushed ids (including a slashed one).

## Out of scope — follow-ups

File these as tracked issues (blocked-by #27), per the pre-plan agreement:

- **Cleanup/reaping of unused streams** — explicit prune (a "forget this
  stream" command deleting its `code`/`sent` refs, and TTL-based reaping. Stable
  bounded ids mean the set doesn't grow per-push, so this is not urgent.
- **Cross-stream isolation/auth** — the transport authenticates no one
  (localhost + SSH tunnel, single trusted user, ADR-0006); namespacing here is
  collision-avoidance among cooperating streams, not a security boundary.
- **The `extra` ref** (ADR-0004/0007) adopting `streams/<id>/extra` lands with
  the force-include work, not here.

## Risks / implementation details to confirm at build time

- **gix config read vs write.** Reading `git-full-send.stream-id` via
  `config_snapshot` is well-supported; writing local config via gix is fiddly,
  so the plan uses `git config --local` for the write. Confirm the read path and
  keep read/write symmetric.
- **Random source.** `getrandom` vs `fastrand` — both already transitively
  present; pick `getrandom` unless it complicates the build. The id is for
  collision-avoidance, not security, so either is acceptable.
- **`StreamId` validation surface.** Reusing `gix::refs::FullName::try_from` on
  the assembled ref is the least-surprising validator (matches what `encode`
  already trusts) — confirm it rejects the cases we care about (empty, `..`,
  trailing slash) when wrapped in the full `refs/git-full-send/streams/<id>/code`
  path.
- **Server `--stream-id` required.** Making it required is a deliberate, small
  behaviour change to `update-worktree`'s CLI; called out in the ADR.
