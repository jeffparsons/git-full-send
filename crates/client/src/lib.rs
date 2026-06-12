//! Client-side library for `git-full-send`.
//!
//! The client synthesises the developer's full sync state — committed history,
//! working-tree changes, and the force-included gitignored files — into Git
//! objects and pushes them to the server, **without** touching the user's
//! branch, index, or working tree (see ADR-0003 and ADR-0004).
//!
//! It implements two building blocks: [`encode`]ing the code commit (committed
//! history plus working-tree changes) and [`push`]ing it to the server's
//! `git receive-pack` over a localhost connection.

use std::path::PathBuf;

use thiserror::Error;

pub mod encode;
mod push;

pub use encode::{CODE_REF, EncodeError, EncodeOutcome, encode};
pub use push::{PushError, SENT_REF, push_ref};

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
    /// Transferring the encoded state to the server failed.
    #[error(transparent)]
    Push(#[from] PushError),
}

/// Synthesise the current sync state and push it to the server.
///
/// Synthesises committed history and working-tree changes into Git objects
/// (ADR-0004), then transfers the `code` ref to the server's `git receive-pack`
/// via `git push --thin` (ADR-0005) and retains the pushed tip locally as the
/// next delta base. `repo_dir` locates the repository (typically the current
/// directory); `remote` is the server endpoint (`HOST:PORT`, typically a
/// tunnelled localhost port).
pub async fn sync(repo_dir: PathBuf, remote: String) -> Result<(), ClientError> {
    let outcome = encode(&repo_dir)?;
    tracing::info!(commit = %outcome.commit, ref_ = CODE_REF, "encoded code state");

    push::push_ref(&repo_dir, &remote, CODE_REF)?;
    push::retain_pushed_tip(&repo_dir, outcome.commit)?;
    tracing::info!(commit = %outcome.commit, %remote, "pushed code state to server");
    Ok(())
}
