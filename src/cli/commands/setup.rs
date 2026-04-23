use std::path::PathBuf;

use async_trait::async_trait;
use clap::Args;
use sqlx::migrate;
use tracing::{
    info
};
use crate::context::Context;
use super::args::parsers::parse_dir;
use super::Executable;


#[derive(Debug, Args)]
pub(crate) struct Command {
    /// Path to migrations source directory
    #[arg(long, default_value = "migrations", value_parser = parse_dir)]
    pub migrations_dir: PathBuf,
}

fn db_migrate() {
    migrate!("./migrations");
}

#[async_trait]
impl Executable for Command {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        // todo - sqlx migration call

        info!(
            "Setting up Agent Vigilo migrations_dir={}",
            self.migrations_dir.display()
        );

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        info!("Command complete");

        Ok(())
    }
}
