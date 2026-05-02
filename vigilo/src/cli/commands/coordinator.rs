use async_trait::async_trait;
use clap::{Args, Subcommand};
use tracing::info;
use super::Executable;
use crate::context::Context;


#[derive(Debug, Subcommand)]
pub(crate) enum SubCommand {
    /// Start a coordinator process
    Start,
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
                // TODO: Scan for finalized runs and compute global gate outcomes.
                // TODO: Advance run lifecycle transitions (running -> finalizing -> completed).
                // TODO: Publish outbox completion events with retry-safe semantics.
                anyhow::bail!("coordinator start is not implemented yet")
            }
            None => anyhow::bail!("missing coordinator subcommand; use `vigilo coordinator start`"),
        }
    }
}
