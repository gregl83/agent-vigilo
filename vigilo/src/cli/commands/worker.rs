use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use std::time::Duration;
use tracing::info;
use tracing::log::warn;
use super::Executable;
use crate::{
    context::Context,
    runtime::ServiceRunner,
};

#[derive(Debug, Subcommand)]
pub(crate) enum SubCommand {
    /// Start a worker process
    Start,

    /// Process a single worker cycle and exit
    Once,
}

#[derive(Debug, Args)]
pub(crate) struct Command {
    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

#[async_trait]
impl Executable for Command {
    async fn exec(self, _context: Context) -> anyhow::Result<()> {
        match self.command {
            Some(SubCommand::Start) => {
                info!("starting worker process");
                handle_start().await
            }
            Some(SubCommand::Once) => {
                info!("running single worker cycle");
                handle_once().await
            }
            None => anyhow::bail!("missing worker subcommand; use `vigilo worker start`"),
        }
    }
}

async fn handle_start() -> anyhow::Result<()> {
    ServiceRunner::new("worker")
        .tick_interval(Duration::from_secs(5))
        .run_loop(|| async { run_worker_start_cycle().await })
        .await
}

async fn handle_once() -> anyhow::Result<()> {
    run_worker_once_cycle().await
}

async fn run_worker_start_cycle() -> anyhow::Result<()> {
    // TODO: Load worker runtime configuration and lease settings.
    // TODO: Start polling pending executions and claiming work safely.
    // TODO: Process execution attempts with heartbeats and retry transitions.

    warn!("worker start is not implemented yet");

    Ok(())
}

async fn run_worker_once_cycle() -> anyhow::Result<()> {
    // TODO: Claim at most one available execution unit.
    // TODO: Execute one attempt cycle and persist resulting state transitions.
    anyhow::bail!("worker once is not implemented yet")
}
