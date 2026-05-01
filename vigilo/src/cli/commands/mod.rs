use crate::context::Context;
use async_trait::async_trait;
use clap::Subcommand;

pub(super) mod evaluators;
pub(super) mod run;
pub(super) mod setup;

use super::Executable;
use super::args;

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Run system setup (install or upgrade)
    Setup(setup::Command),

    /// Manage system evaluators
    Evaluators(evaluators::Command),

    /// Run profiles and datasets
    Run(run::Command),
}

#[async_trait]
impl Executable for Command {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self {
            Command::Evaluators(cmd) => cmd.exec(context).await,
            Command::Run(cmd) => cmd.exec(context).await,
            Command::Setup(cmd) => cmd.exec(context).await,
        }
    }
}
