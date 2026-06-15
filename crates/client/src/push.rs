//! Transfer the encoded `code` ref to the server.
//!
//! The second building block of `sync` (see ADR-0005): once [`encode`] has
//! written `refs/git-full-send/code` locally, push it to the server's
//! `git receive-pack` over the localhost connection, then retain the pushed tip
//! so the next sync has a delta base.
//!
//! ## Transport
//!
//! We open the `TcpStream` ourselves and hand it to `git push` via the `fd::`
//! transport, so `git` speaks the receive-pack protocol directly over the socket
//! — a raw receive-pack stream, no custom framing (ADR-0005). `git` blocks the
//! `fd::`/`ext::` transports by default, so we enable it explicitly with
//! `-c protocol.fd.allow=always`. The delta policy on the wire is chosen per chain
//! (see below).
//!
//! The socket is passed on dedicated file descriptors, **not** as the child's
//! stdin/stdout: `git push` uses its own stdin/stdout, and pointing the
//! transport at fd 0/1 wedges it before the protocol even starts. We reserve two
//! dups of the socket in the parent (`try_clone_to_owned`) and pass their
//! numbers as `fd::<in>,<out>`, leaving `git`'s own stdio untouched. The dups
//! keep `FD_CLOEXEC` in the parent; `git` would normally not inherit them across
//! `exec`, so we clear the flag **only in the forked child** via a `pre_exec`
//! hook (`fcntl(F_SETFD)`, async-signal-safe) just before `exec`. `fork` copies
//! the fd table, so the intended `git push` still inherits the dups while no
//! unrelated concurrent `spawn` can — the dups are never inheritable in the
//! parent's fd table (#57). (The server side is simpler — `git receive-pack`
//! happily takes the raw socket as its stdin/stdout, exactly as `git daemon`
//! feeds it.)
//!
//! ## Per-chain delta policy
//!
//! A single `git push` applies one delta policy to the whole pack. ADR-0005 wants
//! `--thin` deltas for the `code` chain but a *predictable whole-object send* for
//! the volatile `extra` chain, so `sync` pushes the two chains in **separate**
//! exchanges, each with its own [`DeltaPolicy`]. `--thin` lets a changed blob
//! travel as a small delta against a base the server already holds (Research 0003);
//! `--no-thin -c pack.window=0` disables the delta search for the whole-object send.
//!
//! This is Unix-only; the tool is Unix-first. A purely standalone binary could
//! instead bridge the socket with an `ext::` "micro-utility" connector and avoid
//! fd-passing (and `rustix`) entirely; that trade-off is recorded in ADR-0010.
//!
//! ## Prior-tip retention
//!
//! `--thin` only saves bytes when the previous blob is present on **both** ends
//! and surfaced as common by negotiation. The server retains its side
//! automatically (the pushed ref persists and `receive.autogc=false` keeps its
//! objects). On the client, [`encode`] force-overwrites the stream's `code` ref
//! every sync, orphaning the previously-pushed commit; the stream's `sent` ref
//! (`gfs_common::sent_ref`) pins the last-confirmed-pushed tip so its objects
//! survive locally as the delta base. It is advanced **only after** a push
//! succeeds, so a failed push leaves it pointing at the state the server
//! actually has.
//!
//! [`encode`]: crate::encode

use std::net::TcpStream;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use thiserror::Error;
use tokio::process::Command;

/// Errors returned by the transfer step.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PushError {
    /// No Git repository could be found at or above the given path.
    #[error("could not open a Git repository at or above `{path}`")]
    OpenRepo {
        /// The path the push started from.
        path: PathBuf,
        /// The underlying `gix` discovery error.
        source: Box<gix::discover::Error>,
    },
    /// The repository has no working tree (it is bare).
    #[error("repository at `{0}` has no working tree")]
    NoWorktree(PathBuf),
    /// Connecting to the server endpoint failed.
    #[error("could not connect to `{remote}`")]
    Connect {
        /// The endpoint we tried to reach.
        remote: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Spawning `git push` failed.
    #[error("could not spawn `git push`")]
    Spawn(#[source] std::io::Error),
    /// An I/O error while driving the push.
    #[error("I/O error during push")]
    Io(#[source] std::io::Error),
    /// `git push` exited non-zero (e.g. the server rejected the ref).
    #[error("`git push` failed ({status}): {stderr}")]
    PushFailed {
        /// The child's exit status.
        status: ExitStatus,
        /// Captured stderr, trimmed.
        stderr: String,
    },
    /// Updating the retention ref failed.
    #[error("could not update `{ref_name}`")]
    RetainRef {
        /// The ref we tried to write.
        ref_name: String,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// How a push asks `git` to deltify the objects it sends.
///
/// A single `git push` applies **one** delta policy to the whole pack (`--thin`,
/// `pack.window`/`pack.depth` are per-invocation, not per-ref), so a sync that
/// wants different policies for its `code` and `extra` chains issues one push per
/// chain (ADR-0005).
#[derive(Debug, Clone, Copy, Default)]
pub enum DeltaPolicy {
    /// `--thin`: send a changed blob as a small delta against a base the server
    /// already holds. The default; used for the `code` chain (ADR-0005).
    #[default]
    Thin,
    /// `--no-thin -c pack.window=0`: disable the delta search for a *predictable
    /// whole-object send*. Used for the volatile `extra` chain (ADR-0005), whose
    /// big build outputs don't delta well — negotiation still excludes objects the
    /// server already holds, so only the *changed* objects travel, just whole
    /// rather than thin-deltified.
    WholeObject,
}

/// Push `ref_name` from the repository at `repo_dir` to `remote`
/// (`HOST:PORT`) via the server's `git receive-pack`, under `policy`. A thin
/// wrapper over [`push_refs`] for a single ref; the seam tests use it to exercise
/// the server's ref-namespace policy.
pub async fn push_ref(
    repo_dir: &Path,
    remote: &str,
    ref_name: &str,
    policy: DeltaPolicy,
) -> Result<(), PushError> {
    push_refs(repo_dir, remote, &[ref_name], policy).await
}

/// Push `ref_names` from the repository at `repo_dir` to `remote` (`HOST:PORT`)
/// via the server's `git receive-pack`, in **one** `git push` exchange under the
/// given `policy`.
///
/// `sync` pushes the `code` and `extra` chains in **separate** exchanges so each
/// can carry its own [`DeltaPolicy`] (ADR-0005), pinning each chain's delta base
/// with [`retain_pushed_tip`] after its push succeeds. On success every ref (and
/// its objects) is on the server.
pub async fn push_refs(
    repo_dir: &Path,
    remote: &str,
    ref_names: &[&str],
    policy: DeltaPolicy,
) -> Result<(), PushError> {
    let repo = gix::discover(repo_dir).map_err(|source| PushError::OpenRepo {
        path: repo_dir.to_path_buf(),
        source: Box::new(source),
    })?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| PushError::NoWorktree(repo_dir.to_path_buf()))?;

    let sock = TcpStream::connect(remote).map_err(|source| PushError::Connect {
        remote: remote.to_string(),
        source,
    })?;

    // Reserve two dups of the socket for the transport's input and output fds,
    // and pass their numbers to the `fd::` transport. Reserving them in the
    // parent (vs. `dup2`-ing in a `pre_exec` hook) keeps them clear of the fds
    // `Command` uses to wire up the child's own stdio. The dups keep
    // `FD_CLOEXEC`; a `pre_exec` hook clears it in the child just before `exec`
    // (below), so they are never inheritable across an unrelated `spawn` (#57).
    let transport = TransportFds::reserve(&sock).map_err(PushError::Io)?;

    // Force (`+`): the synthetic scratch refs are overwritten each sync, and a
    // new `code` commit is parented on HEAD rather than the previous tip, so
    // successive pushes are deliberately non-fast-forward (ADR-0004/0005).
    let refspecs: Vec<String> = ref_names.iter().map(|r| format!("+{r}:{r}")).collect();
    let mut command = Command::new("git");
    command.arg("-c").arg("protocol.fd.allow=always");
    // The per-chain delta policy (ADR-0005): `code` rides thin deltas; `extra`
    // disables the delta search entirely for a predictable whole-object send.
    match policy {
        DeltaPolicy::Thin => {
            command.arg("push").arg("--thin");
        }
        DeltaPolicy::WholeObject => {
            command
                .arg("-c")
                .arg("pack.window=0")
                .arg("push")
                .arg("--no-thin");
        }
    }
    let in_fd = transport.in_fd.as_raw_fd();
    let out_fd = transport.out_fd.as_raw_fd();
    command
        .arg(format!("fd::{in_fd},{out_fd}"))
        .args(&refspecs)
        .current_dir(workdir)
        // `git push` keeps its own stdio; the transport lives on the reserved fds.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    // Clear `FD_CLOEXEC` on the transport dups in the forked child, just before
    // `exec`, so `git push` inherits them while the parent's copies stay
    // `FD_CLOEXEC` throughout — no unrelated concurrent `spawn` can inherit the
    // connection socket (#57). `tokio::process::Command` doesn't re-expose
    // `CommandExt`, so the hook is registered on the inner `std` command.
    //
    // SAFETY: the closure runs after `fork`, once `std` has wired up the child's
    // stdio, and before `exec`. It only calls `fcntl(F_SETFD)` — async-signal-
    // safe — on two fixed descriptors, with no allocation or locking. The parent
    // keeps the `OwnedFd`s alive (in `transport`) until after `spawn`, and the
    // closure captures only the raw fd numbers, so the descriptors are still open
    // when the child clears the flag.
    unsafe {
        command.as_std_mut().pre_exec(move || {
            for raw in [in_fd, out_fd] {
                let fd = BorrowedFd::borrow_raw(raw);
                rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty())
                    .map_err(std::io::Error::from)?;
            }
            Ok(())
        });
    }

    let child = command.spawn().map_err(PushError::Spawn)?;

    // The child now holds its own copies of the transport fds; drop the parent's
    // so the connection closes cleanly once the push completes. `tokio::process`
    // is built on `std::process`, so the reserved dups (and the `pre_exec` flag
    // flip) pass to the child exactly as a synchronous spawn would — but
    // `child.wait_with_output().await` now yields
    // for the duration of the receive-pack exchange instead of blocking the
    // runtime thread (so a co-located server task can make progress).
    drop(transport);
    drop(sock);
    let output = child.wait_with_output().await.map_err(PushError::Spawn)?;

    if !output.status.success() {
        return Err(PushError::PushFailed {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// A pair of inheritable file descriptors duplicated from the connected socket,
/// passed to `git push` as the `fd::` transport's input and output. The owned
/// dups close the parent's copies on drop (the spawned child keeps its own).
struct TransportFds {
    in_fd: OwnedFd,
    out_fd: OwnedFd,
}

impl TransportFds {
    fn reserve(socket: impl AsFd) -> std::io::Result<Self> {
        let socket = socket.as_fd();
        Ok(Self {
            in_fd: dup_socket(socket)?,
            out_fd: dup_socket(socket)?,
        })
    }
}

/// Duplicate `fd` to a new owned descriptor for the transport. `try_clone_to_owned`
/// dups to the lowest free fd (≥ 3 in practice, since stdio holds 0–2) and sets
/// `FD_CLOEXEC`, so the dup is **not** inherited across `exec` from the parent —
/// the flag is cleared only in the forked child, via the `pre_exec` hook in
/// [`push_refs`], so it is never inheritable across an unrelated `spawn` (#57).
/// The returned `OwnedFd` closes the parent's copy on drop.
fn dup_socket(fd: BorrowedFd<'_>) -> std::io::Result<OwnedFd> {
    fd.try_clone_to_owned()
}

/// Pin `commit` (a just-pushed tip) under `sent_ref` so its objects survive
/// locally as the delta base — and, for `extra`, the parent — of the next push.
///
/// A force create-or-overwrite, mirroring the scratch-ref transaction `encode`
/// uses for `code`. Called once per chain (`gfs_common::sent_ref` /
/// `gfs_common::sent_extra_ref`) only after a successful [`push_refs`].
pub(crate) fn retain_pushed_tip(
    repo_dir: &Path,
    sent_ref: &str,
    commit: gix::ObjectId,
) -> Result<(), PushError> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
    use gix::refs::{FullName, Target};

    let repo = gix::discover(repo_dir).map_err(|source| PushError::OpenRepo {
        path: repo_dir.to_path_buf(),
        source: Box::new(source),
    })?;
    let name = FullName::try_from(sent_ref).map_err(|e| PushError::RetainRef {
        ref_name: sent_ref.to_string(),
        source: Box::new(e),
    })?;
    repo.edit_reference(RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: "git-full-send: retain pushed tip".into(),
            },
            expected: PreviousValue::Any,
            new: Target::Object(commit),
        },
        name,
        deref: false,
    })
    .map_err(|e| PushError::RetainRef {
        ref_name: sent_ref.to_string(),
        source: Box::new(e),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use gfs_common::StreamId;

    #[test]
    fn sent_ref_is_under_the_namespace() {
        let stream = StreamId::new("test").unwrap();
        assert!(gfs_common::sent_ref(&stream).starts_with(gfs_common::REF_NAMESPACE));
    }
}
