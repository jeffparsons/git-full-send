//! Server-side library for `git-full-send`.
//!
//! The server runs on the remote workstation. It has two independent operations
//! (see ADR-0003): a long-running [`listen`] loop that receives transferred
//! objects, and an on-demand [`update_worktree`] that checks the synced state
//! out into the configured worktree.
//!
//! ## How `listen` receives objects
//!
//! `listen` binds a localhost TCP port (ADR-0006) and, for each accepted
//! connection, spawns `git receive-pack <repo>` with the socket wired to the
//! child's stdin/stdout — the same hand-off `sshd` and `git daemon` perform
//! internally (ADR-0005). `git` owns pack ingest; we keep control of the
//! invocation to confine writable refs to the [`gfs_common::REF_NAMESPACE`]
//! namespace (via a `pre-receive` hook installed under a gfs-managed
//! `core.hooksPath`) and to disable `receive.autogc` for the receive window so a
//! post-receive gc cannot prune the delta bases a subsequent push needs
//! (Research 0003).
//!
//! The socket-as-stdio plumbing is Unix-only; the tool is Unix-first (see
//! [ADR-0006] and the client's `encode`). Windows transport support is out of
//! scope.
//!
//! [ADR-0006]: https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0006-transport-and-connectivity.md

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;
use thiserror::Error;

/// Errors returned by server operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerError {
    /// An underlying protocol error.
    #[error(transparent)]
    Protocol(#[from] gfs_common::ProtocolError),
    /// The configured target path is not a Git repository.
    #[error("`{0}` is not a Git repository")]
    NotARepo(PathBuf),
    /// Binding the listen socket failed.
    #[error("could not bind to `{addr}`")]
    Bind {
        /// The address we tried to bind.
        addr: SocketAddr,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Materialising the `pre-receive` hook failed.
    #[error("could not install the receive-pack hook")]
    InstallHook(#[source] std::io::Error),
    /// An I/O error while accepting or serving a connection.
    #[error("I/O error while serving")]
    Io(#[source] std::io::Error),
    /// Spawning `git receive-pack` failed.
    #[error("could not spawn `git receive-pack`")]
    Spawn(#[source] std::io::Error),
    /// The blocking serve task panicked or was cancelled.
    #[error("serve task failed: {0}")]
    Join(String),
}

/// A bound, ready-to-serve listener.
///
/// Produced by [`bind`] and consumed by [`serve`]. Splitting bind from serve
/// lets a caller (notably tests) bind an ephemeral port, read it back via
/// [`Listener::local_addr`], and serve on a background thread.
#[derive(Debug)]
pub struct Listener {
    listener: TcpListener,
    repo: PathBuf,
    /// The gfs-managed hooks directory; kept alive for the listener's lifetime
    /// so the `pre-receive` hook persists for every connection.
    hooks: TempDir,
}

impl Listener {
    /// The actual address the listener is bound to.
    ///
    /// Useful when [`bind`] was given a port of `0` and the OS chose one.
    pub fn local_addr(&self) -> Result<SocketAddr, ServerError> {
        self.listener.local_addr().map_err(ServerError::Io)
    }
}

/// Bind a localhost TCP listener for `addr` that will serve `repo`.
///
/// Validates that `repo` is a Git repository and materialises the namespace
/// `pre-receive` hook, but does not yet accept connections — call [`serve`].
pub fn bind(addr: SocketAddr, repo: PathBuf) -> Result<Listener, ServerError> {
    gix::discover(&repo).map_err(|_| ServerError::NotARepo(repo.clone()))?;
    let listener = TcpListener::bind(addr).map_err(|source| ServerError::Bind { addr, source })?;
    let hooks = install_hooks()?;
    Ok(Listener {
        listener,
        repo,
        hooks,
    })
}

/// Serve connections until the listener is shut down.
///
/// Blocking: loops over accepted connections, handling each on its own thread by
/// spawning `git receive-pack`. A connection that fails is logged and skipped;
/// it never brings the loop down.
pub fn serve(listener: Listener) -> Result<(), ServerError> {
    let Listener {
        listener,
        repo,
        hooks,
    } = listener;
    let hooks_dir = hooks.path().to_path_buf();
    tracing::info!(repo = %repo.display(), "serving git receive-pack");
    for stream in listener.incoming() {
        match stream {
            Ok(sock) => {
                let repo = repo.clone();
                let hooks_dir = hooks_dir.clone();
                std::thread::spawn(move || {
                    if let Err(error) = handle_connection(sock, &repo, &hooks_dir) {
                        tracing::warn!(%error, "connection handler failed");
                    }
                });
            }
            Err(error) => tracing::warn!(%error, "accept failed"),
        }
    }
    // `hooks` is intentionally held until here so the hook files outlive every
    // connection; `incoming()` only ends if the listener itself errors.
    drop(hooks);
    Ok(())
}

/// Run the long-running listener that accepts sync requests.
///
/// CLI entry point: binds `addr` (localhost only, ADR-0006), then serves until
/// shut down. The blocking accept loop runs on a dedicated thread so it does not
/// occupy the async executor.
pub async fn listen(addr: SocketAddr, repo: PathBuf) -> Result<(), ServerError> {
    tokio::task::spawn_blocking(move || serve(bind(addr, repo)?))
        .await
        .map_err(|e| ServerError::Join(e.to_string()))?
}

/// Handle one connection: spawn `git receive-pack` with the socket as its
/// stdin/stdout, confining writes to the namespace and disabling autogc.
fn handle_connection(sock: TcpStream, repo: &Path, hooks_dir: &Path) -> Result<(), ServerError> {
    let out = sock.try_clone().map_err(ServerError::Io)?;
    let hooks_path = format!("core.hooksPath={}", hooks_dir.display());

    let mut child = Command::new("git")
        .arg("-c")
        .arg("receive.autogc=false")
        .arg("-c")
        .arg(&hooks_path)
        .arg("receive-pack")
        .arg(repo)
        .stdin(Stdio::from(OwnedFd::from(sock)))
        .stdout(Stdio::from(OwnedFd::from(out)))
        .stderr(Stdio::piped())
        .spawn()
        .map_err(ServerError::Spawn)?;

    // Drain receive-pack's stderr (progress, hook rejections) to the log.
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stderr);
    }
    let status = child.wait().map_err(ServerError::Io)?;
    let stderr = stderr.trim();

    // A non-zero exit (e.g. the namespace hook declining a push) is the
    // per-connection outcome, not a server fault: log it and keep serving.
    if status.success() {
        if !stderr.is_empty() {
            tracing::debug!(%stderr, "receive-pack finished");
        }
    } else {
        tracing::warn!(?status, %stderr, "receive-pack exited non-zero");
    }
    Ok(())
}

/// Materialise the namespace-confining `pre-receive` hook into a fresh
/// gfs-managed directory, returned as a [`TempDir`] the caller keeps alive.
///
/// Pointing `git receive-pack` at this directory via `core.hooksPath` (rather
/// than writing into the target repo's own `hooks/`) keeps the repository's
/// hooks untouched.
fn install_hooks() -> Result<TempDir, ServerError> {
    let dir = tempfile::tempdir().map_err(ServerError::InstallHook)?;
    let hook = dir.path().join("pre-receive");
    std::fs::write(&hook, pre_receive_hook()).map_err(ServerError::InstallHook)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
            .map_err(ServerError::InstallHook)?;
    }
    Ok(dir)
}

/// The `pre-receive` hook body: reject any updated ref outside the
/// [`gfs_common::REF_NAMESPACE`] namespace (ADR-0005).
fn pre_receive_hook() -> String {
    let ns = gfs_common::REF_NAMESPACE;
    format!(
        "#!/bin/sh\n\
         while read -r old new ref; do\n\
         \tcase \"$ref\" in\n\
         \t{ns}*) ;;\n\
         \t*) echo \"git-full-send: refusing ref outside {ns}: $ref\" >&2; exit 1 ;;\n\
         \tesac\n\
         done\n",
    )
}

/// Check the synced state out into the configured worktree.
///
/// An authoritative, destructive overwrite of the remote worktree (ADR-0008),
/// invoked independently of [`listen`]. Not implemented yet.
pub async fn update_worktree() -> Result<(), ServerError> {
    todo!("check the synced state out into the worktree — see ADR-0003/0008")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_guards_the_shared_namespace_constant() {
        let hook = pre_receive_hook();
        assert!(hook.contains(gfs_common::REF_NAMESPACE));
        assert!(hook.starts_with("#!/bin/sh"));
    }
}
