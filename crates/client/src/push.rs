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
//! `-c protocol.fd.allow=always`. `--thin` lets a changed blob travel as a small
//! delta against a base the server already holds (Research 0003).
//!
//! The socket is passed on dedicated file descriptors, **not** as the child's
//! stdin/stdout: `git push` uses its own stdin/stdout, and pointing the
//! transport at fd 0/1 wedges it before the protocol even starts. We reserve two
//! inheritable dups of the socket in the parent — `try_clone_to_owned` for the
//! dup, then `rustix` to clear `FD_CLOEXEC` so they survive `exec` — and pass
//! their numbers as `fd::<in>,<out>`, leaving `git`'s own stdio untouched. (The
//! server side is simpler — `git receive-pack` happily takes the raw socket as
//! its stdin/stdout, exactly as `git daemon` feeds it.)
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
//! objects). On the client, [`encode`] force-overwrites `code` every sync,
//! orphaning the previously-pushed commit; [`SENT_REF`] pins the
//! last-confirmed-pushed tip so its objects survive locally as the delta base.
//! It is advanced **only after** a push succeeds, so a failed push leaves it
//! pointing at the state the server actually has.
//!
//! [`encode`]: crate::encode

use std::io::Read;
use std::net::TcpStream;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use thiserror::Error;

/// The ref that retains the last-confirmed-pushed `code` tip on the client (a
/// delta base for the next push). Under [`gfs_common::REF_NAMESPACE`].
pub const SENT_REF: &str = "refs/git-full-send/sent/code";

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
    #[error("could not update `{SENT_REF}`")]
    RetainRef(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Push `ref_name` from the repository at `repo_dir` to `remote`
/// (`HOST:PORT`) via the server's `git receive-pack`.
///
/// On success the ref (and its objects) are on the server. `sync` calls this for
/// the `code` ref and then [`retain_pushed_tip`] to pin the delta base; it is
/// also the seam tests use to exercise the server's ref-namespace policy.
pub fn push_ref(repo_dir: &Path, remote: &str, ref_name: &str) -> Result<(), PushError> {
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

    // Reserve two inheritable dups of the socket for the transport's input and
    // output fds, and pass their numbers to the `fd::` transport. Reserving them
    // in the parent — rather than `dup2`-ing in a `pre_exec` hook — keeps them
    // clear of the fds `Command` uses to wire up the child's own stdio.
    let transport = TransportFds::reserve(&sock).map_err(PushError::Io)?;

    // Force (`+`): the synthetic scratch refs are overwritten each sync, and a
    // new `code` commit is parented on HEAD rather than the previous tip, so
    // successive pushes are deliberately non-fast-forward (ADR-0004/0005).
    let refspec = format!("+{ref_name}:{ref_name}");
    let status = Command::new("git")
        .arg("-c")
        .arg("protocol.fd.allow=always")
        .arg("push")
        .arg("--thin")
        .arg(format!(
            "fd::{},{}",
            transport.in_fd.as_raw_fd(),
            transport.out_fd.as_raw_fd()
        ))
        .arg(&refspec)
        .current_dir(workdir)
        // `git push` keeps its own stdio; the transport lives on the reserved fds.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            // The child now holds its own copies of the transport fds; drop the
            // parent's so the connection closes cleanly once the push completes.
            drop(transport);
            drop(sock);
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            child.wait().map(|status| (status, stderr))
        })
        .map_err(PushError::Spawn)?;

    let (status, stderr) = status;
    if !status.success() {
        return Err(PushError::PushFailed {
            status,
            stderr: stderr.trim().to_string(),
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
            in_fd: dup_inheritable(socket)?,
            out_fd: dup_inheritable(socket)?,
        })
    }
}

/// Duplicate `fd` to a new owned descriptor with `FD_CLOEXEC` cleared, so it is
/// inherited across `exec`. `try_clone_to_owned` dups to the lowest free fd
/// (≥ 3 in practice, since stdio holds 0–2) but sets `FD_CLOEXEC`; `rustix`
/// clears it. The returned `OwnedFd` closes the parent's copy on drop.
fn dup_inheritable(fd: BorrowedFd<'_>) -> std::io::Result<OwnedFd> {
    let dup = fd.try_clone_to_owned()?;
    rustix::io::fcntl_setfd(&dup, rustix::io::FdFlags::empty()).map_err(std::io::Error::from)?;
    Ok(dup)
}

/// Pin `commit` (the just-pushed tip) under [`SENT_REF`] so its objects survive
/// locally as the delta base for the next push.
///
/// A force create-or-overwrite, mirroring the scratch-ref transaction `encode`
/// uses for `code`. Called only after a successful [`push_ref`].
pub(crate) fn retain_pushed_tip(repo_dir: &Path, commit: gix::ObjectId) -> Result<(), PushError> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
    use gix::refs::{FullName, Target};

    let repo = gix::discover(repo_dir).map_err(|source| PushError::OpenRepo {
        path: repo_dir.to_path_buf(),
        source: Box::new(source),
    })?;
    let name = FullName::try_from(SENT_REF).map_err(|e| PushError::RetainRef(Box::new(e)))?;
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
    .map_err(|e| PushError::RetainRef(Box::new(e)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sent_ref_is_under_the_namespace() {
        assert!(SENT_REF.starts_with(gfs_common::REF_NAMESPACE));
    }
}
