# Plan — #80: Additive `--extra-include` so callers stop mirroring the user-include lookup

`--user-include` *replaces* the per-user lookup, so a caller that wants to
contribute its own force-include patterns without silently dropping the
developer's own file has to mirror the lookup order
(`GIT_FULL_SEND_USER_INCLUDE` → `$XDG_CONFIG_HOME` → `$HOME/.config`) and
concatenate — a copy that diverges with no signal (Stile's `dev` CLI does
exactly this today).

## Decision (settled in the issue, plus two calls made here)

Add a repeatable **`sync --extra-include <path>`** that layers *on top of* the
normal per-user lookup instead of replacing it. Evaluation order becomes
`[project, user, extra…]` with last-match-wins (extras in command-line order),
so a caller's patterns win over the user's, and a later extra wins over an
earlier one. `--user-include` keeps its replacing semantics unchanged; when
both are given, the extras layer on top of the override.

Two points not fixed by the issue:

- **A missing `--extra-include` path is a hard error**, unlike every other
  pattern file (project, user, and an explicit `--user-include` path all treat
  missing as an empty layer). The flag exists for programmatic callers passing
  a file they just wrote, so a missing path is always a bug — and silently
  dropping patterns is exactly the failure mode this issue exists to
  eliminate.
- **No new ADR.** ADR-0007's model (layered gitignore-syntax allow-lists,
  last-match-wins) is untouched; this is a composition affordance at the CLI,
  within the "low-stakes, revisable details left to implementation" latitude
  ADR-0007 grants.

`doctor` / `unanchored_patterns` are deliberately unchanged: the extra layer
is per-invocation caller state, not repo state a repo examination could know
about.

## Changes

### Code

- `crates/client/src/select.rs`:
  - `load_search` gains `extra_includes: &[PathBuf]`, appended after the user
    buffer (gix `Search` matches lists in reverse, so appending later means
    higher precedence). Extras are read with a *required* read: `NotFound`
    maps to a new `SelectError::MissingExtraInclude` variant, other I/O errors
    to the existing `ReadPatternFile`.
  - `select_in` and `select_extra_paths_measured` grow the same parameter;
    `select_extra_paths` / `select_extra_paths_with` keep their signatures,
    delegating with `&[]`. `unanchored_patterns` passes `&[]`.
  - Module-header layering doc updated to describe the third layer.
- `crates/client/src/encode.rs`: `encode_extra` gains
  `extra_includes: &[PathBuf]`, forwarded to selection; doc updated.
- `crates/client/src/lib.rs`: `sync` gains `extra_includes: Vec<PathBuf>`
  after `user_include`; doc updated.
- `crates/cli/src/main.rs`: `SyncArgs` gains
  `extra_include: Vec<PathBuf>` (repeatable `--extra-include <PATH>`), passed
  through to `sync`.

### Docs

- `docs/operating.md` §4: document the extra layer alongside the existing two,
  including the ordering and the missing-file error.

### Tests

- `crates/client/src/select.rs` unit tests (the race-free home for layer
  semantics, per the note at the top of `tests/extra.rs`):
  - an extra layer adds includes on top of project + user;
  - an extra layer wins over the user layer (carve-out and re-include);
  - a later extra file wins over an earlier one;
  - a missing extra file is `MissingExtraInclude`, not an empty layer.
- `crates/cli/tests/end_to_end.rs`:
  - `command_line_surface_is_wired_up` asserts `--extra-include` is exposed;
  - an end-to-end sync with `GIT_FULL_SEND_USER_INCLUDE` pointing at a real
    file *and* `--extra-include` delivers files from both layers — the
    anti-replacement regression the issue is about;
  - `sync --extra-include <missing>` exits non-zero naming the path.
- Existing `gfs_client::sync` / `encode_extra` call sites in
  `crates/client/tests/{transfer,extra}.rs` updated mechanically
  (`Vec::new()` / `&[]`).

## Validation

- `cargo test --workspace`, `cargo clippy --workspace --all-targets`,
  `cargo fmt --check`.
