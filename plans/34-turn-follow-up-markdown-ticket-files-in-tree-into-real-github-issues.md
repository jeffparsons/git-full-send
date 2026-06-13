# Plan: Turn follow-up markdown ticket files in tree into real GitHub issues (#34)

## Goal

Convert the staged follow-up ticket markdown under `docs/follow-ups/` into a
real GitHub issue (via `ghwf`), delete the source file, and repair the one
code reference that points at the deleted path. This mirrors what #3 did for
the `research-tickets/` drafts.

## Source material

There is exactly **one** follow-up file in the tree:

| File | Becomes |
| --- | --- |
| `docs/follow-ups/prune-force-include-walk.md` | one new GitHub issue: *"Prune the force-include walk to skip directories that cannot contain a match"* |

The directory holds only that file (no README/index), so the issue's plural
"files" resolves to this single file. The file's own header states it "exists
so a follow-up issue can be opened whose only job is to convert it into a real
GitHub issue" — i.e. converting it is precisely the intended end state.

One code reference points at the file and will dangle once it's deleted:

- `crates/client/src/select.rs:50-51` — a module doc-comment that tells readers
  the prune is "tracked as a follow-up in `docs/follow-ups/prune-force-include-walk.md`".

(`grep -rn "follow-ups"` confirms no other file references the path; the other
hits are unrelated prose uses of "follow-up".)

## Steps

### 1. File one GitHub issue with `ghwf create-issue --no-block`

Run `ghwf create-issue --no-block --label enhancement --title "Prune the force-include walk to skip directories that cannot contain a match"`
with the body on stdin.

- `--no-block` keeps it standalone — it must **not** be blocked by #34, which is
  just the conversion chore. The real blocker context (it was deferred from #20,
  which is already done) lives in prose, not a ghwf block.
- `enhancement` label: this is a performance refinement, and the repo has no
  `performance` label. (If the label apply fails for any reason, fall back to
  filing without a label rather than blocking.)

**Body** = the file's existing **Context / Goal / Why it wasn't done in #20 /
Sketch of an approach / Acceptance** sections, verbatim, with these
transformations:

- **Drop** the leading meta blockquote ("Deferred from #20. This file exists
  so a follow-up issue can be opened…") — its job is done once the issue exists.
- Keep a one-line provenance note in prose at the top instead, e.g.
  *"Deferred from #20."* so the lineage isn't lost.
- The body's only path-ish references are to `crates/client/src/select.rs` and
  ignored-dir examples, which stay as-is (they're code paths, not the doc we're
  deleting). No relative `.md` cross-links exist to rewrite.

### 2. Delete the source file

`git rm docs/follow-ups/prune-force-include-walk.md`. That empties the
directory, so the whole `docs/follow-ups/` directory goes too (git drops empty
dirs automatically once the file is removed).

### 3. Repair the dangling code reference

Edit the `crates/client/src/select.rs` module doc-comment so it no longer
points at the deleted file. Repoint it at the new issue, e.g.:

> …pruning subtrees that cannot contain a match is tracked as a follow-up:
> https://github.com/jeffparsons/git-full-send/issues/<N>.

The issue number `<N>` is only known after step 1, so this edit happens after
the issue is filed.

### 4. Verify

- `gh issue list` shows the new issue, `enhancement`-labelled, standalone (no
  "blocked by #34").
- `docs/follow-ups/` no longer exists in the working tree.
- `grep -rn "docs/follow-ups"` returns nothing (the `select.rs` reference is
  updated; no other references exist).
- `cargo build` / existing checks still pass (the change is a doc-comment only,
  so this is a sanity check, not expected to surface anything).

## Out of scope

- **Doing the prune work itself** — implementing the directory-skipping
  optimisation is the deliverable of the *new* issue, not of #34.
- Any change to `select.rs` behaviour; only its doc-comment text changes.

## Files changed in this PR

- Deleted: `docs/follow-ups/prune-force-include-walk.md` (and with it the empty
  `docs/follow-ups/` directory).
- Edited: `crates/client/src/select.rs` — doc-comment repointed from the deleted
  file path to the new issue URL.

The new GitHub issue is created via `ghwf` as a side effect, not as a repo file
change.
