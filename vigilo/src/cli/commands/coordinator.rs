use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use tracing::info;

use super::Executable;
use crate::{
    context::Context,
    runtime::ServiceRunner,
};

#[derive(Debug, Subcommand)]
pub(crate) enum SubCommand {
    /// Start a coordinator process
    Start,

    /// Run one coordinator cycle and exit
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
                info!("starting coordinator process");
                handle_start().await
            }
            Some(SubCommand::Once) => {
                info!("running single coordinator cycle");
                handle_once().await
            }
            None => anyhow::bail!("missing coordinator subcommand; use `vigilo coordinator start`"),
        }
    }
}

async fn handle_start() -> anyhow::Result<()> {
    ServiceRunner::new("coordinator")
        .run_loop(|| async { run_coordinator_start_cycle().await })
        .await
}

async fn handle_once() -> anyhow::Result<()> {
    run_coordinator_once_cycle().await
}

async fn run_coordinator_start_cycle() -> anyhow::Result<()> {
    // TODO: Scan for finalized runs and compute global gate outcomes.
    // TODO: Advance run lifecycle transitions (running -> finalizing -> completed).
    // TODO: Publish outbox completion events with retry-safe semantics.
    anyhow::bail!("coordinator start is not implemented yet")
}

async fn run_coordinator_once_cycle() -> anyhow::Result<()> {
    // TODO: Perform one pass of run-finalization and outbox publication work.
    // TODO: Exit after one bounded cycle suitable for cron/batch invocation.
    anyhow::bail!("coordinator once is not implemented yet")
}
