# ADR-0009 — Working-tree fidelity for the `code` commit

- Status: accepted
- Date: 2026-06-12

## Context

[ADR-0004](0004-encoding-the-sync-state-in-git.md) settled the *topology* of the
encoded sync state: working-tree changes collapse to the current on-disk
contents, captured as a single tree in one commit parented on `HEAD` under
`refs/git-full-send/code`, synthesised with gix's native tree `Editor` (no
scratch index, no `git` shell-out). Implementing it (issue #17) surfaced
behavioural questions ADR-0004 did not pin down: **which** on-disk paths the
`code` tree contains, and how non-trivial file kinds are represented. These need
to be recorded so the client and the (later) remote checkout agree.

## Decision

The `code` tree is a faithful snapshot of the developer's working tree:

- **Tracked _and_ untracked, non-ignored files are included.** A brand-new file
  that has never been `git add`ed still represents the developer's current code
  state, so it syncs. Only `.gitignore`d files are excluded here — deliberately
  force-including some of those is the separate `extra` commit's job
  ([ADR-0007](0007-syncing-extra-gitignored-files.md)).
- **Deletions are represented by absence.** A tracked file removed from the
  working tree is simply not in the tree; the authoritative remote checkout
  ([ADR-0008](0008-remote-worktree-disposability.md)) removes it.
- **File modes are preserved from disk:** regular (`100644`) vs. executable
  (`100755`) blobs, and symlinks (`120000`) whose blob content is the link
  target. Submodule gitlinks (`160000`) are carried through from the index
  unchanged (we do not recurse into submodules).
- **Staged and unstaged changes both collapse to the on-disk content.** A file
  staged as `X` and then edited to `Y` syncs as `Y`.

### How it is built (efficiently)

The base of the tree is the **index** — already the staged state, with object
ids known — so unchanged tracked files cost zero hashing and zero worktree I/O.
Only the **index → worktree** delta is overlaid (via gix's `status`, which
applies the same stat shortcut Git uses), so hashing is bounded by the actual
working-tree delta plus the untracked files, not the repository size. The index
snapshot is read-only and never written back, which — together with writing the
commit via a raw object write and moving only the scratch ref — is how the
encode step keeps the user's branch, index, and working tree untouched.

## Consequences

- Untracked files appearing on the remote is intended behaviour, not a leak —
  but genuinely secret/ignored files stay out unless force-included via the
  `extra` path.
- Mode fidelity is **Unix-first**: on platforms without a meaningful executable
  bit or symlinks the encoder falls back to the index-recorded mode. Acceptable
  for a developer tool; revisit if a Windows client is ever needed.
- A further optimisation — seeding the base from `HEAD`'s tree and applying the
  index↔`HEAD` staged diff, to make even the base-tree build proportional to the
  number of staged changes — is deferred; the index-as-base already gives the
  no-re-hash property.
