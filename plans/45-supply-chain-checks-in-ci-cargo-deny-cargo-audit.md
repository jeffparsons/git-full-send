# Plan — #45: Supply-chain checks in CI (cargo-deny / cargo-audit)

## Goal

Add an automated supply-chain gate so a PR (or push to `main`) fails when the
dependency tree picks up a **security advisory** or a **disallowed/unknown
license**, and so duplicate-version and crate-source drift are surfaced. Commit
a `deny.toml` that encodes the policy and documents any deliberate exceptions.

Acceptance (from the issue): a CI job fails on a new security advisory or a
disallowed/unknown license; `deny.toml` is committed and documents exceptions.

## Decided in the pre-plan thread (approved)

- **`cargo-deny` only, not `cargo-audit`.** cargo-deny's `advisories` check is a
  strict superset of cargo-audit for our needs, and the one tool/config also
  covers licenses, bans (duplicates), and sources. Adding cargo-audit too would
  be redundant.
- **Allow `MPL-2.0`.** The tree is all-permissive except `uluru 3.1.0`
  (`MPL-2.0`), pulled transitively by `gix` as an LRU cache. MPL-2.0 is a
  file-level (weak) copyleft that applies only to `uluru`'s own files and imposes
  no obligation on our code; replacing the dep isn't in our control. Allow it,
  with a comment recording why.
- **Duplicates = `warn`, not deny.** The only duplicates are `hashbrown` (3) and
  `wit-bindgen` (2), both from the gix/wasi transitive tree and not fixable from
  our side. `warn` surfaces them without a brittle gate.
- **Scheduled advisory run.** In addition to PR/push, run the checks on a daily
  cron so a new advisory landing against an *unchanged* tree is caught promptly,
  not just on the next unrelated PR.
- **Same workflow as #44.** Wire it into the existing `.github/workflows/ci.yml`,
  using `EmbarkStudios/cargo-deny-action@v2` pinned to a **tag** (consistent with
  #44's "tags, not SHAs" convention).
- **Scope.** This PR is the supply-chain gate only; shipping our own LICENSE
  files stays with sibling issue #46.

## Background (verified locally)

Installed `cargo-deny 0.19.8` and ran every check against the current tree
(`--all-features`):

- **`check advisories` → ok.** No RUSTSEC advisories and no unmaintained-crate
  warnings today, so `deny.toml` needs **no advisory `ignore` entries** to start
  green.
- **`check licenses`** — the full set of SPDX licenses present is: `Apache-2.0`
  (incl. `Apache-2.0 WITH LLVM-exception`), `MIT`, `MIT-0`, `BSD-3-Clause`,
  `CC0-1.0`, `Unicode-3.0`, `Unlicense`, `Zlib`, and `MPL-2.0` (uluru only).
  `r-efi 6.0.0` reports as LGPL but its expression is
  `MIT OR Apache-2.0 OR LGPL-2.1-or-later`, satisfied by allowing MIT/Apache — so
  **no LGPL allowance is needed**.
- **`check bans` →** two duplicate-version warnings (`hashbrown`, `wit-bindgen`);
  no bans.
- **`check sources` → ok.** Everything resolves from the crates.io registry; no
  git sources.

cargo-deny 0.19.x uses the modern config schema: the `[licenses]` section is a
strict allow-list (anything not in `allow` is denied — the old
`unlicensed`/`copyleft`/`allow-osi-fsf-free` knobs are gone), and `[advisories]`
denies vulnerabilities/unmaintained by default (no per-kind lint-level fields).

## Changes

### 1. New file — `deny.toml` (workspace root)

```toml
# Supply-chain policy for `cargo deny` (see issue #45). Enforced in CI by the
# `supply-chain` job in .github/workflows/ci.yml. Run locally with:
#   cargo deny check
#
# Config schema: cargo-deny 0.19.x.

[graph]
# Match the rest of CI, which builds with --all-features.
all-features = true

[advisories]
# Vulnerabilities and unmaintained crates are denied by default in this
# cargo-deny version; a fresh advisory in the tree fails the check. List
# deliberate, documented exceptions here when (and only when) we accept one,
# e.g.:
#   ignore = [
#     { id = "RUSTSEC-0000-0000", reason = "why we accept this, and the plan to remove it" },
#   ]
ignore = []

[licenses]
# The workspace is `MIT OR Apache-2.0`; we accept permissive (and permissive-
# equivalent) licenses from dependencies. Anything not listed here fails the
# check. The set below is exactly what the current tree resolves to, plus `ISC`
# (ubiquitous in the Rust ecosystem) to avoid a spurious failure the first time
# a routine `cargo update` pulls an ISC-licensed crate.
allow = [
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "MIT",
    "MIT-0",
    "BSD-3-Clause",
    "ISC",
    "CC0-1.0",
    "Unicode-3.0",
    "Unlicense",
    "Zlib",
    # Weak, file-level copyleft. Pulled transitively by `gix` (the `uluru` LRU
    # cache). MPL-2.0 obligations apply only to MPL-licensed files, not to our
    # own code, and the dep isn't under our control — so we accept it.
    "MPL-2.0",
]
confidence-threshold = 0.8

[bans]
# Duplicate versions are reported but don't fail CI: the only duplicates come
# from the gix/wasi transitive tree and aren't fixable from our side.
multiple-versions = "warn"
wildcards = "deny"

[sources]
# Only the public crates.io registry is allowed; an unknown registry or any git
# dependency fails the check.
unknown-registry = "deny"
unknown-git = "deny"
allow-registry = ["https://github.com/rust-lang/crates.io-index"]
```

Notes:
- `wildcards = "deny"` and the strict `[sources]` levels go slightly beyond the
  literal acceptance criteria but are cheap hardening that the current tree
  already satisfies (all versions are pinned in `Cargo.toml`; everything is from
  crates.io). They cost nothing now and catch real drift later. If the reviewer
  prefers the minimal gate, these can be relaxed to `warn`.
- The `ISC` entry is the only allowed license **not** currently in the tree; it's
  included deliberately so a routine dependency bump doesn't trip the gate on one
  of the most common permissive licenses. Everything else in `allow` is present
  today and verified.

### 2. Wire into CI — `.github/workflows/ci.yml`

Add a daily schedule trigger and a `supply-chain` job; keep the existing
`check` and `msrv` jobs from running on the daily cron (they gate code, not the
advisory DB).

**`on:` block** — add a `schedule` trigger:

```yaml
on:
  pull_request:
  push:
    branches: [main]
  schedule:
    # Daily, so a new advisory against an unchanged tree is caught promptly.
    - cron: "0 6 * * *"
```

**Existing jobs** — guard `check` and `msrv` so the daily cron only runs the
supply-chain gate (add to each job, alongside `runs-on:`):

```yaml
    if: github.event_name != 'schedule'
```

**New job `supply-chain`** (`runs-on: ubuntu-latest`):

```yaml
  supply-chain:
    name: cargo-deny (advisories + licenses + bans + sources)
    runs-on: ubuntu-latest
    timeout-minutes: 20
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2
        with:
          command: check
          # advisories + bans + licenses + sources
          arguments: --all-features
```

The action downloads its own `cargo-deny` binary (no Rust toolchain or
`rust-cache` step needed) and, with `command: check` and no explicit check
names, runs **all** checks against the committed `deny.toml`. A failure in any
check fails the job and therefore the overall workflow.

## Verification

- **Locally (already done, will re-confirm on the final tree):** with the new
  `deny.toml` in place, `cargo deny check` passes all four checks
  (advisories/licenses/bans/sources) — bans emits the two expected duplicate
  *warnings* without failing.
- **In CI:** push the branch; ghwf opens the draft PR, triggering the
  `pull_request` workflow. Confirm via `ghwf pr-checks` that the new
  `supply-chain` job runs and passes (green on the current tree), and that
  `check` + `msrv` still run on the PR (the `schedule` guard only excludes the
  cron event, not `pull_request`/`push`).
- **Gate bites (reasoned, not landed):** removing a needed license from `allow`,
  or an advisory appearing in the tree, makes `cargo deny check` exit non-zero —
  which fails the job. The local runs demonstrate the command is the real bar; we
  won't land a deliberate breakage.

## Out of scope / deferred

- **`cargo-audit`** — superseded by cargo-deny's advisories check (decided above).
- **Our own LICENSE files** — sibling issue #46.
- **Pinning third-party actions to commit SHAs** — tags only, matching #44; a
  future supply-chain-hardening pass could tighten this for both workflows.
- **An ADR** — this adds CI policy/plumbing, not a new architectural decision. If
  the reviewer judges the "permissive-only + allow MPL-2.0" license policy worth
  recording as a decision, a short ADR can be added; the `deny.toml` comments
  already capture the rationale inline.
