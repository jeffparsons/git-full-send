# Plan — #22 MVP hardening: end-to-end tests, CLI/config, operator docs

## Goal

Harden and document the MVP now that all three commands work. Three deliverables:

1. **One consolidated end-to-end test** that drives the real `git-full-send`
   binary as a subprocess across a full loopback round-trip and asserts an exact
   remote-worktree match, including `extra` files at identity paths and deletions
   of both `code`-tree files and dropped force-includes.
2. **CLI finalisation**: add `--user-include <PATH>` to `sync`; otherwise the
   `clap` surface for `sync` / `listen` / `update-worktree` / `list-streams` is
   already complete (confirmed during pre-plan).
3. **Operator docs**: a short README quickstart plus a fuller
   `docs/operating.md` guide covering the SSH tunnel, running the server
   commands, and writing force-include pattern files.

## Decisions locked at pre-plan (approved 👍)

The issue framing predates the current code; pre-plan confirmed the real gaps:

1. **End-to-end test drives the actual binary** (subprocess), not library calls
   — it is the only way to exercise the "finalised CLI surface". Existing
   library-level tests in `crates/client/tests/{transfer,extra,integration}.rs`
   stay as-is (they already cover the round-trip mechanics at the library level).
2. **`sync` gains `--user-include <PATH>`** — a CLI equivalent of the existing
   `GIT_FULL_SEND_USER_INCLUDE` env override (mirrors git's `core.excludesFile`).
   The **project** pattern file stays fixed at `.git-full-send-include`: ADR-0007
   makes it a committed, shared artifact, so a path flag would undercut that.
3. **No config-file format for the MVP** — `clap` flags cover per-invocation
   args, git-config already persists `git-full-send.stream-id`, and env/XDG
   covers the per-user include. A TOML/config mechanism is deferred until a
   concrete need appears.
4. **Docs split**: README quickstart + `docs/operating.md` operator guide.

## Architecture / changes

### 1. CLI: `--user-include` on `sync`

The override must thread from the CLI down to selection. Today
`select_extra_paths(workdir)` resolves the per-user file internally via
`user_include_path()` (env → XDG → `$HOME`), while the test seam `select_in`
already takes an explicit `Option<&Path>`. Plan:

- **`crates/cli/src/main.rs`**: add to `SyncArgs`
  `#[arg(long, value_name = "PATH")] user_include: Option<PathBuf>` and pass it
  into `gfs_client::sync`.
- **`crates/client/src/lib.rs`**: extend `sync(repo_dir, remote, stream,
  user_include: Option<PathBuf>)` and forward the override to `encode_extra`.
- **`crates/client/src/encode.rs`**: give `encode_extra` an explicit
  `user_include: Option<&Path>` parameter, passed to a new
  `select_extra_paths_with(workdir, user_include_override)`.
- **`crates/client/src/select.rs`**: add a public entry point that takes an
  explicit override and falls back to `user_include_path()` when `None`:
  `None` → today's env/XDG behaviour, `Some(path)` → use `path` directly. Keep
  `select_extra_paths` as the no-override convenience wrapper so existing call
  sites and tests are undisturbed. The flag thus has the **same precedence** as
  `GIT_FULL_SEND_USER_INCLUDE` (it *is* that override, chosen explicitly); if
  both are somehow set, the CLI flag wins (document this).

This is a small signature change rippling through `sync` → `encode_extra` →
`select`. Update the existing `sync`/`encode_extra` call sites in
`crates/client/tests/*` to pass `None` (no behaviour change for them).

Doc-comments: update `sync` and `encode_extra` to mention the override; note on
the flag that it mirrors `GIT_FULL_SEND_USER_INCLUDE`.

### 2. End-to-end test (new file: `crates/cli/tests/end_to_end.rs`)

A single `#[test]` (sync/parsing is synchronous from the test's view — it shells
out to the built binary) that exercises the whole pipeline through the **CLI
binary**, resolved via `env!("CARGO_BIN_EXE_git-full-send")` (Cargo sets this for
integration tests of the crate that defines the binary). Shape:

1. **Server**: bind `listen` on an ephemeral loopback port. The binary's
   `--addr` needs a concrete port; `127.0.0.1:0` lets the OS pick but the child
   must report it back. Two viable approaches — pick during implementation:
   - **(preferred)** call `gfs_server::bind("127.0.0.1:0")` + `serve` on a
     background thread *in-process* (as `transfer.rs` does) to get a known port,
     and drive only the **client** side (`sync`) and the **checkout** side
     (`update-worktree`) through the CLI binary. This still exercises the full
     CLI surface for the two commands an operator runs by hand each cycle, and
     sidesteps port-discovery flakiness. `listen`'s own arg-parsing is covered by
     a tiny separate assertion (e.g. `--help` / a bind smoke check).
   - **(alternative)** spawn `git-full-send listen` as a child on a fixed-but-
     unlikely port with ret/bind-retry. Rejected unless needed: racy and flaky.

   Go with the preferred approach: **in-process server, CLI-driven `sync` and
   `update-worktree`.** Document in the test why (determinism), so the intent is
   clear.
2. **Client repo** (`init_temp_repo`): committed code (incl. a nested path and
   a file that will later be deleted), a `.gitignore`, and a
   `.git-full-send-include` selecting a gitignored build dir. Add working-tree
   state: a modified tracked file, a staged file, an untracked file, and a
   gitignored-but-force-included file — and a force-included file at a path that
   **also** exists in `code` to prove the `extra` overlay wins (identity-path
   same-name collision).
3. **Round 1**: run `git-full-send sync --repo <client> --remote <addr>
   --stream-id e2e` as a subprocess; assert success. Run `git-full-send
   update-worktree --repo <server> --worktree <wt> --stream-id e2e`; assert
   success. Assert the worktree's **exact** file set and contents equal the union
   of the server `code` and `extra` trees (reuse the `worktree_files` /
   `tree_paths` helper pattern from `transfer.rs`), and that the overlaid
   same-path file has the `extra` content.
4. **Round 2 (deletions)**: on the client, delete one committed `code` file
   (commit it) **and** drop one force-included file from the selection; re-run
   `sync` then `update-worktree` against the **same** worktree. Assert both the
   dropped `code` file and the dropped `extra` file are **gone**, surviving files
   are intact, and the worktree still equals the new `code`∪`extra` union
   exactly.

Helpers: `crates/test-support` already provides `init_temp_repo`,
`init_bare_repo`, `git`, `write_file`, `commit_all`. The `worktree_files` /
`tree_paths` helpers currently live inside `transfer.rs`; copy the small versions
into the new test file (keeping tests independent) rather than promoting them to
`test-support`, unless promotion is obviously cleaner — decide during
implementation, but prefer a local copy to avoid widening the shared surface for
one consumer.

Crate wiring: `crates/cli` needs `gfs_server`, `tempfile`, and `test-support` as
**dev-dependencies** for the in-process server + temp repos (check
`crates/cli/Cargo.toml`; add what's missing). The binary target name must match
the `CARGO_BIN_EXE_*` env var — confirm the `[[bin]]`/package name resolves to
`git-full-send`.

### 3. Operator docs

- **`README.md`**: replace the stub with a concise quickstart — what the tool
  does (one paragraph, link to the ADRs), install/build, and a minimal
  three-step round-trip (tunnel up → `listen` on remote → `sync` from client →
  `update-worktree` on remote). Link to `docs/operating.md` for detail.
- **`docs/operating.md`** (new): the operator guide.
  - **SSH tunnel** (ADR-0006): `ssh -N -L 9419:localhost:9419 user@workstation`
    (local-forward the client's localhost port to the remote's loopback
    `listen`), and point `sync --remote 127.0.0.1:9419` at the local end.
    Explain *why* it's localhost-only (no built-in auth/encryption; the tunnel is
    the trust boundary) and that the tunnel is a manual prerequisite.
  - **Running the server**: `git-full-send listen --repo <bare-or-target>
    [--addr 127.0.0.1:9419]` as the long-running receiver; `git-full-send
    update-worktree --repo <repo> --worktree <dir> --stream-id <id>` as the
    on-demand authoritative checkout (note its destructive overwrite semantics,
    ADR-0008, and that `list-streams` discovers synced stream ids).
  - **Force-include pattern files** (ADR-0007): the committed project file
    `.git-full-send-include` at the repo root; the optional per-user file via
    `$XDG_CONFIG_HOME/git-full-send/include` (or `$HOME/.config/...`),
    `GIT_FULL_SEND_USER_INCLUDE`, or the new `sync --user-include`. Explain the
    gitignore syntax with inverted polarity (bare pattern *includes*, `!` carves
    out), the `[project, then user]` last-match-wins layering, and a worked
    example (force-include `dist/`, carve out `dist/secret`).
  - **Stream ids**: caller-chosen, stable across syncs, may be branch-shaped;
    auto-generated and persisted to `git-full-send.stream-id` when `--stream-id`
    is omitted on the client.

Keep docs consistent with the constants/ADRs (cross-check `DEFAULT_LISTEN_ADDR`,
`PROJECT_INCLUDE_FILE`, `USER_INCLUDE_ENV`, and the XDG path in `select.rs` so
the doc never drifts from the code).

## Testing

- New `crates/cli/tests/end_to_end.rs` as above — the single consolidated
  round-trip test (rounds 1 and 2) plus a minimal `listen` arg-parse smoke check.
- Existing library-level tests remain green (the `sync`/`encode_extra` signature
  change requires touching their call sites to pass `None`).
- `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt --check` all green.

## Sequencing

1. Thread `--user-include` through `select` → `encode_extra` → `sync` → CLI;
   update existing call sites; build green.
2. Add `crates/cli` dev-dependencies; write `end_to_end.rs`; iterate to green.
3. Write `README.md` quickstart and `docs/operating.md`.
4. Full `cargo build` / `test` / `clippy` / `fmt --check` sweep.

## Acceptance (from the issue)

- Single end-to-end test drives the full sync round-trip and asserts an exact
  remote match incl. extra files and deletions. ✓ step 2
- All three commands have finalised, documented args/config. ✓ steps 1, 3
  (`--user-include` added; no config file needed — pre-plan decision)
- Operator docs cover tunnel setup, running the server commands, and writing
  force-include patterns. ✓ step 3
- Build/test/clippy/fmt green. ✓ step 4

## Out of scope (per the issue)

- Built-in transport auth/encryption (ADR-0006 defers it).
- CI configuration.
- A `--include` project-path override (pre-plan: project file stays fixed) and
  any standalone config-file format (pre-plan: deferred).
