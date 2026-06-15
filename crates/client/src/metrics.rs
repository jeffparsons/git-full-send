//! The per-sync metrics record (issue #42).
//!
//! Shapes the JSON Lines record `sync` writes to the client's metrics sink
//! (`gfs_common::metrics`): the phase timings and the per-layer size metadata
//! gathered during [`crate::encode`] / [`crate::encode_extra`]. Writing is
//! best-effort — see [`record_sync`].

use std::path::Path;

use serde::Serialize;

use crate::encode::{EncodeOutcome, ExtraOutcome};

/// Wall-clock phase timings for one `sync`, in milliseconds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timings {
    pub total_ms: f64,
    pub code_encode_ms: f64,
    pub extra_encode_ms: f64,
    pub code_push_ms: f64,
    pub extra_push_ms: f64,
    pub retain_ms: f64,
}

/// One `sync` operation's metrics record.
#[derive(Serialize)]
struct SyncRecord<'a> {
    kind: &'static str,
    ts_unix_ms: u64,
    tool_version: &'a str,
    stream: &'a str,
    remote: &'a str,
    total_ms: f64,
    code_encode_ms: f64,
    extra_encode_ms: f64,
    code_push_ms: f64,
    extra_push_ms: f64,
    retain_ms: f64,
    code: CodeLayer,
    extra: ExtraLayer,
}

/// The code layer's delta size plus the resulting commit/tree ids.
#[derive(Serialize)]
struct CodeLayer {
    files_overlaid: usize,
    bytes_overlaid: u64,
    files_removed: usize,
    commit: String,
    tree: String,
}

/// The full extra (force-include) set's size plus the resulting commit/tree ids.
#[derive(Serialize)]
struct ExtraLayer {
    files: usize,
    bytes: u64,
    commit: String,
    tree: String,
}

/// Write the per-sync metrics record to the client repo's sink, best-effort.
///
/// Resolves the git dir from `repo_dir`; a discovery failure is logged and
/// swallowed rather than failing the (already successful) sync — metrics are
/// observability only (ADR-0013).
pub(crate) fn record_sync(
    repo_dir: &Path,
    stream: &gfs_common::StreamId,
    remote: &str,
    code: &EncodeOutcome,
    extra: &ExtraOutcome,
    timings: Timings,
) {
    let git_dir = match gix::discover(repo_dir) {
        Ok(repo) => repo.git_dir().to_path_buf(),
        Err(error) => {
            tracing::warn!(%error, "could not locate git dir for metrics");
            return;
        }
    };

    let record = SyncRecord {
        kind: "sync",
        ts_unix_ms: gfs_common::metrics::now_unix_millis(),
        tool_version: gfs_common::metrics::tool_version(),
        stream: stream.as_str(),
        remote,
        total_ms: timings.total_ms,
        code_encode_ms: timings.code_encode_ms,
        extra_encode_ms: timings.extra_encode_ms,
        code_push_ms: timings.code_push_ms,
        extra_push_ms: timings.extra_push_ms,
        retain_ms: timings.retain_ms,
        code: CodeLayer {
            files_overlaid: code.stats.files_overlaid,
            bytes_overlaid: code.stats.bytes_overlaid,
            files_removed: code.stats.files_removed,
            commit: code.commit.to_string(),
            tree: code.tree.to_string(),
        },
        extra: ExtraLayer {
            files: extra.stats.files,
            bytes: extra.stats.bytes,
            commit: extra.commit.to_string(),
            tree: extra.tree.to_string(),
        },
    };
    gfs_common::metrics::record(&git_dir, &record);
}
