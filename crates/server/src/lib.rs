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
    /// The repository has no `code` ref to check out — nothing has been synced
    /// yet.
    #[error(
        "no `{}` to check out; nothing has been synced yet",
        gfs_common::CODE_REF
    )]
    MissingCodeRef,
    /// Creating the worktree directory (or its sidecar index directory) failed.
    #[error("could not create the worktree directory")]
    CreateWorktree(#[source] std::io::Error),
    /// Running a `git` step of the worktree update failed to spawn.
    #[error("could not run `git {step}`")]
    RunGit {
        /// The worktree-update step (e.g. `read-tree`, `clean`).
        step: &'static str,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A `git` step of the worktree update exited non-zero.
    #[error("`git {step}` failed during worktree update: {stderr}")]
    Worktree {
        /// The worktree-update step (e.g. `read-tree`, `clean`).
        step: &'static str,
        /// The trimmed stderr from the failed `git` invocation.
        stderr: String,
    },
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

/// Check the synced `code` state out into the configured worktree.
///
/// An authoritative, destructive overwrite of the remote worktree (ADR-0008),
/// invoked independently of [`listen`] (a build orchestrator triggers it). After
/// it returns, `worktree` matches the synced [`gfs_common::CODE_REF`] tree
/// *exactly*: remote-side edits are stomped (even to files whose blob is
/// unchanged between syncs), files dropped between syncs are removed, and
/// untracked remote additions are removed.
///
/// The blocking `git` work runs on a dedicated thread so it does not occupy the
/// async executor (mirroring [`listen`]).
pub async fn update_worktree(repo: PathBuf, worktree: PathBuf) -> Result<(), ServerError> {
    tokio::task::spawn_blocking(move || update_worktree_blocking(&repo, &worktree))
        .await
        .map_err(|e| ServerError::Join(e.to_string()))?
}

/// The blocking body of [`update_worktree`].
///
/// Reassembles the worktree with the persistent-index pipeline of ADR-0011:
/// resolve the `code` tree, then `read-tree --reset -u` (reset index + worktree
/// to the tree, discarding remote-local edits and removing dropped files) and
/// `clean -fdx` (prune untracked leftovers), keyed on a per-worktree index so
/// Git's stat cache keeps the work proportional to the sync delta.
fn update_worktree_blocking(repo: &Path, worktree: &Path) -> Result<(), ServerError> {
    let discovered = gix::discover(repo).map_err(|_| ServerError::NotARepo(repo.to_path_buf()))?;
    let git_dir = discovered.git_dir().to_path_buf();

    // Resolve the `code` tree first, so a never-synced repo fails cleanly before
    // any worktree mutation.
    let tree = resolve_code_tree(&git_dir)?;

    // The worktree, and the per-worktree index that records what we last checked
    // out (kept under the git dir, never inside the worktree itself — `clean`
    // would delete it there). A missing/stale index is pure cache: the next
    // `--reset` simply has no stat shortcut and does a one-time full rewrite.
    std::fs::create_dir_all(worktree).map_err(ServerError::CreateWorktree)?;
    let index = worktree_index_path(&git_dir, worktree)?;

    run_git_step(
        "read-tree",
        &git_dir,
        worktree,
        &index,
        &["read-tree", "--reset", "-u", &tree],
    )?;
    run_git_step(
        "clean",
        &git_dir,
        worktree,
        &index,
        &["clean", "-d", "-f", "-x"],
    )?;
    Ok(())
}

/// Resolve [`gfs_common::CODE_REF`] to its tree id, or [`ServerError::MissingCodeRef`].
///
/// `rev-parse --verify --quiet` exits non-zero with empty output when the ref is
/// absent, which we map to the dedicated error rather than a confusing
/// downstream `read-tree` failure.
fn resolve_code_tree(git_dir: &Path) -> Result<String, ServerError> {
    let spec = format!("{}^{{tree}}", gfs_common::CODE_REF);
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(&spec)
        .output()
        .map_err(|source| ServerError::RunGit {
            step: "rev-parse",
            source,
        })?;
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || tree.is_empty() {
        return Err(ServerError::MissingCodeRef);
    }
    Ok(tree)
}

/// The path of the persistent index for `worktree`, under the git dir.
///
/// Keyed by the canonical worktree path so distinct worktrees of one repo get
/// distinct indexes. The parent directory is created.
fn worktree_index_path(git_dir: &Path, worktree: &Path) -> Result<PathBuf, ServerError> {
    use std::hash::{Hash, Hasher};

    // Canonicalise so the same worktree maps to the same index across runs
    // regardless of how the path was spelled (the dir exists by now).
    let canonical = worktree
        .canonicalize()
        .map_err(ServerError::CreateWorktree)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    let key = format!("{:016x}", hasher.finish());

    let dir = git_dir.join("git-full-send").join("worktrees").join(key);
    std::fs::create_dir_all(&dir).map_err(ServerError::CreateWorktree)?;
    Ok(dir.join("index"))
}

/// Run one `git` step of the worktree update with the per-worktree index, mapping
/// a spawn failure and a non-zero exit to the matching [`ServerError`].
fn run_git_step(
    step: &'static str,
    git_dir: &Path,
    worktree: &Path,
    index: &Path,
    args: &[&str],
) -> Result<(), ServerError> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .arg("--work-tree")
        .arg(worktree)
        .env("GIT_INDEX_FILE", index)
        .args(args)
        .output()
        .map_err(|source| ServerError::RunGit { step, source })?;
    if !output.status.success() {
        return Err(ServerError::Worktree {
            step,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
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
