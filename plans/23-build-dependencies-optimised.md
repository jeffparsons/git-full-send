# Plan — #23: Build dependencies optimised

## Goal

Make Cargo compile **external dependencies** (everything from crates.io and other
non-path sources) with optimisations turned on even in debug/dev builds, while
leaving our own workspace crates unoptimised so they stay fast to compile and
fully debuggable. This is the common Cargo trick for keeping iterative dev builds
quick *and* not paying an interpreter-speed penalty when running/testing against
heavy dependencies.

Per the pre-plan discussion on #23, this is explicitly **not** about our local
path crates — those continue to build at `opt-level = 0`.

## Design

Add a per-package dev-profile override to the root `Cargo.toml`:

```toml
# Compile external dependencies with optimisations even in debug builds. The
# "*" glob matches only non-path (registry/git) packages, so our own workspace
# crates (gfs-common, gfs-client, gfs-server, test-support) stay at opt-level 0
# for fast, debuggable iterative builds.
[profile.dev.package."*"]
opt-level = 3
```

### Why this placement / form

- **Root `Cargo.toml`, not a member crate.** Profile settings are only honoured
  in the workspace root manifest; Cargo ignores `[profile.*]` in member crates
  and would warn. So this belongs alongside `[workspace]` in the top-level
  `Cargo.toml`.
- **`[profile.dev.package."*"]`** applies the override to every dependency. The
  `"*"` glob is Cargo's well-defined "all packages that are *not* path
  dependencies of the workspace" selector, so it deliberately excludes our four
  local crates — exactly matching the issue's "not my local path crates" note.
- **`opt-level = 3`** is the conventional choice for this pattern (matches what
  most projects use). It only affects how *dependencies* are built, so the hit
  to our own edit-compile-run loop is limited to the one-off cost of compiling
  deps optimised — which is cached across rebuilds.
- **Scope: dev profile only.** The `test` profile inherits from `dev`, so test
  builds pick this up automatically; no separate `[profile.test...]` entry is
  needed. `release` is already fully optimised, so it is untouched.

### No ADR

This is a low-stakes, trivially reversible build-config tweak with a single
obvious convention behind it. Per the pre-plan discussion it does not warrant an
ADR; the rationale lives in the inline comment in `Cargo.toml`.

## Files touched

- `Cargo.toml` — add the `[profile.dev.package."*"]` block with the explanatory
  comment above. No other files change.

## Verification

- `cargo build` from a clean target builds successfully; on first build the
  dependencies are compiled optimised (visible as the longer one-off dep-compile
  step), then cached.
- `cargo metadata --format-version=1` / `cargo build -v` confirms the override is
  parsed without warnings (a malformed profile key would warn or error).
- Sanity-check that our own crates are *not* optimised: a `cargo build -v` shows
  `-C opt-level=0` for the `gfs-*` / `test-support` crate invocations and a
  higher opt-level for registry deps. (Manual spot-check; not worth an automated
  test.)
- Existing `cargo test` suite still passes.

## Out of scope

- Any change to local path crates' optimisation level.
- `release` profile tuning, LTO, codegen-units, or other build-perf knobs — not
  requested here.
