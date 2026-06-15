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
    tracing_subscriber::fmt()
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
            print_sync_summary(&summary);
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
            let mode = if args.wait {
                gfs_server::LockMode::Wait {
                    timeout: args.timeout.map(std::time::Duration::from_secs),
                }
            } else {
                gfs_server::LockMode::FailFast
            };
            gfs_server::update_worktree(args.repo, args.worktree, args.stream_id, mode).await?
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

/// Print the operator-facing end-of-sync summary block to stdout (issue #53).
///
/// A deliberate human-readable surface, distinct from the per-phase `tracing`
/// progress lines (stderr) and the durable JSONL metrics record (ADR-0013): the
/// numbers come from the same `sync` computation, formatted for a glance —
/// binary byte units and second/millisecond durations.
fn print_sync_summary(summary: &gfs_client::SyncSummary) {
    let t = &summary.timings;
    println!(
        "Synced stream {} to {} in {}",
        summary.stream,
        summary.remote,
        human_ms(t.total_ms),
    );
    println!(
        "  code:  {} file(s) (+{}), {} removed   encode {} · push {}",
        summary.code.files_overlaid,
        human_bytes(summary.code.bytes_overlaid),
        summary.code.files_removed,
        human_ms(t.code_encode_ms),
        human_ms(t.code_push_ms),
    );
    println!(
        "  extra: {} file(s) ({})   encode {} · push {}",
        summary.extra.files,
        human_bytes(summary.extra.bytes),
        human_ms(t.extra_encode_ms),
        human_ms(t.extra_push_ms),
    );
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
