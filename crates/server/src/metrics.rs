//! The server's per-operation metrics records (issue #42).
//!
//! Shapes the JSON Lines records the server writes to its metrics sink
//! (`gfs_common::metrics`): one `receive` record per `git receive-pack`
//! connection and one `update_worktree` record per checkout. Writing is
//! best-effort — failures are logged, never propagated (ADR-0013).

use std::path::Path;

use serde::Serialize;

/// One received `git receive-pack` connection's metrics record.
#[derive(Debug, Serialize)]
pub(crate) struct ReceiveRecord {
    kind: &'static str,
    ts_unix_ms: u64,
    tool_version: &'static str,
    /// Wall time from spawning `receive-pack` to its exit, in milliseconds.
    pub duration_ms: f64,
    /// Whether `receive-pack` exited zero.
    pub success: bool,
    /// Its exit code, if it exited via a code (vs. a signal).
    pub exit_code: Option<i32>,
    /// Bytes read off the socket into `receive-pack` (the inbound pack).
    pub bytes_in: u64,
    /// Bytes written from `receive-pack` back to the socket (the report-status).
    pub bytes_out: u64,
    /// The refs accepted by the namespace hook for this push.
    pub refs_updated: Vec<String>,
}

impl ReceiveRecord {
    pub(crate) fn new(
        duration_ms: f64,
        success: bool,
        exit_code: Option<i32>,
        bytes_in: u64,
        bytes_out: u64,
        refs_updated: Vec<String>,
    ) -> Self {
        Self {
            kind: "receive",
            ts_unix_ms: gfs_common::metrics::now_unix_millis(),
            tool_version: gfs_common::metrics::tool_version(),
            duration_ms,
            success,
            exit_code,
            bytes_in,
            bytes_out,
            refs_updated,
        }
    }
}

/// One `update_worktree` checkout's metrics record.
#[derive(Debug, Serialize)]
pub(crate) struct UpdateWorktreeRecord {
    kind: &'static str,
    ts_unix_ms: u64,
    tool_version: &'static str,
    pub stream: String,
    pub worktree: String,
    /// Total wall time for the checkout, in milliseconds.
    pub total_ms: f64,
    /// Resolving the `code`/`extra` trees and building the combined tree.
    pub resolve_ms: f64,
    /// The `git read-tree --reset -u` step.
    pub read_tree_ms: f64,
    /// The `git clean -fdx` step.
    pub clean_ms: f64,
    /// The combined tree that was checked out.
    pub tree: String,
}

impl UpdateWorktreeRecord {
    pub(crate) fn new(
        stream: String,
        worktree: String,
        total_ms: f64,
        resolve_ms: f64,
        read_tree_ms: f64,
        clean_ms: f64,
        tree: String,
    ) -> Self {
        Self {
            kind: "update_worktree",
            ts_unix_ms: gfs_common::metrics::now_unix_millis(),
            tool_version: gfs_common::metrics::tool_version(),
            stream,
            worktree,
            total_ms,
            resolve_ms,
            read_tree_ms,
            clean_ms,
            tree,
        }
    }
}

/// Best-effort: write a record to the repo's sink under `git_dir`.
pub(crate) fn record(git_dir: &Path, record: &impl Serialize) {
    gfs_common::metrics::record(git_dir, record);
}
