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

use serde::Serialize;
use thiserror::Error;

pub mod encode;
mod metrics;
pub mod probe;
mod push;
pub mod select;
mod stream;

/// Milliseconds elapsed since `start`, for a metrics phase timing.
fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

pub use encode::{
    CodeEncodePhases, CodeLayerStats, EncodeError, EncodeOutcome, ExtraEncodePhases,
    ExtraLayerStats, ExtraOutcome, encode, encode_extra,
};
pub use gfs_common::{StreamId, StreamIdError};
pub use probe::{ProbeError, ProbeReport, probe};
pub use push::{DeltaPolicy, PushError, PushWire, push_ref, push_refs};
pub use select::{
    SelectError, SelectStats, Selection, select_extra_paths, select_extra_paths_measured,
    select_extra_paths_with, unanchored_patterns,
};
pub use stream::StreamResolveError;

/// The record of one completed `sync` — the single value that is written to the
/// durable JSONL sink, returned to the caller, and printed by `sync --json`
/// (ADR-0017).
///
/// Before ADR-0017 this was two structs saying the same thing: a private
/// `SyncRecord` for the sink and a public `SyncSummary` for the CLI. They are one
/// now, so an integrator parsing `--json` and an operator reading the sink see
/// exactly the same numbers.
///
/// Fields are grouped per **layer** rather than flattened: each of `code` and
/// `extra` rides its own receive-pack exchange with its own delta policy
/// (ADR-0005), so each carries its own encode/push timings beside the sizes they
/// explain. `retain_ms` is the (small) cost of pinning both pushed tips as the
/// next delta bases.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct SyncSummary {
    /// `kind`/`schema`/`ts_unix_ms`/`tool_version`, flattened into the record.
    #[serde(flatten)]
    pub envelope: gfs_common::metrics::Envelope,
    /// The stream the state was synced under.
    pub stream: StreamId,
    /// The server endpoint pushed to (`HOST:PORT`).
    pub remote: String,
    /// Total wall time for the whole sync, in milliseconds.
    pub total_ms: f64,
    /// Pinning both pushed tips as the next delta bases, in milliseconds.
    pub retain_ms: f64,
    /// The `code` layer: committed history plus the working-tree delta.
    pub code: CodeLayer,
    /// The `extra` layer: the full force-included set.
    pub extra: ExtraLayer,
}

/// The `code` layer's contribution to one [`SyncSummary`].
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct CodeLayer {
    /// Synthesising the code commit, in milliseconds.
    pub encode_ms: f64,
    /// The `code` chain's own `git push` exchange, in milliseconds.
    pub push_ms: f64,
    /// Sizes of the index→worktree delta this sync encoded.
    #[serde(flatten)]
    pub stats: CodeLayerStats,
    /// What the push cost on the wire, protocol overhead separated from payload.
    pub wire: PushWire,
    /// The synthetic commit that was pushed.
    pub commit: String,
    /// The tree that commit holds.
    pub tree: String,
}

/// The `extra` layer's contribution to one [`SyncSummary`].
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct ExtraLayer {
    /// Selecting and encoding the force-included set, in milliseconds.
    pub encode_ms: f64,
    /// The `extra` chain's own `git push` exchange, in milliseconds.
    pub push_ms: f64,
    /// Sizes of the full force-included set this sync encoded.
    #[serde(flatten)]
    pub stats: ExtraLayerStats,
    /// What the push cost on the wire, protocol overhead separated from payload.
    pub wire: PushWire,
    /// The synthetic commit that was pushed.
    pub commit: String,
    /// The tree that commit holds.
    pub tree: String,
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
    let code_wire =
        push::push_refs(&repo_dir, &remote, &[&code.code_ref], DeltaPolicy::Thin).await?;
    let code_push_ms = elapsed_ms(t);
    let t = Instant::now();
    push::retain_pushed_tip(&repo_dir, &gfs_common::sent_ref(&stream), code.commit)?;
    let mut retain_ms = elapsed_ms(t);

    let t = Instant::now();
    let extra_wire = push::push_refs(
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

    // One value, three surfaces (ADR-0017): it is written to the durable sink,
    // returned to the caller for the human summary, and printed verbatim by
    // `sync --json`.
    let summary = SyncSummary {
        envelope: gfs_common::metrics::Envelope::new("sync"),
        stream,
        remote,
        total_ms: elapsed_ms(t_total),
        retain_ms,
        code: CodeLayer {
            encode_ms: code_encode_ms,
            push_ms: code_push_ms,
            stats: code.stats,
            wire: code_wire,
            commit: code.commit.to_string(),
            tree: code.tree.to_string(),
        },
        extra: ExtraLayer {
            encode_ms: extra_encode_ms,
            push_ms: extra_push_ms,
            stats: extra.stats,
            wire: extra_wire,
            commit: extra.commit.to_string(),
            tree: extra.tree.to_string(),
        },
    };

    // Best-effort (ADR-0013): a sink that can't be written never fails the sync.
    metrics::record_sync(&repo_dir, &summary);

    Ok(summary)
}
