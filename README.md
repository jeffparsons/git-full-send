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

Plus, for when it feels slow: **`probe`** (is the server up, and what does a
connection cost?), **`doctor`** (which repository conditions are hurting, and
what to do about them), and **`metrics`** (p50/p95 of everything recorded).
Every operation explains its own cost — see
[ADR-0017](docs/adr/0017-making-operation-cost-self-explaining.md) — and `--json`
gives an integrator the same numbers to parse.

The server binds **localhost only**; connectivity from the client is via a
**manual SSH tunnel** ([ADR-0006](docs/adr/0006-transport-and-connectivity.md)).

## Installing

Rolling pre-built binaries of the latest `main` are published as the
[`dev` prerelease](https://github.com/jeffparsons/git-full-send/releases/tag/dev)
for `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, and
`aarch64-unknown-linux-gnu`. The asset names are stable across snapshots, so a
host can always fetch the latest with the same URL:

```sh
target=aarch64-apple-darwin    # or x86_64-unknown-linux-gnu / aarch64-unknown-linux-gnu
curl -fsSL "https://github.com/jeffparsons/git-full-send/releases/download/dev/git-full-send-${target}.tar.gz" \
    | tar -xz -C ~/.local/bin git-full-send
```

`git-full-send --version` on a snapshot binary reports the commit it was built
from. To build from source instead: `cargo install --path crates/cli` from a
checkout (or `cargo build --release`).

## Quickstart

On the remote workstation, start the receiver against a target Git repo:

```sh
head -c 32 /dev/urandom | base64 > ~/.config/git-full-send/token   # a shared secret
chmod 600 ~/.config/git-full-send/token
git-full-send listen --repo /path/to/target-repo \
    --token-file ~/.config/git-full-send/token     # binds 127.0.0.1:9419
```

Copy that token to your laptop, open an SSH tunnel to the remote's listen port,
then sync:

```sh
ssh -N -L 9419:localhost:9419 you@workstation &     # leave running
git-full-send sync --repo . --remote 127.0.0.1:9419 --stream-id my-laptop \
    --token-file ~/.config/git-full-send/token
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

MVP. The transport has no built-in encryption — it leans entirely on the SSH
tunnel for confidentiality ([ADR-0006](docs/adr/0006-transport-and-connectivity.md)).
Pushes are authenticated with a shared secret, because reaching the receiver's
port should not be the same thing as being allowed to push code it will run
([ADR-0019](docs/adr/0019-authenticating-the-receive-pack-connection.md)); the
receiver refuses to start until you either configure one or say
`--allow-anonymous` out loud.

## AI use

This project is developed with **Claude Code** (Anthropic's CLI coding agent).
The **implementation plans** Claude writes for each piece of work are committed
to the git tree under [`plans/`](plans/), so the reasoning behind a change is
visible alongside the change itself.

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
