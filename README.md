# git-full-send

A tool for syncing a developer's Git working state — committed code, working-tree
(staged & unstaged) changes, and a deliberately force-included set of normally
gitignored files (e.g. locally-built artifacts and per-user config) — from a
client machine to a remote workstation, using Git to move the data.

It is **Unix-first** and assumes the `git` CLI is present on both ends. The
client never touches your branch, index, or working tree: it synthesises the
sync state into scratch Git objects and pushes them under a namespaced set of
refs (`refs/git-full-send/…`).

## How it works

Three commands (a single `git-full-send` binary):

- **`sync`** (client) — synthesise the current working state and push it to the
  server under a *stream*.
- **`listen`** (server) — long-running receiver that ingests pushed objects.
- **`update-worktree`** (server) — authoritative, destructive checkout of a
  stream's synced state into a worktree directory.

The server binds **localhost only**; connectivity from the client is via a
**manual SSH tunnel** ([ADR-0006](docs/adr/0006-transport-and-connectivity.md)).

## Quickstart

On the remote workstation, start the receiver against a target Git repo:

```sh
git-full-send listen --repo /path/to/target-repo   # binds 127.0.0.1:9419
```

From your laptop, open an SSH tunnel to the remote's listen port, then sync:

```sh
ssh -N -L 9419:localhost:9419 you@workstation &     # leave running
git-full-send sync --repo . --remote 127.0.0.1:9419 --stream-id my-laptop
```

Back on the remote, check the synced state out into a disposable worktree:

```sh
git-full-send update-worktree \
    --repo /path/to/target-repo --worktree /path/to/worktree --stream-id my-laptop
```

To also carry normally-gitignored files (build outputs, per-user config), list
them in a committed `.git-full-send-include` file at the repo root.

See **[docs/operating.md](docs/operating.md)** for the full operator guide
(tunnel setup, the server commands, stream ids, and writing force-include
patterns), and [`docs/adr/`](docs/adr/) for the architecture decisions.

## Status

MVP. The transport has no built-in authentication or encryption — it leans
entirely on the SSH tunnel for confidentiality and access control
([ADR-0006](docs/adr/0006-transport-and-connectivity.md)).
