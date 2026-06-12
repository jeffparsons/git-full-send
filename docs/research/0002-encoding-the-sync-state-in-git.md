# Research 0002 — Encoding the sync state in Git

- Date: 2026-06-12
- Source ADR: [ADR-0004 — Encoding the sync state in Git](../adr/0004-encoding-the-sync-state-in-git.md)
- Related: [ADR-0005](../adr/0005-transfer-mechanism.md) (transfer),
  [ADR-0007](../adr/0007-syncing-extra-gitignored-files.md) (force-include config),
  [ADR-0008](../adr/0008-remote-worktree-disposability.md) (remote-worktree disposability),
  [ADR-0002](../adr/0002-git-manipulation-strategy.md) (git-manipulation strategy)
- Builds on [Research 0001](0001-gix-git-plumbing-vs-libgit2-capability-gap.md)
  (gix / `git` CLI / libgit2 capability gap, pinned to gix 0.84.0). The gix
  capability claims below are **inherited from that report's pin**, not
  re-derived; re-check `crate-status.md` before relying on a gap being open or
  closed.

## TL;DR

All three ADR-0004 options are built from the **same capture primitives** — a
scratch ref plus an alternate (off-to-the-side) index or an in-memory tree
builder — so none of them, correctly implemented, touches the user's branch,
main index, or working tree. They differ only in **how the captured trees are
arranged into commits** and **how the remote reassembles them**.

The recommendation is **Option 2 (a separate commit/tree for the force-included
files), refined**:

- Capture the working tree (staged **and** unstaged collapsed to *current
  on-disk contents*) as a single tree, wrapped in **one commit parented on
  `HEAD`** under a scratch ref (e.g. `refs/git-full-send/code`). Because it is
  parented on `HEAD`, push negotiation shares the entire committed history with
  the remote and only the working-tree delta crosses the wire.
- Capture the force-included files as a **second tree/commit** under its own
  scratch ref (e.g. `refs/git-full-send/extra`), parented on the *previous*
  sync's extra commit so the prior (large) build outputs are retained as delta
  bases. This keeps generated artifacts **out of the code lineage** (the
  ADR-0004 downside of Option 1) and gives the volatile big files their own,
  predictable delta-base chain.
- Synthesise the trees with **gix's native tree `Editor`**, *not* a scratch
  index — the index-centric mechanism (Option 3's framing) is the one thing in
  this area that forces a `git` shell-out per Research 0001. Reuse the
  unchanged committed-code blobs/subtrees by digest.
- Reassemble on the remote as an **authoritative, destructive overwrite**
  (ADR-0008): check the code tree into the disposable worktree, then explode the
  extra tree over it with `git checkout-index`.

Crucially, the **encoding is not the main lever on the intermittent slow
transfers** flagged in ADR-0005 — *delta-base availability* is. The single
highest-impact encoding decision for pack predictability is to **retain the
previous sync's tips on both ends** so each push can delta against them; the
transfer-mechanism root-cause itself stays ADR-0005's ticket.

## The three sync-state components and the non-disturbance constraint

ADR-0004 requires encoding three things:

1. **Committed history** — already in the object database; nothing to synthesise,
   only to *reference* (and to reuse as delta bases / shared negotiation base).
2. **Working-tree changes — staged and unstaged.** For our purpose these
   **collapse to a single thing: the current on-disk contents.** The remote only
   needs the files as they are on disk to build and run; it never needs to
   restore the client's index/worktree split. This is a real simplification over
   `git stash`, which *must* keep separate index (`I`) and worktree (`W`) commits
   precisely so it can later restore both
   ([git-stash, "A stash entry is represented as a commit whose tree records the
   state of the working directory… the second parent records the state of the
   index"](https://git-scm.com/docs/git-stash)). We need only the `W`-equivalent:
   one tree capturing on-disk contents (a file staged as `X` but further edited
   to `Y` syncs as `Y`; a deleted file is simply absent from the tree, and the
   authoritative checkout removes it on the remote).
3. **Force-included, normally-gitignored files** — a deliberately selected set
   (large-ish build outputs, per-user config; *which* files is ADR-0007's
   concern). Captured as blobs and arranged into a tree.

**The non-disturbance constraint is shared by all options** and is solved the
same way Git's own plumbing solves it — never operate on the real index or move a
real branch:

- **Scratch index** (the `git`-CLI path): point `GIT_INDEX_FILE` at a throwaway
  path, seed it with `git read-tree -i <tree>` (the `-i` flag "disables the check
  with the working tree… used when creating a merge of trees that are not
  directly related to the current working tree status into a temporary index
  file"), inject already-written blobs by digest with
  `git update-index --add --cacheinfo <mode>,<oid>,<path>` (or `--index-info` in
  bulk — "useful… when the object is in the database but the file isn't available
  locally"), then `git write-tree`. `git read-tree --index-output=<file>` can
  even write the scratch index elsewhere while the real index stays locked.
  Sources: [git-read-tree](https://git-scm.com/docs/git-read-tree),
  [git-update-index](https://git-scm.com/docs/git-update-index).
- **In-memory tree builder** (the gix-native path): Research 0001 confirms gix's
  tree `Editor` "builds trees directly without an index", and blob/commit writes
  are native. No scratch index is involved at all.
- **No real ref moves:** commits are created with `commit-tree` / gix
  `commit_as` (which do not update any ref), and only ever pointed at by a
  **scratch ref** in a private namespace (e.g. `refs/git-full-send/*`), never the
  user's branch.

So the "without disturbing the user's working state" requirement is **not a
differentiator** between the options — it is table stakes that each meets via the
same primitives. What follows compares how they *arrange* the result.

## Options × criteria matrix

Legend: **✅ good fit** · **➖ neutral / minor cost** · **⚠ notable drawback**.

| Criterion | Opt 1 — Stacked commits | Opt 2 — Separate extra commit | Opt 3 — Alt-index, no scratch commits |
| --- | --- | --- | --- |
| **Non-disturbance** (branch/index/worktree untouched) | ✅ scratch ref + alt index/Editor | ✅ scratch refs + alt index/Editor | ✅ alt index, no real ref by definition |
| **Build cost with our drivers** (gix-native vs shell-out) | ✅ native via gix `Editor` | ✅ native via gix `Editor` | ⚠ as literally "alt **index**" it's the gix gap → `git` shell-out (Research 0001) |
| **Keeps Git happy** (well-formed, reusable digests) | ➖ code tree carries generated files → noisier digest | ✅ code tree == pure working-tree state, stable digest | ✅ trees well-formed; but trees-only fights ref-based transfer |
| **Pack-shape predictability** | ➖ big files ride the code lineage; base mgmt entangled | ✅ big files isolated in own chain → clean delta bases | ➖ same blobs regardless; topology barely matters |
| **Force-included files handling** | ⚠ ADR-0004's noted downside: mixes generated/large files into the code lineage | ✅ cleanly separated overlay | ➖ goes in a tree either way |
| **Remote reassembly** | ✅ single checkout of the tip tree | ➖ checkout + one explode step | ⚠ no commit/ref → transfer & checkout both awkward |
| **Incrementality across syncs** | ➖ shared lineage couples code + artifact bases | ✅ independent code / extra base chains | ➖ depends on retained objects, like the others |

The decisive columns are **force-included-files handling** (where Option 1 has
ADR-0004's own called-out downside) and **build cost** (where Option 3's literal
index-centric reading is the one approach that forces a shell-out).

## Per-option narrative

### Option 1 — Stacked commits (the prototype approach)

Build `HEAD → W → X`: a working-tree commit `W` parented on `HEAD`, then an extra
commit `X` parented on `W` whose tree is `W`'s tree with the force-included files
overlaid. Transfer the single tip `X` under one scratch ref; the remote checks
out `X`'s tree in one step.

- **Strengths.** Simplest topology and the simplest reassembly — one ref, one
  tree, one checkout. Fully buildable with gix's native `Editor` (write changed
  blobs, edit `HEAD`'s tree, overlay the extra paths, write tree, commit). Shared
  subtrees mean unchanged code costs nothing extra.
- **Weakness (the ADR's own).** It "mixes generated/large files into the same
  commit lineage as real code." `X`'s tree is the canonical sync artifact, so the
  generated outputs are now part of the tree digest that downstream logic (build
  planning) sees, and the volatile big blobs share a lineage with the code. There
  is no *correctness* problem — trees stay well-formed — but the clean,
  reusable "this is exactly the working-tree state" digest is lost, and
  delta-base bookkeeping for the big files is entangled with the code commits.

### Option 2 — Separate commit/tree for the extra files (recommended)

Build **two** scratch refs from one synthesis pass:

- `refs/git-full-send/code` → commit `W` (tree = current on-disk working-tree
  state), parented on `HEAD`.
- `refs/git-full-send/extra` → commit `E` (tree = just the force-included files),
  parented on the **previous** sync's `E` (or parentless on first sync).

Both are pushed in **one exchange** (`git push` advertises multiple refs and
packs them together), so "two things to transfer" is one pack on the wire, not
two round-trips. The remote reassembles in two steps (below).

- **Strengths.** The code commit's tree is *exactly* the working-tree state —
  the stable, meaningful digest ADR-0004 wants for "reuse digests already present
  in a Git tree." Generated artifacts never enter the code lineage. The extra
  files get their **own delta-base chain**: parenting each `E` on the prior `E`
  means `git pack-objects` naturally has last sync's build outputs in-window as
  delta bases (it sorts candidates "by type, size and optionally names" and
  deltas within `--window`, default 10 — [git-pack-objects](https://git-scm.com/docs/git-pack-objects)),
  and you can decide *per chain* how to treat them (e.g. accept whole-object
  sends for the big files for *predictable* cost rather than intermittent
  delta-compression spikes). Fully gix-native via two `Editor` passes.
- **Cost.** One extra ref and one extra reassembly ("explode") step on the
  remote. Both are cheap (see reassembly below). This is the only real price for
  the separation, and it buys the clean lineage and independent base management.

### Option 3 — Alternate-index tree construction, no scratch commits on a real branch

Taken literally — "build the tree(s) via an alternate index without ever
materialising scratch commits on a real branch" — this is **two claims bundled**,
and they pull apart on inspection:

- *"Via an alternate **index**."* This is precisely the gix gap from Research
  0001: gix "cannot stage entries or write-tree-from-index" (gix #293), so an
  index-centric build **forces a `git` CLI shell-out** (`read-tree` +
  `update-index` + `write-tree`). gix's *tree `Editor`* achieves the same result
  natively **without** an index — so the index is the costlier mechanism, not a
  benefit. Under ADR-0002's gix-first posture, we want the `Editor`, i.e. *not*
  this framing.
- *"No scratch commits on a real branch."* This is already satisfied by every
  option: commits are pointed at by **scratch refs** in a private namespace, not
  the user's branch. Going further and transferring *bare trees with no commit*
  fights the transfer leg: `git push` / `receive-pack` negotiation is **ref- and
  commit-oriented**, and a commit object is essentially free (one tiny object).
  So dropping commits saves nothing measurable and complicates the most
  battle-tested transfer path (the one Research 0001 steers ADR-0005 toward).

**Net:** Option 3 contributes a *principle the recommendation already adopts*
(scratch refs, never touch the user's index/branch, build trees off to the side)
but its distinguishing mechanics — an on-disk scratch **index** and
**commit-less** transfer — are each the *less* favourable choice here. There is
no separate winning design hiding in Option 3.

## Why the encoding is a minor lever on pack shape (and what the real lever is)

ADR-0005 records intermittent slow transfers, with pathological pack shapes the
leading suspicion. The encoding's contribution to this is **smaller than it
looks**, and worth stating plainly so ADR-0005's root-cause work starts in the
right place:

- The **bytes that must move** are the changed working-tree blobs and the changed
  force-included blobs — *identical across all three topologies*. Commit/tree
  arrangement changes a handful of tiny tree/commit objects, not the large blobs.
- The large build outputs are the cost centre. Two Git facts dominate:
  1. **Objects above `core.bigFileThreshold` are never deltified** — "delta
     compression is not used on objects larger than the `core.bigFileThreshold`
     configuration variable" ([git-pack-objects](https://git-scm.com/docs/git-pack-objects)).
     Big outputs are sent whole regardless of encoding.
  2. For outputs *below* that threshold, transfer size hinges on **delta-base
     availability**: a thin push (`--thin`, "omitting the common objects between
     a sender and a receiver") can encode a new output as an `OBJ_REF_DELTA`
     against the *previous* output — a delta whose base is "an object outside the
     pack" ([gitformat-pack](https://git-scm.com/docs/gitformat-pack)) — **only
     if that previous blob still exists and negotiation establishes the remote
     has it.** If the prior objects were pruned (scratch refs deleted, GC ran),
     there is no base, the output is sent whole, and `pack-objects` may also burn
     CPU re-attempting delta compression over large blobs. **That on/off
     base-availability is exactly the "sometimes fast, sometimes slow" symptom.**
- gix additionally **cannot compute new deltas and has no bitmaps** (Research
  0001), which is *why* Research 0001 already routes the pack-and-transfer leg
  through `git push` → `git receive-pack`. Object *count* here is small, so the
  missing-bitmap cost is minor; the delta-compression cost on big blobs is the
  real one — and that is `git`'s job under the recommended split.

**The encoding lever that does matter is ref retention.** Keep the previous
sync's tips (`refs/git-full-send/code@{prev}`, `refs/git-full-send/extra@{prev}`)
alive on **both** ends across syncs, so their trees and (large) blobs remain as
delta bases and as negotiation common-base. ADR-0008 makes only the remote
*worktree* disposable — the **object store persists** — so retaining these refs
is cheap and is what turns an intermittent delta-base hit into a reliable one.
Option 2's dedicated extra-files chain makes this retention explicit and easy to
reason about. Root-causing the slow transfers, and the `pack-objects` tuning
(`--window` / `--depth` / `--thin` / `bigFileThreshold`), remain ADR-0005's
ticket; this report only fixes the encoding so it *cooperates* with a predictable
pack.

## Remote reassembly (recommended option)

Per ADR-0008 the remote worktree is **disposable** and updated by
**authoritative, destructive overwrite** — no merge against remote-local edits.
That makes reassembly simple and unconditional:

1. **Receive** both scratch refs in one push into the remote's persistent object
   store / repo.
2. **Code:** point the disposable worktree at `code`'s tree and force it to
   match — e.g. read the tree into the worktree's index and
   `git checkout-index -a -f` (the man page's documented "export as tree"
   pattern: "read the desired tree into the index, and do
   `git checkout-index --prefix=… -a`" — [git-checkout-index](https://git-scm.com/docs/git-checkout-index)),
   removing files absent from the tree so client-side deletions propagate.
3. **Extra:** explode `extra`'s tree over the same worktree with a second
   `git checkout-index -a -f` driven from a temp index loaded with that tree
   (`--prefix=<dir>/` if the force-included set lives under a fixed subdirectory).
   This overlays the build outputs / config into place.
4. *(Optional, ADR-0008)* record what was overwritten/deleted purely as
   diagnostics.

Exactly *where* the extra files land in the worktree is governed by their paths
in the `extra` tree, which is ADR-0007's configuration concern; this report only
fixes that they arrive as a separate overlay tree exploded after the code
checkout.

## Bearing on other ADRs

- **ADR-0004 (source).** Recommend **Option 2, refined** as above; the callout is
  resolved and the status can move off bare `proposed`. Key encoding decisions:
  single working-tree commit parented on `HEAD`; separate extra-files commit on
  its own retained chain; gix `Editor` (not a scratch index); scratch-ref
  namespace; retain previous tips.
- **ADR-0005 (transfer).** The encoding cooperates with predictable packs via
  **ref retention for delta bases**; the actual root-cause of the intermittent
  slow transfers and `pack-objects` tuning stay there. Reinforces Research 0001's
  steer toward `git push` → `git receive-pack` for the pack/transfer leg.
- **ADR-0007 (force-include config).** This report consumes the force-include set
  as the `extra` tree and fixes that it is a *separate overlay*; it does **not**
  decide *which* files are included or how that is declared.
- **ADR-0008 (disposable worktree).** Reassembly is an authoritative overwrite;
  the recommended encoding supports a clean, unconditional checkout-plus-explode.
  Relies on the object store (not the worktree) persisting so retained tips
  survive between syncs.
- **ADR-0002 (git strategy).** Validates gix-first-with-shell-out: synthesise
  trees/commits natively with gix's `Editor`; **avoid** the index-centric build
  (the gix gap); shell out to `git push` for pack+transfer.

## Sources

- [git-stash](https://git-scm.com/docs/git-stash) — stash commit structure
  (`H`/`I`/`W`), `git stash create`, `--include-untracked` / `--all`. Prior art
  for snapshotting dirty state into commits without touching the worktree.
- [git-read-tree](https://git-scm.com/docs/git-read-tree) — scratch index from a
  tree (`-i`, `--index-output`, `GIT_INDEX_FILE`).
- [git-update-index](https://git-scm.com/docs/git-update-index) — `--cacheinfo` /
  `--index-info` to stage blobs by digest with no working-tree file.
- [git-checkout-index](https://git-scm.com/docs/git-checkout-index) — "export as
  tree" via `--prefix -a`, `-f` force overwrite; remote explode step.
- [git-pack-objects](https://git-scm.com/docs/git-pack-objects) — delta selection
  (`--window` 10 / `--depth` 50), `--thin`, `--delta-base-offset`, delta reuse,
  `core.bigFileThreshold` (large objects not deltified).
- [gitformat-pack](https://git-scm.com/docs/gitformat-pack) — `OBJ_OFS_DELTA` vs
  `OBJ_REF_DELTA`, thin-pack external bases, bitmaps.
- [Research 0001](0001-gix-git-plumbing-vs-libgit2-capability-gap.md) — gix tree
  `Editor` is native; alternate-**index** construction and new-delta computation
  are gix gaps (→ shell-out); push/receive-pack missing in gix.
- [Git, Compression, and Deltas — an explanation](https://gist.github.com/matthewmccullough/2695758)
  and community reports of
  [slow pushes with large/binary files](https://groups.google.com/g/repo-discuss/c/zQ5aAxq0Ufg)
  — corroborating that big-blob delta handling and base availability, not commit
  topology, drive push-time cost.
