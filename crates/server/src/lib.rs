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
//! The accept loop is async (tokio) and bounded (issue #47): a
//! [`Semaphore`]-gated cap means at most `max_connections` handlers run at once,
//! so a burst can't exhaust threads — further connections wait for a slot. Each
//! accepted socket is served by the blocking [`handle_connection`] on a
//! `spawn_blocking` thread, with a per-connection wall-clock timeout that aborts
//! a stuck client rather than letting it pin a slot. A SIGTERM/SIGINT stops the
//! accept loop, drains the in-flight handlers, and returns cleanly so the
//! `hooks` [`TempDir`] is dropped.
//!
//! The socket-as-stdio plumbing is Unix-only; the tool is Unix-first (see
//! [ADR-0006] and the client's `encode`). Windows transport support is out of
//! scope.
//!
//! [ADR-0006]: https://github.com/jeffparsons/git-full-send/blob/main/docs/adr/0006-transport-and-connectivity.md

use std::future::Future;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use thiserror::Error;
use tokio::sync::Semaphore;

pub mod doctor;
mod metrics;

pub use doctor::{Check, DoctorReport, doctor};

/// Environment variable naming the file the `pre-receive` hook appends accepted
/// ref names to, so [`handle_connection`] can record which refs a push updated
/// (issue #42). Set per connection to a unique path.
const ACCEPTED_REFS_ENV: &str = "GFS_ACCEPTED_REFS_FILE";

/// Milliseconds elapsed since `start`, for a metrics timing field.
fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

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
    /// The repository has no `code` ref for the requested stream to check out —
    /// nothing has been synced for it yet.
    #[error("no `{ref_name}` to check out; nothing has been synced for this stream yet")]
    MissingCodeRef {
        /// The `code` ref we looked for.
        ref_name: String,
    },
    /// Resolving the stream's `extra` tree (when present) failed.
    #[error("could not resolve the `extra` tree")]
    ResolveExtra(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Building the combined `code`+`extra` tree to check out failed.
    #[error("could not build the combined worktree tree")]
    BuildTree(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// An explicitly requested stream id was malformed.
    #[error(transparent)]
    StreamId(#[from] gfs_common::StreamIdError),
    /// Listing the synced streams failed.
    #[error("could not list streams")]
    ListStreams(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Deleting a stream's refs failed.
    #[error("could not forget stream")]
    ForgetStream(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Reaping stale streams failed (enumerating, or reading a `code` commit).
    #[error("could not reap streams")]
    Reap(#[source] Box<dyn std::error::Error + Send + Sync>),
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
    /// Another `update-worktree` run already holds this worktree's lock and we
    /// were asked to fail fast (the default — see [`LockMode`]).
    #[error("another update is already in progress for worktree `{worktree}`")]
    WorktreeBusy {
        /// The worktree whose lock was contended.
        worktree: PathBuf,
    },
    /// We waited for the worktree lock (`--wait --timeout`) but the holder did
    /// not release it before the deadline.
    #[error("timed out after {timeout:?} waiting for the lock on worktree `{worktree}`")]
    LockTimeout {
        /// The worktree whose lock we waited for.
        worktree: PathBuf,
        /// How long we waited before giving up.
        timeout: Duration,
    },
    /// Opening or locking the per-worktree lock file failed for a reason other
    /// than contention.
    #[error("could not acquire the worktree lock")]
    Lock(#[source] std::io::Error),
    /// The blocking serve task panicked or was cancelled.
    #[error("serve task failed: {0}")]
    Join(String),
}

/// A bound, ready-to-serve listener.
///
/// Produced by [`bind`] and consumed by [`serve_async`]. Splitting bind from
/// serve lets a caller (notably tests) bind an ephemeral port, read it back via
/// [`Listener::local_addr`], and serve it as a task on its own runtime.
#[derive(Debug)]
pub struct Listener {
    listener: TcpListener,
    repo: PathBuf,
    /// The repository's git dir, resolved once at bind time, where the metrics
    /// sink lives (`<git-dir>/git-full-send/metrics.jsonl` — issue #42).
    git_dir: PathBuf,
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

/// Tunables for the [`listen`]/[`serve_async`] accept loop (issue #47).
///
/// [`Default`] sources the `gfs_common::DEFAULT_*` constants; the CLI overrides
/// them from `listen --max-connections` / `--connection-timeout`.
#[derive(Debug, Clone, Copy)]
pub struct ListenConfig {
    /// Maximum number of `git receive-pack` handlers in flight at once. Further
    /// accepted connections wait for a slot.
    pub max_connections: usize,
    /// Per-connection wall-clock budget; a handler that overruns it is aborted
    /// (its socket is shut down) so a stuck client can't pin a slot.
    pub connection_timeout: Duration,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            max_connections: gfs_common::DEFAULT_MAX_CONNECTIONS,
            connection_timeout: Duration::from_secs(gfs_common::DEFAULT_CONNECTION_TIMEOUT_SECS),
        }
    }
}

/// Bind a localhost TCP listener for `addr` that will serve `repo`.
///
/// Validates that `repo` is a Git repository and materialises the namespace
/// `pre-receive` hook, but does not yet accept connections — call [`serve_async`]
/// (or [`listen`], which binds and serves with signal-driven shutdown).
pub fn bind(addr: SocketAddr, repo: PathBuf) -> Result<Listener, ServerError> {
    let git_dir = gix::discover(&repo)
        .map_err(|_| ServerError::NotARepo(repo.clone()))?
        .git_dir()
        .to_path_buf();
    let listener = TcpListener::bind(addr).map_err(|source| ServerError::Bind { addr, source })?;
    let hooks = install_hooks()?;
    // Say the cheap, expensive-to-ignore things once per process, unprompted: the
    // operator who most needs them is the one who did not think to run `doctor`
    // (ADR-0018).
    doctor::log_startup_checks(&repo);
    Ok(Listener {
        listener,
        repo,
        git_dir,
        hooks,
    })
}

/// Serve connections asynchronously until `shutdown` resolves, then drain.
///
/// Bounds concurrency to `config.max_connections` via a [`Semaphore`] (a burst
/// can't exhaust threads — excess connections wait for a slot) and serves each
/// accepted socket with the blocking [`handle_connection`] on a `spawn_blocking`
/// thread, under a per-connection wall-clock timeout. When `shutdown` resolves,
/// the accept loop stops, in-flight handlers are drained, and the `hooks`
/// [`TempDir`] is dropped — the clean stop the old unbounded loop never reached.
pub async fn serve_async(
    listener: Listener,
    config: ListenConfig,
    shutdown: impl Future<Output = ()>,
) -> Result<(), ServerError> {
    let Listener {
        listener,
        repo,
        git_dir,
        hooks,
    } = listener;
    let hooks_dir = hooks.path().to_path_buf();

    // Re-home the std listener onto the tokio reactor (it must be non-blocking
    // for `from_std`). `bind` keeps producing a std listener so `local_addr`
    // works without an ambient runtime.
    listener.set_nonblocking(true).map_err(ServerError::Io)?;
    let listener = tokio::net::TcpListener::from_std(listener).map_err(ServerError::Io)?;

    tracing::info!(
        repo = %repo.display(),
        max_connections = config.max_connections,
        "serving git receive-pack",
    );

    let timeout = config.connection_timeout;
    accept_loop(listener, config.max_connections, shutdown, move |sock| {
        let repo = repo.clone();
        let git_dir = git_dir.clone();
        let hooks_dir = hooks_dir.clone();
        async move {
            // Hand the socket to the blocking handler as a plain blocking std
            // socket (`into_std` leaves it non-blocking, which the byte-pump
            // loop in `handle_connection` does not expect).
            let sock = match sock.into_std().and_then(|s| {
                s.set_nonblocking(false)?;
                Ok(s)
            }) {
                Ok(sock) => sock,
                Err(error) => {
                    tracing::warn!(%error, "could not prepare accepted socket");
                    return;
                }
            };
            match tokio::task::spawn_blocking(move || {
                handle_connection(sock, &repo, &git_dir, &hooks_dir, timeout)
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "connection handler failed"),
                Err(error) => tracing::warn!(%error, "connection task panicked"),
            }
        }
    })
    .await;

    // The accept loop has drained every in-flight handler, so the hook files are
    // no longer needed by any connection: drop the dir. Reaching this on
    // shutdown is the point of issue #47 — the old loop only ended on a listener
    // error, leaving this unreachable.
    drop(hooks);
    Ok(())
}

/// The bounded, drained accept loop shared by [`serve_async`].
///
/// Accepts at most `max_connections` connections concurrently: a permit is taken
/// before each accept, and released when the spawned handler finishes. When
/// `shutdown` resolves the loop stops accepting and awaits the in-flight
/// handlers before returning. Factored out (and generic over `handle`) so a unit
/// test can drive it with a stub handler and assert the cap holds.
async fn accept_loop<F, Fut>(
    listener: tokio::net::TcpListener,
    max_connections: usize,
    shutdown: impl Future<Output = ()>,
    mut handle: F,
) where
    F: FnMut(tokio::net::TcpStream) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let sem = Arc::new(Semaphore::new(max_connections));
    let mut handlers = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        // Acquire a slot first, but inside the `select!` so a shutdown at the cap
        // is still observed promptly rather than parking on the permit.
        let permit = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            permit = sem.clone().acquire_owned() => permit.expect("semaphore is never closed"),
        };
        // With a slot in hand, wait for a connection — still racing shutdown.
        let sock = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((sock, _)) => sock,
                Err(error) => {
                    tracing::warn!(%error, "accept failed");
                    continue;
                }
            },
        };
        let fut = handle(sock);
        handlers.spawn(async move {
            fut.await;
            drop(permit);
        });
    }

    // Drain: let every in-flight handler run to completion before returning.
    while handlers.join_next().await.is_some() {}
}

/// Run the long-running listener that accepts sync requests.
///
/// CLI entry point: binds `addr` (localhost only, ADR-0006), then serves with
/// `config`'s concurrency cap and per-connection timeout until a SIGTERM/SIGINT
/// triggers a graceful, draining shutdown.
pub async fn listen(
    addr: SocketAddr,
    repo: PathBuf,
    config: ListenConfig,
) -> Result<(), ServerError> {
    let listener = bind(addr, repo)?;
    serve_async(listener, config, shutdown_signal()).await
}

/// Resolve when the process receives SIGTERM or SIGINT — the graceful-shutdown
/// trigger for [`listen`].
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM; draining and shutting down"),
        _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT; draining and shutting down"),
    }
}

/// Handle one connection: spawn `git receive-pack` and pump the raw
/// receive-pack stream between the socket and the child, confining writes to the
/// namespace and disabling autogc.
///
/// Unlike the obvious "hand the socket straight to the child as its stdin/stdout"
/// wiring, we copy the bytes through two threads so we can *count* them for the
/// metrics record (issue #42): the bytes seen are exactly the same raw stream
/// (no framing — ADR-0005 is unchanged), just observed in transit. The added
/// localhost-bandwidth copy is negligible against git's own pack work.
///
/// A `timeout` watchdog bounds the whole exchange (issue #47): if the handler
/// runs longer than `timeout`, the watchdog shuts the socket down, which forces
/// the pumps to EOF and makes `git receive-pack` exit — so a stuck client can't
/// pin a concurrency slot. A blocking `spawn_blocking` task can't be cancelled
/// from the outside, so the budget is enforced here, where we own the socket.
fn handle_connection(
    sock: TcpStream,
    repo: &Path,
    git_dir: &Path,
    hooks_dir: &Path,
    timeout: Duration,
) -> Result<(), ServerError> {
    use std::io::Read;

    let hooks_path = format!("core.hooksPath={}", hooks_dir.display());

    // A unique per-connection file the hook appends accepted ref names to.
    // Best-effort: if it can't be created we simply record no refs.
    let accepted_refs = tempfile::NamedTempFile::new().ok();

    let started = Instant::now();
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("receive.autogc=false")
        .arg("-c")
        .arg(&hooks_path)
        .arg("receive-pack")
        .arg(repo)
        // Pipe receive-pack's stdio so the two pump threads below can *count* the
        // bytes in each direction for the metrics record (issue #42). The pumps
        // use an explicit read/write loop ([`pump_counting`]) rather than
        // `std::io::copy`: on Linux the latter takes a `splice`/`sendfile`
        // zero-copy fast path between the socket and the pipe that deadlocked the
        // bidirectional receive-pack exchange (issue #44) — a plain byte loop
        // both avoids it and yields the count for free.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(file) = &accepted_refs {
        command.env(ACCEPTED_REFS_ENV, file.path());
    }
    let mut child = command.spawn().map_err(ServerError::Spawn)?;

    let mut child_stdin = child.stdin.take().expect("piped stdin");
    let mut child_stdout = child.stdout.take().expect("piped stdout");

    // Two clones drive the pumps; a third stays here so we can shut the socket
    // down after `receive-pack` exits, sending the client its FIN and unblocking
    // the inbound pump's blocking read.
    let mut sock_in = sock.try_clone().map_err(ServerError::Io)?;
    let mut sock_out = sock.try_clone().map_err(ServerError::Io)?;

    // Per-connection timeout watchdog (issue #47): a fourth clone for a thread
    // that, if the handler hasn't finished within `timeout`, shuts the socket
    // down. That unblocks both pumps and gives `receive-pack` EOF on stdin / a
    // broken stdout, so it exits and `child.wait()` returns — the same teardown
    // the normal path performs, just triggered early. The handler signals the
    // watchdog to stand down (by dropping `done_tx`) before it returns.
    let watchdog_sock = sock.try_clone().map_err(ServerError::Io)?;
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        use std::sync::mpsc::RecvTimeoutError;
        match done_rx.recv_timeout(timeout) {
            // Handler finished (sender dropped) within the budget: nothing to do.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    timeout_secs = timeout.as_secs(),
                    "connection exceeded its timeout; aborting"
                );
                let _ = watchdog_sock.shutdown(Shutdown::Both);
            }
        }
    });

    // Inbound: socket → child stdin (the ref-update commands, then the pushed
    // pack). A broken-pipe error once the child has exited is expected, not a
    // failure; we keep the byte counts.
    let in_pump = std::thread::spawn(move || {
        let n = pump_counting(&mut sock_in, &mut child_stdin);
        drop(child_stdin); // close the child's stdin
        n
    });
    // Outbound: child stdout → socket (the ref advertisement, then the
    // report-status).
    let out_pump = std::thread::spawn(move || pump_counting(&mut child_stdout, &mut sock_out));

    // Drain receive-pack's stderr (progress, hook rejections) to the log.
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let status = child.wait().map_err(ServerError::Io)?;

    // The child has exited, so its stdout has hit EOF; join the outbound pump to
    // be sure the whole report-status reached the socket, then shut the socket
    // down to send the client its FIN and unblock the inbound pump.
    let bytes_out = out_pump.join().unwrap_or_default();
    let _ = sock.shutdown(Shutdown::Both);
    let bytes_in = in_pump.join().unwrap_or_default();

    // Stand the watchdog down (dropping the sender wakes its `recv_timeout`) and
    // join it, so a fired-or-not watchdog never outlives the connection.
    drop(done_tx);
    let _ = watchdog.join();

    let stderr = stderr.trim();
    let refs_updated = accepted_refs.map(read_accepted_refs).unwrap_or_default();

    // What this connection *was* — not merely whether the child exited zero
    // (ADR-0018). A prober that connects and hangs up leaves `receive-pack` dead
    // of SIGPIPE with nothing pushed; that is a healthy liveness check, and
    // reporting it as a failed push misleads whoever is reading the log for a
    // real problem.
    let outcome = classify(&status, &bytes_in, &refs_updated, stderr);
    match outcome {
        Outcome::Updated => tracing::info!(
            outcome = outcome.as_str(),
            refs = refs_updated.len(),
            pack_bytes = bytes_in.post_flush,
            advertisement_bytes = bytes_out.pre_flush,
            refs_advertised = bytes_out.pre_flush_pkts,
            duration_ms = elapsed_ms(started),
            "received git push",
        ),
        // Nothing was pushed, so nothing failed. Visible on request, silent by
        // default: an orchestrator may probe once or twice per invocation.
        Outcome::NoOp | Outcome::Probe => tracing::debug!(
            outcome = outcome.as_str(),
            advertisement_bytes = bytes_out.pre_flush,
            refs_advertised = bytes_out.pre_flush_pkts,
            duration_ms = elapsed_ms(started),
            "connection carried no ref updates",
        ),
        // The two cases a human should actually look at.
        Outcome::Rejected | Outcome::Failed => tracing::warn!(
            outcome = outcome.as_str(),
            ?status,
            %stderr,
            refs = refs_updated.len(),
            duration_ms = elapsed_ms(started),
            "receive-pack did not complete a push",
        ),
    }

    // Best-effort metrics record (ADR-0013), even on a failed receive.
    metrics::record(
        git_dir,
        &metrics::ReceiveRecord::new(
            elapsed_ms(started),
            outcome.as_str(),
            &status,
            bytes_in,
            bytes_out,
            refs_updated,
        ),
    );
    Ok(())
}

/// What a `git receive-pack` connection turned out to be (ADR-0018).
///
/// Derived from what the exchange *contained*, not only from the exit status:
/// "were any ref updates even attempted" is the question that separates a broken
/// push from a liveness check, and it also gives `rejected` and `no_op` honest
/// names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Refs were pushed and accepted.
    Updated,
    /// A complete, well-formed exchange that updated nothing — what
    /// `git-full-send probe` sends.
    NoOp,
    /// No ref updates arrived and the exchange ended badly: a prober that hung
    /// up mid-advertisement, leaving `receive-pack` to die of SIGPIPE.
    Probe,
    /// The namespace hook declined a ref (ADR-0005).
    Rejected,
    /// A genuine failure.
    Failed,
}

impl Outcome {
    /// The stable string used in the record and the log.
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Updated => "updated",
            Outcome::NoOp => "no_op",
            Outcome::Probe => "probe",
            Outcome::Rejected => "rejected",
            Outcome::Failed => "failed",
        }
    }
}

/// Classify a finished connection. See [`Outcome`].
///
/// Note that a hook rejection does **not** make `receive-pack` exit non-zero:
/// the refusal travels in the report-status, so the child exits 0 with no refs
/// accepted. Classifying on the exit status alone would call that a success.
fn classify(
    status: &std::process::ExitStatus,
    inbound: &gfs_common::pktline::WireCounts,
    refs_updated: &[String],
    stderr: &str,
) -> Outcome {
    if stderr.contains(HOOK_REFUSAL_MARKER) {
        return Outcome::Rejected;
    }
    if status.success() {
        return match (refs_updated.is_empty(), inbound.pre_flush_pkts) {
            (false, _) => Outcome::Updated,
            // Ref updates were asked for and none were accepted.
            (true, 1..) => Outcome::Rejected,
            // Nothing was asked for: a complete, empty conversation.
            (true, 0) => Outcome::NoOp,
        };
    }
    // Nothing was being pushed, so nothing was broken by failing to push it.
    if inbound.pre_flush_pkts == 0 {
        return Outcome::Probe;
    }
    Outcome::Failed
}

/// The shared counting byte pump ([`gfs_common::pktline::pump_splitting`]),
/// which also splits each direction into protocol overhead and payload.
use gfs_common::pktline::pump_splitting as pump_counting;

/// Read the hook's accepted-ref file into a deduplicated, order-preserving list
/// of ref names (one per line). A missing/unreadable file yields no refs.
fn read_accepted_refs(file: tempfile::NamedTempFile) -> Vec<String> {
    let mut refs = Vec::new();
    if let Ok(contents) = std::fs::read_to_string(file.path()) {
        for line in contents.lines() {
            let line = line.trim();
            if !line.is_empty() && !refs.iter().any(|r| r == line) {
                refs.push(line.to_string());
            }
        }
    }
    refs
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

/// The text the `pre-receive` hook prints when it declines a ref, used to tell a
/// policy rejection apart from a genuine failure when classifying a connection
/// (ADR-0018). Shared by the hook body and [`classify`] so the two cannot drift.
const HOOK_REFUSAL_MARKER: &str = "git-full-send: refusing ref outside";

/// The `pre-receive` hook body: reject any updated ref outside the
/// [`gfs_common::REF_NAMESPACE`] namespace (ADR-0005), and append each accepted
/// ref to the file named by [`ACCEPTED_REFS_ENV`] so the connection handler can
/// record which refs the push updated (issue #42).
fn pre_receive_hook() -> String {
    let ns = gfs_common::REF_NAMESPACE;
    let refs_env = ACCEPTED_REFS_ENV;
    let refusal = HOOK_REFUSAL_MARKER;
    format!(
        "#!/bin/sh\n\
         while read -r old new ref; do\n\
         \tcase \"$ref\" in\n\
         \t{ns}*)\n\
         \t\t[ -n \"${refs_env}\" ] && printf '%s\\n' \"$ref\" >> \"${refs_env}\" ;;\n\
         \t*) echo \"{refusal} {ns}: $ref\" >&2; exit 1 ;;\n\
         \tesac\n\
         done\n",
    )
}

/// How [`update_worktree`] reacts when another run already holds the
/// per-worktree lock (issue #49).
///
/// The per-worktree advisory lock serialises the destructive `read-tree` →
/// `clean` sequence so two concurrent updates of the *same* worktree cannot
/// interleave. Distinct worktrees take distinct locks and never contend.
#[derive(Debug, Clone, Copy, Default)]
pub enum LockMode {
    /// Fail immediately with [`ServerError::WorktreeBusy`] if the worktree is
    /// already being updated. The default — the caller (e.g. a build
    /// orchestrator) decides whether to retry.
    #[default]
    FailFast,
    /// Wait for the lock. `None` blocks until the holder finishes; `Some(d)`
    /// polls until `d` elapses, then fails with [`ServerError::LockTimeout`].
    Wait {
        /// Optional wait deadline; `None` waits indefinitely.
        timeout: Option<Duration>,
    },
}

/// The record of one completed `update-worktree` — the single value written to
/// the server repo's durable sink, returned to the caller, and printed by
/// `update-worktree --json` (ADR-0017).
///
/// The `--json` form is how a *client* orchestrating a remote checkout over SSH
/// gets the server's numbers back: before ADR-0017 they only landed in a file on
/// the server.
///
/// Every duration here is accompanied by the size of the work it did, because a
/// duration alone is not actionable: a 4-second `read_tree_ms` means one thing
/// when [`ChangedPaths`] says thousands of files were written and quite another
/// when it says none were.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct UpdateWorktreeReport {
    /// `kind`/`schema`/`ts_unix_ms`/`tool_version`, flattened into the record.
    #[serde(flatten)]
    pub envelope: gfs_common::metrics::Envelope,
    /// The stream that was checked out.
    pub stream: gfs_common::StreamId,
    /// The worktree directory it was checked out into.
    pub worktree: String,
    /// The combined `code`+`extra` tree that was checked out.
    pub tree: String,
    /// Total wall time for the checkout, in milliseconds.
    pub total_ms: f64,
    /// Resolving the `code`/`extra` trees and building the combined tree.
    pub resolve_ms: f64,
    /// What this checkout cost us to *measure*, in milliseconds — the
    /// tree-vs-index diff, and the worktree walk under `measure_worktree`.
    ///
    /// Recorded rather than hidden: instrumentation that quietly inflates the
    /// thing it measures is worse than none (ADR-0017).
    pub measure_ms: f64,
    /// The `git read-tree --reset -u` step.
    pub read_tree_ms: f64,
    /// The `git clean -fd` step.
    pub clean_ms: f64,
    /// The state of the per-worktree index this checkout ran against.
    pub index: IndexState,
    /// How much of the worktree actually had to change.
    pub changed: ChangedPaths,
    /// `read-tree`'s internals, harvested from git's own instrumentation.
    /// `None` when trace2 gave us nothing (see [`gfs_common::trace2`]).
    pub read_tree: Option<ReadTreeBreakdown>,
    /// What the `clean` sweep removed.
    pub clean: CleanStats,
    /// Files in the worktree afterwards. Only counted under `measure_worktree`
    /// (a full filesystem walk), so `None` by default.
    pub worktree_files: Option<usize>,
}

/// The per-worktree index as this checkout found it (ADR-0011).
///
/// A cold index makes a slow checkout *expected* — there is no stat cache, so
/// `read-tree` rewrites the whole worktree. A **warm** index taking seconds is
/// the interesting case, and telling the two apart used to require guessing.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct IndexState {
    /// `"warm"` if `read-tree` loaded an existing index, `"cold"` if it built one
    /// from scratch, `"unknown"` if git's instrumentation didn't say.
    pub state: &'static str,
    /// Size of the index file before the checkout, in bytes; `None` if absent.
    pub bytes: Option<u64>,
    /// Entries in the index after the checkout.
    pub entries: Option<i64>,
}

/// How far the worktree was from the tree being checked out.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct ChangedPaths {
    /// Whether this is the very tree the worktree last checked out — in which
    /// case the tree side of the work is *definitionally* zero, however long
    /// `read_tree_ms` turns out to be.
    pub tree_unchanged: bool,
    /// Paths differing between the target tree and the index, counted without a
    /// single `lstat` — the work `read-tree` must do from the tree side.
    pub vs_index: Option<PathDelta>,
    /// Paths differing between the target tree and what is on *disk*. Costs an
    /// `lstat` per index entry, so only measured under `measure_worktree`.
    pub vs_worktree: Option<PathDelta>,
}

/// A count of paths a checkout would have to change.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
#[non_exhaustive]
pub struct PathDelta {
    /// Paths that must be created or rewritten.
    pub to_write: usize,
    /// Paths that must be removed.
    pub to_remove: usize,
}

impl PathDelta {
    /// Whether nothing at all differs.
    pub fn is_empty(&self) -> bool {
        self.to_write == 0 && self.to_remove == 0
    }
}

/// `read-tree`'s internal phases, harvested from git's trace2 stream (ADR-0017).
///
/// Three very different problems hide inside one number: loading a large index,
/// walking the tree, and writing files. Each is missing individually if git's
/// instrumentation didn't report it.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct ReadTreeBreakdown {
    /// Reading the per-worktree index in (`index:do_read_index`).
    pub load_index_ms: Option<f64>,
    /// Walking the tree being checked out (`unpack_trees:traverse_trees`).
    pub resolve_tree_ms: Option<f64>,
    /// Applying it to the index and the worktree — the outer
    /// `unpack_trees:unpack_trees` region less the traversal inside it. This is
    /// where writing files (and stat-ing the ones that don't need writing) lands.
    pub apply_ms: Option<f64>,
    /// Writing the index back out (`index:do_write_index`).
    pub write_index_ms: Option<f64>,
}

/// What the post-checkout `clean -fd` sweep removed.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
#[non_exhaustive]
pub struct CleanStats {
    /// Untracked non-ignored entries removed (ADR-0016).
    pub removed: usize,
}

/// Tunables for one [`update_worktree`] run.
///
/// Grouped into a struct rather than added as parameters so that the next knob
/// (there will be one) does not change the signature again. Deliberately *not*
/// `#[non_exhaustive]` — unlike the report types, this is an input a caller has
/// to be able to build, and it mirrors [`ListenConfig`].
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateOptions {
    /// What to do when another run already holds this worktree's lock.
    pub lock: LockMode,
    /// Also measure the worktree itself: how many paths differ from what is on
    /// disk, and how many files the worktree holds.
    ///
    /// Off by default because it is the one genuinely expensive measurement here
    /// — an `lstat` per index entry plus a full filesystem walk, both
    /// proportional to the tree rather than to the change (ADR-0017). Everything
    /// else on the record is cheap enough to always pay for.
    pub measure_worktree: bool,
}

impl UpdateOptions {
    /// Options with the given lock mode and no extra measurement — the common
    /// case, and what a caller that only cares about contention wants.
    pub fn with_lock(lock: LockMode) -> Self {
        Self {
            lock,
            measure_worktree: false,
        }
    }
}

impl From<LockMode> for UpdateOptions {
    fn from(lock: LockMode) -> Self {
        Self::with_lock(lock)
    }
}

/// Check a stream's synced `code` state out into the given worktree.
///
/// An authoritative, destructive overwrite of the remote worktree (ADR-0008),
/// invoked independently of [`listen`] (a build orchestrator triggers it). After
/// it returns, `worktree` matches `stream`'s synced `code` tree
/// (`gfs_common::code_ref`) *exactly*: remote-side edits are stomped (even to
/// files whose blob is unchanged between syncs), files dropped between syncs are
/// removed, and untracked remote additions are removed.
///
/// `stream` and `worktree` are independent (ADR-0012): the caller decides which
/// stream lands in which worktree — a dedicated worktree per stream, several
/// streams taking turns in one, etc. — and this imposes no 1:1 mapping.
///
/// Concurrent updates of the same worktree are serialised by a per-worktree
/// advisory lock; `mode` controls whether a contended run fails fast (default)
/// or waits (issue #49).
///
/// The blocking `git` work runs on a dedicated thread so it does not occupy the
/// async executor (mirroring [`listen`]).
pub async fn update_worktree(
    repo: PathBuf,
    worktree: PathBuf,
    stream: gfs_common::StreamId,
    options: impl Into<UpdateOptions>,
) -> Result<UpdateWorktreeReport, ServerError> {
    let options = options.into();
    tokio::task::spawn_blocking(move || {
        update_worktree_blocking(&repo, &worktree, &stream, options)
    })
    .await
    .map_err(|e| ServerError::Join(e.to_string()))?
}

/// List the streams that have a synced `code` ref in `repo`.
///
/// Enumerates refs under [`gfs_common::STREAMS_PREFIX`], recovering each
/// (possibly slash-containing) stream id from the `…/streams/<id>/code` refs. An
/// orchestrator uses this to discover which streams are available to check out.
pub fn list_streams(repo: &Path) -> Result<Vec<gfs_common::StreamId>, ServerError> {
    let repo = gix::discover(repo).map_err(|_| ServerError::NotARepo(repo.to_path_buf()))?;
    let platform = repo
        .references()
        .map_err(|e| ServerError::ListStreams(Box::new(e)))?;
    let iter = platform
        .prefixed(gfs_common::STREAMS_PREFIX)
        .map_err(|e| ServerError::ListStreams(Box::new(e)))?;

    let mut streams = Vec::new();
    for reference in iter {
        let reference = reference.map_err(ServerError::ListStreams)?;
        let name = reference.name().as_bstr().to_string();
        if let Some(stream) = stream_id_from_code_ref(&name) {
            streams.push(stream);
        }
    }
    Ok(streams)
}

/// Recover a stream id from a `refs/git-full-send/streams/<id>/code` ref name,
/// or `None` if `name` isn't a stream's `code` ref.
///
/// Shared by [`list_streams`] and [`reap_streams`] so the layout recovery lives
/// in one place. The companion `…/extra` ref (and anything else under the
/// prefix) lacks the `/code` suffix and is left out.
fn stream_id_from_code_ref(name: &str) -> Option<gfs_common::StreamId> {
    let rest = name.strip_prefix(gfs_common::STREAMS_PREFIX)?;
    let id = rest.strip_suffix("/code")?;
    gfs_common::StreamId::new(id).ok()
}

/// Delete every ref of `stream` from `repo`, returning how many were removed.
///
/// Removes everything under [`gfs_common::stream_prefix`] in one transaction —
/// the explicit "forget this stream" path ADR-0012 deferred (issue #48). The
/// command is **symmetric**: run against the server repo it drops the stream's
/// `code`/`extra`; run against the client repo it drops the local `sent/*`
/// delta-base pins. After it returns the stream no longer appears in
/// [`list_streams`].
///
/// Idempotent: a stream with no refs (never synced, or already forgotten) yields
/// `Ok(0)` rather than an error. Streams and worktrees are orthogonal (ADR-0012),
/// so the per-worktree index dir is deliberately *not* touched — it is keyed by
/// worktree path, not stream id, and the worktree is disposable anyway
/// (ADR-0008). The client's `git-full-send.stream-id` config key is likewise left
/// alone (see `docs/operating.md`).
pub fn forget_stream(repo: &Path, stream: &gfs_common::StreamId) -> Result<usize, ServerError> {
    use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};

    let repo = gix::discover(repo).map_err(|_| ServerError::NotARepo(repo.to_path_buf()))?;
    let prefix = gfs_common::stream_prefix(stream);

    // Snapshot the matching ref names into owned edits before mutating, so we are
    // not deleting out from under a live iterator borrow.
    let platform = repo
        .references()
        .map_err(|e| ServerError::ForgetStream(Box::new(e)))?;
    let iter = platform
        .prefixed(prefix.as_str())
        .map_err(|e| ServerError::ForgetStream(Box::new(e)))?;
    let mut edits = Vec::new();
    for reference in iter {
        let reference = reference.map_err(ServerError::ForgetStream)?;
        edits.push(RefEdit {
            change: Change::Delete {
                // Unconditional: we are forgetting the stream, not guarding
                // against a concurrent update (mirrors `retain_pushed_tip`).
                expected: PreviousValue::Any,
                log: RefLog::AndReference,
            },
            name: reference.name().to_owned(),
            deref: false,
        });
    }
    if edits.is_empty() {
        return Ok(0);
    }
    let applied = repo
        .edit_references(edits)
        .map_err(|e| ServerError::ForgetStream(Box::new(e)))?;
    Ok(applied.len())
}

/// One stream considered by [`reap_streams`].
#[derive(Debug, Clone)]
pub struct ReapedStream {
    /// The stale stream.
    pub stream: gfs_common::StreamId,
    /// The `code` commit's committer time, in Unix seconds, that made it stale.
    pub committed_unix_secs: i64,
    /// Refs removed forgetting it — `0` under `dry_run`, where the count is the
    /// number that *would* be removed.
    pub refs_removed: usize,
}

/// The outcome of a [`reap_streams`] pass.
#[derive(Debug)]
pub struct ReapOutcome {
    /// Streams scanned — every stream with a `code` ref.
    pub scanned: usize,
    /// Streams found stale (and, unless `dry_run`, forgotten).
    pub reaped: Vec<ReapedStream>,
    /// Whether this was a dry run (nothing was deleted).
    pub dry_run: bool,
}

/// Forget every stream in `repo` whose `code` commit is older than the cutoff.
///
/// TTL-based reaping (issue #63, ADR-0015): the complement to the manual
/// `forget-stream` ([`forget_stream`], ADR-0014). A stream's age is the committer
/// date of its `code` commit, which the client re-stamps to "now" on every sync
/// (ADR-0009 / the client's `encode`), so it tracks "last synced" without any
/// sidecar marker. A stream is stale when that committer time is **strictly
/// older** than `cutoff_unix_secs`; the caller picks the cutoff (typically
/// `now - max_age`), keeping this a pure function of `(repo, cutoff)`.
///
/// Reaping is exactly "list the stale streams, then [`forget_stream`] each", so
/// it inherits that path's guarantees: idempotent, and safe on a live stream (a
/// later `sync` re-creates the refs). With `dry_run` nothing is deleted — the
/// returned [`ReapOutcome`] reports which streams *would* be reaped. Server-only:
/// the client's `sent/*` pins are left to the manual `forget-stream` (ADR-0015).
pub fn reap_streams(
    repo: &Path,
    cutoff_unix_secs: i64,
    dry_run: bool,
) -> Result<ReapOutcome, ServerError> {
    let discovered = gix::discover(repo).map_err(|_| ServerError::NotARepo(repo.to_path_buf()))?;

    // First pass: snapshot the stale streams (and the committer time that made
    // each stale) without mutating refs out from under the live iterator.
    let mut scanned = 0usize;
    let mut stale: Vec<(gfs_common::StreamId, i64)> = Vec::new();
    {
        let platform = discovered
            .references()
            .map_err(|e| ServerError::Reap(Box::new(e)))?;
        let iter = platform
            .prefixed(gfs_common::STREAMS_PREFIX)
            .map_err(|e| ServerError::Reap(Box::new(e)))?;
        for reference in iter {
            let mut reference = reference.map_err(ServerError::Reap)?;
            let name = reference.name().as_bstr().to_string();
            let Some(stream) = stream_id_from_code_ref(&name) else {
                continue;
            };
            scanned += 1;
            let id = reference
                .peel_to_id()
                .map_err(|e| ServerError::Reap(Box::new(e)))?
                .detach();
            let committed = discovered
                .find_commit(id)
                .map_err(|e| ServerError::Reap(Box::new(e)))?
                .time()
                .map_err(|e| ServerError::Reap(Box::new(e)))?
                .seconds;
            if committed < cutoff_unix_secs {
                stale.push((stream, committed));
            }
        }
    }

    // Second pass: forget each stale stream (or, in a dry run, count the refs it
    // would shed). `forget_stream` re-discovers the repo and edits refs in one
    // transaction per stream.
    let mut reaped = Vec::with_capacity(stale.len());
    for (stream, committed_unix_secs) in stale {
        let refs_removed = if dry_run {
            count_stream_refs(&discovered, &stream)?
        } else {
            forget_stream(repo, &stream)?
        };
        reaped.push(ReapedStream {
            stream,
            committed_unix_secs,
            refs_removed,
        });
    }

    Ok(ReapOutcome {
        scanned,
        reaped,
        dry_run,
    })
}

/// Count the refs `stream` holds in `repo` without deleting any — the dry-run
/// equivalent of the count [`forget_stream`] returns.
fn count_stream_refs(
    repo: &gix::Repository,
    stream: &gfs_common::StreamId,
) -> Result<usize, ServerError> {
    let prefix = gfs_common::stream_prefix(stream);
    let platform = repo
        .references()
        .map_err(|e| ServerError::Reap(Box::new(e)))?;
    let iter = platform
        .prefixed(prefix.as_str())
        .map_err(|e| ServerError::Reap(Box::new(e)))?;
    let mut count = 0;
    for reference in iter {
        reference.map_err(ServerError::Reap)?;
        count += 1;
    }
    Ok(count)
}

/// The blocking body of [`update_worktree`].
///
/// Reassembles the worktree with the persistent-index pipeline of ADR-0011 (as
/// amended by ADR-0016). First resolve the `code` tree and overlay the stream's
/// `extra` tree (force-included, normally-gitignored files — ADR-0007) onto it
/// at identity paths, producing a single **combined** tree; then `read-tree
/// --reset -u` (reset index + worktree to that tree, discarding remote-local
/// edits and removing files dropped since the last sync) and `clean -fd` (prune
/// untracked non-ignored leftovers), keyed on a per-worktree index so Git's stat
/// cache keeps the work proportional to the sync delta.
///
/// Folding `extra` into the checked-out tree makes the volatile force-include set
/// fall out of the same machinery as `code`: dropped `extra` files were in the
/// prior combined index and are removed by `--reset -u`, while surviving `extra`
/// files are index-tracked so `clean` never considers them.
///
/// `clean` deliberately runs without `-x` (ADR-0016): read-tree already manages
/// the whole delivered set, `extra` included, so `clean`'s only job is sweeping
/// untracked non-ignored cruft. Gitignored files gfs didn't deliver (a co-hosted
/// dev environment's `.env`, caches, build state) belong to the user and are
/// left alone.
fn update_worktree_blocking(
    repo: &Path,
    worktree: &Path,
    stream: &gfs_common::StreamId,
    options: UpdateOptions,
) -> Result<UpdateWorktreeReport, ServerError> {
    let discovered = gix::discover(repo).map_err(|_| ServerError::NotARepo(repo.to_path_buf()))?;
    let git_dir = discovered.git_dir().to_path_buf();

    // Time each phase for the per-checkout metrics record (issue #42, ADR-0013).
    let t_total = Instant::now();

    // Resolve the stream's `code` tree first, so a never-synced stream fails
    // cleanly before any worktree mutation (or lock). Then overlay `extra`
    // (absent ⇒ nothing to overlay) onto it at identity paths to get the tree to
    // check out.
    let t = Instant::now();
    let code_tree = resolve_code_tree(&git_dir, stream)?;
    let extra_tree = resolve_extra_tree(&git_dir, stream)?;
    let tree = overlay_extra_onto_code(&discovered, &code_tree, extra_tree.as_deref())?;
    let resolve_ms = elapsed_ms(t);

    // The worktree, and the per-worktree index that records what we last checked
    // out (kept under the git dir, never inside the worktree itself — `clean`
    // would delete it there). A missing/stale index is pure cache: the next
    // `--reset` simply has no stat shortcut and does a one-time full rewrite.
    std::fs::create_dir_all(worktree).map_err(ServerError::CreateWorktree)?;
    let index = worktree_index_path(&git_dir, worktree)?;

    // Serialise against concurrent updates of *this* worktree (issue #49): hold
    // the per-worktree advisory lock across the destructive `read-tree` →
    // `clean` window. `_lock` releases on drop at the end of this function (or on
    // process exit, since the OS owns the `flock`). The read-only tree
    // resolution above deliberately runs unlocked.
    let state_dir = worktree_state_dir(&git_dir, worktree)?;
    let lock_path = state_dir.join("lock");
    let _lock = acquire_worktree_lock(&lock_path, worktree, options.lock)?;

    // Everything from here to the checkout is measurement, and it is timed as
    // such: instrumentation that inflates the number it explains, invisibly, is
    // worse than none (ADR-0017).
    let t = Instant::now();
    // The index as we found it. Its *warmth* comes from git itself below; what we
    // can see from out here is whether the file exists and how big it is.
    let index_bytes = std::fs::metadata(&index).ok().map(|m| m.len());
    // The strongest no-op signal, and free: the tree the previous successful
    // checkout wrote here. Equal ids mean the tree side of the work is zero,
    // whatever `read_tree_ms` turns out to be.
    let last_tree_path = state_dir.join("last-tree");
    let tree_unchanged = std::fs::read_to_string(&last_tree_path)
        .map(|last| last.trim() == tree)
        .unwrap_or(false);
    // What `read-tree` will have to change, counted without touching the disk.
    let vs_index = count_changed_paths(&git_dir, worktree, &index, &tree, Compare::Index);
    // The expensive pair, opt-in: an `lstat` per index entry, and a full walk.
    let (vs_worktree, worktree_files) = if options.measure_worktree {
        (
            count_changed_paths(&git_dir, worktree, &index, &tree, Compare::Worktree),
            count_worktree_files(worktree),
        )
    } else {
        (None, None)
    };
    let measure_ms = elapsed_ms(t);

    let t = Instant::now();
    let trace = run_git_step(
        "read-tree",
        &git_dir,
        worktree,
        &index,
        &["read-tree", "--reset", "-u", &tree],
    )?;
    let read_tree_ms = elapsed_ms(t);

    let t = Instant::now();
    let clean_output = run_git_step("clean", &git_dir, worktree, &index, &["clean", "-d", "-f"])?;
    let clean_ms = elapsed_ms(t);
    let clean = CleanStats {
        // `clean` prints one `Removing <path>` line per entry it deletes.
        removed: clean_output
            .stdout
            .lines()
            .filter(|l| l.starts_with("Removing "))
            .count(),
    };

    // Remember what we just checked out, so the *next* run can tell a no-op from
    // real work for free. Best-effort, like every other measurement here.
    if let Err(error) = std::fs::write(&last_tree_path, &tree) {
        tracing::debug!(%error, "could not record the checked-out tree id");
    }

    let index = index_state(index_bytes, trace.as_ref());
    let read_tree = trace.as_ref().and_then(read_tree_breakdown);

    let total_ms = elapsed_ms(t_total);
    tracing::info!(
        stream = %stream, worktree = %worktree.display(),
        total_ms, resolve_ms, measure_ms, read_tree_ms, clean_ms,
        index_state = index.state,
        index_entries = index.entries,
        tree_unchanged,
        paths_to_write = vs_index.map(|d| d.to_write),
        paths_to_remove = vs_index.map(|d| d.to_remove),
        "updated worktree"
    );

    // One value, three surfaces (ADR-0017): the durable sink, the caller's human
    // summary, and `update-worktree --json`.
    let report = UpdateWorktreeReport {
        envelope: gfs_common::metrics::Envelope::new("update_worktree"),
        stream: stream.clone(),
        worktree: worktree.display().to_string(),
        tree,
        total_ms,
        resolve_ms,
        measure_ms,
        read_tree_ms,
        clean_ms,
        index,
        changed: ChangedPaths {
            tree_unchanged,
            vs_index,
            vs_worktree,
        },
        read_tree,
        clean,
        worktree_files,
    };

    // Best-effort metrics record (ADR-0013).
    metrics::record(&git_dir, &report);
    Ok(report)
}

/// Resolve `stream`'s `code` ref (`gfs_common::code_ref`) to its tree id, or
/// [`ServerError::MissingCodeRef`].
///
/// `rev-parse --verify --quiet` exits non-zero with empty output when the ref is
/// absent, which we map to the dedicated error rather than a confusing
/// downstream `read-tree` failure.
fn resolve_code_tree(git_dir: &Path, stream: &gfs_common::StreamId) -> Result<String, ServerError> {
    let code_ref = gfs_common::code_ref(stream);
    let spec = format!("{code_ref}^{{tree}}");
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
        return Err(ServerError::MissingCodeRef { ref_name: code_ref });
    }
    Ok(tree)
}

/// Resolve `stream`'s `extra` ref (`gfs_common::extra_ref`) to its tree id, or
/// `None` when the ref is absent.
///
/// Unlike the `code` ref, a missing `extra` ref is **not** an error: it means
/// this stream has never carried force-included files (the client always pushes
/// `extra` alongside `code`, but we stay robust if it hasn't), so there is simply
/// nothing to overlay. `rev-parse --verify --quiet` exits non-zero with empty
/// output when the ref is absent.
fn resolve_extra_tree(
    git_dir: &Path,
    stream: &gfs_common::StreamId,
) -> Result<Option<String>, ServerError> {
    let extra_ref = gfs_common::extra_ref(stream);
    let spec = format!("{extra_ref}^{{tree}}");
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
        return Ok(None);
    }
    Ok(Some(tree))
}

/// Overlay the `extra` tree onto the `code` tree at identity paths and return the
/// id of the resulting combined tree.
///
/// Seeds a gix tree `Editor` with `code` and upserts every `extra` leaf (blob,
/// executable, or symlink) at its full repo-relative path, so `extra` wins on any
/// path collision (ADR-0007's same-path overlay). With no `extra` tree there is
/// nothing to overlay and the `code` tree id is returned unchanged. The combined
/// tree's objects are written to the repo's object database so the subsequent
/// `git read-tree` (a separate process) can resolve it.
fn overlay_extra_onto_code(
    repo: &gix::Repository,
    code_tree: &str,
    extra_tree: Option<&str>,
) -> Result<String, ServerError> {
    let Some(extra_tree) = extra_tree else {
        return Ok(code_tree.to_string());
    };

    let code_id = gix::ObjectId::from_hex(code_tree.as_bytes())
        .map_err(|e| ServerError::BuildTree(Box::new(e)))?;
    let extra_id = gix::ObjectId::from_hex(extra_tree.as_bytes())
        .map_err(|e| ServerError::ResolveExtra(Box::new(e)))?;

    let mut editor = repo
        .edit_tree(code_id)
        .map_err(|e| ServerError::BuildTree(Box::new(e)))?;
    let extra = repo
        .find_tree(extra_id)
        .map_err(|e| ServerError::ResolveExtra(Box::new(e)))?;

    // Record every entry of the `extra` tree with its full path, then upsert the
    // leaves onto the `code`-seeded editor (the editor recreates the intermediate
    // trees from the slash-separated paths). Tree entries are skipped: they are
    // implied by their leaves.
    let mut recorder = gix::traverse::tree::Recorder::default();
    extra
        .traverse()
        .breadthfirst(&mut recorder)
        .map_err(|e| ServerError::ResolveExtra(Box::new(e)))?;
    for entry in &recorder.records {
        if entry.mode.is_tree() {
            continue;
        }
        editor
            .upsert(
                gix::bstr::BStr::new(&entry.filepath),
                entry.mode.kind(),
                entry.oid,
            )
            .map_err(|e| ServerError::BuildTree(Box::new(e)))?;
    }

    let combined = editor
        .write()
        .map_err(|e| ServerError::BuildTree(Box::new(e)))?
        .detach();
    Ok(combined.to_string())
}

/// The per-worktree state directory under the git dir, created if absent.
///
/// Keyed by the canonical worktree path so distinct worktrees of one repo get
/// distinct directories (and thus distinct indexes and locks); the same worktree
/// maps to the same directory across runs regardless of how the path was spelled.
/// Everything in here lives under the git dir, never inside the worktree itself —
/// `clean -fd` would otherwise delete it.
fn worktree_state_dir(git_dir: &Path, worktree: &Path) -> Result<PathBuf, ServerError> {
    use std::hash::{Hash, Hasher};

    // The dir exists by now (the worktree was created), so `canonicalize` works.
    let canonical = worktree
        .canonicalize()
        .map_err(ServerError::CreateWorktree)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    let key = format!("{:016x}", hasher.finish());

    let dir = git_dir.join("git-full-send").join("worktrees").join(key);
    std::fs::create_dir_all(&dir).map_err(ServerError::CreateWorktree)?;
    Ok(dir)
}

/// The path of the persistent index for `worktree`, under the git dir.
fn worktree_index_path(git_dir: &Path, worktree: &Path) -> Result<PathBuf, ServerError> {
    Ok(worktree_state_dir(git_dir, worktree)?.join("index"))
}

/// The git dir of the repository at `repo` — where its `git-full-send/` state
/// and metrics sink live (ADR-0013).
///
/// Public so a caller that needs the sink's location (the `metrics` command) can
/// resolve it without taking its own `gix` dependency.
pub fn git_dir(repo: &Path) -> Result<PathBuf, ServerError> {
    Ok(gix::discover(repo)
        .map_err(|_| ServerError::NotARepo(repo.to_path_buf()))?
        .git_dir()
        .to_path_buf())
}

/// The path of the per-worktree advisory lock file, under the git dir.
///
/// Beside the persistent `index` in the same per-worktree state directory, so it
/// inherits the same per-worktree keying and never lands inside the worktree.
/// Public so a caller (e.g. an orchestrator, or a test) can locate the lock that
/// [`update_worktree`] takes during a checkout.
pub fn worktree_lock_path(repo: &Path, worktree: &Path) -> Result<PathBuf, ServerError> {
    let git_dir = gix::discover(repo)
        .map_err(|_| ServerError::NotARepo(repo.to_path_buf()))?
        .git_dir()
        .to_path_buf();
    Ok(worktree_state_dir(&git_dir, worktree)?.join("lock"))
}

/// Acquire the per-worktree advisory lock at `lock_path` per `mode`.
///
/// Returns the open lock-file handle as an RAII guard: the OS releases the
/// `flock` when the handle is dropped (or the process exits — so a crashed
/// holder never leaves a stale lock). The caller must keep the returned `File`
/// alive for the whole critical section.
fn acquire_worktree_lock(
    lock_path: &Path,
    worktree: &Path,
    mode: LockMode,
) -> Result<std::fs::File, ServerError> {
    use std::fs::TryLockError;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(ServerError::Lock)?;

    match mode {
        LockMode::FailFast => match file.try_lock() {
            Ok(()) => Ok(file),
            Err(TryLockError::WouldBlock) => Err(ServerError::WorktreeBusy {
                worktree: worktree.to_path_buf(),
            }),
            Err(TryLockError::Error(e)) => Err(ServerError::Lock(e)),
        },
        LockMode::Wait { timeout: None } => {
            file.lock().map_err(ServerError::Lock)?;
            Ok(file)
        }
        LockMode::Wait {
            timeout: Some(timeout),
        } => {
            // std offers no timed lock, so poll `try_lock` until the deadline.
            // We are on the blocking `update_worktree` thread, so sleeping is
            // fine. The first poll is immediate; if the lock is free it returns
            // at once without sleeping.
            let deadline = Instant::now() + timeout;
            let poll = Duration::from_millis(100);
            loop {
                match file.try_lock() {
                    Ok(()) => return Ok(file),
                    Err(TryLockError::WouldBlock) => {
                        let now = Instant::now();
                        if now >= deadline {
                            return Err(ServerError::LockTimeout {
                                worktree: worktree.to_path_buf(),
                                timeout,
                            });
                        }
                        std::thread::sleep(poll.min(deadline - now));
                    }
                    Err(TryLockError::Error(e)) => return Err(ServerError::Lock(e)),
                }
            }
        }
    }
}

/// What a completed `git` step of the worktree update reported about itself.
struct GitStepOutput {
    /// The step's stdout, for the steps whose output carries a count (`clean`).
    stdout: String,
    /// git's own trace2 stream for the step, when we captured one (ADR-0017).
    trace: Option<gfs_common::trace2::Trace2>,
}

impl GitStepOutput {
    /// The harvested trace2 stream, if any.
    fn as_ref(&self) -> Option<&gfs_common::trace2::Trace2> {
        self.trace.as_ref()
    }
}

/// Run one `git` step of the worktree update with the per-worktree index, mapping
/// a spawn failure and a non-zero exit to the matching [`ServerError`].
///
/// The child runs with a trace2 capture attached, so a step that dominates a
/// phase can be decomposed from the inside afterwards (ADR-0017). Capturing is
/// best-effort: no capture simply means no sub-timings.
fn run_git_step(
    step: &'static str,
    git_dir: &Path,
    worktree: &Path,
    index: &Path,
    args: &[&str],
) -> Result<GitStepOutput, ServerError> {
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(git_dir)
        .arg("--work-tree")
        .arg(worktree)
        .env("GIT_INDEX_FILE", index)
        .args(args);
    let capture = gfs_common::trace2::Trace2Capture::new();
    if let Some(capture) = &capture {
        capture.apply(&mut command);
    }
    let output = command
        .output()
        .map_err(|source| ServerError::RunGit { step, source })?;
    if !output.status.success() {
        return Err(ServerError::Worktree {
            step,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(GitStepOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        trace: capture.and_then(|c| c.harvest()),
    })
}

/// Which side of the checkout to compare the target tree against.
#[derive(Debug, Clone, Copy)]
enum Compare {
    /// The index only — no `lstat`, so proportional to the index but with no
    /// filesystem I/O per path. Cheap enough to always measure.
    Index,
    /// The working tree — one `lstat` per index entry. Opt-in only.
    Worktree,
}

/// Count the paths a checkout of `tree` would have to write and remove.
///
/// `git diff-index` reports the *index or worktree* relative to the tree, so its
/// letters read backwards from what we want and are flipped here: a path present
/// in the tree but not the index shows as `D` and is a path to **write**; one
/// present in the index but not the tree shows as `A` and is a path to
/// **remove**.
///
/// Returns `None` if the diff could not be taken — a missing count is a missing
/// explanation, never a failed checkout (ADR-0013).
fn count_changed_paths(
    git_dir: &Path,
    worktree: &Path,
    index: &Path,
    tree: &str,
    compare: Compare,
) -> Option<PathDelta> {
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(git_dir)
        .arg("--work-tree")
        .arg(worktree)
        .env("GIT_INDEX_FILE", index)
        .args(["diff-index", "--name-status", "--no-renames"]);
    if let Compare::Index = compare {
        command.arg("--cached");
    }
    let output = command.arg(tree).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut delta = PathDelta::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        match line.as_bytes().first() {
            // In the index/worktree but not the tree: the checkout removes it.
            Some(b'A') => delta.to_remove += 1,
            // In the tree but not here, or different here: the checkout writes it.
            Some(b'D' | b'M' | b'T' | b'C' | b'R') => delta.to_write += 1,
            // `U` (unmerged) and anything unfamiliar: counted as work to do
            // rather than silently dropped.
            Some(_) => delta.to_write += 1,
            None => {}
        }
    }
    Some(delta)
}

/// Count the files in `worktree`, skipping `.git`.
///
/// A full filesystem walk — the reason [`UpdateOptions::measure_worktree`]
/// exists. `None` if the walk hit an error partway; a partial count would be
/// worse than none.
fn count_worktree_files(worktree: &Path) -> Option<usize> {
    fn walk(dir: &Path, count: &mut usize) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_name() == std::ffi::OsStr::new(".git") {
                continue;
            }
            if entry.file_type()?.is_dir() {
                walk(&entry.path(), count)?;
            } else {
                *count += 1;
            }
        }
        Ok(())
    }
    let mut count = 0;
    walk(worktree, &mut count).ok().map(|()| count)
}

/// Describe the per-worktree index this checkout ran against.
///
/// Warmth comes from git itself: `read-tree` emits an `index read/cache_nr`
/// counter only when it had an index to read, so its presence *is* the warm
/// signal. Without a trace2 stream we decline to guess.
fn index_state(bytes: Option<u64>, trace: Option<&gfs_common::trace2::Trace2>) -> IndexState {
    let read = trace.and_then(|t| t.data_i64("index", "read/cache_nr"));
    let wrote = trace.and_then(|t| t.data_i64("index", "write/cache_nr"));
    IndexState {
        state: match (trace, read) {
            (None, _) => "unknown",
            (Some(_), Some(_)) => "warm",
            (Some(_), None) => "cold",
        },
        bytes,
        entries: wrote.or(read),
    }
}

/// Split `read-tree`'s time across its internal phases (ADR-0017).
///
/// `None` when git reported none of them, so the record says "we don't know"
/// rather than "all zero".
fn read_tree_breakdown(trace: &gfs_common::trace2::Trace2) -> Option<ReadTreeBreakdown> {
    let load_index_ms = trace.region_ms("index", "do_read_index");
    let resolve_tree_ms = trace.region_ms("unpack_trees", "traverse_trees");
    let unpack_ms = trace.region_ms("unpack_trees", "unpack_trees");
    let write_index_ms = trace.region_ms("index", "do_write_index");
    // Traversal happens *inside* the unpack region, so the file-touching part is
    // the difference. Clamped at zero: the two come from separate clock reads.
    let apply_ms = match (unpack_ms, resolve_tree_ms) {
        (Some(unpack), Some(traverse)) => Some((unpack - traverse).max(0.0)),
        (Some(unpack), None) => Some(unpack),
        (None, _) => None,
    };
    if load_index_ms.is_none()
        && resolve_tree_ms.is_none()
        && apply_ms.is_none()
        && write_index_ms.is_none()
    {
        return None;
    }
    Some(ReadTreeBreakdown {
        load_index_ms,
        resolve_tree_ms,
        apply_ms,
        write_index_ms,
    })
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

    #[test]
    fn hook_records_accepted_refs_to_the_env_file() {
        let hook = pre_receive_hook();
        // Accepted refs are appended to the file named by the env var so the
        // connection handler can report which refs a push updated (issue #42).
        assert!(hook.contains(ACCEPTED_REFS_ENV));
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The bounded accept loop never runs more than `max_connections` handlers at
    /// once, even when more connections arrive than there are slots (issue #47).
    #[tokio::test]
    async fn accept_loop_respects_the_concurrency_cap() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        const MAX: usize = 2;
        const CONNS: usize = 6;
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let live_h = live.clone();
        let peak_h = peak.clone();
        let server = tokio::spawn(async move {
            accept_loop(
                listener,
                MAX,
                async {
                    let _ = shutdown_rx.await;
                },
                move |sock| {
                    let live = live_h.clone();
                    let peak = peak_h.clone();
                    async move {
                        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        // Hold the slot long enough that, were the cap not
                        // enforced, all `CONNS` handlers would overlap.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        live.fetch_sub(1, Ordering::SeqCst);
                        drop(sock);
                    }
                },
            )
            .await;
        });

        // Open more connections than slots; the loop must serialise them.
        let mut conns = Vec::new();
        for _ in 0..CONNS {
            conns.push(tokio::net::TcpStream::connect(addr).await.expect("connect"));
        }
        // Long enough for every connection to have been served in waves of MAX.
        tokio::time::sleep(Duration::from_millis(600)).await;
        shutdown_tx.send(()).expect("send shutdown");
        server.await.expect("server task");

        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(
            observed_peak <= MAX,
            "peak concurrency {observed_peak} exceeded the cap {MAX}",
        );
        assert!(observed_peak >= 1, "the loop never served a connection");
    }

    /// A shutdown signal stops the accept loop and lets `serve_async` return
    /// cleanly, dropping the hooks `TempDir` — the path the old unbounded loop
    /// never reached (issue #47).
    #[tokio::test]
    async fn serve_async_shuts_down_cleanly_and_drops_the_hooks_dir() {
        let repo = test_support::init_bare_repo();
        let listener =
            bind("127.0.0.1:0".parse().unwrap(), repo.path().to_path_buf()).expect("bind listener");
        let hooks_dir = listener.hooks.path().to_path_buf();
        assert!(hooks_dir.exists(), "hooks dir exists while serving");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            serve_async(listener, ListenConfig::default(), async {
                let _ = shutdown_rx.await;
            })
            .await
        });

        // Let the loop reach its accept point, then ask it to stop.
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown_tx.send(()).expect("send shutdown");

        let result = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("serve_async returns promptly after shutdown")
            .expect("server task");
        assert!(result.is_ok(), "serve_async returned an error: {result:?}");
        assert!(
            !hooks_dir.exists(),
            "hooks dir should be removed once serve_async drains and returns",
        );
    }
}
