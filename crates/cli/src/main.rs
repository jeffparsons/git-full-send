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
            gfs_client::sync(repo, args.remote, args.stream_id, args.user_include).await?;
        }
        Command::Listen(args) => {
            let config = gfs_server::ListenConfig {
                max_connections: args.max_connections,
                connection_timeout: std::time::Duration::from_secs(args.connection_timeout),
            };
            gfs_server::listen(args.addr, args.repo, config).await?
        }
        Command::UpdateWorktree(args) => {
            gfs_server::update_worktree(args.repo, args.worktree, args.stream_id).await?
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
    }

    Ok(())
}
