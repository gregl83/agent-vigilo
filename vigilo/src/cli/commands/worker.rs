use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use tracing::info;

use super::Executable;
use crate::context::Context;

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

                // TODO: Load worker runtime configuration and lease settings.
                // TODO: Start polling pending executions and claiming work safely.
                // TODO: Process execution attempts with heartbeats and retry transitions.
                anyhow::bail!("worker start is not implemented yet")
            }
            Some(SubCommand::Once) => {
                info!("running single worker cycle");

                // TODO: Claim at most one available execution unit.
                // TODO: Execute one attempt cycle and persist resulting state transitions.
                anyhow::bail!("worker once is not implemented yet")
            }
            None => anyhow::bail!("missing worker subcommand; use `vigilo worker start`"),
        }
    }
}
