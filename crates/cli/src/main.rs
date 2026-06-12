//! `git-full-send` command-line interface.
//!
//! A single binary exposing every command: the client `sync`, and the server
//! `listen` and `update-worktree` (see ADR-0003). Each subcommand is a thin
//! wrapper that dispatches into the [`gfs_client`] / [`gfs_server`] libraries,
//! which currently stub their work out.

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    Sync,
    /// Run the long-running server that receives sync requests (server).
    Listen,
    /// Check the synced state out into the configured worktree (server).
    UpdateWorktree,
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
        Command::Sync => gfs_client::sync().await?,
        Command::Listen => gfs_server::listen().await?,
        Command::UpdateWorktree => gfs_server::update_worktree().await?,
    }

    Ok(())
}
