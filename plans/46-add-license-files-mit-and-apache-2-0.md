# Plan — #46: Add LICENSE files (MIT and Apache-2.0)

## Goal

`Cargo.toml` (workspace.package) declares `license = "MIT OR Apache-2.0"`, but
the tree carries no license text. Add the conventional Rust dual-license pair —
`LICENSE-MIT` and `LICENSE-APACHE` at the repo root — with the correct copyright
line, and reference them from the README so the declared license is backed by
actual text.

Acceptance (from the issue): both license files exist at the repo root and match
the `Cargo.toml` declaration; the README links them.

## Decided in the pre-plan thread (approved)

- **Copyright line:** `Copyright (c) 2026 Jeff Parsons` (matching the git author
  / repo owner). Approved via 👍 with no change requested.
- **Conventional Rust dual-license pair:** verbatim MIT and Apache-2.0 text;
  the Apache appendix boilerplate is left as-is (standard practice — it is part
  of the canonical license text, not a per-project field to fill in).
- **README:** add a short **License** section linking both files, using the
  standard dual-license + contribution wording.

## Changes

### 1. New file — `LICENSE-MIT` (workspace root)

The standard MIT License text, with the copyright line:

```
Copyright (c) 2026 Jeff Parsons
```

This is the canonical SPDX `MIT` text (the same wording used across the Rust
ecosystem).

### 2. New file — `LICENSE-APACHE` (workspace root)

The verbatim Apache License, Version 2.0 text (the canonical SPDX `Apache-2.0`
text, including the standard "APPENDIX: How to apply the Apache License to your
work" boilerplate). No copyright line is filled into the appendix — that matches
the conventional dual-licensed Rust project layout, where the human-readable
copyright lives in `LICENSE-MIT` and the README.

### 3. `README.md` — add a **License** section

Append a section near the end of the README linking both files, with the
standard dual-license statement and the conventional contribution clause:

```markdown
## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
```

This is the standard wording recommended by the Rust API guidelines for
dual-licensed crates; it matches the `MIT OR Apache-2.0` declaration exactly.

## Verification

- **Files match the declaration:** `LICENSE-MIT` and `LICENSE-APACHE` exist at
  the repo root; the SPDX expression `MIT OR Apache-2.0` in `Cargo.toml` is now
  backed by both texts. Spot-check the MIT copyright line reads
  `Copyright (c) 2026 Jeff Parsons` and the Apache text is the unmodified
  Version 2.0 body.
- **README links resolve:** the new section's relative links
  (`LICENSE-APACHE`, `LICENSE-MIT`) point at the new root files.
- **Supply-chain gate (#45) still passes:** `cargo deny check` is unaffected —
  this PR adds only top-level files and README prose, no dependency or manifest
  changes — but the new files give the license policy concrete artifacts to
  point at, as the issue notes.

## Out of scope / deferred

- **`COPYING` / a single combined file** — the dual `LICENSE-*` pair is the Rust
  convention and what `Cargo.toml`'s `license` field maps to; no combined file.
- **Per-crate license fields or symlinks** — workspace members already inherit
  `license` via `workspace.package`; crates.io reads the SPDX expression, so no
  per-crate license files are needed for the MVP.
- **An ADR** — adding the license text for the already-declared license is not a
  new architectural decision.
