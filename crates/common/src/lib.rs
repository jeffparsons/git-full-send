//! Shared protocol types and constants for `git-full-send`.
//!
//! This crate holds the pieces both the client ([`gfs_client`]) and the server
//! ([`gfs_server`]) need to agree on — wire/protocol constants and shared error
//! types. It is intentionally tiny for now and grows as the protocol does.
//!
//! [`gfs_client`]: https://docs.rs/gfs-client
//! [`gfs_server`]: https://docs.rs/gfs-server

use bstr::ByteSlice;
use thiserror::Error;

pub mod auth;
pub mod metrics;
pub mod pktline;
pub mod trace2;

/// Ref namespace that `git-full-send` confines its synced refs to.
///
/// The server restricts the writable refs of each `git receive-pack` to this
/// namespace (see ADR-0005), and the client pushes its scratch refs underneath
/// it (see ADR-0004).
pub const REF_NAMESPACE: &str = "refs/git-full-send/";

/// Prefix under [`REF_NAMESPACE`] beneath which every per-stream ref subtree
/// lives: `refs/git-full-send/streams/<stream-id>/…` (see ADR-0012).
///
/// Used both to *build* per-stream ref names ([`code_ref`], [`sent_ref`]) and to
/// *enumerate* streams on the server by stripping this prefix off matching refs.
pub const STREAMS_PREFIX: &str = "refs/git-full-send/streams/";

/// An independent, reusable slot of synced state — the unit that
/// `git-full-send` namespaces refs by so that concurrent senders don't clobber
/// each other (ADR-0012).
///
/// A stream id is caller-chosen and **stable across syncs** (the delta-base
/// retention of ADR-0005 only pays off when the same refs are reused). It may be
/// branch-shaped and contain slashes (e.g. `feature/foo`); the wrapped string is
/// validated on construction so the assembled ref
/// `refs/git-full-send/streams/<id>/code` is always a well-formed Git ref.
/// Serialises as the bare id string, so a metrics record (ADR-0013/ADR-0017)
/// carries `"stream": "my-laptop"` rather than a wrapper object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct StreamId(String);

/// The reason a string was rejected as a [`StreamId`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StreamIdError {
    /// The id was empty.
    #[error("stream id must not be empty")]
    Empty,
    /// The id would not form a valid Git ref path (e.g. it contains `..`, a
    /// leading/trailing slash, `*`, `@{`, or a control character).
    #[error("invalid stream id `{id}`: must be a valid Git ref path ({reason})")]
    Invalid {
        /// The rejected id.
        id: String,
        /// The underlying validation reason.
        reason: String,
    },
}

impl StreamId {
    /// Validate `id` and wrap it as a [`StreamId`].
    ///
    /// Validation is performed on the *assembled* ref
    /// `refs/git-full-send/streams/<id>/code` via [`gix_validate`], so anything
    /// Git would reject in a ref name (repeated/leading/trailing slashes, `..`,
    /// `*`, `@{`, control bytes, a `.lock` suffix, …) is rejected here too.
    /// Slashes *within* the id are allowed, so branch-shaped ids round-trip.
    pub fn new(id: impl Into<String>) -> Result<Self, StreamIdError> {
        let id = id.into();
        if id.is_empty() {
            return Err(StreamIdError::Empty);
        }
        let candidate = format!("{STREAMS_PREFIX}{id}/code");
        gix_validate::reference::name(candidate.as_bytes().as_bstr()).map_err(|err| {
            StreamIdError::Invalid {
                id: id.clone(),
                reason: err.to_string(),
            }
        })?;
        Ok(Self(id))
    }

    /// The validated id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for StreamId {
    type Err = StreamIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// The ref holding `stream`'s encoded `code` state (committed history plus
/// working-tree changes; see ADR-0004): `…/streams/<id>/code`.
///
/// The client writes the synthetic code commit here and pushes it; the server
/// checks this ref's tree out into a worktree (ADR-0008). Built from
/// [`STREAMS_PREFIX`] so neither side hard-codes the layout.
pub fn code_ref(stream: &StreamId) -> String {
    format!("{STREAMS_PREFIX}{}/code", stream.as_str())
}

/// The client-local ref pinning `stream`'s last-confirmed-pushed `code` tip as
/// the next delta base (ADR-0005): `…/streams/<id>/sent/code`.
pub fn sent_ref(stream: &StreamId) -> String {
    format!("{STREAMS_PREFIX}{}/sent/code", stream.as_str())
}

/// The ref holding `stream`'s encoded `extra` state — the force-included,
/// normally-gitignored files (ADR-0004/ADR-0007): `…/streams/<id>/extra`.
///
/// The client writes the synthetic `extra` commit here (parented on the prior
/// sync's `extra` tip so the volatile build outputs keep a delta-base chain) and
/// pushes it alongside [`code_ref`] in the same exchange. Built from
/// [`STREAMS_PREFIX`] so neither side hard-codes the layout.
pub fn extra_ref(stream: &StreamId) -> String {
    format!("{STREAMS_PREFIX}{}/extra", stream.as_str())
}

/// The client-local ref pinning `stream`'s last-confirmed-pushed `extra` tip as
/// the next delta base, and as the parent of the next `extra` commit (ADR-0005):
/// `…/streams/<id>/sent/extra`.
pub fn sent_extra_ref(stream: &StreamId) -> String {
    format!("{STREAMS_PREFIX}{}/sent/extra", stream.as_str())
}

/// The ref-name prefix under which *every* ref of `stream` lives:
/// `…/streams/<id>/`.
///
/// The trailing slash is significant — it bounds the prefix at a path segment so
/// stream `foo` does not match a `foobar` ref. Used to enumerate and delete a
/// stream's refs wholesale when forgetting it (issue #48): `code`, `extra`, and
/// the client-local `sent/*` pins all sit beneath it. Built from
/// [`STREAMS_PREFIX`] so neither side hard-codes the layout (ADR-0012).
pub fn stream_prefix(stream: &StreamId) -> String {
    format!("{STREAMS_PREFIX}{}/", stream.as_str())
}

/// Default address the server `listen` binds to.
///
/// Localhost only (ADR-0006): connectivity from a real client is via a manual
/// SSH tunnel, so binding loopback is sufficient and keeps the receive-pack
/// stream off the network. Overridable via the `--addr` flag.
pub const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:9419";

/// Default cap on concurrently-served `git receive-pack` handlers (issue #47).
///
/// Bounds in-flight connections so a burst can't exhaust threads; further
/// connections wait for a slot rather than each spawning unconditionally.
/// Overridable via the `listen --max-connections` flag.
pub const DEFAULT_MAX_CONNECTIONS: usize = 16;

/// Default per-connection wall-clock timeout, in seconds (issue #47).
///
/// A handler that runs longer than this is aborted (its socket is shut down) so a
/// stuck client can't pin a concurrency slot indefinitely. Overridable via the
/// `listen --connection-timeout` flag.
pub const DEFAULT_CONNECTION_TIMEOUT_SECS: u64 = 300;

/// How long the server waits for a client's authentication preamble, in seconds
/// (ADR-0019).
///
/// Much shorter than [`DEFAULT_CONNECTION_TIMEOUT_SECS`], and for a different
/// job: the preamble is the first thing an authenticating client writes, so
/// anything slower than this is a client that will never send one — most often
/// one with no token configured, waiting for a ref advertisement that is not
/// coming. Reaching the deadline is what lets the server answer it with a
/// diagnosis ([`auth::AuthOutcome::Absent`]) rather than leaving it to hang.
pub const DEFAULT_AUTH_TIMEOUT_SECS: u64 = 10;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_live_under_the_namespace_and_prefix() {
        let id = StreamId::new("laptop").unwrap();
        assert!(code_ref(&id).starts_with(STREAMS_PREFIX));
        assert!(sent_ref(&id).starts_with(STREAMS_PREFIX));
        assert!(extra_ref(&id).starts_with(STREAMS_PREFIX));
        assert!(sent_extra_ref(&id).starts_with(STREAMS_PREFIX));
        assert!(STREAMS_PREFIX.starts_with(REF_NAMESPACE));
        assert_eq!(code_ref(&id), "refs/git-full-send/streams/laptop/code");
        assert_eq!(sent_ref(&id), "refs/git-full-send/streams/laptop/sent/code");
        assert_eq!(extra_ref(&id), "refs/git-full-send/streams/laptop/extra");
        assert_eq!(
            sent_extra_ref(&id),
            "refs/git-full-send/streams/laptop/sent/extra"
        );
    }

    #[test]
    fn stream_prefix_bounds_every_ref_at_a_path_segment() {
        let id = StreamId::new("laptop").unwrap();
        let prefix = stream_prefix(&id);
        assert_eq!(prefix, "refs/git-full-send/streams/laptop/");
        // The trailing slash is load-bearing for prefix deletion (issue #48).
        assert!(prefix.ends_with('/'));
        // Every per-stream ref sits beneath the prefix...
        for r in [
            code_ref(&id),
            sent_ref(&id),
            extra_ref(&id),
            sent_extra_ref(&id),
        ] {
            assert!(r.starts_with(&prefix), "`{r}` is under `{prefix}`");
        }
        // ...while a *different* stream that shares a name prefix is not, because
        // the trailing slash stops `laptop` matching `laptop-2`'s refs.
        let sibling = StreamId::new("laptop-2").unwrap();
        assert!(!code_ref(&sibling).starts_with(&prefix));
    }

    #[test]
    fn branch_shaped_ids_with_slashes_are_accepted_and_round_trip() {
        let id = StreamId::new("feature/foo").unwrap();
        assert_eq!(id.as_str(), "feature/foo");
        assert_eq!(code_ref(&id), "refs/git-full-send/streams/feature/foo/code");
    }

    #[test]
    fn invalid_ids_are_rejected() {
        assert!(matches!(StreamId::new(""), Err(StreamIdError::Empty)));
        for bad in [
            "/leading",
            "trailing/",
            "a//b",
            "a..b",
            "has space",
            "star*",
            "ref@{x",
        ] {
            assert!(
                matches!(StreamId::new(bad), Err(StreamIdError::Invalid { .. })),
                "expected `{bad}` to be rejected"
            );
        }
    }
}
