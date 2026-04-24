use std::{
    collections::HashSet,
    path::PathBuf,
    time::Instant,
};

use async_trait::async_trait;
use clap::Args;
use sqlx::{
    migrate::{
        Migrate,
        Migrator,
    },
    PgPool,
};
use tracing::{
    debug,
    info,
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

async fn db_migrate(db: &PgPool, migrations_dir: PathBuf) -> anyhow::Result<()> {
    let migrator = Migrator::new(
        migrations_dir.as_path()
    ).await?;

    let mut conn = db.acquire().await?;

    debug!("ensuring existence of migrations table");
    conn.ensure_migrations_table().await?;

    debug!("fetching applied migrations");
    let applied_migrations = conn.list_applied_migrations().await?;
    let applied_versions: HashSet<i64> = applied_migrations
        .into_iter()
        .map(|m| m.version)
        .collect();

    debug!("checking for unapplied migrations");
    for migration in migrator.iter() {
        if applied_versions.contains(&migration.version) {
            info!(
                "migration {}: \"{}\" already exists, skipping",
                migration.version,
                migration.description,
            );
        } else {
            debug!("applying migration {}: {}", migration.version, migration.description);
            let start = Instant::now();
            match conn.apply(migration).await {
                Ok(_) => {
                    let elapsed = start.elapsed();
                    info!(
                        "migration {}: \"{}\" applied in {:?}",
                        migration.version,
                        migration.description,
                        elapsed,
                    );
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("migration {} failed: {}", migration.version, e));
                }
            }
        }
    }

    Ok(())
}

#[async_trait]
impl Executable for Command {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        let db = context.db().await?;

        info!("running database migrations");
        db_migrate(db, self.migrations_dir).await?;

        info!("installing evaluations");
        // todo - install evaluations

        Ok(())
    }
}
