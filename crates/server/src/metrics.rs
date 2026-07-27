//! The server's per-operation metrics records (issue #42).
//!
//! Shapes the JSON Lines records the server writes to its metrics sink
//! (`gfs_common::metrics`): one `receive` record per `git receive-pack`
//! connection and one `update_worktree` record per checkout. Writing is
//! best-effort — failures are logged, never propagated (ADR-0013).
//!
//! The `update_worktree` record is *also* the value [`crate::update_worktree`]
//! returns and `update-worktree --json` prints (ADR-0017), so it lives in the
//! public API rather than here; this module keeps the receive-side shape and the
//! shared best-effort write.

use std::path::Path;

use serde::Serialize;

/// One received `git receive-pack` connection's metrics record.
#[derive(Debug, Serialize)]
pub(crate) struct ReceiveRecord {
    #[serde(flatten)]
    envelope: gfs_common::metrics::Envelope,
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
            envelope: gfs_common::metrics::Envelope::new("receive"),
            duration_ms,
            success,
            exit_code,
            bytes_in,
            bytes_out,
            refs_updated,
        }
    }
}

/// Best-effort: write a record to the repo's sink under `git_dir`.
pub(crate) fn record(git_dir: &Path, record: &impl Serialize) {
    gfs_common::metrics::record(git_dir, record);
}
