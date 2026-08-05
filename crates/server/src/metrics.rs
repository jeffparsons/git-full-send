//! The server's per-operation metrics records (issue #42).
//!
//! Shapes the JSON Lines records the server writes to its metrics sink
//! (`git_full_send_common::metrics`): one `receive` record per `git receive-pack`
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
    envelope: git_full_send_common::metrics::Envelope,
    /// Wall time for the whole connection, in milliseconds.
    pub duration_ms: f64,
    /// What this connection actually was — see [`crate::Outcome`]. The field to
    /// read: a healthy liveness probe is `success: false` and entirely fine.
    pub outcome: &'static str,
    /// Why authentication was refused (`mismatch`/`malformed`/`absent`), on the
    /// connections that were (ADR-0019). Absent from every other record, so the
    /// shape of a normal receive is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_failure: Option<&'static str>,
    /// Whether `receive-pack` exited zero.
    pub success: bool,
    /// Its exit code, if it exited via a code (vs. a signal).
    pub exit_code: Option<i32>,
    /// The signal that killed it, if one did — `13` (SIGPIPE) is what a prober
    /// hanging up looks like from here.
    pub signal: Option<i32>,
    /// Bytes read off the socket, split into the ref-update commands and the
    /// pack that follows them (ADR-0017).
    pub inbound: Inbound,
    /// Bytes written back to the socket, split into the ref advertisement and
    /// the report-status that follows it.
    pub outbound: Outbound,
    /// The refs accepted by the namespace hook for this push.
    pub refs_updated: Vec<String>,
}

/// The inbound half of a connection: what the client sent.
#[derive(Debug, Serialize)]
pub(crate) struct Inbound {
    /// Every byte received.
    pub total: u64,
    /// The ref-update command block, up to and including its flush-pkt.
    pub commands: u64,
    /// How many ref-update commands that was. Zero means nothing was being
    /// pushed — the basis for classifying a probe (ADR-0018).
    pub command_pkts: u64,
    /// The pack itself: the only part that is the user's actual data.
    pub pack: u64,
}

/// The outbound half of a connection: what the server sent.
#[derive(Debug, Serialize)]
pub(crate) struct Outbound {
    /// Every byte sent.
    pub total: u64,
    /// The ref advertisement, which every connection pays for in full regardless
    /// of how little is being pushed.
    pub advertisement: u64,
    /// Refs advertised (one pkt-line each; a repo with no refs still gets one
    /// placeholder line).
    pub refs_advertised: u64,
    /// The report-status sent after the pack was ingested.
    pub report: u64,
}

impl ReceiveRecord {
    /// Assemble the record from how the child ended and what crossed the wire.
    ///
    /// `status` is unpacked here rather than by the caller so the three
    /// exit-related fields cannot disagree with each other.
    pub(crate) fn new(
        duration_ms: f64,
        outcome: &'static str,
        status: &std::process::ExitStatus,
        inbound: git_full_send_common::pktline::WireCounts,
        outbound: git_full_send_common::pktline::WireCounts,
        refs_updated: Vec<String>,
    ) -> Self {
        use std::os::unix::process::ExitStatusExt;
        Self {
            envelope: git_full_send_common::metrics::Envelope::new("receive"),
            duration_ms,
            outcome,
            auth_failure: None,
            success: status.success(),
            exit_code: status.code(),
            signal: status.signal(),
            inbound: Inbound {
                total: inbound.total,
                commands: inbound.pre_flush,
                command_pkts: inbound.pre_flush_pkts,
                pack: inbound.post_flush,
            },
            outbound: Outbound {
                total: outbound.total,
                advertisement: outbound.pre_flush,
                refs_advertised: outbound.pre_flush_pkts,
                report: outbound.post_flush,
            },
            refs_updated,
        }
    }

    /// The record for a connection refused before `receive-pack` was spawned
    /// (ADR-0019).
    ///
    /// Its own constructor because there is no child to describe: the exit fields
    /// are `None`/`false` by construction rather than by a caller remembering to
    /// pass a status that never existed, and the wire counts are zero because
    /// nothing but the preamble and the refusal crossed.
    pub(crate) fn unauthenticated(
        duration_ms: f64,
        outcome: &'static str,
        auth_failure: &'static str,
    ) -> Self {
        Self {
            envelope: git_full_send_common::metrics::Envelope::new("receive"),
            duration_ms,
            outcome,
            auth_failure: Some(auth_failure),
            success: false,
            exit_code: None,
            signal: None,
            inbound: Inbound {
                total: 0,
                commands: 0,
                command_pkts: 0,
                pack: 0,
            },
            outbound: Outbound {
                total: 0,
                advertisement: 0,
                refs_advertised: 0,
                report: 0,
            },
            refs_updated: Vec::new(),
        }
    }
}

/// Best-effort: write a record to the repo's sink under `git_dir`.
pub(crate) fn record(git_dir: &Path, record: &impl Serialize) {
    git_full_send_common::metrics::record(git_dir, record);
}
