//! `git-full-send` command-line interface.
//!
//! A single binary exposing every command: the client `sync`, and the server
//! `listen` and `update-worktree` (see ADR-0003). Each subcommand is a thin
//! wrapper that dispatches into the [`gfs_client`] / [`gfs_server`] libraries,
//! which currently stub their work out.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

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
    UpdateWorktree,
}

#[derive(Debug, Args)]
struct SyncArgs {
    /// Path to the repository to sync. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    repo: Option<PathBuf>,
    /// Server endpoint to push to (typically a tunnelled localhost port).
    #[arg(long, value_name = "HOST:PORT")]
    remote: String,
}

#[derive(Debug, Args)]
struct ListenArgs {
    /// Path to the target Git repository that receives synced refs.
    #[arg(long, value_name = "PATH")]
    repo: PathBuf,
    /// Address to bind. Localhost only by default (ADR-0006).
    #[arg(long, value_name = "IP:PORT", default_value = gfs_common::DEFAULT_LISTEN_ADDR)]
    addr: SocketAddr,
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
            gfs_client::sync(repo, args.remote).await?;
        }
        Command::Listen(args) => gfs_server::listen(args.addr, args.repo).await?,
        Command::UpdateWorktree => gfs_server::update_worktree().await?,
    }

    Ok(())
}
