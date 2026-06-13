# Plan — #44: Add CI: fmt, clippy, test, and an MSRV build gate

## Goal

Add a GitHub Actions workflow that automatically gates every PR (and every push
to `main`) on the project's existing by-hand quality bar, so a red `fmt`,
`clippy`, `test`, or MSRV build fails the check rather than relying on memory:

- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all`
- a build (`cargo check`) pinned to the declared MSRV, so the workspace's
  `rust-version` claim stays honest.

## Decided in the pre-plan thread

- **MSRV = current stable (1.94).** There are no existing consumers, so we don't
  carry support for an older toolchain. Bump `rust-version = "1.85"` → `"1.94"`
  in `Cargo.toml`; the MSRV job pins the **exact** `1.94` toolchain (not floating
  `stable`). When stable later advances, the main job follows it while the MSRV
  job stays at 1.94 and fails if anything stops building on the declared minimum —
  which is the signal that a deliberate `rust-version` bump is warranted.
- **One workflow, two parallel jobs:** a `check` job (fmt + clippy + test on the
  repo's pinned stable) and an `msrv` job (`cargo check` on 1.94).
- **Caching** via `Swatinem/rust-cache` to keep the large `gix` graph cheap.
- **Third-party actions pinned to tags** (not commit SHAs).
- **`ubuntu-latest` only** — no macOS/Windows matrix for now.

## Background (verified)

- Cargo workspace, 5 crates (`cli`, `client`, `common`, `server`,
  `test-support`), `edition = "2024"`, `resolver = "3"`, `rust-version = "1.85"`
  in `[workspace.package]` of the root `Cargo.toml`.
- `rust-toolchain.toml` pins `channel = "stable"` with
  `components = ["rustfmt", "clippy"]`. On a GitHub runner, the preinstalled
  rustup auto-installs this toolchain **and its components** on the first `cargo`
  invocation — so the `check` job needs no explicit toolchain-setup step.
- **The MSRV wrinkle:** a `rust-toolchain.toml` in the tree takes precedence over
  a toolchain a CI action tries to select, so the `msrv` job must neutralise the
  file (delete it) *before* selecting 1.94, or it would silently run on stable.
- **CI needs `git` on PATH** (ADR-0002): both the tool and the integration tests
  shell out to `git`. GitHub-hosted `ubuntu-latest` ships `git`, so this is
  covered with no extra install.
- Tests need **no global git identity**: `crates/test-support/src/lib.rs` sets
  *repo-local* `user.name` / `user.email` on each scratch repo it creates.
- Tests bind **ephemeral loopback TCP ports** (`crates/server/src/lib.rs` `bind`
  reads back an OS-assigned port on `127.0.0.1`), so they're self-contained and
  CI-safe with no networking setup.
- **The current tree is already green** for all four checks, verified locally on
  1.94.0 (`rustc 1.94.0 (4a4ef493e 2026-03-02)`): `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test --all` all pass; 1.94 is the active toolchain so the MSRV
  `cargo check` is exercised too. The acceptance "green on the current tree" is
  therefore expected to hold on the first CI run.

## Changes

### 1. Bump the declared MSRV — `Cargo.toml`

In `[workspace.package]`, change `rust-version = "1.85"` to `rust-version = "1.94"`.
This is the single source of truth the MSRV job's pinned toolchain mirrors.

### 2. New workflow — `.github/workflows/ci.yml`

A single workflow file (the `.github/workflows/` tree does not exist yet and is
created here).

**Triggers**
- `pull_request` (default branches — i.e. PRs targeting `main`).
- `push` to `main`.

**Top-level**
- A `concurrency` group keyed on the workflow + ref with
  `cancel-in-progress: true`, so a new push to a PR cancels the superseded run.
- `env: CARGO_TERM_COLOR: always` for readable logs.

**Job `check`** (`runs-on: ubuntu-latest`) — the stable quality bar:
1. `actions/checkout@v4`.
2. `Swatinem/rust-cache@v2` (caches the cargo registry + `target`; keys on the
   toolchain rustup resolves from `rust-toolchain.toml`).
3. `cargo fmt --check`.
4. `cargo clippy --all-targets --all-features -- -D warnings`.
5. `cargo test --all`.

   No explicit toolchain step: `rust-toolchain.toml` (stable + rustfmt + clippy)
   drives this job, keeping the repo's pinned toolchain the single source of
   truth for the main checks.

**Job `msrv`** (`runs-on: ubuntu-latest`) — the MSRV gate, runs in parallel:
1. `actions/checkout@v4`.
2. **Neutralise the toolchain pin:** `rm rust-toolchain.toml` (a `run` step) so
   it can't override the next step's selection.
3. `dtolnay/rust-toolchain@1.94` (installs and selects the exact 1.94 toolchain).
4. `Swatinem/rust-cache@v2` (separate cache key — different rustc version).
5. `cargo check --workspace --all-targets --all-features`.

Jobs run concurrently (no `needs`), and a failure in any step fails its job and
therefore the overall check.

## Verification

- Push the branch; ghwf opens the draft PR, which triggers the `pull_request`
  workflow. Confirm via `ghwf pr-checks` that **both** jobs (`check`, `msrv`) run
  and pass on the unmodified tree (acceptance: green on the current tree).
- Sanity-prove the gate actually bites before final hand-off — e.g. confirm the
  reviewer can reason that a `-D warnings` clippy finding or an MSRV-incompatible
  construct would fail the relevant job. (We will not land a deliberate breakage;
  the local runs above already demonstrate the commands are the real bar.)
- Re-run the four commands locally once more on the final tree if `Cargo.toml`'s
  `rust-version` bump is the only code change, to confirm nothing regressed.

## Out of scope / deferred

- No release/publish, coverage, or security-audit (`cargo-deny` / `cargo-audit`)
  jobs — those are separate concerns; file follow-up issues if wanted.
- No macOS/Windows matrix.
- No pinning of third-party actions to commit SHAs (tags only, as agreed) — could
  be tightened later under a supply-chain hardening ticket.
- No ADR: this adds CI plumbing, not a new architectural decision. (If the
  reviewer feels the MSRV-tracks-stable policy deserves recording, a short ADR
  can be added.)
