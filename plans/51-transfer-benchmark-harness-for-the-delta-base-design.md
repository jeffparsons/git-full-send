# Plan — #51 Transfer benchmark harness for the delta-base design

Relevant ADRs / research:
[ADR-0005 — Transfer mechanism](../docs/adr/0005-transfer-mechanism.md),
[ADR-0013 — Recording operation metrics](../docs/adr/0013-recording-operation-metrics.md),
[Research-0003 — Transfer mechanism & pack-performance root-cause](../docs/research/0003-transfer-mechanism-and-pack-performance.md).
Related issue: [#50 per-chain delta policy](50-per-chain-delta-policy-split-the-extra-and-code-pushes.md).

## Goal

Validate the ADR-0005 delta-base design with **real numbers**: a runnable harness
that constructs a repo with a large changed artifact and reports the
bytes-on-the-wire of syncing it **with vs. without** the retained delta base, so the
design's payoff is measurable and regressions are visible. Capture the measured
numbers in Research-0003 (its §5 caveat names exactly this follow-up:
"sync a changed build output with vs. without the prior tip retained, measuring pack
size").

## What's already in place (so this is assembly, not new infrastructure)

- **Bytes-on-the-wire is already measured by the server.** Every `git receive-pack`
  connection writes a `receive` record to the repo's JSONL metrics sink
  (`gfs_common::metrics::metrics_path(git_dir)` →
  `<git-dir>/git-full-send/metrics.jsonl`) carrying `bytes_in` (the inbound pack,
  counted by `pump_counting`) and `refs_updated` (the refs that push landed) —
  `crates/server/src/metrics.rs`, written at `crates/server/src/lib.rs:508`. That
  `bytes_in` **is** the transfer-size number the benchmark needs; no new measurement
  seam is required.
- **An in-process A/B harness pattern already exists.**
  `crates/client/tests/transfer.rs` stands up the real `gfs_server` listener on an
  ephemeral localhost port (`start_server`) and drives the real client push over it
  — genuine `git push` → `git receive-pack` exchanges. The benchmark reuses this
  shape exactly.
- **The needed entry points are already public:** `gfs_client::push_ref`,
  `gfs_client::DeltaPolicy` (`Thin` / `WholeObject`), and
  `gfs_common::metrics::metrics_path`. The harness drives **single-ref** pushes via
  `push_ref` (not full `sync`) for exact control of policy and base state, and one
  unambiguous `receive` record per measured push.

## Decisions locked in pre-plan (approved 👍 on the issue)

- **Form: an `#[ignore]`-d `#[tokio::test]` benchmark**, runnable on demand with one
  command, that prints a with/without results table and **asserts** the key
  inequalities (so running it doubles as a regression check). Chosen over adding
  `criterion`/a `benches/` target (statistical sampling we don't need) or a
  standalone bin — it reuses the existing harness directly. `#[ignore]` keeps a
  multi-MiB, report-oriented test out of the default `cargo test` run.
- **Primary metric: bytes-on-the-wire (`bytes_in`).** Wall time is reported as a
  *soft, caveated secondary* (`duration_ms` from the same record) — in-process,
  single-machine, so noisy and not load-bearing.
- **Cover both halves of the story** (the user approved including the `extra`-vs-thin
  contrast):
  1. **Core ADR-0005 payoff** — a *delta-friendly* artifact through the **code/thin**
     chain: base present → small delta; base absent → whole object (the bimodal split
     of Research-0003 §2.1).
  2. **#50 justification** — a *delta-hostile* (incompressible, fully-rebuilt)
     artifact: the **extra/whole-object** policy costs ~the same bytes as a **thin**
     push of the same change, confirming the delta search buys nothing for content
     that won't delta (Research-0003 §2.3) so whole-object is the predictable choice.
- **Capture results in Research-0003** (a new "Benchmark results" section), per the
  acceptance criterion's first suggestion, rather than a new doc.

## Out of scope (explicitly)

- Any change to production `src/` code — the harness is purely additive (a new test
  file + one dev-dependency + a doc section). All entry points it needs are already
  public.
- `criterion`, a `benches/` target, or statistical sampling/CPU-cycle profiling.
- Wiring the benchmark into CI as a required gate (it's `#[ignore]`-d). Noted as an
  optional follow-up; not done here.
- Tuning `pack.window`/`pack.depth`/`core.bigFileThreshold` — the lever under test is
  base-present-vs-absent and whole-object-vs-thin, not fine pack tuning.
- Re-running the original prototype's exact slow instances (Research-0003 §5 scopes
  that out); we reproduce the *mechanism* (bimodal delta-base cost), not a historical
  trace.

## Design

### File and shape

A new test file `crates/client/tests/delta_base_benchmark.rs` with a single
`#[ignore]`-d `#[tokio::test]` (e.g. `delta_base_transfer_benchmark`). Run with:

```
cargo test -p gfs-client --test delta_base_benchmark -- --ignored --nocapture
```

`--nocapture` surfaces the printed results table; `--ignored` opts the multi-MiB
harness in. (Debug profile is fine — `bytes_in` is independent of the build profile;
`--release` only matters if one cares about the soft `duration_ms` column.)

### Helpers in the test file

- `start_server(repo) -> SocketAddr` — copied verbatim from `transfer.rs` (test files
  are separate compilation units, so it can't be imported). ~10 lines; the small
  duplication is acceptable for a self-contained harness. (Optional alternative:
  promote it into `test_support`, but that would make `test_support` depend on
  `gfs_server`/`tokio` — deferred to keep scope tight.)
- `measure_push(client_repo, addr, ref_name, policy) -> (bytes_in, duration_ms)` —
  runs `gfs_client::push_ref`, then **polls** the server's metrics sink
  (`gfs_common::metrics::metrics_path(server_path)`) until a `receive` record whose
  `refs_updated` contains `ref_name` appears, and returns its `bytes_in`/`duration_ms`.
  Polling is required because the server writes the record *after* it shuts the
  socket down (`lib.rs:478` → `:508`), which can be just after the client's push
  returns — a real ordering race. Bounded loop (e.g. ≤2 s, 20 ms steps); take the
  **last** matching record so a reused server is still unambiguous. Parsing uses
  `serde_json` (added as a dev-dependency; see below) over `metrics.jsonl` lines.
  The helper takes the server's repo path (the bare repo dir is its git dir, so
  `metrics_path(server.path())`).
- Deterministic artifact generators (no RNG dependence, so numbers reproduce):
  - `delta_friendly(seed) -> Vec<u8>` — ~4 MiB of structured text (e.g. ~100k
    numbered lines). The v2 variant changes a single line in the middle, modelling an
    incremental rebuild touching a small region.
  - `delta_hostile(seed) -> Vec<u8>` — ~4 MiB of incompressible pseudo-random bytes
    from a tiny inline seeded PRNG (e.g. xorshift64; not `getrandom`, which would be
    non-deterministic). The v2 variant uses a *different seed* → fully different
    bytes, modelling a rebuilt binary that shares nothing with its predecessor.

  4 MiB keeps each push sub-second and stays well under `core.bigFileThreshold`
  (512 MiB) so the artifact is delta-*eligible* (otherwise git would send it whole
  unconditionally — Research-0003 §2.2).

Each measured push is set up by writing the artifact into a fresh client repo
(`test_support::{init_temp_repo, write_file, commit_all}`), pointing the chain ref at
`HEAD` with `git update-ref <ref> HEAD`, and pushing to a **fresh** bare server
(`init_bare_repo`) so base presence is controlled by *which server already holds the
prior tip*, not by gc timing.

### Scenarios (the measurement matrix)

Refs use a fixed test stream: `code = gfs_common::code_ref(&stream)`,
`extra = gfs_common::extra_ref(&stream)`.

**A — code/thin, base PRESENT (delta-friendly).** Repo R: commit baseline +
artifact_v1; `update-ref code HEAD`; push `code`/`Thin` to fresh server S_A (this
establishes v1 as the server's advertised base — the first push's bytes are
discarded). Then in R: change artifact to v2; commit; `update-ref code HEAD`; push
`code`/`Thin` to S_A → **measure A**. Negotiation excludes everything reachable from
S_A's `code` (v1), so v2 rides as a thin `OBJ_REF_DELTA` against v1.

**B — code/thin, base ABSENT (same v2 content).** Repo R2: commit baseline +
artifact_v2; `update-ref code HEAD`; push `code`/`Thin` to fresh, empty server S_B →
**measure B**. No advertised base ⇒ v2 sent whole.

Expectation: **A ≪ B** — the core ADR-0005 delta-base payoff. Assert conservatively
(e.g. `A * 4 < B`) to demonstrate the order-of-magnitude split without flakiness.

**C — extra/whole-object, base PRESENT (delta-hostile).** Repo R3: commit baseline +
dh_v1; `update-ref extra HEAD`; push `extra`/`WholeObject` to fresh S_C (establishes
base). Then regenerate dh → v2 (different seed); commit; `update-ref extra HEAD`; push
`extra`/`WholeObject` to S_C → **measure C**. This is exactly the production `extra`
policy (`--no-thin -c pack.window=0`) on a changed, poorly-deltifying artifact.

**D — code/thin of the same delta-hostile change (contrast).** Same setup as C but
through `code`/`Thin`: push dh_v1 base to fresh S_D, then push the **same** dh_v2 with
`Thin` → **measure D**. The thin delta search runs but finds no usable base (v2 shares
nothing with v1), so the object is sent whole.

Expectation: **C ≈ D**, and both ≈ the whole artifact size (≫ A). Assert thin saved
little over whole on delta-hostile content (e.g. `D as f64 > C as f64 * 0.8`). This
is the #50 justification: when the output won't delta, `--thin` buys ~nothing on the
wire while still paying the futile delta-search CPU (Research-0003 §2.3) — so the
predictable whole-object policy loses nothing and is the right call. (The CPU cost
isn't captured in `bytes_in`; the soft `duration_ms` column hints at it but is not
asserted on.)

### Output

Print a Markdown table to stdout (visible under `--nocapture`), e.g.:

```
| Scenario                                   | Chain  | Policy       | Base    | bytes_in |
| ------------------------------------------ | ------ | ------------ | ------- | -------- |
| A delta-friendly, changed line             | code   | thin         | present |      … |
| B delta-friendly, same content             | code   | thin         | absent  |      … |
| C delta-hostile, rebuilt artifact          | extra  | whole-object | present |      … |
| D delta-hostile, rebuilt artifact          | code   | thin         | present |      … |
```

plus one-line derived ratios (A/B payoff; D/C thin-vs-whole on delta-hostile).

### Assertions (regression visibility)

Inside the test, after building the table:

- `A * 4 < B` — base retention gives at least a ~4× (in practice far larger)
  reduction for delta-friendly content.
- `D as f64 > C as f64 * 0.8` — thin does **not** materially beat whole-object on
  delta-hostile content (they're within ~20%).
- Both `C` and `D` are within a small factor of the raw artifact size (sanity: the
  changed delta-hostile artifact really did cross whole).

Factors are deliberately conservative so the assertions encode the *direction* of the
result, not a brittle exact byte count, and won't flake across git versions.

### Dev-dependency

Add to `crates/client/Cargo.toml` `[dev-dependencies]`:

```toml
serde_json.workspace = true
```

(`serde_json` is already a pinned workspace dependency; the harness uses it to parse
the `receive` records out of `metrics.jsonl`.) No other new dependencies — `gfs_server`,
`gfs_common`, `test_support`, `tempfile`, and `tokio` (macros, rt) are already client
dev-dependencies.

### Research-0003 results section

Append a new section to
`docs/research/0003-transfer-mechanism-and-pack-performance.md`, e.g.
**"## 6. Benchmark results (issue #51)"**, containing:

- the one-line reproduce command,
- the results table populated from a **real run** in the build phase,
- a short interpretation tying A≪B back to §2.1 (the bimodal base-present/absent
  split) and §3 (predictability via retention + `--thin`), and C≈D back to §2.3 and
  the #50 whole-object decision,
- a note that this resolves the §5 "follow-up benchmark" caveat (and a back-reference
  near §5 pointing to §6).

Numbers are filled in from an actual `--ignored` run during implementation, not
guessed.

## Acceptance criteria mapping

- *A runnable benchmark reports the with/without-delta-base transfer cost* →
  `delta_base_benchmark.rs`: scenarios A (with base) vs B (without base) on the
  code/thin chain, reported as `bytes_in` in the printed table; run with the one-line
  command above.
- *Results captured (e.g. appended to Research-0003 or a short notes doc)* → the new
  Research-0003 §6 populated from a real run.
- *Payoff measurable / regressions visible* → the assertions (`A*4 < B`, `D > 0.8·C`)
  fail if the delta-base advantage regresses or if `extra`'s whole-object policy
  starts costing materially more than thin would.
- *Useful input for #50* → scenarios C/D quantify that whole-object ≈ thin in bytes
  for delta-hostile outputs, the empirical backing for #50's per-chain split.

## Definition of done

- `crates/client/tests/delta_base_benchmark.rs` added; `cargo test -p gfs-client
  --test delta_base_benchmark -- --ignored --nocapture` runs green and prints the
  table.
- `serde_json` added to client `[dev-dependencies]`.
- Research-0003 §6 added and populated from a real run; the §5 caveat cross-references
  it.
- The existing default suite is unaffected (the new test is `#[ignore]`-d); `cargo
  build`, `cargo test`, `cargo clippy`, `cargo fmt --check` all green. No `src/`
  changes.
