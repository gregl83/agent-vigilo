use std::path::PathBuf;

use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use tracing::info;

use crate::context::Context;
use super::args::parsers::parse_dir;
use super::Executable;


#[derive(Debug, Subcommand)]
pub(crate) enum SubCommand {
    /// Add evaluator to system
    Add {
        /// Path to evaluator crate
        #[arg(value_parser = parse_dir)]
        evaluator_path: PathBuf,
    },
    /// Show system evaluator
    Show {
        /// Evaluator name
        #[arg()]
        evaluator_name: String,
    },
    /// Deactivate system evaluator
    Deactivate {
        /// Evaluator name
        #[arg()]
        evaluator_name: String,
    },
    /// Activate system evaluator
    Activate {
        /// Evaluator name
        #[arg()]
        evaluator_name: String,
    },
    /// Remove system evaluator
    Remove {
        /// Evaluator name
        #[arg()]
        evaluator_name: String,
    },
}

#[async_trait]
impl Executable for SubCommand {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self {
            SubCommand::Add{ evaluator_path } => {
                println!("executing run");

                info!("adding evaluator: {}", evaluator_path.display());

                let component = context.wasm().await?.build(
                    evaluator_path
                )?;

                println!("component wasm hash: {}", component.wasm_hash);

                // todo - move wasm code to module

                // Example async work
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("run complete");
                Ok(())
            }
            SubCommand::Show{ evaluator_name } => {
                println!("executing run");

                // todo

                // Example async work
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("run complete");
                Ok(())
            }
            SubCommand::Deactivate{ evaluator_name } => {
                println!("executing run");

                // todo

                // Example async work
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("run complete");
                Ok(())
            }
            SubCommand::Activate{ evaluator_name } => {
                println!("executing run");

                // todo

                // Example async work
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("run complete");
                Ok(())
            }
            SubCommand::Remove{ evaluator_name } => {
                println!("executing run");

                // todo

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
