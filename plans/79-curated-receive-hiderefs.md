# Plan — #79: Curated `receive.hideRefs` with a derived anchor set

Measured twice against the real 28.7k-ref workstation repo over a real link
(2026-08-05 stale-clone, 2026-08-06 freshly-fetched-clone). Steady state: packs
are byte-identical with and without hiding (the client deltas against its
retained `sent/*` pin), so hiding saves ~6.3 MB of advertisement and ~2.3 s per
sync with no trade. Cold: pack size is governed by the freshness of the best
advertised ref — the issue's pattern (`!refs/heads/`, `!refs/git-full-send/`)
pushed a 14.0 MB pack against a fresh clone where baseline pushed 195 B,
because on a workstation clone the only fresh ref is `refs/remotes/origin/*`,
which that pattern hides. Adding `!refs/remotes/origin/master` gave 196 B of
pack *and* ~1 KB of advertisement: better than baseline in every scenario
measured.

Anchor derivation was also measured: freshest-N-by-committer-date costs ~350 ms
per run on the 29k-ref repo and surfaces just-pushed branches the client may
not have fetched; resolving `refs/remotes/origin/HEAD` costs ~4 ms and names
exactly the ref that measured as the perfect base. Note the measured repo's own
`HEAD` is detached, so "the repo's default branch" is not a usable anchor
source; the *remote's* default branch is.

Decision recorded as [ADR-0020](../docs/adr/0020-curating-the-ref-advertisement.md).

## Decision

`listen` passes a curated `receive.hideRefs` to every `receive-pack` it
spawns, on by default: hide `refs/`, unhide `refs/git-full-send/`,
`refs/heads/`, and one derived anchor per configured remote — the symref
target of `refs/remotes/<remote>/HEAD`, else `refs/remotes/<remote>/main` /
`…/master` if present, else nothing (and `doctor` says so). Escape hatches:
`--no-hide-refs` and repeatable `--advertise-ref`.

## Changes

### Code

- `crates/server/src/lib.rs`:
  - `advertised_refs(git_dir) -> Vec<String>`: the unhide list —
    `refs/git-full-send/`, `refs/heads/`, plus the per-remote anchors derived
    with `gix` (open the repo, enumerate `remote_names()`, resolve
    `refs/remotes/<r>/HEAD` as a symbolic ref, fall back to `main`/`master`
    existence). Shared by the connection path and `doctor`.
  - `ListenConfig` gains `hide_refs: bool` (default `true`) and
    `advertise: Vec<String>` (default empty; extra unhide patterns).
  - `handle_connection`: when hiding is on, derive the anchor set and pass
    `-c receive.hideRefs=refs/` followed by one `-c receive.hideRefs=!<p>` per
    advertised pattern (derived + `config.advertise`), before the existing
    `-c` flags. Derived per connection — a few ref reads, no cache.
  - `serve_async`: log the advertised set once at startup (info) when hiding
    is on, so an operator can see what a connection will be offered.
- `crates/server/src/doctor.rs`:
  - New `check_anchors`: reports the derived anchor set; WARN per remote that
    contributes no anchor (remedy: `git remote set-head <remote> --auto`);
    WARN when the freshest anchor commit is older than 14 days (remedy: fetch
    — a stale anchor is what makes a cold sync pay full-pack price).
  - `check_refs`: reword the large-advertisement remedy — the hide is now the
    default behaviour of `listen` (ADR-0020), the dedicated-repo/`alternates`
    shape demoted to second-line for costs hiding cannot fix.
- `crates/cli/src/main.rs`: `listen --no-hide-refs` and
  `--advertise-ref <REF_PREFIX>` (repeatable), threaded into `ListenConfig`.

### Tests

- `crates/server` unit tests (using `test-support`): anchor derivation with a
  remote whose `HEAD` symref exists / is absent with `main` / is absent with
  `master` / resolves nowhere; no remotes at all.
- `crates/cli/tests/end_to_end.rs`:
  - A repo padded with dummy `refs/remotes/origin/*` refs: probe through a
    default listener advertises only the curated set (count collapses);
    `--no-hide-refs` restores the full advertisement; `--advertise-ref` adds
    its pattern. Sync still round-trips through the hidden-ref listener.

## Validation

- `cargo test --workspace`, `cargo clippy --workspace --all-targets`,
  `cargo fmt --check`.
- Re-run the real-repo probe/sync through a locally built `listen` with hiding
  active and confirm the advertisement matches the measured curated numbers.
