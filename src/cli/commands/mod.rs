use async_trait::async_trait;
use clap::Subcommand;

pub mod placeholder;
pub mod setup;

use super::Executable;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Placeholder and bootstrap operations
    Placeholder(placeholder::Command),

    /// Run system setup (install or upgrade)
    Setup(setup::Command),
}

#[async_trait]
impl Executable for Command {
    async fn exec(self) -> anyhow::Result<()> {
        match self {
            Command::Placeholder(cmd) => cmd.exec().await,
            Command::Setup(cmd) => cmd.exec().await,
        }
    }
}
