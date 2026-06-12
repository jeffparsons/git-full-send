//! Server-side library for `git-full-send`.
//!
//! The server runs on the remote workstation. It has two independent operations
//! (see ADR-0003): a long-running [`listen`] loop that receives transferred
//! objects, and an on-demand [`update_worktree`] that checks the synced state
//! out into the configured worktree.
//!
//! This crate is currently a stub: the entry points exist with their async,
//! typed-error shapes, but no logic is implemented yet.

use thiserror::Error;

/// Errors returned by server operations.
///
/// Placeholder enum; concrete variants are added as the operations below are
/// implemented.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerError {
    /// An underlying protocol error.
    #[error(transparent)]
    Protocol(#[from] gfs_common::ProtocolError),
}

/// Run the long-running listener that accepts sync requests.
///
/// Binds localhost only (ADR-0006) and spawns a `git receive-pack` per
/// connection (ADR-0005), running until explicitly shut down. Not implemented
/// yet.
pub async fn listen() -> Result<(), ServerError> {
    todo!("accept connections and serve git receive-pack — see ADR-0003/0005")
}

/// Check the synced state out into the configured worktree.
///
/// An authoritative, destructive overwrite of the remote worktree (ADR-0008),
/// invoked independently of [`listen`]. Not implemented yet.
pub async fn update_worktree() -> Result<(), ServerError> {
    todo!("check the synced state out into the worktree — see ADR-0003/0008")
}
