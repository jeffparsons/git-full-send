# Operating git-full-send

This guide covers running `git-full-send` end to end: establishing the SSH
tunnel, running the server commands on the remote workstation, choosing stream
ids, and writing the force-include pattern files. It assumes the `git-full-send`
binary is built and on `PATH` on both machines, and that `git` is installed on
both (the tool shells out to it — [ADR-0002](adr/0002-git-manipulation-strategy.md)).

Terminology: the **client** is your local machine (where you edit code); the
**server** is the remote workstation that receives your working state and checks
it out.

## 1. The SSH tunnel

The server binds **localhost only** and there is **no built-in authentication or
encryption** — `git-full-send` leans entirely on an SSH tunnel for
confidentiality and access control
([ADR-0006](adr/0006-transport-and-connectivity.md)). Setting up the tunnel is a
manual prerequisite to syncing.

From the **client**, forward a local port to the server's loopback listen port:

```sh
ssh -N -L 9419:localhost:9419 you@workstation
```

- `-L 9419:localhost:9419` forwards your local `127.0.0.1:9419` to
  `localhost:9419` *as seen from the server* — i.e. the loopback address the
  server's `listen` is bound to.
- `-N` runs the tunnel without a remote shell. Leave it running for the duration
  of your session (background it, or use your usual tunnel manager).

The client then points `sync --remote` at the **local** end of the tunnel
(`127.0.0.1:9419`); traffic emerges on the server's loopback and reaches
`listen`. The default port is `9419`; if you change the server's `--addr` port,
match it on both ends of the `-L` forward.

## 2. Running the server

Both server commands operate on a **target Git repository** on the remote — the
repository that receives the synced refs. It can be a bare repo or an ordinary
one; `git-full-send` only writes refs under `refs/git-full-send/…` and never
touches your branches.

### `listen` — the receiver

A long-running process that accepts pushed objects:

```sh
git-full-send listen --repo /path/to/target-repo [--addr 127.0.0.1:9419]
```

- `--repo` — the target repository.
- `--addr` — the bind address; defaults to `127.0.0.1:9419`. Keep it on
  loopback (see the tunnel section); the flag exists for choosing a different
  port, not for exposing the server on the network.

Leave it running. It serves each connection independently and stays up across
syncs.

### `update-worktree` — the checkout

An on-demand, **authoritative and destructive** overwrite of a worktree
directory with a stream's synced `code` state, with its force-included `extra`
files overlaid on top ([ADR-0008](adr/0008-remote-worktree-disposability.md),
[ADR-0007](adr/0007-syncing-extra-gitignored-files.md)):

```sh
git-full-send update-worktree \
    --repo /path/to/target-repo \
    --worktree /path/to/worktree \
    --stream-id my-laptop
```

- It **stomps remote-local edits**, removes files dropped since the last sync,
  and prunes untracked leftovers — after it returns, the worktree matches the
  synced state exactly. Treat the worktree as disposable; don't keep work you
  care about there.
- `--repo` and `--worktree` are independent: you choose which stream lands in
  which directory (a dedicated worktree per stream, or several streams taking
  turns in one — [ADR-0012](adr/0012-namespacing-managed-refs-per-stream.md)).
- Run it whenever you want the remote to reflect the latest `sync` (e.g. from a
  build orchestrator).
- **Concurrency:** updates of the *same* worktree are serialised by a
  per-worktree advisory lock, so two runs can't interleave their checkout steps
  (issue #49). By default a run whose worktree is already being updated **fails
  fast** with a non-zero exit and an "update already in progress" error. Pass
  `--wait` to block until the in-progress run finishes instead, and
  `--wait --timeout <secs>` to give up after a bounded wait. Distinct worktrees
  never contend.

### `list-streams` — discover synced streams

```sh
git-full-send list-streams --repo /path/to/target-repo
```

Prints the stream ids that have a synced `code` ref — useful for finding what is
available to check out.

### `forget-stream` — retire a stream

```sh
git-full-send forget-stream --repo /path/to/target-repo --stream-id my-laptop
```

Deletes every ref of a stream
(`refs/git-full-send/streams/<id>/…`) from `--repo`, so it no longer appears in
`list-streams`. `git-full-send` otherwise keeps a stream's refs forever — stable
ids keep the set bounded, but nothing reclaims a stream you are done with
([ADR-0012](adr/0012-namespacing-managed-refs-per-stream.md),
[ADR-0014](adr/0014-forgetting-a-stream.md)); this is the explicit way to drop
one.

- It removes only refs. It does **not** delete the worktree a stream was checked
  out into, nor its per-worktree index dir — those are keyed by worktree path,
  not stream id (a stream and a worktree are independent — ADR-0012), and the
  worktree is disposable anyway ([ADR-0008](adr/0008-remote-worktree-disposability.md)).
  Remove the worktree directory yourself if you no longer want it.
- **Idempotent:** forgetting a stream that has no refs (never synced, or already
  forgotten) succeeds and reports that there was nothing to forget.
- The command is **symmetric** — see "Retiring a stream" below for cleaning up
  the client side as well.

## 3. Stream ids

A **stream** is an independent, reusable slot of synced state. Refs are
namespaced per stream so concurrent senders don't clobber each other
([ADR-0012](adr/0012-namespacing-managed-refs-per-stream.md)).

- On the **client**, choose one with `sync --stream-id <id>`. If you omit it, an
  id is generated and persisted to the repo's local Git config
  (`git-full-send.stream-id`) on first use and reused thereafter.
- Pick a **stable** id and reuse it across syncs — the delta-base retention that
  keeps transfers small only pays off when the same refs are reused.
- Ids may be branch-shaped (contain slashes), e.g. `feature/foo`; they are
  validated to form well-formed Git ref names.
- On the **server**, pass the matching `--stream-id` to `update-worktree`.

### Retiring a stream

`forget-stream` deletes a stream's refs in whichever repo you point it at, so a
stream has two sides to clean up:

- On the **server**, `forget-stream --repo <server-repo>` drops the synced
  `code`/`extra` refs (this is what removes it from `list-streams`).
- On the **client**, the repo also keeps the stream's local refs — the scratch
  `code`/`extra` refs a sync pushes from and the `sent/*` delta-base pins — plus
  the `git-full-send.stream-id` config key if this stream is the repo's default.
  Run `forget-stream --repo <client-repo> --stream-id <id>` to drop the refs, and
  `git config --unset git-full-send.stream-id` if you also want to stop this repo
  defaulting to that id (otherwise the next bare `sync` regenerates the stream
  under the same id).

Forgetting a stream that is still in use is safe: a later `sync` simply
re-creates its refs from scratch (without a delta base for that first push).

## 4. Force-include pattern files

By default `git-full-send` syncs your committed code plus working-tree changes,
but excludes gitignored files. To deliberately carry a controlled set of
normally-gitignored files — CPU-intensive build outputs, per-user config — you
declare them as **allow-list patterns**
([ADR-0007](adr/0007-syncing-extra-gitignored-files.md)).

### Where the patterns live

Two layers, both optional:

- **Project file** — `.git-full-send-include` at the repo root. Committed and
  shared with the team; this is the primary place to declare force-includes.
- **Per-user file** — for machine- or user-specific additions, outside the repo.
  Resolved (in order) from:
  1. `sync --user-include <path>` (highest precedence), or
  2. the `GIT_FULL_SEND_USER_INCLUDE` environment variable, or
  3. `$XDG_CONFIG_HOME/git-full-send/include` (falling back to
     `$HOME/.config/git-full-send/include`).

The two layers are evaluated `[project, then user]` with **last-match-wins**, so
a per-user pattern can override a project one.

### Syntax

The files use **gitignore pattern syntax**, but with **inverted polarity**: a
bare pattern *includes* a path into the sync, and a leading `!` *carves it back
out*. (This is the opposite of `.gitignore`, where bare patterns exclude.)

Selection is an independent walk of the working-tree filesystem — it is **not**
`!`-negations on your real `.gitignore`. That is deliberate: it sidesteps Git's
"cannot re-include a file under an ignored parent directory" limitation, so you
can pull files back out of an ignored `dist/` or `target/` freely.

### Example

```gitignore
# .git-full-send-include
dist/                # force-include the whole gitignored build output…
!dist/secret.env     # …except this one file
target/release/app   # a single deeply-nested artifact, even though target/ is ignored
```

A per-user file could then add machine-specific config or carve out a project
include for just your machine:

```gitignore
# ~/.config/git-full-send/include
config/local.toml    # add a per-user file the project doesn't ship
!dist/big-cache.bin  # don't sync this from my machine
```

### Performance note

The selection walk prunes itself: a directory is entered only if it is already
inside a selected subtree, or an include pattern could still match beneath it.
Patterns with a literal directory prefix — anchored by a leading `/` or an
interior `/` (e.g. `/dist/`, `web-client/dist/`) — let the walk skip unrelated
trees, so a large ignored `node_modules` is never descended when nothing in it
is selected. The prune is a deliberate over-approximation: it never skips a
directory the exhaustive walk would have selected from.

The residual caveat is the **unanchored** pattern: a bare basename or
`basename/` (e.g. `*.wasm`, `dist/`), or one starting with `**`/a wildcard, can
match at any depth, so it forces the full exhaustive walk and emits a warning
(such a pattern is usually an accidental include). Keep the include set curated
and prefer anchored patterns. See `crates/client/src/select.rs` and
[ADR-0007](adr/0007-syncing-extra-gitignored-files.md) for detail.

## 5. Metrics

Every operation appends one structured **JSON Lines** record to a per-side sink
for retrospective analysis ([ADR-0013](adr/0013-recording-operation-metrics.md)):

```text
<git-dir>/git-full-send/metrics.jsonl
```

— on the **client** (e.g. `.git/git-full-send/metrics.jsonl`) for each `sync`,
and on the **server** repo for each `receive` (one per `git receive-pack`
connection) and each `update-worktree`. Each record carries a `kind` tag, a
timestamp, phase timings (in milliseconds), and size metadata — the client's
per-layer file/byte counts, the server's on-wire `bytes_in`/`bytes_out` and the
refs a push updated. Writing is best-effort: if the file can't be written the
operation still succeeds and a warning is logged.

Inspect or aggregate it with any JSON tool, e.g. the slowest syncs:

```sh
jq -r 'select(.kind=="sync") | [.total_ms, .stream] | @tsv' \
    .git/git-full-send/metrics.jsonl | sort -rn | head
```

The file grows unbounded for now (no rotation); delete it freely — it is regenerated.
