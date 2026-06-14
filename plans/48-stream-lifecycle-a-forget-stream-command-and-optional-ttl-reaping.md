# Plan — #48: Stream lifecycle — a `forget-stream` command (and optional TTL reaping)

## Goal

ADR-0012 deferred stream cleanup as a non-goal: stable ids keep the ref set
bounded, but nothing removes a stream that is no longer wanted, so its `code`,
`extra`, and (client-local) `sent/*` refs persist forever. Add an explicit
"forget this stream" path:

1. A **`forget-stream --repo --stream-id`** command that deletes every ref under
   `refs/git-full-send/streams/<id>/` in the target repo, so the stream no longer
   appears in `list-streams`.
2. The same command is **symmetric**: pointed at the server repo it drops
   `code`/`extra`; pointed at the client repo it drops the local `sent/*`
   retention pins — the issue's "client-side counterpart", with one command.
3. Document it in `docs/operating.md`; record the lifecycle decision as an ADR and
   retire ADR-0012's deferral note.
4. **TTL-based reaping is out of scope** here — split into its own follow-up issue
   (as the issue invites).

Approved in the pre-plan hand-off (👍): one symmetric command; the per-worktree
index dir is **not** reaped (it is keyed by worktree path, not stream id —
orthogonal per ADR-0012, and the worktree is disposable per ADR-0008); the
client `git-full-send.stream-id` config key is **left untouched** (documented,
not auto-unset); forgetting a stream with no refs is **idempotent** (a friendly
no-op, not an error).

## Design

### `crates/common/src/lib.rs`

Add a `stream_prefix` builder next to `code_ref` / `sent_ref`, so the deletion
path is assembled from the shared layout rather than hard-coded (ADR-0012):

```rust
/// The ref-name prefix under which every ref of `stream` lives:
/// `…/streams/<id>/`. The trailing slash is significant — it bounds the prefix
/// so `foo` does not match `foobar`. Used to enumerate and delete a stream's
/// refs wholesale (`forget-stream`, issue #48).
pub fn stream_prefix(stream: &StreamId) -> String {
    format!("{STREAMS_PREFIX}{}/", stream.as_str())
}
```

Extend the existing `refs_live_under_the_namespace_and_prefix` unit test (or add a
focused one) asserting `stream_prefix` has the trailing slash and that
`code_ref`/`sent_ref`/`extra_ref`/`sent_extra_ref` all `starts_with` it — and a
`foo` vs `foobar` non-prefix check.

### `crates/server/src/lib.rs`

Add `forget_stream` next to `list_streams` (the streams-management surface), using
gix for enumeration and deletion — consistent with `list_streams`'
`references().prefixed(...)` and `push.rs`' `edit_reference`:

```rust
/// Delete every ref of `stream` (everything under `gfs_common::stream_prefix`)
/// from `repo`, returning the number of refs removed.
///
/// Symmetric across both ends (issue #48 / ADR-00NN): run against the server repo
/// it removes the stream's `code`/`extra`; run against the client repo it removes
/// the local `sent/*` delta-base pins. Idempotent — a stream with no refs yields
/// `Ok(0)`. Streams and worktrees are orthogonal (ADR-0012), so the per-worktree
/// index dir is deliberately not touched; the worktree is disposable (ADR-0008).
pub fn forget_stream(repo: &Path, stream: &gfs_common::StreamId) -> Result<usize, ServerError> {
    let repo = gix::discover(repo).map_err(|_| ServerError::NotARepo(repo.to_path_buf()))?;
    let prefix = gfs_common::stream_prefix(stream);

    // Collect the matching ref names first, then delete them in one transaction.
    let platform = repo.references().map_err(/* ForgetStream */)?;
    let iter = platform.prefixed(prefix.as_str()).map_err(/* ForgetStream */)?;
    let mut edits = Vec::new();
    for reference in iter {
        let reference = reference.map_err(/* ForgetStream */)?;
        edits.push(RefEdit {
            change: Change::Delete {
                expected: PreviousValue::Any,
                log: RefLog::AndReference,
            },
            name: reference.name().to_owned(),
            deref: false,
        });
    }
    if edits.is_empty() {
        return Ok(0);
    }
    let applied = repo.edit_references(edits).map_err(/* ForgetStream */)?;
    Ok(applied.len())
}
```

Notes / details to settle in implementation:
- **Snapshot-then-delete.** Enumerate into an owned `Vec<RefEdit>` (taking owned
  `FullName`s via `reference.name().to_owned()`) before mutating, so we are not
  deleting from a live iterator borrow.
- **Error variant.** Add `ServerError::ForgetStream(#[source] Box<dyn Error + Send
  + Sync>)` mirroring `ListStreams`, with `#[error("could not forget stream")]`.
  (Both the `references()`/`prefixed()`/iteration errors and `edit_references`
  funnel through it; box as needed.)
- **`PreviousValue::Any`** — we are unconditionally forgetting, not guarding
  against a concurrent update; matches `retain_pushed_tip`'s force semantics.
- **`RefLog::AndReference`** deletes the ref and its reflog.
- If gix's `prefixed` matching ever proves coarser than a path-segment prefix, the
  trailing slash in `stream_prefix` already bounds it; no extra filtering needed.
  (Will verify against the `foo`/`foobar` test below.)

### `crates/cli/src/main.rs`

Add a `ForgetStream` subcommand parallel to `ListStreams`:

```rust
/// Delete a stream's refs so it no longer appears in `list-streams` (server or client).
ForgetStream(ForgetStreamArgs),

#[derive(Debug, Args)]
struct ForgetStreamArgs {
    /// Path to the repository holding the stream's refs (server or client repo).
    #[arg(long, value_name = "PATH")]
    repo: PathBuf,
    /// Stream whose refs to delete.
    #[arg(long, value_name = "ID")]
    stream_id: StreamId,
}
```

Dispatch:

```rust
Command::ForgetStream(args) => {
    let removed = gfs_server::forget_stream(&args.repo, &args.stream_id)?;
    if removed == 0 {
        println!("no refs for stream `{}`; nothing to forget", args.stream_id);
    } else {
        println!("forgot stream `{}` ({removed} ref(s) removed)", args.stream_id);
    }
}
```

`StreamId`'s `FromStr` already gives clap validation/rejection of malformed ids at
the boundary, same as the other subcommands.

### Documentation — `docs/operating.md`

- **New subsection under §2** (server commands), after `list-streams`:
  `### \`forget-stream\` — retire a stream`. Cover: deletes the stream's
  `refs/git-full-send/streams/<id>/…` refs so it drops out of `list-streams`;
  idempotent; that it does **not** touch the worktree itself (disposable —
  ADR-0008) or its index dir; example invocation.
- **§3 Stream ids:** add a short "Retiring a stream" note — run `forget-stream`
  against the **server** repo to drop `code`/`extra`; the client also keeps
  `sent/*` delta-base refs and the `git-full-send.stream-id` config key, so to
  fully retire a stream locally run `forget-stream` against the client repo too
  and `git config --unset git-full-send.stream-id` (the latter otherwise keeps the
  repo defaulting to that id; left as a manual step by design).

### ADRs

- **Add `docs/adr/0014-forgetting-a-stream.md`** (status: accepted) recording the
  decision: one symmetric, ref-prefix deletion command; idempotent; worktree index
  dir intentionally not reaped (streams ⟂ worktrees, ADR-0012; worktree disposable,
  ADR-0008); client config left to a documented manual step; TTL reaping remains a
  separate follow-up. Reference back to ADR-0012.
- **Update `docs/adr/0012-…`** "Cleanup / reaping of unused streams" non-goal to
  note the explicit forget path now exists (see ADR-0014), with TTL reaping still
  deferred.
- **Update `docs/adr/README.md`** index with the 0014 row.

### Follow-up issue (TTL reaping)

File via `ghwf create-issue` (blocked-by #48): "Stream lifecycle: optional
TTL-based reaping of unused streams" — auto-reap streams whose `code` ref is older
than a configurable age, as a complement to the explicit `forget-stream`. Capture
the open questions (where the age is measured — committer date vs a sidecar
last-touched marker; opt-in vs default; client vs server) so the design isn't
pre-judged here.

## Tests

### `crates/common/src/lib.rs`
- `stream_prefix` has the trailing slash; every per-stream ref builder
  `starts_with` it; `foo`'s prefix does not match a `foobar` ref name.

### `crates/client/tests/transfer.rs` (end-to-end, mirrors existing stream tests)
- **Server forget:** sync two streams (`alice`, `bob`) to a server, `forget_stream`
  one, assert `list_streams` returns only the other, and the forgotten stream's
  `code`/`extra` refs no longer resolve (`git rev-parse --verify --quiet` fails).
  Assert the return count is the number of refs that existed (2: code + extra).
- **Client-side `sent/*` removal:** after a `sync`, the client repo holds
  `sent/code` + `sent/extra` for the stream; `forget_stream` against the *client*
  repo removes them (assert via `git rev-parse`), and a subsequent `sync` still
  succeeds (regenerating them) — i.e. forgetting locally is safe.
- **Idempotent:** `forget_stream` on a never-synced / already-forgotten id returns
  `Ok(0)` and does not error.

### `crates/cli/tests/end_to_end.rs`
- Add `"forget-stream"` to the top-level subcommand-listing assertion (line ~356).
- `forget-stream --help` exposes `--repo` and `--stream-id`.
- Optionally: a CLI-level happy-path invocation asserting the "nothing to forget"
  message on a fresh repo (idempotent path) — lightweight, no server needed.

## Out of scope / explicitly deferred

- **TTL reaping** — its own follow-up issue (above).
- **Reaping the per-worktree index dir** — keyed by worktree path, not stream id;
  cannot be reliably associated with a stream (ADR-0012). Documented as operator
  responsibility.
- **Auto-unsetting `git-full-send.stream-id`** — documented manual step; an
  `--unset-config` flag can be added later if it proves worth it.

## Touched files (summary)

- `crates/common/src/lib.rs` — `stream_prefix` builder + unit test.
- `crates/server/src/lib.rs` — `forget_stream` fn + `ServerError::ForgetStream`.
- `crates/cli/src/main.rs` — `forget-stream` subcommand + dispatch.
- `crates/client/tests/transfer.rs` — end-to-end forget tests.
- `crates/cli/tests/end_to_end.rs` — CLI help/subcommand assertions.
- `docs/operating.md` — `forget-stream` docs + client-cleanup note.
- `docs/adr/0014-forgetting-a-stream.md` (new), `docs/adr/0012-…md` (non-goal
  update), `docs/adr/README.md` (index row).
- New follow-up issue for TTL reaping (filed via ghwf, not a file change).
