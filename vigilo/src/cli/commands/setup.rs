//! Environment setup command.
//!
//! The setup command prepares local runtime dependencies required by Vigilo,
//! currently focused on applying database migrations.

use std::path::PathBuf;

use async_trait::async_trait;
use clap::Args;
use tracing::info;

use super::{
    Executable,
    args::parsers::parse_dir,
};
use crate::{
    context::Context,
    db::migrations,
};

#[derive(Debug, Args)]
/// Arguments for `vigilo setup`.
///
/// This command is intended to be safe to re-run; migrations are applied using
/// the existing migration workflow and only pending migrations are executed.
pub(crate) struct Command {
    /// Path to migrations source directory
    #[arg(long, default_value = "migrations", value_parser = parse_dir)]
    pub migrations_dir: PathBuf,
}

#[async_trait]
impl Executable for Command {
    /// Runs setup tasks in sequence.
    ///
    /// Current behavior:
    /// - acquires a database handle from [`Context`]
    /// - executes SQL migrations from `migrations_dir`
    /// - reserves a hook for evaluator bootstrapping (not implemented yet)
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        let db = context.db().await?;

        info!("running database migrations");
        migrations::migrate(db, self.migrations_dir).await?;

        info!("adding evaluators");
        // todo - add evaluators

        Ok(())
    }
}
