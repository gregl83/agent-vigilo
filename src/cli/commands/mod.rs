use async_trait::async_trait;
use clap::Subcommand;
use crate::context::Context;

pub(super) mod placeholder;
pub(super) mod setup;

use super::args;
use super::Executable;

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Placeholder and bootstrap operations
    Placeholder(placeholder::Command),

    /// Run system setup (install or upgrade)
    Setup(setup::Command),
}

#[async_trait]
impl Executable for Command {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self {
            Command::Placeholder(cmd) => cmd.exec(context).await,
            Command::Setup(cmd) => cmd.exec(context).await,
        }
    }
}
