//! Client-side library for `git-full-send`.
//!
//! The client synthesises the developer's full sync state — committed history,
//! working-tree changes, and the force-included gitignored files — into Git
//! objects and pushes them to the server, **without** touching the user's
//! branch, index, or working tree (see ADR-0003 and ADR-0004).
//!
//! This crate is currently a stub: the entry point exists with its async,
//! typed-error shape, but no logic is implemented yet.

use thiserror::Error;

/// Errors returned by client operations.
///
/// Placeholder enum; concrete variants are added as [`sync`] is implemented.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientError {
    /// An underlying protocol error.
    #[error(transparent)]
    Protocol(#[from] gfs_common::ProtocolError),
}

/// Synthesise the current sync state and push it to the server.
///
/// Synthesises committed history, working-tree changes, and force-included
/// files into Git objects (ADR-0004) and transfers them via `git push`
/// (ADR-0005). Not implemented yet.
pub async fn sync() -> Result<(), ClientError> {
    todo!("synthesise the sync state and push it to the server — see ADR-0004/0005")
}
