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

#[async_trait]
impl Executable for Command {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        info!(
            "Setting up Agent Vigilo migrations_dir={}",
            self.migrations_dir.display()
        );

        let db = context.db().await?;

        migrate!("./migrations")
            .run(db)
            .await?;

        info!("Command complete");

        Ok(())
    }
}
