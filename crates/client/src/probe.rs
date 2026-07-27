//! Asking the server whether it is up — without faking a push (ADR-0018).
//!
//! An orchestrator has to know that `listen` is accepting before it pushes, and
//! the only way to ask used to be to connect and hang up. That leaves
//! `git receive-pack` dead of SIGPIPE and the server logging a failed push, once
//! or twice per invocation, for a check that was perfectly healthy.
//!
//! A probe is instead a *complete* receive-pack conversation that happens to
//! update nothing: read the ref advertisement, send a flush-pkt, read to end of
//! stream. `git receive-pack` treats that as "no ref updates, thank you" and
//! exits **zero**, so the server records a clean `no_op` and logs nothing at
//! warning level.
//!
//! It also answers the question that made observation 2 of issue #75 so
//! expensive to diagnose: the advertisement is measured **here**, on the client,
//! so `probe` reports the exact byte cost every connection pays and the number of
//! refs behind it — no shelling into the workstation, no counting refs by hand.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use thiserror::Error;

/// A flush-pkt: the whole of what a probe sends.
const FLUSH_PKT: &[u8] = b"0000";

/// How long to wait for the server to say anything at all.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors returned by [`probe`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProbeError {
    /// The endpoint could not be reached — the server is down, or the SSH tunnel
    /// is not up (ADR-0006).
    #[error("could not connect to `{remote}`")]
    Connect {
        /// The endpoint we tried to reach.
        remote: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The connection was established but the exchange failed.
    #[error("could not complete the probe exchange with `{remote}`")]
    Exchange {
        /// The endpoint we were talking to.
        remote: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Something answered, but not `git receive-pack`.
    #[error("`{remote}` answered, but not with a ref advertisement")]
    NotReceivePack {
        /// The endpoint we were talking to.
        remote: String,
    },
}

/// What a [`probe`] found: the server is up, and this is what every connection
/// to it costs before a single byte of the developer's data moves.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct ProbeReport {
    /// `kind`/`schema`/`ts_unix_ms`/`tool_version`, flattened into the record.
    #[serde(flatten)]
    pub envelope: gfs_common::metrics::Envelope,
    /// The endpoint probed.
    pub remote: String,
    /// Round trip for the whole exchange, in milliseconds.
    pub total_ms: f64,
    /// Bytes of ref advertisement — paid on **every** connection, including the
    /// two a sync makes, whether or not anything is pushed.
    pub advertisement_bytes: u64,
    /// Refs advertised. A repository with no refs still advertises one
    /// placeholder line, which is not counted here.
    pub refs_advertised: u64,
    /// Refs advertised that are `git-full-send`'s own (`refs/git-full-send/…`),
    /// so an operator can see how much of the cost is ours versus the repo's.
    pub refs_ours: u64,
}

/// Check that a `git-full-send` server is accepting connections, and measure what
/// its ref advertisement costs.
///
/// Completes a real receive-pack exchange that updates nothing (see the [module
/// docs](self)), so the server records it as a clean no-op rather than a failed
/// push.
pub fn probe(remote: &str) -> Result<ProbeReport, ProbeError> {
    let started = Instant::now();
    let mut sock = TcpStream::connect(remote).map_err(|source| ProbeError::Connect {
        remote: remote.to_string(),
        source,
    })?;
    // A server that accepts and then says nothing must not hang an orchestrator.
    let _ = sock.set_read_timeout(Some(DEFAULT_TIMEOUT));
    let _ = sock.set_write_timeout(Some(DEFAULT_TIMEOUT));

    let advertisement = read_advertisement(&mut sock).map_err(|source| ProbeError::Exchange {
        remote: remote.to_string(),
        source,
    })?;
    if advertisement.refs.is_empty() && advertisement.bytes == 0 {
        return Err(ProbeError::NotReceivePack {
            remote: remote.to_string(),
        });
    }

    // Complete the conversation rather than hanging up on it: this is the
    // difference between a clean `no_op` and a SIGPIPE'd `receive-pack`.
    sock.write_all(FLUSH_PKT)
        .and_then(|()| sock.flush())
        .map_err(|source| ProbeError::Exchange {
            remote: remote.to_string(),
            source,
        })?;
    // Drain whatever follows (nothing, in practice) so the server sees us read to
    // the end before we go.
    let mut sink = Vec::new();
    let _ = sock.read_to_end(&mut sink);

    let refs_ours = advertisement
        .refs
        .iter()
        .filter(|name| name.starts_with(gfs_common::REF_NAMESPACE))
        .count() as u64;
    Ok(ProbeReport {
        envelope: gfs_common::metrics::Envelope::new("probe"),
        remote: remote.to_string(),
        total_ms: started.elapsed().as_secs_f64() * 1000.0,
        advertisement_bytes: advertisement.bytes,
        refs_advertised: advertisement.refs.len() as u64,
        refs_ours,
    })
}

/// The ref advertisement as read off the wire.
struct Advertisement {
    /// Its exact size, flush-pkt included.
    bytes: u64,
    /// The ref names in it, excluding git's `capabilities^{}` placeholder.
    refs: Vec<String>,
}

/// Read pkt-lines until the flush-pkt that ends the ref advertisement.
///
/// Parsed here rather than counted with [`gfs_common::pktline`] because a probe
/// wants the ref *names* too — enough to tell `git-full-send`'s own refs from the
/// repository's, and to drop git's placeholder line for a repo with no refs.
fn read_advertisement(sock: &mut TcpStream) -> std::io::Result<Advertisement> {
    let mut bytes = 0u64;
    let mut refs = Vec::new();
    loop {
        let mut header = [0u8; 4];
        match sock.read_exact(&mut header) {
            Ok(()) => {}
            // The server hung up mid-advertisement: report what we saw.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        bytes += 4;
        let Ok(len) = std::str::from_utf8(&header)
            .map_err(|_| ())
            .and_then(|text| usize::from_str_radix(text, 16).map_err(|_| ()))
        else {
            // Not pkt-line framed: whatever is listening, it isn't receive-pack.
            return Ok(Advertisement {
                bytes,
                refs: Vec::new(),
            });
        };
        // The flush-pkt ends the advertisement.
        if len == 0 {
            break;
        }
        if len < 4 {
            break;
        }
        let mut payload = vec![0u8; len - 4];
        sock.read_exact(&mut payload)?;
        bytes += payload.len() as u64;
        if let Some(name) = ref_name(&payload) {
            refs.push(name);
        }
    }
    Ok(Advertisement { bytes, refs })
}

/// The ref name in an advertisement line (`<oid> <name>\0<capabilities>`), or
/// `None` for git's `capabilities^{}` placeholder, which a repository with no
/// refs gets instead of a real one.
fn ref_name(payload: &[u8]) -> Option<String> {
    let line = String::from_utf8_lossy(payload);
    let line = line.split('\0').next().unwrap_or_default();
    let name = line.split_whitespace().nth(1)?;
    if name == "capabilities^{}" {
        return None;
    }
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ref_line_yields_its_name_and_the_placeholder_yields_none() {
        assert_eq!(
            ref_name(b"1234567890abcdef refs/heads/main\0report-status side-band-64k"),
            Some("refs/heads/main".to_string()),
        );
        assert_eq!(ref_name(b"1234567890abcdef refs/tags/v1\n"), {
            Some("refs/tags/v1".to_string())
        });
        // An empty repository advertises this instead of a ref; counting it would
        // report one ref where there are none.
        assert_eq!(
            ref_name(b"0000000000000000000000000000000000000000 capabilities^{}\0report-status"),
            None,
        );
    }
}
