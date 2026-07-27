//! Writing the per-sync record to the client's durable sink (issue #42).
//!
//! The record *shape* is [`crate::SyncSummary`] — since ADR-0017 the same value
//! `sync` returns and `sync --json` prints, rather than a private twin of it. All
//! that is left here is locating the client repo's sink and appending to it,
//! best-effort.

use std::path::Path;

/// Append the completed sync's record to the client repo's sink, best-effort.
///
/// Resolves the git dir from `repo_dir`; a discovery failure is logged and
/// swallowed rather than failing the (already successful) sync — metrics are
/// observability only (ADR-0013).
pub(crate) fn record_sync(repo_dir: &Path, summary: &crate::SyncSummary) {
    let git_dir = match gix::discover(repo_dir) {
        Ok(repo) => repo.git_dir().to_path_buf(),
        Err(error) => {
            tracing::warn!(%error, "could not locate git dir for metrics");
            return;
        }
    };
    gfs_common::metrics::record(&git_dir, summary);
}
