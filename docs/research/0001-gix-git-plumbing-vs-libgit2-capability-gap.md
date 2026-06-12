# Research 0001 — gix / `git` plumbing CLI vs libgit2 capability gap analysis

- Date: 2026-06-12
- Source ADR: [ADR-0002 — Git manipulation strategy](../adr/0002-git-manipulation-strategy.md)
- Related: [ADR-0004](../adr/0004-encoding-the-sync-state-in-git.md),
  [ADR-0005](../adr/0005-transfer-mechanism.md)
- **Snapshot pinned to `gix` 0.84.0 (released 2026-05-26)** — the latest published
  release at the time of writing. gitoxide moves fast; treat capability claims as
  a dated snapshot and re-check `crate-status.md` before relying on a gap being
  open or closed.

## TL;DR

For the four operations `git-full-send` needs, **gix is fully sufficient for the
"synthesise objects" half and has real gaps in the "pack and transfer" half.**

- **Object / tree / commit synthesis** — native in gix today. No shell-out, no gap.
- **Alternate index construction** — *not* native in gix (cannot stage entries or
  write-tree-from-index), but this is **architecturally avoidable**: gix can build
  trees directly without an index. If we genuinely want a scratch index, it's a
  `git` CLI shell-out.
- **Pack generation** — partial in gix: it can emit packs and reuse/forward
  existing deltas, but it **cannot compute new deltas and has no reachability
  bitmaps**. This is the gap that bears directly on the ADR-0005
  pathological-pack-shape / slow-transfer concern.
- **send / receive-pack** — **missing in gix on both sides**: no client push
  (send-pack) and no server receive-pack/`accept()`. Both are explicitly post-1.0
  for gitoxide.

**Practical consequence:** the pack-generation + transfer leg is cleanest as a
single shell-out to `git` (`git push` → `git receive-pack` / `git daemon`), which
hands us battle-tested delta compression and transfer for free and sidesteps all
three gix gaps at once. The synthesis leg stays native in gix. This validates
ADR-0002's gix-first-with-shell-out posture and points ADR-0005 at its "reuse
Git's machinery" option for now. None of the gaps is worth *blocking* tool work to
upstream today (see [Upstream candidates](#upstream-candidates)).

## Capability matrix

Legend: **✅ native** · **⚠ partial** · **🐚 only via shelling out to `git`** ·
**❌ missing in both gix and the `git` CLI**. The libgit2 column is the mature
reference baseline (we do **not** depend on it — [ADR-0002](../adr/0002-git-manipulation-strategy.md)).

| Capability | gix 0.84.0 | `git` plumbing CLI | libgit2 (reference) |
| --- | --- | --- | --- |
| **1. Object / tree / commit synthesis** | | | |
| Write blob to ODB | ✅ `Repository::write_blob` / `write_blob_stream` | `git hash-object -w --stdin` | `git_blob_create_from_buffer` |
| Synthesise & write tree | ✅ `Repository::edit_tree` (tree `Editor`), `write_object` | `git mktree` | `git_treebuilder_*` + `_write` |
| Create commit (no ref move) | ✅ `Repository::commit_as` / `write_object` | `git commit-tree` | `git_commit_create(update_ref=NULL)` |
| Raw object write, any type | ✅ `Repository::write_object`, `gix-odb` | `git hash-object -t …` | `git_odb_write` |
| **2. Alternate index construction** | | | |
| Derive index from a tree | ✅ `Repository::index_from_tree` | `GIT_INDEX_FILE=… git read-tree <tree>` | `git_index_read_tree` |
| Stage files / add-remove index entries | ❌ gix: `[ ] add and remove entries` → 🐚 | `GIT_INDEX_FILE=… git update-index --index-info` / `--add --cacheinfo` | `git_index_add` / `git_index_add_from_buffer` |
| Write tree **from** an index | ❌ gix: `[ ] tree from index` → 🐚 | `GIT_INDEX_FILE=… git write-tree` | `git_index_write_tree_to` |
| **3. Pack generation** | | | |
| Objects → pack (store whole) | ✅ `gix-pack` `data::output::*` | `git pack-objects` | `git_packbuilder_*` |
| Thin pack | ✅ `[x] create 'thin' pack` | `git pack-objects --thin` | (via `git_remote_upload`) |
| Reuse **existing** deltas | ✅ (base-object compression / delta forwarding) | `git pack-objects` (default) | yes |
| Compute **new** deltas (window/depth) | ❌ gix: `[ ] delta compression` → 🐚 | `git pack-objects --window=<n> --depth=<n>` | ✅ `git_packbuilder` (does delta compression) |
| Reachability bitmaps | ❌ gix: `[ ] 'bitmap' file` → 🐚 | `git pack-objects --write-bitmap-index` / `git repack -b` | n/a (not exposed) |
| Multi-pack index (MIDX) | ✅ `[x]` read/write/verify | `git multi-pack-index` | n/a |
| **4. send / receive-pack** | | | |
| Client fetch / clone (proto v2) | ✅ `Remote::fetch`, `[x] V2 handshake` | `git fetch` / `git fetch-pack` | `git_remote_fetch` / `git_remote_download` |
| Client push (send-pack) | ❌ gix: `[ ] push` (#306, outscoped from 1.0) → 🐚 | `git push` / `git send-pack` | ✅ `git_remote_push` / `git_remote_upload` |
| Server upload-pack (serve fetch) | ❌ gix: `[ ]` server plumbing (#307) → 🐚 | `git upload-pack` / `git daemon` | ❌ not provided |
| Server receive-pack (accept push) | ❌ gix: `[ ]` server `accept(…)` (#307) → 🐚 | `git receive-pack` / `git daemon --enable=receive-pack` | ❌ not provided |
| Ingest received pack | ⚠ `gix-pack` indexing exists; no end-to-end server | `git index-pack --stdin --fix-thin` | `git_indexer` |

Notes on the matrix:

- **No cell is ❌-in-both** for anything this tool needs — every gix gap has a
  `git` CLI fallback (🐚). The two ❌s in the libgit2 column (server upload/receive-pack)
  are libgit2 limitations, not ours; they only reinforce that *no* library gives us
  a server side, so the server is a `git`-CLI / `git daemon` job regardless.
- gix's "reuse existing deltas" means it can forward deltas already present in a
  source pack and emit thin-pack deltas against bases the receiver has; it does
  **not** search for and compute *new* deltas between objects (the `--window`/`--depth`
  work). That distinction is the crux of the pack-generation gap.

## Per-operation analysis

### 1. Object / tree / commit synthesis — native in gix, no gap

gix exposes a complete, public, stable write path:

- `gix::Repository::write_blob` / `write_blob_stream` persist blobs and return an
  `Id`; `write_object` persists any `gix_object::WriteTo` (blob, tree, commit, tag).
- Trees can be synthesised programmatically with the tree `Editor`
  (`Repository::edit_tree`) or by encoding a `gix_object::Tree` and calling
  `write_object`. Commits via `Repository::commit_as` (or encode + `write_object`)
  write the object **without moving any ref** — exactly the "don't disturb the
  user's branch" constraint from ADR-0004.
- `crate-status.md` marks the gix-object encode side (`encode owned objects`,
  `edit trees efficiently and write changes back`) and the gix-odb write side
  (`streaming write for blobs`, `write objects and obtain id`) as done.

The `git` CLI equivalents (`hash-object -w`, `mktree`, `commit-tree`) and libgit2
(`git_treebuilder_*`, `git_commit_create` with `update_ref = NULL`) match this
one-to-one. **Recommendation: do this natively in gix.** No shell-out.

### 2. Alternate index construction — gix gap, but architecturally avoidable

`crate-status.md` for `gix-index` is explicit: `[x] index from tree`,
`[ ] add and remove entries`, `[ ] tree from index`. So gix can turn a *tree* into
an in-memory index (`Repository::index_from_tree`), but it **cannot**:

- stage arbitrary working-tree files by adding index entries, nor
- write a tree object **from** an index (no `write-tree` equivalent).

That means the classic "scratch index" recipe — ADR-0004 option 3 — is **not
available natively** in gix today. The mature paths are:

- **`git` CLI (🐚):** the canonical isolated pattern is
  `export GIT_INDEX_FILE=<scratch>` then `git read-tree <tree>` (no `-u`, so the
  working tree is untouched) to seed, `git update-index --index-info` /
  `--add --cacheinfo <mode>,<sha>,<path>` to stage synthesised entries, and
  `git write-tree` to emit the tree id. The user's real `.git/index` and working
  tree are never opened. (The `git(1)` man page documents `GIT_INDEX_FILE` as
  "an alternate index file"; `read-tree` without `-u` updates only the index.)
- **libgit2 (reference):** `git_index_new()` (in-memory) or `git_index_open(path)`
  + `git_index_add` / `git_index_add_from_buffer` + `git_index_write_tree_to(repo)`
  do this fully in-process — the capability gix is missing.

**Key insight for this tool:** we likely **don't need an index at all.** The goal
of ADR-0004 is to synthesise *trees* from (committed history + working-tree changes
+ force-included files). gix's tree `Editor` lets us build those trees directly
from blob ids — start from the existing commit's tree, overlay synthesised
blob/sub-tree entries, write the result — without ever materialising an index.
That keeps the work native (operation 1) and routes around the operation-2 gap
entirely. The alternate-index approach only becomes a forced `git` shell-out if we
deliberately choose the index-centric encoding. See
[Bearing on ADR-0004](#bearing-on-adr-0004).

### 3. Pack generation — partial in gix; the new-delta gap is the important one

gix has a real pack-writing pipeline (`gix-pack` `data::output`: count objects →
objects-to-entries iterator → entries-to-pack-bytes), with thin packs, a
"scales perfectly" parallel implementation, and MIDX read/write/verify. What it
**lacks** (`crate-status.md`):

- `[ ] delta compression` — gix does **not** compute new deltas between objects.
  It stores objects whole (zlib) or reuses/forwards deltas that already exist in a
  source pack and emits thin-pack deltas against bases the receiver has.
- `[ ] 'bitmap' file` — no reachability bitmaps, so no bitmap-accelerated object
  counting.

The `git` CLI (`git pack-objects`) and libgit2 (`git_packbuilder`) both compute
new deltas. `git pack-objects` additionally exposes the **delta-shape controls**
that ADR-0005 cares about: `--window=<n>` and `--depth=<n>` (defaults 10 / 50),
`--delta-base-offset`, `--thin`, and `--write-bitmap-index`.

This is the gap that bears on the ADR-0005 observation of **intermittent slow
transfers / pathological pack shapes.** The tool's payload includes large-ish
changed build outputs; whether successive versions of those files delta well
against what the server already has is exactly a function of delta compression and
window/depth tuning. A native gix pack — storing those objects whole — would tend
to *maximise* transfer size for changed binaries, which is the opposite of what we
want. **Recommendation: do not hand-roll pack generation in gix for the transfer
path. Let `git` build the pack** (whether via `git pack-objects` directly or
implicitly inside `git push`), so we get delta compression and the window/depth
knobs. gix-pack remains useful for local pack *reading*/MIDX, not for producing the
wire pack.

### 4. send / receive-pack — missing in gix on both sides

- **Client push (send-pack): not implemented in gix.** `crate-status.md` shows
  `[ ] push` and `[ ] send-pack / receive-pack client plumbing`; the README's
  feature list shows `* [ ] push`; the `gix` remote connection module contains
  only `fetch`, no `push`/`send` method. It is tracked in **#306** (open) and is
  explicitly in the **"Outscoped"** section of the **#470 "gix towards 1.0"**
  roadmap — i.e. deliberately deferred past 1.0.
- **Server side (upload-pack / receive-pack / `accept()`): not implemented.**
  `gix-transport`'s only server bullet, `[ ] general purpose accept(…) for
  servers`, is unchecked, as is `gix-protocol`'s `[ ] upload-pack / receive-pack
  server plumbing`. Tracked in **#307** (open); not started.
- **Client fetch/clone, including protocol v2, is mature** (`Remote::fetch`,
  `[x] V2 handshake`) — but the tool's transfer is a *push from client + receive on
  server*, so fetch maturity doesn't help the hot path.

Both the `git` CLI and libgit2 do client push. **Neither library does the server
side** (libgit2 issue #5605 is still open; people build servers atop its
primitives). So the server is a `git`-CLI job no matter what. For this tool that
means ADR-0005's "reuse Git's machinery" option:

- **Client:** `git push` (or `git send-pack`) over the localhost SSH tunnel — git
  builds the (delta-compressed, thin) pack and streams it.
- **Server:** `git receive-pack` invoked directly, or a confined
  `git daemon --enable=receive-pack` on localhost, ingesting the pack via
  `index-pack --fix-thin` and updating refs.

A **native gix transfer** (ADR-0005 option 2) is currently blocked on *three*
substantial gix gaps simultaneously — push, server `accept()`, and (for good wire
size) delta compression — and on the maintainers' explicit post-1.0 sequencing.
It is not feasible in the 0.84 timeframe.

## Gaps that force a shell-out

Consolidated, in the order they hit the tool's pipeline:

1. **Write-tree-from-index / index-entry staging** (gix #293) — forces a `git`
   shell-out **only if** we adopt the alternate-index encoding. *Avoidable* by
   building trees directly with gix's tree `Editor` (preferred).
2. **Pack delta compression + window/depth control + bitmaps** (gix #306 / #2531) —
   forces `git pack-objects` / `git push` for any pack that goes on the wire, to
   avoid pathological (whole-object) pack sizes.
3. **Client push / send-pack** (gix #306, outscoped from 1.0 per #470) — forces
   `git push` / `git send-pack` for the client side of the transfer.
4. **Server receive-pack / `accept()`** (gix #307) — forces `git receive-pack` /
   `git daemon` on the server side. (libgit2 wouldn't help here either.)

Gaps 2–4 collapse into **one** clean boundary: *let `git` own pack-and-transfer.*
A single `git push` → `git receive-pack` exchange satisfies all three at once.

## Upstream candidates

We're open to pausing tool work to upstream fixes to gitoxide ([ADR-0002](../adr/0002-git-manipulation-strategy.md)).
Assessed against value-to-this-tool and tractability:

| Gap | Upstream issue | State | Worth upstreaming now? |
| --- | --- | --- | --- |
| Pack delta compression | #306 (delta task), #2531 | open, **no active impl** | **High value, high effort.** Most impactful for our transfer size, but a large, careful piece of work. Not started upstream. |
| Client push / send-pack | #306 | open, **outscoped from 1.0** (#470) | **High value, high effort, deprioritised upstream.** Maintainers have deliberately deferred it; contributing it is a big lift against the grain of the roadmap. |
| Server receive-pack / `accept()` | #307 | open, not started | **High value, very high effort.** A whole server side; even libgit2 never did this. |
| Index entry mutation / write-tree-from-index | #293 | open, **stale** (no 2025–26 activity) | **Low value for us** (avoidable via direct tree synthesis), though the most self-contained of the four. |

**Recommendation: do not block tool work to upstream anything right now.** The
shell-out to `git` for pack-and-transfer is cheap, correct, and battle-tested, and
`git` is already an assumed dependency on both ends. The three high-value gaps
(delta, push, server) are exactly the areas gitoxide has sequenced for *after* 1.0,
so upstreaming them would be swimming upstream in both senses. Revisit when:

- gix push (#306) lands — at which point a native client send path becomes a
  drop-in replacement for the `git push` shell-out; and/or
- we measure the `git`-CLI transfer as a real bottleneck that a native path would
  fix (it almost certainly won't be — `git` itself is the performance reference).

If we ever *do* want to contribute, **delta compression (#306)** is the gap whose
closure would most benefit both this tool and gitoxide broadly — but only pursue it
if a native-gix transfer becomes a project goal, since today `git push` already
gives us delta compression for free.

## Bearing on the proposed ADRs

These notes are inputs to those ADRs' own research tickets, **not** decisions.

### Bearing on ADR-0004 (encoding the sync state in Git)

- Option 3 ("alternate-index-based tree construction") would, today, force a `git`
  CLI shell-out for index staging + `write-tree` (gix gap #293).
- Options 1 and 2 (stacked commits / separate commit, built as **trees**) can be
  done **fully natively** in gix via `write_blob` + the tree `Editor` +
  `commit_as`, with no index and no shell-out, while honouring the
  "don't touch the user's branch/index/worktree" constraint.
- This is a mild argument for a **tree-synthesis encoding over an index-centric
  one**, purely on the "stay native in gix" axis. It does not settle the
  layering/efficiency questions ADR-0004 still owns.

### Bearing on ADR-0005 (transfer mechanism)

- Option 1 (`git push` → server `receive-pack` / `git daemon`) is the only option
  presently feasible: it covers the pack-delta, client-push, and server-receive
  gaps in gix all at once.
- Option 2 (native gix smart-protocol transfer) is **blocked** on gix #306 (push),
  #307 (server), and the delta-compression gap — and on gitoxide's post-1.0
  sequencing. Not viable now.
- The pack-delta gap is a concrete, named cause that fits the ADR-0005
  "pathological pack shape" suspicion: a hand-rolled gix pack stores changed build
  outputs whole. Using `git`'s packer (with its `--window`/`--depth` controls)
  is the lever for predictable transfer size. The *root-cause* of the intermittent
  slowness remains ADR-0005's own ticket; this analysis only says **which tool
  must own pack generation** (answer: `git`).

## Sources

Primary sources consulted (fetched 2026-06-12):

- gitoxide `crate-status.md` (canonical capability checklist):
  <https://raw.githubusercontent.com/GitoxideLabs/gitoxide/main/crate-status.md>
- gitoxide `README.md` feature list:
  <https://github.com/GitoxideLabs/gitoxide/blob/main/README.md>
- `gix` 0.84.0 `Repository` docs:
  <https://docs.rs/gix/0.84.0/gix/struct.Repository.html>
- gix source — `gix/src/repository/object.rs`, `gix/src/remote/` (fetch only),
  `gix-pack/src/data/output/`, `gix-object/src/`:
  <https://github.com/GitoxideLabs/gitoxide/tree/main/gix>
- gitoxide issues: #470 "gix towards 1.0" (push outscoped)
  <https://github.com/GitoxideLabs/gitoxide/issues/470> ·
  #306 "client push to remote" <https://github.com/GitoxideLabs/gitoxide/issues/306> ·
  #307 "Server-side of fetch/pull" <https://github.com/GitoxideLabs/gitoxide/issues/307> ·
  #293 "gix-index towards 1.0" <https://github.com/GitoxideLabs/gitoxide/issues/293> ·
  #2531 "Customizing Delta Topological Relationships for Pack Files"
  <https://github.com/GitoxideLabs/gitoxide/issues/2531> ·
  #2421 "index.write() can cause index corruption …"
  <https://github.com/GitoxideLabs/gitoxide/issues/2421>
- `git` plumbing man pages: `git-hash-object`, `git-mktree`, `git-commit-tree`,
  `git-write-tree`, `git-update-index`, `git-read-tree`, `git-pack-objects`,
  `git-rev-list`, `git-index-pack`, `git-send-pack`, `git-receive-pack`,
  `git-upload-pack`, `git-daemon`, `git-push`, and `git(1)` (`GIT_INDEX_FILE`):
  <https://git-scm.com/docs>
- libgit2 public headers (`include/git2/`): `blob.h`, `tree.h`, `commit.h`,
  `odb.h`, `index.h`, `pack.h`, `remote.h`, `sys/transport.h`, `net.h`:
  <https://github.com/libgit2/libgit2/tree/main/include/git2> ·
  libgit2 server-side issues #1496 <https://github.com/libgit2/libgit2/issues/1496>
  and #5605 <https://github.com/libgit2/libgit2/issues/5605>

### Caveats / unverified

- gix capability claims are pinned to 0.84.0 via `crate-status.md`; gitoxide
  iterates quickly, so re-check before depending on a gap being open/closed.
- No in-flight gitoxide PR was found delivering push, server support, delta
  compression, or index-entry mutation — but "found no PR" is not proof none
  exists. Issue *states* (open/outscoped/stale) are reported as read on 2026-06-12.
- libgit2 protocol-v2 support could not be confirmed from its public headers or
  changelog and is treated as **unverified** (likely protocol v0 on the client).
  This does not affect any decision here, since we don't use libgit2.
</content>
