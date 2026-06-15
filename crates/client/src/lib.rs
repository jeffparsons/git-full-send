//! Client-side library for `git-full-send`.
//!
//! The client synthesises the developer's full sync state — committed history,
//! working-tree changes, and the force-included gitignored files — into Git
//! objects and pushes them to the server, **without** touching the user's
//! branch, index, or working tree (see ADR-0003 and ADR-0004).
//!
//! It implements three building blocks: [`encode`]ing the code commit (committed
//! history plus working-tree changes), [`select`]ing and [`encode_extra`]-ing the
//! force-included gitignored files into the `extra` commit, and [`push`]ing both
//! to the server's `git receive-pack` over a localhost connection.

use std::path::PathBuf;
use std::time::Instant;

use thiserror::Error;

pub mod encode;
mod metrics;
mod push;
pub mod select;
mod stream;

/// Milliseconds elapsed since `start`, for a metrics phase timing.
fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

pub use encode::{
    CodeLayerStats, EncodeError, EncodeOutcome, ExtraLayerStats, ExtraOutcome, encode, encode_extra,
};
pub use gfs_common::{StreamId, StreamIdError};
pub use push::{DeltaPolicy, PushError, push_ref, push_refs};
pub use select::{SelectError, select_extra_paths, select_extra_paths_with};
pub use stream::StreamResolveError;

/// Wall-clock per-phase timings for one `sync`, in milliseconds.
///
/// The two encode phases and the two pushes are timed separately because each
/// chain rides its own receive-pack exchange with its own delta policy
/// (ADR-0005); `retain_ms` is the (small) cost of pinning each pushed tip as the
/// next delta base. These feed both the durable metrics record (issue #42) and
/// the operator-facing [`SyncSummary`] (issue #53).
#[derive(Debug, Clone, Copy)]
pub struct SyncTimings {
    pub total_ms: f64,
    pub code_encode_ms: f64,
    pub extra_encode_ms: f64,
    pub code_push_ms: f64,
    pub extra_push_ms: f64,
    pub retain_ms: f64,
}

/// Operator-facing summary of one completed `sync` (issue #53).
///
/// Carries the counts, sizes, and per-phase timings a `sync` already computes for
/// its metrics record (issue #42), returned so the caller can present them however
/// it likes. The durable JSONL record (ADR-0013) is still written independently
/// inside [`sync`]; this is the live summary surface.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SyncSummary {
    /// The stream the state was synced under.
    pub stream: StreamId,
    /// The server endpoint pushed to (`HOST:PORT`).
    pub remote: String,
    /// Code-layer sizes: the index→worktree delta (overlaid/removed).
    pub code: CodeLayerStats,
    /// Extra-layer sizes: the full force-include set.
    pub extra: ExtraLayerStats,
    /// Per-phase wall-clock timings.
    pub timings: SyncTimings,
}

/// Errors returned by client operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientError {
    /// An underlying protocol error.
    #[error(transparent)]
    Protocol(#[from] gfs_common::ProtocolError),
    /// Resolving (or persisting) the stream id failed.
    #[error(transparent)]
    Stream(#[from] StreamResolveError),
    /// Encoding the local sync state failed.
    #[error(transparent)]
    Encode(#[from] EncodeError),
    /// Transferring the encoded state to the server failed.
    #[error(transparent)]
    Push(#[from] PushError),
}

/// Synthesise the current sync state and push it to the server under a stream.
///
/// Synthesises two commits: the `code` commit (committed history plus
/// working-tree changes) and the `extra` commit (the force-included,
/// normally-gitignored files — ADR-0007), each under its per-stream ref
/// (ADR-0004). Each chain is transferred to the server's `git receive-pack` in its
/// own `git push` exchange so it can carry its own delta policy (ADR-0005): `code`
/// rides `--thin` deltas, `extra` gets a predictable whole-object send. The pushed
/// tips are retained locally as the next delta bases. Refs are namespaced per
/// stream so concurrent senders don't clobber each other (ADR-0012).
///
/// `repo_dir` locates the repository (typically the current directory);
/// `remote` is the server endpoint (`HOST:PORT`, typically a tunnelled localhost
/// port). `stream` selects the stream: `Some` uses that id, `None` falls back to
/// the repo's configured `git-full-send.stream-id`, generating and persisting
/// one on first use. `user_include` overrides the per-user force-include pattern
/// file (the `--user-include` flag); `None` resolves it from the environment
/// (`GIT_FULL_SEND_USER_INCLUDE` / `$XDG_CONFIG_HOME` / `$HOME`) as usual.
pub async fn sync(
    repo_dir: PathBuf,
    remote: String,
    stream: Option<StreamId>,
    user_include: Option<PathBuf>,
) -> Result<SyncSummary, ClientError> {
    let stream = stream::resolve_stream(&repo_dir, stream)?;

    // Time each phase for the per-sync metrics record (issue #42, ADR-0013).
    let t_total = Instant::now();

    let t = Instant::now();
    let code = encode(&repo_dir, &stream)?;
    let code_encode_ms = elapsed_ms(t);
    tracing::info!(commit = %code.commit, stream = %stream, ref_ = %code.code_ref, "encoded code state");

    let t = Instant::now();
    let extra = encode_extra(&repo_dir, &stream, user_include.as_deref())?;
    let extra_encode_ms = elapsed_ms(t);
    tracing::info!(commit = %extra.commit, stream = %stream, ref_ = %extra.extra_ref, "encoded extra state");

    // Push each chain in its own receive-pack exchange so it can carry its own
    // delta policy (ADR-0005): `code` rides `--thin` deltas; `extra` gets a
    // predictable whole-object send. Retain each chain's tip right after its own
    // push succeeds, so a `code` success survives a later `extra` failure.
    let t = Instant::now();
    push::push_refs(&repo_dir, &remote, &[&code.code_ref], DeltaPolicy::Thin).await?;
    let code_push_ms = elapsed_ms(t);
    let t = Instant::now();
    push::retain_pushed_tip(&repo_dir, &gfs_common::sent_ref(&stream), code.commit)?;
    let mut retain_ms = elapsed_ms(t);

    let t = Instant::now();
    push::push_refs(
        &repo_dir,
        &remote,
        &[&extra.extra_ref],
        DeltaPolicy::WholeObject,
    )
    .await?;
    let extra_push_ms = elapsed_ms(t);
    let t = Instant::now();
    push::retain_pushed_tip(
        &repo_dir,
        &gfs_common::sent_extra_ref(&stream),
        extra.commit,
    )?;
    retain_ms += elapsed_ms(t);

    let total_ms = elapsed_ms(t_total);
    let timings = SyncTimings {
        total_ms,
        code_encode_ms,
        extra_encode_ms,
        code_push_ms,
        extra_push_ms,
        retain_ms,
    };

    // Best-effort: record the sync's timings and per-layer sizes (ADR-0013). The
    // human-readable summary is the caller's job (issue #53): we return the same
    // counts/sizes/timings as a `SyncSummary` rather than logging a summary line.
    metrics::record_sync(&repo_dir, &stream, &remote, &code, &extra, timings);

    Ok(SyncSummary {
        stream,
        remote,
        code: code.stats,
        extra: extra.stats,
        timings,
    })
}
