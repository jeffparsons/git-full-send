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
mod stream;

pub use encode::{EncodeError, EncodeOutcome, encode};
pub use gfs_common::{StreamId, StreamIdError};
pub use push::{PushError, push_ref};
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
/// Synthesises committed history and working-tree changes into Git objects
/// (ADR-0004), then transfers the stream's `code` ref to the server's
/// `git receive-pack` via `git push --thin` (ADR-0005) and retains the pushed
/// tip locally as the next delta base. Refs are namespaced per stream so
/// concurrent senders don't clobber each other (ADR-0012).
///
/// `repo_dir` locates the repository (typically the current directory);
/// `remote` is the server endpoint (`HOST:PORT`, typically a tunnelled localhost
/// port). `stream` selects the stream: `Some` uses that id, `None` falls back to
/// the repo's configured `git-full-send.stream-id`, generating and persisting
/// one on first use.
pub async fn sync(
    repo_dir: PathBuf,
    remote: String,
    stream: Option<StreamId>,
) -> Result<(), ClientError> {
    let stream = stream::resolve_stream(&repo_dir, stream)?;

    let outcome = encode(&repo_dir, &stream)?;
    tracing::info!(commit = %outcome.commit, stream = %stream, ref_ = %outcome.code_ref, "encoded code state");

    push::push_ref(&repo_dir, &remote, &outcome.code_ref)?;
    push::retain_pushed_tip(&repo_dir, &stream, outcome.commit)?;
    tracing::info!(commit = %outcome.commit, stream = %stream, %remote, "pushed code state to server");
    Ok(())
}
