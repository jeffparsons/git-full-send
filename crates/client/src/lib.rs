//! Client-side library for `git-full-send`.
//!
//! The client synthesises the developer's full sync state — committed history,
//! working-tree changes, and the force-included gitignored files — into Git
//! objects and pushes them to the server, **without** touching the user's
//! branch, index, or working tree (see ADR-0003 and ADR-0004).
//!
//! So far it implements the first building block: [`encode`]ing the code commit
//! (committed history plus working-tree changes). Transfer to the server is not
//! implemented yet.

use std::path::PathBuf;

use thiserror::Error;

pub mod encode;

pub use encode::{CODE_REF, EncodeError, EncodeOutcome, encode};

/// Errors returned by client operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientError {
    /// An underlying protocol error.
    #[error(transparent)]
    Protocol(#[from] gfs_common::ProtocolError),
    /// Encoding the local sync state failed.
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

/// Synthesise the current sync state and push it to the server.
///
/// Synthesises committed history and working-tree changes into Git objects
/// (ADR-0004) and transfers them via `git push` (ADR-0005). For now it performs
/// the [`encode`] step only — the `code` ref is written locally and no transfer
/// happens yet. `repo_dir` locates the repository (typically the current
/// directory).
pub async fn sync(repo_dir: PathBuf) -> Result<(), ClientError> {
    let outcome = encode(&repo_dir)?;
    tracing::info!(commit = %outcome.commit, ref_ = CODE_REF, "encoded code state");
    Ok(())
}
