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
pub use push::{PushError, push_ref, push_refs};
pub use select::{SelectError, select_extra_paths, select_extra_paths_with};
pub use stream::StreamResolveError;

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
/// (ADR-0004). Both refs are transferred to the server's `git receive-pack` in a
/// single `git push --thin` exchange (ADR-0005), and the pushed tips are retained
/// locally as the next delta bases. Refs are namespaced per stream so concurrent
/// senders don't clobber each other (ADR-0012).
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
) -> Result<(), ClientError> {
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

    // Push both refs in one receive-pack exchange (ADR-0004/0005).
    let t = Instant::now();
    push::push_refs(&repo_dir, &remote, &[&code.code_ref, &extra.extra_ref])?;
    let push_ms = elapsed_ms(t);

    // Retain both delta bases only after the push succeeds.
    let t = Instant::now();
    push::retain_pushed_tip(&repo_dir, &gfs_common::sent_ref(&stream), code.commit)?;
    push::retain_pushed_tip(
        &repo_dir,
        &gfs_common::sent_extra_ref(&stream),
        extra.commit,
    )?;
    let retain_ms = elapsed_ms(t);

    let total_ms = elapsed_ms(t_total);
    tracing::info!(
        stream = %stream, %remote, total_ms,
        code_files = code.stats.files_overlaid, code_bytes = code.stats.bytes_overlaid,
        extra_files = extra.stats.files, extra_bytes = extra.stats.bytes,
        "pushed code and extra state to server"
    );

    // Best-effort: record the sync's timings and per-layer sizes (ADR-0013).
    metrics::record_sync(
        &repo_dir,
        &stream,
        &remote,
        &code,
        &extra,
        metrics::Timings {
            total_ms,
            code_encode_ms,
            extra_encode_ms,
            push_ms,
            retain_ms,
        },
    );
    Ok(())
}
