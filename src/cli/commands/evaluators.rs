use std::path::PathBuf;

use async_trait::async_trait;
use clap::{Args, Subcommand};

use crate::context::Context;
use super::args::parsers::parse_filepath;
use super::Executable;



#[derive(Debug, Subcommand)]
pub(crate) enum SubCommand {
    /// Add evaluator to system
    Add {
        /// Path to evaluator
        #[arg(value_parser = parse_filepath)]
        evaluator_path: PathBuf,
    },
}

#[async_trait]
impl Executable for SubCommand {
    async fn exec(self, _context: Context) -> anyhow::Result<()> {
        match self {
            SubCommand::Add{ evaluator_path } => {
                println!("executing run");

                // Example async work
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("run complete");
                Ok(())
            }
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct Command {
    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

#[async_trait]
impl Executable for Command {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self.command {
            Some(subcommand) => subcommand.exec(context).await,
            None => {

                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("Command complete");
                Ok(())
            }
        }
    }
}
