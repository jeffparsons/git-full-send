# Research 0004 — Force-include configuration mechanism

- Date: 2026-06-12
- Source ADR: [ADR-0007 — Syncing extra (normally-gitignored) files](../adr/0007-syncing-extra-gitignored-files.md)
- Related: [ADR-0004](../adr/0004-encoding-the-sync-state-in-git.md) (encoding —
  consumes the selected set as the `extra` tree),
  [ADR-0008](../adr/0008-remote-worktree-disposability.md) (disposable worktree /
  overwrite authority), [ADR-0002](../adr/0002-git-manipulation-strategy.md)
  (gix-first git strategy)
- Builds on [Research 0002](0002-encoding-the-sync-state-in-git.md) (encoding)
  and [Research 0001](0001-gix-git-plumbing-vs-libgit2-capability-gap.md) (gix
  capability survey, pinned to gix 0.84.0). The gix capability claims below are
  read from `crate-status.md` at the same pin; **re-check it before relying on a
  capability**, as gitoxide moves quickly.

## TL;DR

`git-full-send` needs to declare a deliberately-included set of
**normally-gitignored** files (build outputs, per-user config) and land them in
the remote worktree. [ADR-0004 / Research 0002](0002-encoding-the-sync-state-in-git.md)
already fixed *how those files travel and reassemble* — a separate `extra`
tree/commit, exploded as an authoritative overlay on the remote — and explicitly
deferred *which* files and *what paths* to this ticket. This report answers those.

Recommendation:

- **Where.** Declare the set with **gitignore-syntax glob patterns** in a
  **committed, project-level file at the repo root** (shared, version-controlled —
  the natural home for "the build outputs this project produces"), plus an
  **optional per-user layer** outside the repo (mirroring Git's
  `core.excludesFile`) for personal config. This is Git's own well-understood
  *in-tree + per-user* split.
- **Granularity.** **Globs, not explicit path lists.** Build outputs are whole,
  volatile directory trees; an explicit manifest would rot immediately. Use the
  full gitignore glob vocabulary (anchoring, `**`, character classes, `!`
  carve-outs, last-match-wins) — which gix parses natively.
- **Scope / composition.** Two ordered layers, **project then user**, evaluated
  **last-match-wins** so the user has final say on their own machine. Both layers
  can *add* includes; the user layer can also *carve out* with `!`.
- **How placed.** Keep Research 0002's overlay, with **identity path-mapping**:
  each force-included file lands at its **same repo-relative path** on the remote
  (no `--prefix` remapping), because build/run tooling expects the outputs exactly
  where they were produced.

Two findings shape the design:

1. **The whole selection→tree-build pipeline is gix-native** at the 0.84.0 pin —
   `gix-ignore` parses `.gitignore`-style files, `gix-glob` matches patterns, and
   `gix-dir` walks and classifies the worktree — so enumerating the set needs **no
   `git` shell-out**, reinforcing ADR-0002. (Tree synthesis was already gix-native
   per Research 0001/0002.)
2. **Treating force-include as an independent allow-list over the filesystem
   sidesteps Git's "can't re-include under an excluded parent" limitation.** We are
   *not* adding `!` negations inside the project's real `.gitignore`; we run our
   own pattern set against the working tree, so the well-known gitignore
   re-inclusion trap never binds us.

## What this ticket owns (and what is already settled)

[Research 0002](0002-encoding-the-sync-state-in-git.md) decided the *transport
and reassembly* of the force-included files and named the two open questions it
left for here:

> Exactly *where* the extra files land in the worktree is governed by their paths
> in the `extra` tree, which is ADR-0007's configuration concern; this report only
> fixes that they arrive as a separate overlay tree exploded after the code
> checkout.

So the settled context is:

- **Encoding / transport** ([ADR-0004](../adr/0004-encoding-the-sync-state-in-git.md)):
  the selected files become a separate `extra` tree/commit under
  `refs/git-full-send/extra`, on its own retained delta-base chain.
- **Reassembly** ([ADR-0008](../adr/0008-remote-worktree-disposability.md)): the
  remote worktree is disposable; the overlay is an authoritative, destructive
  overwrite with no merge logic.

This ticket owns the **selection + declaration** half: *where* the set is
declared, *granularity*, *scope*, and the *path-mapping* of the overlay.

## Prior art — how comparable tools declare an include/exclude set

Five precedents, each a real answer to "name a subset of a tree with patterns":

| Tool / mechanism | Where declared | Granularity | Scope layering | Ordering rule |
| --- | --- | --- | --- | --- |
| **Git `.gitignore`** | in-tree per-dir file + `info/exclude` (per-clone) + `core.excludesFile` (per-user) | globs (`*`, `?`, `**`, ranges), anchoring, `!` negate | **three layers**, command-line > in-tree > info/exclude > user | **last-matching pattern wins** within a layer |
| **Git sparse-checkout (cone)** | `.git/info/sparse-checkout` | **directories only** (translated to patterns) | per-clone | hash-based dir membership |
| **Git sparse-checkout (non-cone, deprecated)** | same | arbitrary gitignore patterns used for *inclusion* | per-clone | O(N·M) pattern match → why it was deprecated |
| **git-lfs `track`** | `.gitattributes` (committed, per-project) | globs (`*.psd`) | in-tree (+ per-user attrs) | attribute resolution |
| **rsync `--filter`** | command line / filter files | globs, leading-`/` anchoring | merge files can be per-dir | **first matching pattern wins** |
| **Syncthing `.stignore`** | per-folder root file | gitignore-like globs, `**`, `!`, `(?i)`, `(?d)` | per-folder | **first match decides** |
| **Mutagen ignores** | session config (`~/.mutagen.yml` defaults + `-i` flags) | gitignore-like globs, `!` negate | default + per-session; **no in-tree files** | gitignore-style |

What the survey settles:

- **Globs are universal.** Every tool that names a file subset uses glob
  patterns, not hand-maintained path lists. Explicit paths appear only where the
  set is a few stable files; volatile build trees rule them out.
- **Gitignore syntax is the lingua franca.** Syncthing, Mutagen, and
  sparse-checkout all adopt gitignore-style patterns. Reusing it means **zero new
  syntax to learn** and, for us, a native parser already exists (`gix-ignore`).
- **The committed/per-user split is Git's own model and worth copying.** Git
  separates shared, version-controlled patterns (`.gitignore`, `.gitattributes`)
  from personal ones (`core.excludesFile`, `info/exclude`). Project build outputs
  are shared; per-user config is personal — the same split maps cleanly onto our
  two layers. Mutagen, by contrast, has *no in-tree file* — fine for a generic
  syncer, but it means the project can't ship its own list, which is exactly what
  we want for build outputs.
- **Two ordering conventions exist** — Git's *last-match-wins* and rsync /
  Syncthing's *first-match-wins*. Either works; we pick **last-match-wins** to
  match Git (the mental model our users already have) and to make the layering
  rule "later layer overrides" fall out naturally.
- **The sparse-checkout non-cone deprecation is a direct cautionary tale** (see
  the polarity note below).

## The four questions

### 1. Where the set is declared

**Recommendation: a committed, project-level gitignore-syntax file at the repo
root** (working name `.gitfullsend/include`, or a single `.git-full-send.include`
— the exact name is a low-stakes, revisable call), **plus an optional per-user
file** outside the repo (e.g. under the user's XDG/Git config dir, mirroring
`core.excludesFile`).

Rationale:

- The project layer is **shared and version-controlled** — "the outputs this
  project's build produces" is a property of the project, so it belongs in the
  repo and rides along in the `code` tree automatically (it is itself a committed
  file). This is the `.gitignore` / `.gitattributes` / git-lfs-`track` precedent.
- A **dedicated pattern file** (rather than a `force_include = [...]` array inside
  a future `git-full-send.toml`) lets us hand the file straight to **`gix-ignore`,
  which "parse[s] `.gitignore` files"** — no bespoke parsing, full gitignore
  semantics for free, and a line-oriented, diff-friendly format. The TOML-array
  alternative is viable (and could fold into a central project config later) but
  would route each entry through `gix-glob` by hand and lose the file-level parse;
  it is the considered-but-not-chosen option, not a blocker.
- The **per-user layer lives outside the repo** so personal choices never get
  committed, exactly as Git keeps `core.excludesFile` out of the tree. It is read
  on the **client only** — it drives selection and need not travel.

### 2. Granularity — globs, not explicit paths

**Recommendation: globs**, using the full gitignore vocabulary the survey shows is
standard and that `gix-glob` already implements:

- `*`/`?`/character ranges, `**` for cross-directory matching, leading-`/`
  anchoring to the repo root, trailing-`/` for directory-only, and `!` carve-outs.
- A directory pattern (`web-client/dist/`) is the common, encouraged case — it
  pulls a whole build-output tree with one line and survives the tree's contents
  churning. Explicit per-file paths would have to be regenerated on every build.

We **consciously accept full glob expressiveness** (the sparse-checkout
"non-cone" shape) rather than restricting to cone-mode directory-only patterns.
Git restricted sparse-checkout to cone mode because arbitrary patterns cost
**O(N·M) matches on every worktree-updating operation** — but our pattern set is a
small curated list evaluated **once per sync**, so that performance argument does
not bind, and the extra expressiveness (e.g. `**/*.wasm`, `!**/*.map`) is worth
keeping.

### 3. Scope — per-project and per-user, and how they compose

**Recommendation: two layers, evaluated in order `[project, then user]` with
last-match-wins.**

- Both layers may **add** includes (project: build outputs; user: personal config
  the project shouldn't dictate).
- Because the user layer is evaluated **last**, a per-user `!` can **carve out**
  something the project included, and the user always has the final say on their
  own machine — appropriate, since the per-user layer is inherently the operator's
  call. (We do not give the project a way to *force* an include the user can't
  drop; there is no requirement for that, and it keeps the model simple.)
- This mirrors Git's own multi-source precedence — "*within one level of
  precedence, the last matching pattern decides the outcome*" — minus the
  per-directory nesting, which a small curated list does not need (a single file
  per layer at the repo root, anchored repo-root-relative like a top-level
  `.gitignore`).

#### Polarity caution — learn from sparse-checkout's deprecation

Our file uses gitignore **syntax** but **inverted semantics**: a bare pattern
*includes* (force-adds) and `!` *carves out* — the opposite of `.gitignore`,
where bare *excludes* and `!` *re-includes*. The Git docs flag this exact
inversion as the reason non-cone sparse-checkout confused users (gitignore
patterns "are designed for exclusion, but sparse-checkout uses them for
inclusion"). We mitigate the confusion without giving up the familiar glob rules:

- The file is **named and documented as an include / allow-list**, so its purpose
  is unambiguous at the point of use.
- Unlike sparse-checkout, we are **not** simultaneously using real `.gitignore`
  files for the opposite purpose on the same surface — there is one allow-list
  with one polarity, so there is no live "same syntax, two meanings" trap.
- The expensive O(N·M)-on-every-op cost that ultimately doomed non-cone does not
  apply (selection runs once per sync over a small list).

### 4. How the selected files are placed in the remote worktree

**Recommendation: keep Research 0002's overlay with identity path-mapping.**

- The selected files become the `extra` tree (per ADR-0004), and the remote
  explodes that tree over the `code` checkout with `git checkout-index`
  (Research 0002).
- **Path-mapping is identity**: a file at `web-client/dist/app.js` in the working
  tree is stored at `web-client/dist/app.js` in the `extra` tree and lands at the
  **same repo-relative path** on the remote. **No `--prefix` remapping** — build
  and run tooling references these outputs by their normal in-repo location, and
  remapping would break those references. (The `--prefix=<dir>/` option Research
  0002 mentioned is therefore *not* exercised for the force-include set; it stays
  available only if some future use wants a relocated overlay.)

This keeps the configuration concern (paths in the `extra` tree) entirely on the
*client* side; the remote stays a dumb, authoritative explode.

## Selection mechanics under the gix-first stack

Enumerating the set is the new capability question this ticket raises (Research
0001/0002 covered tree *synthesis*, not pattern *selection*). At the gix 0.84.0
pin, the whole pipeline is **gix-native**:

- **`gix-ignore`** — `[x]` "parse `.gitignore` files" and `[x]` "an attributes
  stack for checking if paths are excluded". Parses our pattern files directly.
- **`gix-glob`** — `[x]` "parse pattern" and `[x]` "pattern matching of paths …
  optionally case-insensitively". Evaluates each pattern.
- **`gix-dir`** — `[x]` "list untracked files", `[x]` "list ignored files",
  `[x]` "pathspec based filtering". Walks the worktree and classifies paths.

So the client can: walk the working tree, match each path against the combined
project+user include patterns (last-match-wins), and feed the matched blobs into
the gix `Editor` that builds the `extra` tree (Research 0002) — **no `git`
shell-out anywhere in selection or synthesis.** This strengthens ADR-0002's
gix-first posture for this leg specifically. *(Caveat: pinned/dated; re-check
`crate-status.md`. `gix-pathspec` lacks line-by-line file parsing at this pin, but
we parse our pattern files via `gix-ignore`/`gix-glob`, not the pathspec file
parser, so that gap does not bite.)*

**Why we run an independent allow-list, not `!` negations inside `.gitignore`.**
Git "*does not list excluded directories for performance reasons*", so "*it is not
possible to re-include a file if a parent directory of that file is excluded*" —
the classic gitignore re-inclusion trap. A force-include built as `!` lines layered
on the project's real `.gitignore` would hit this constantly (build outputs
typically live under an ignored `dist/` or `target/`). We avoid it entirely by
treating force-include as a **separate allow-list matched against the working-tree
filesystem**: our patterns are evaluated on their own, independent of Git's ignore
tree, so an ignored parent directory is no obstacle to selecting files beneath it.

## Interaction notes (flagged, not re-decided)

- **Stale force-included files across syncs.** Because the set is volatile, a file
  force-included last sync but no longer selected must **disappear** from the
  remote. The two-step "checkout `code`, then explode `extra`" reassembly
  (Research 0002) overwrites and adds but does not by itself remove a stale
  *extra* file. ADR-0008's disposable/authoritative worktree gives the latitude to
  fix this (clean-rebuild, or diff the previous `extra` tree's paths and delete
  those absent from the new one); the precise mechanism is ADR-0004/0008's
  reassembly detail, surfaced here only because force-include volatility is what
  makes it matter.
- **The project pattern file is itself committed**, so it travels in the `code`
  tree with no special handling and is identical on both ends.

## Bearing on the ADRs

- **ADR-0007 (source).** Resolves the `⚠ Research task needed` callout with the
  recommendation above; status can move off bare `proposed`. Key decisions:
  gitignore-syntax glob patterns; committed project file + optional per-user file;
  two layers, project-then-user, last-match-wins; identity path-mapping for the
  overlay; selection as an independent filesystem allow-list.
- **ADR-0004 / Research 0002 (encoding).** Unchanged. This report supplies the
  *contents and paths* of the `extra` tree it already specified, and confirms the
  overlay needs no path remapping.
- **ADR-0008 (disposable worktree).** Unchanged; noted as the latitude for stale
  force-included-file removal.
- **ADR-0002 (git strategy).** Reinforced: selection (`gix-ignore`/`gix-glob`/
  `gix-dir`) and synthesis (gix `Editor`) are both gix-native at the current pin —
  no shell-out on this leg.

## Sources

- [gitignore](https://git-scm.com/docs/gitignore) — pattern-source precedence
  (command-line > in-tree `.gitignore` > `info/exclude` > `core.excludesFile`),
  last-matching-pattern-wins, glob/anchoring/`**` semantics, `!` negation, and the
  "cannot re-include under an excluded parent directory" limitation.
- [git-sparse-checkout](https://git-scm.com/docs/git-sparse-checkout) — cone
  (directory-only) vs non-cone (arbitrary gitignore patterns) modes, the O(N·M)
  cost, and the deprecation of non-cone for using exclusion patterns to express
  inclusion.
- [rsync manpage](https://download.samba.org/pub/rsync/rsync.1) — filter rules,
  first-matching-pattern-wins, leading-`/` anchoring.
- [Syncthing — Ignoring Files](https://docs.syncthing.net/users/ignoring.html) —
  per-folder `.stignore`, gitignore-like globs with `**`/`!`/`(?i)`/`(?d)`,
  first-match-decides, and the negation/scan-cost interaction.
- [Mutagen — Ignores](https://mutagen.io/documentation/synchronization/ignores/) —
  default + per-session ignores (no in-tree files), gitignore-style syntax with
  `!` negation.
- [Git LFS](https://git-lfs.com/) — `git lfs track "*.psd"` storing committed,
  per-project glob patterns in `.gitattributes`.
- [gitoxide `crate-status.md`](https://raw.githubusercontent.com/GitoxideLabs/gitoxide/main/crate-status.md)
  — `gix-ignore` (`.gitignore` parsing, exclusion attributes stack), `gix-glob`
  (pattern parse + match), `gix-dir` (list untracked/ignored, pathspec filtering);
  pinned to the Research 0001 survey at gix 0.84.0.
- [Research 0002 — Encoding the sync state in Git](0002-encoding-the-sync-state-in-git.md)
  and [Research 0001 — gix capability gap](0001-gix-git-plumbing-vs-libgit2-capability-gap.md)
  — the settled encoding/reassembly and the gix capability baseline this builds on.
