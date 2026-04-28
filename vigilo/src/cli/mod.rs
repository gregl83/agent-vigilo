use std::path::PathBuf;

use async_trait::async_trait;
use clap::{
    ArgAction,
    crate_description,
    crate_version,
    Parser,
};

mod args;
mod commands;
use commands::Command;
use super::context::Context;


#[async_trait]
pub(super) trait Executable {
    async fn exec(self, context: Context) -> anyhow::Result<()>;
}

#[derive(Debug, Parser)]
#[command(
    name = "agent-vigilo",
    version = crate_version!(),
    about = crate_description!(),
    long_about = None
)]
pub(crate) struct App {
    /// Path to config file
    #[arg(global = true, long)]
    pub config: Option<PathBuf>,

    /// Database URI (connection string)
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: String,

    /// Output as JSON when supported
    #[arg(global = true, long, action = ArgAction::SetTrue)]
    pub json: bool,

    /// Suppress all diagnostic output and progress messages
    #[arg(global = true, short, long, default_value_t = false)]
    pub quiet: bool,

    /// Increase log verbosity (-v for DEBUG, -vv for TRACE)
    #[arg(global = true, short, long, action = ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[async_trait]
impl Executable for App {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        self.command.exec(context).await
    }
}
