//! `git-full-send` command-line interface.
//!
//! A single binary exposing every command: the client `sync`, and the server
//! `listen` and `update-worktree` (see ADR-0003). Each subcommand is a thin
//! wrapper that dispatches into the [`gfs_client`] / [`gfs_server`] libraries.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use gfs_common::StreamId;

/// Sync a developer's Git working state to a remote workstation.
#[derive(Debug, Parser)]
#[command(name = "git-full-send", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Synthesise the local sync state and push it to the server (client).
    Sync(SyncArgs),
    /// Run the long-running server that receives sync requests (server).
    Listen(ListenArgs),
    /// Check the synced state out into the configured worktree (server).
    UpdateWorktree(UpdateWorktreeArgs),
    /// List the streams that have a synced `code` ref (server).
    ListStreams(ListStreamsArgs),
    /// Delete a stream's refs so it no longer appears in `list-streams`.
    ForgetStream(ForgetStreamArgs),
    /// Forget streams whose `code` is older than a cutoff age (server).
    Reap(ReapArgs),
}

#[derive(Debug, Args)]
struct SyncArgs {
    /// Path to the repository to sync. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    repo: Option<PathBuf>,
    /// Server endpoint to push to (typically a tunnelled localhost port).
    #[arg(long, value_name = "HOST:PORT")]
    remote: String,
    /// Stream to sync under. Defaults to this repo's configured
    /// `git-full-send.stream-id`, generated and persisted on first use.
    #[arg(long, value_name = "ID")]
    stream_id: Option<StreamId>,
    /// Per-user force-include pattern file. Overrides the
    /// `GIT_FULL_SEND_USER_INCLUDE` / `$XDG_CONFIG_HOME` lookup; the committed
    /// project file (`.git-full-send-include`) is always consulted as well.
    #[arg(long, value_name = "PATH")]
    user_include: Option<PathBuf>,
    /// Print the operation's record as one JSON object on stdout instead of the
    /// human summary — the same record appended to the metrics sink (ADR-0017).
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ListenArgs {
    /// Path to the target Git repository that receives synced refs.
    #[arg(long, value_name = "PATH")]
    repo: PathBuf,
    /// Address to bind. Localhost only by default (ADR-0006).
    #[arg(long, value_name = "IP:PORT", default_value = gfs_common::DEFAULT_LISTEN_ADDR)]
    addr: SocketAddr,
    /// Maximum number of connections served concurrently; further connections
    /// wait for a slot (issue #47).
    #[arg(long, value_name = "N", default_value_t = gfs_common::DEFAULT_MAX_CONNECTIONS)]
    max_connections: usize,
    /// Per-connection wall-clock timeout in seconds; a handler that overruns it
    /// is aborted so a stuck client can't pin a slot (issue #47).
    #[arg(long, value_name = "SECS", default_value_t = gfs_common::DEFAULT_CONNECTION_TIMEOUT_SECS)]
    connection_timeout: u64,
}

#[derive(Debug, Args)]
struct UpdateWorktreeArgs {
    /// Path to the target Git repository holding the synced refs.
    #[arg(long, value_name = "PATH")]
    repo: PathBuf,
    /// Path to the disposable worktree directory to check the `code` tree into.
    #[arg(long, value_name = "PATH")]
    worktree: PathBuf,
    /// Stream whose `code` tree to check out. Required — the server has no
    /// repo-local default (see `list-streams` to discover synced streams).
    #[arg(long, value_name = "ID")]
    stream_id: StreamId,
    /// Wait for an in-progress update of the same worktree instead of failing
    /// fast. Without this, a checkout of a worktree already being updated exits
    /// non-zero with an "update already in progress" error.
    #[arg(long)]
    wait: bool,
    /// With `--wait`, give up after this many seconds rather than waiting
    /// indefinitely. Has no effect without `--wait`.
    #[arg(long, value_name = "SECS", requires = "wait")]
    timeout: Option<u64>,
    /// Also measure the worktree: how many paths differ from what is on disk,
    /// and how many files it holds. Costs an `lstat` per index entry plus a full
    /// filesystem walk, both proportional to the tree — hence opt-in. Everything
    /// else on the record is measured either way.
    #[arg(long)]
    measure_worktree: bool,
    /// Print the operation's record as one JSON object on stdout instead of the
    /// human summary — the same record appended to the metrics sink (ADR-0017).
    /// This is how a client driving a remote checkout over SSH gets the server's
    /// numbers back.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ListStreamsArgs {
    /// Path to the target Git repository holding the synced refs.
    #[arg(long, value_name = "PATH")]
    repo: PathBuf,
}

#[derive(Debug, Args)]
struct ForgetStreamArgs {
    /// Path to the repository holding the stream's refs. Point it at the server
    /// repo to drop the stream's `code`/`extra`, or at a client repo to drop its
    /// local `sent/*` delta-base pins (see `docs/operating.md`).
    #[arg(long, value_name = "PATH")]
    repo: PathBuf,
    /// Stream whose refs to delete.
    #[arg(long, value_name = "ID")]
    stream_id: StreamId,
}

#[derive(Debug, Args)]
struct ReapArgs {
    /// Path to the server repository whose stale streams to reap.
    #[arg(long, value_name = "PATH")]
    repo: PathBuf,
    /// Forget streams whose `code` was last synced more than this many days ago.
    /// Required — there is no default age, so reaping never happens implicitly.
    #[arg(long, value_name = "DAYS")]
    older_than_days: u64,
    /// Report which streams would be reaped without deleting anything.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // The progress log goes to **stderr**, keeping stdout for the operation's own
    // output — the human summary block, or the `--json` record (ADR-0013's three
    // surfaces, as ADR-0017 relies on them). `tracing_subscriber::fmt()` defaults
    // to stdout, which interleaved log lines into the summary and left `--json`
    // unparseable.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Sync(args) => {
            let repo = match args.repo {
                Some(path) => path,
                None => std::env::current_dir()?,
            };
            let summary =
                gfs_client::sync(repo, args.remote, args.stream_id, args.user_include).await?;
            if args.json {
                print_json(&summary);
            } else {
                print_sync_summary(&summary);
            }
        }
        Command::Listen(args) => {
            let config = gfs_server::ListenConfig {
                max_connections: args.max_connections,
                connection_timeout: std::time::Duration::from_secs(args.connection_timeout),
            };
            gfs_server::listen(args.addr, args.repo, config).await?
        }
        Command::UpdateWorktree(args) => {
            // `--timeout` is gated on `--wait` by clap (`requires = "wait"`), so
            // a timeout only ever reaches the `Wait` arm.
            let lock = if args.wait {
                gfs_server::LockMode::Wait {
                    timeout: args.timeout.map(std::time::Duration::from_secs),
                }
            } else {
                gfs_server::LockMode::FailFast
            };
            let options = gfs_server::UpdateOptions {
                lock,
                measure_worktree: args.measure_worktree,
            };
            let report =
                gfs_server::update_worktree(args.repo, args.worktree, args.stream_id, options)
                    .await?;
            if args.json {
                print_json(&report);
            } else {
                print_update_worktree_summary(&report);
            }
        }
        Command::ListStreams(args) => {
            for stream in gfs_server::list_streams(&args.repo)? {
                println!("{stream}");
            }
        }
        Command::ForgetStream(args) => {
            let removed = gfs_server::forget_stream(&args.repo, &args.stream_id)?;
            if removed == 0 {
                println!("no refs for stream `{}`; nothing to forget", args.stream_id);
            } else {
                println!(
                    "forgot stream `{}` ({removed} ref(s) removed)",
                    args.stream_id
                );
            }
        }
        Command::Reap(args) => {
            let now_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let cutoff = now_unix - (args.older_than_days as i64) * SECONDS_PER_DAY;
            let outcome = gfs_server::reap_streams(&args.repo, cutoff, args.dry_run)?;
            print_reap_outcome(&outcome, args.older_than_days, now_unix);
        }
    }

    Ok(())
}

/// Seconds in a day, for converting `--older-than-days` to a cutoff instant.
const SECONDS_PER_DAY: i64 = 86_400;

/// Print the operator-facing result of a `reap` pass to stdout.
///
/// One line per stale stream (with its age and the ref count it shed, or would
/// shed in a dry run) plus a summary, mirroring `forget-stream`'s plain style.
fn print_reap_outcome(outcome: &gfs_server::ReapOutcome, older_than_days: u64, now_unix: i64) {
    if outcome.reaped.is_empty() {
        println!(
            "no streams older than {older_than_days} day(s); nothing to reap \
             ({} scanned)",
            outcome.scanned,
        );
        return;
    }
    for reaped in &outcome.reaped {
        let age_days = (now_unix - reaped.committed_unix_secs).max(0) / SECONDS_PER_DAY;
        if outcome.dry_run {
            println!(
                "would reap `{}` (last synced {age_days} day(s) ago, {} ref(s))",
                reaped.stream, reaped.refs_removed,
            );
        } else {
            println!(
                "reaped `{}` (last synced {age_days} day(s) ago, {} ref(s) removed)",
                reaped.stream, reaped.refs_removed,
            );
        }
    }
    let n = outcome.reaped.len();
    if outcome.dry_run {
        println!(
            "{n} of {} stream(s) would be reaped (re-run without --dry-run to delete)",
            outcome.scanned,
        );
    } else {
        println!("reaped {n} of {} stream(s)", outcome.scanned);
    }
}

/// Print an operation's record as one JSON object on stdout (`--json`, ADR-0017).
///
/// Byte-for-byte the record appended to the metrics sink, so an integrator parses
/// what the operator reads. Serialisation of a record we just built cannot
/// realistically fail, but a metrics surface is never worth failing an operation
/// for (ADR-0013), so a failure warns and prints nothing.
fn print_json(record: &impl serde::Serialize) {
    match serde_json::to_string(record) {
        Ok(line) => println!("{line}"),
        Err(error) => tracing::warn!(%error, "could not serialise the record for --json"),
    }
}

/// Print the operator-facing end-of-sync summary block to stdout (issue #53).
///
/// A deliberate human-readable surface, distinct from the per-phase `tracing`
/// progress lines (stderr) and the durable JSONL metrics record (ADR-0013): the
/// numbers come from the same `sync` computation, formatted for a glance —
/// binary byte units and second/millisecond durations.
fn print_sync_summary(summary: &gfs_client::SyncSummary) {
    println!(
        "Synced stream {} to {} in {}",
        summary.stream,
        summary.remote,
        human_ms(summary.total_ms),
    );
    println!(
        "  code:  {} file(s) (+{}), {} removed   encode {} · push {}",
        summary.code.stats.files_overlaid,
        human_bytes(summary.code.stats.bytes_overlaid),
        summary.code.stats.files_removed,
        human_ms(summary.code.encode_ms),
        human_ms(summary.code.push_ms),
    );
    println!(
        "  extra: {} file(s) ({})   encode {} · push {}",
        summary.extra.stats.files,
        human_bytes(summary.extra.stats.bytes),
        human_ms(summary.extra.encode_ms),
        human_ms(summary.extra.push_ms),
    );

    // What the two encodes actually spent their time on. The `extra` walk is the
    // line that turns "sync feels slow" into "this include pattern is unanchored"
    // (ADR-0017).
    let code = &summary.code.stats;
    println!(
        "         index {} entries · status {} item(s) · hashed {} file(s) in {}",
        human_count(code.index_entries),
        human_count(code.status_items),
        human_count(code.files_overlaid),
        human_ms(code.encode_phases.hash_ms),
    );
    let walk = &summary.extra.stats.select;
    let mut walk_line = format!(
        "         walk {} dir(s), {} pruned, {} path(s) considered in {}",
        human_count(walk.dirs_entered),
        human_count(walk.dirs_pruned),
        human_count(walk.paths_considered),
        human_ms(summary.extra.stats.encode_phases.select_ms),
    );
    if walk.unanchored_patterns > 0 {
        walk_line.push_str(&format!(
            "   ({} unanchored pattern(s) — no directory could be pruned)",
            walk.unanchored_patterns,
        ));
    }
    println!("{walk_line}");
}

/// Print the operator-facing end-of-checkout summary block to stdout.
///
/// `update-worktree` previously left its numbers to a `tracing` line and the
/// sink; it now has the same human-summary surface `sync` does (ADR-0017), with
/// `--json` as the machine-readable alternative.
///
/// The block is arranged so the *explanation* sits next to the duration it
/// explains: how much had to change, what the index looked like, and where
/// `read-tree`'s time actually went.
fn print_update_worktree_summary(report: &gfs_server::UpdateWorktreeReport) {
    println!(
        "Updated worktree {} from stream {} in {}",
        report.worktree,
        report.stream,
        human_ms(report.total_ms),
    );

    // What had to change — the line that says whether a big `read-tree` was
    // earned. A tree we already checked out is a no-op by definition.
    let mut work = match report.changed.vs_index {
        Some(delta) if delta.is_empty() => "nothing to write or remove".to_string(),
        Some(delta) => format!(
            "{} to write, {} to remove",
            human_count(delta.to_write),
            human_count(delta.to_remove),
        ),
        None => "changed paths not measured".to_string(),
    };
    if report.changed.tree_unchanged {
        work.push_str(" (same tree as the last checkout)");
    }
    if let Some(delta) = report.changed.vs_worktree {
        work.push_str(&format!(
            "; vs. disk {} to write, {} to remove",
            human_count(delta.to_write),
            human_count(delta.to_remove),
        ));
    }
    println!("  tree {} — {work}", short_oid(&report.tree));

    // The index, whose warmth is the other half of the explanation.
    let index = &report.index;
    let entries = match index.entries {
        Some(n) => format!("{} entries", human_count(n.max(0) as usize)),
        None => "entry count unknown".to_string(),
    };
    let size = match index.bytes {
        Some(bytes) => human_bytes(bytes),
        None => "absent".to_string(),
    };
    let mut line = format!("  index {}: {entries}, {size}", index.state);
    if let Some(files) = report.worktree_files {
        line.push_str(&format!("   worktree {} file(s)", human_count(files)));
    }
    println!("{line}");

    println!(
        "  resolve {} · measure {} · read-tree {} · clean {} ({} removed)",
        human_ms(report.resolve_ms),
        human_ms(report.measure_ms),
        human_ms(report.read_tree_ms),
        human_ms(report.clean_ms),
        report.clean.removed,
    );

    // Inside `read-tree`, when git told us (ADR-0017).
    if let Some(rt) = &report.read_tree {
        let part = |label: &str, ms: Option<f64>| {
            ms.map(|ms| format!("{label} {}", human_ms(ms)))
                .unwrap_or_default()
        };
        let parts: Vec<String> = [
            part("load index", rt.load_index_ms),
            part("resolve tree", rt.resolve_tree_ms),
            part("apply", rt.apply_ms),
            part("write index", rt.write_index_ms),
        ]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect();
        if !parts.is_empty() {
            println!("    read-tree: {}", parts.join(" · "));
        }
    }
}

/// Format a count with `_`-free thousands separators, so a five-digit entry
/// count is readable at a glance.
fn human_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Abbreviate an object id for display, as `git` does. The record keeps the full
/// id; only this display shortens it.
fn short_oid(oid: &str) -> &str {
    let n = oid.len().min(12);
    &oid[..n]
}

/// Format a byte count with binary (1024) units: `B` exactly, one decimal above.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Format a millisecond duration: whole `ms` under a second, else `s` with one
/// decimal. The metrics keep the raw value; only this display rounds.
fn human_ms(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{}ms", ms.round() as u64)
    } else {
        format!("{:.1}s", ms / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 GiB");
    }

    #[test]
    fn human_ms_switches_to_seconds_at_one_second() {
        assert_eq!(human_ms(0.0), "0ms");
        assert_eq!(human_ms(12.4), "12ms");
        assert_eq!(human_ms(999.0), "999ms");
        assert_eq!(human_ms(1000.0), "1.0s");
        assert_eq!(human_ms(1400.0), "1.4s");
    }
}
