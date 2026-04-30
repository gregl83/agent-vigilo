use std::path::PathBuf;

use async_trait::async_trait;
use clap::Args;
use tracing::info;

use super::Executable;
use super::args::parsers::parse_dir;
use crate::context::Context;
use crate::db::migrations;

#[derive(Debug, Args)]
pub(crate) struct Command {
    /// Path to migrations source directory
    #[arg(long, default_value = "migrations", value_parser = parse_dir)]
    pub migrations_dir: PathBuf,
}

#[async_trait]
impl Executable for Command {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        let db = context.db().await?;

        info!("running database migrations");
        migrations::migrate(db, self.migrations_dir).await?;

        info!("adding evaluators");
        // todo - add evaluators

        Ok(())
    }
}
