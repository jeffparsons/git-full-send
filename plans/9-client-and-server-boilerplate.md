# Plan — #9 Client and server boilerplate

Relevant ADRs: [ADR-0001 — Language, runtime & core crates](../docs/adr/0001-language-runtime-and-core-crates.md),
[ADR-0003 — Client/server architecture](../docs/adr/0003-client-server-architecture.md),
[ADR-0002 — Git manipulation strategy](../docs/adr/0002-git-manipulation-strategy.md),
[ADR-0005 — Transfer mechanism](../docs/adr/0005-transfer-mechanism.md).

## Goal

Stand up the **Cargo workspace skeleton**: library crates for the client-side,
server-side, and shared/protocol parts, wrapped by a **single CLI binary**, with
**no real logic** — only typed stubs where logic will land, the latest
dependencies wired up, and shared dependency versions pinned at the workspace
level. Plus **token integration tests** that create a real local git repository
in a temp dir, establishing the harness even though they assert essentially
nothing yet.

This is the first **code** ticket; everything before it was ADRs/research. The
bar is: `cargo build`, `cargo test`, `cargo clippy`, and `cargo fmt --check` all
green on an empty-but-well-shaped workspace.

## Decisions locked in pre-plan (approved 👍 on the issue)

- **Crate layout:** a workspace with `crates/common`, `crates/client`,
  `crates/server`, `crates/cli`. The shared `common` crate is created **now**
  (protocol/ref-namespace constants and shared error types are obviously coming).
- **Naming:** package names `gfs-common` / `gfs-client` / `gfs-server` /
  `gfs-cli`; the **binary is `git-full-send`** (no short alias for now).
- **CLI surface:** **flat** top-level subcommands — `sync` (client), `listen`
  (server), `update-worktree` (server) — since one CLI exposes all commands.
- **Dependencies:** `tokio`, `clap` (derive), `anyhow`, `thiserror`, `gix`,
  `tracing`, `tracing-subscriber`; `tempfile` for tests.

## Out of scope (explicitly)

- Any real Git object synthesis, push/receive, listen loop, or worktree update —
  those are their own tickets. Stubs only.
- CI configuration (possible follow-up ticket; not required here).
- Transport/SSH wiring (ADR-0006), config-file parsing (ADR-0007). Not now.

## Target layout

```
Cargo.toml                      # [workspace] — members, shared package metadata, [workspace.dependencies]
rust-toolchain.toml             # pin stable channel (reproducible builds)
.gitignore                      # /target
crates/
  common/
    Cargo.toml
    src/lib.rs                  # ref-namespace constant, shared error stub
  client/
    Cargo.toml
    src/lib.rs                  # async fn sync(...) -> Result<_, ClientError>  (todo!())
    tests/integration.rs        # token test: temp dir + git init
  server/
    Cargo.toml
    src/lib.rs                  # async fn listen(...), async fn update_worktree(...) (todo!())
    tests/integration.rs        # token test: temp dir + git init
  cli/
    Cargo.toml                  # [[bin]] name = "git-full-send"
    src/main.rs                 # clap derive, tokio main, tracing init, dispatch
  test-support/
    Cargo.toml
    src/lib.rs                  # init_temp_repo() helper — temp dir + `git init`, reused by both test suites
```

A small **`test-support`** helper crate holds the temp-git-repo helper so the
client and server integration tests don't duplicate it, and it models where
real shared test fixtures will live. It is a normal workspace member used only
as a `dev-dependency` (never published / never a runtime dep).

## Approach

### Step 1 — Workspace root

- `Cargo.toml` with `[workspace]` (`members = ["crates/*"]`, `resolver = "3"`).
- `[workspace.package]` for shared `edition = "2024"`, `version`, `license`,
  `repository`, `rust-version` — each crate inherits with `field.workspace = true`.
- `[workspace.dependencies]` pinning every shared dep **once**, so crates depend
  on them with `dep.workspace = true`. Populate via `cargo add` so versions are
  the **current latest** at implementation time (the issue's "latest
  dependencies / version determined at the workspace level"). Note: Research 0001
  pinned gix at 0.84.0 on 2026-06-12; record whatever `cargo add` resolves and
  don't pin below it.
- `rust-toolchain.toml` pinning the `stable` channel; `.gitignore` with `/target`.

### Step 2 — `gfs-common`

- `pub const REF_NAMESPACE: &str = "refs/git-full-send/";` — the writable-ref
  namespace from ADR-0005, the one constant both sides will share.
- A `thiserror`-derived error enum stub (e.g. `CommonError`) to establish the
  typed-error-at-library-boundaries convention from ADR-0001. Keep it minimal
  (a single placeholder variant) — it exists to be extended.
- No `gix`/`tokio` deps yet unless a stub needs them; keep `common` lean.

### Step 3 — `gfs-client` (library)

- Deps: `gfs-common`, `gix`, `tokio`, `thiserror`, `tracing` (all
  `*.workspace = true`).
- Stub the client entry point mirroring ADR-0003 (synthesise + push):
  `pub async fn sync(/* opts */) -> Result<(), ClientError>` with a
  `todo!("synthesise sync state and git push — see ADR-0004/0005")` body and a
  `ClientError` enum (`thiserror`). Async from the start per ADR-0001.
- The stub is intentionally **callable but unimplemented**: `todo!()` makes the
  not-yet-done status compiler-visible and honest if the CLI is run.

### Step 4 — `gfs-server` (library)

- Deps: same shape as client.
- Two stubs matching ADR-0003's two server operations:
  - `pub async fn listen(/* opts */) -> Result<(), ServerError>` — the
    long-running receive loop (`todo!()`).
  - `pub async fn update_worktree(/* opts */) -> Result<(), ServerError>` — the
    on-demand checkout (`todo!()`).
- `ServerError` enum via `thiserror`.

### Step 5 — `gfs-cli` (binary `git-full-send`)

- `[[bin]] name = "git-full-send"`.
- Deps: `gfs-client`, `gfs-server`, `gfs-common`, `clap` (derive feature),
  `anyhow`, `tokio` (`macros` + `rt-multi-thread`), `tracing`,
  `tracing-subscriber`.
- `clap` derive: a top-level `Cli` with a `Commands` enum — `Sync`, `Listen`,
  `UpdateWorktree` (flat, kebab-cased on the CLI). Each variant carries a
  placeholder doc comment; args filled in by later tickets.
- `#[tokio::main] async fn main() -> anyhow::Result<()>`: initialise
  `tracing-subscriber` (env-filter), parse args, `match` to the corresponding
  library stub. `anyhow` at this top-level binary boundary per ADR-0001.

### Step 6 — `test-support` + token integration tests

- `test-support`: `pub fn init_temp_repo() -> tempfile::TempDir` — create a temp
  dir with `tempfile`, run `git init` in it via `std::process::Command`
  (shelling out to the assumed-present `git` CLI, consistent with ADR-0002/0005),
  return the `TempDir` (kept alive by the caller so it isn't cleaned up early).
  Deps: `tempfile` (normal dep here, since this crate *is* test tooling).
- `crates/client/tests/integration.rs` and `crates/server/tests/integration.rs`:
  one token test each — call `init_temp_repo()`, assert the repo exists (e.g.
  `.git` dir present, or `git rev-parse --is-inside-work-tree` succeeds). They
  **deliberately do not** call the `todo!()` library stubs yet; they only prove
  the temp-repo harness works, ready for real tests to build on.
- Wire `test-support` and `tempfile` as `dev-dependencies` of `client` and
  `server`.

### Step 7 — Verify green

Run and confirm all pass:

- `cargo build --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Acceptance criteria

- Workspace with `common`, `client`, `server`, `cli` (+ `test-support`) crates;
  binary is `git-full-send`.
- Shared dependency versions pinned once under `[workspace.dependencies]`; crates
  inherit them.
- Client/server libraries expose async stubs (`sync`; `listen`,
  `update_worktree`) with `thiserror` error types; CLI wires flat `sync` /
  `listen` / `update-worktree` subcommands to them with `clap` + `tokio` +
  `tracing`.
- Token integration tests create a real git repo in a temp dir and pass.
- `cargo build` / `test` / `clippy -D warnings` / `fmt --check` all green.

## Risks / notes

- **`todo!()` stubs panic if invoked.** Intentional and honest for boilerplate —
  the token tests don't call them, so the suite stays green. If a reviewer would
  rather the stubs return `Ok(())` so the CLI runs inertly, that's a trivial swap.
- **`gix` may resolve newer than 0.84.0.** Fine — record the resolved version;
  Research 0001's findings were pinned to 0.84.0 but we're not exercising the
  capability gaps yet.
- **edition 2024 / resolver 3** assume a recent stable toolchain; `rust-toolchain.toml`
  makes that explicit and reproducible.
