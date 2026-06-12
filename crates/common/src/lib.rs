//! Shared protocol types and constants for `git-full-send`.
//!
//! This crate holds the pieces both the client ([`gfs_client`]) and the server
//! ([`gfs_server`]) need to agree on — wire/protocol constants and shared error
//! types. It is intentionally tiny for now and grows as the protocol does.
//!
//! [`gfs_client`]: https://docs.rs/gfs-client
//! [`gfs_server`]: https://docs.rs/gfs-server

use thiserror::Error;

/// Ref namespace that `git-full-send` confines its synced refs to.
///
/// The server restricts the writable refs of each `git receive-pack` to this
/// namespace (see ADR-0005), and the client pushes its scratch refs underneath
/// it (see ADR-0004).
pub const REF_NAMESPACE: &str = "refs/git-full-send/";

/// Default address the server `listen` binds to.
///
/// Localhost only (ADR-0006): connectivity from a real client is via a manual
/// SSH tunnel, so binding loopback is sufficient and keeps the receive-pack
/// stream off the network. Overridable via the `--addr` flag.
pub const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:9419";

/// Errors shared across the client and server boundaries.
///
/// Placeholder enum establishing the `thiserror`-at-library-boundaries
/// convention from ADR-0001; variants are added as the protocol grows.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// A protocol invariant was violated. Replaced with concrete variants as
    /// the protocol is implemented.
    #[error("protocol error: {0}")]
    Protocol(String),
}
