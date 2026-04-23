use std::path::PathBuf;

use async_trait::async_trait;
use clap::{Args, Subcommand};
use crate::context::Context;
use super::Executable;


#[derive(Debug, Subcommand)]
pub(crate) enum SubCommand {
    Do,
}

#[async_trait]
impl Executable for SubCommand {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self {
            SubCommand::Do => {
                println!("Executing run");

                // Example async work
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("Run complete");
                Ok(())
            }
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct Command {
    /// Path to migrations directory
    #[arg(long, default_value = "migrations")]
    pub migrations_dir: PathBuf,

    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

#[async_trait]
impl Executable for Command {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self.command {
            Some(subcommand) => subcommand.exec(context).await,
            None => {
                println!(
                    "Executing command directly with migrations_dir={}",
                    self.migrations_dir.display()
                );

                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("Command complete");
                Ok(())
            }
        }
    }
}
