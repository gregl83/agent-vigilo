use std::{collections::HashSet, path::PathBuf, time::Instant};

use sqlx::{
    PgPool,
    migrate::{Migrate, Migrator},
};
use tracing::{debug, info};

pub(crate) async fn migrate(db: &PgPool, migrations_dir: PathBuf) -> anyhow::Result<()> {
    let migrator = Migrator::new(migrations_dir.as_path()).await?;

    let mut conn = db.acquire().await?;

    debug!("ensuring existence of migrations table");
    conn.ensure_migrations_table().await?;

    debug!("fetching applied migrations");
    let applied_migrations = conn.list_applied_migrations().await?;
    let applied_versions: HashSet<i64> =
        applied_migrations.into_iter().map(|m| m.version).collect();

    debug!("checking for unapplied migrations");
    for migration in migrator.iter() {
        if applied_versions.contains(&migration.version) {
            info!(
                "migration {}: \"{}\" already exists, skipping",
                migration.version, migration.description,
            );
        } else {
            debug!(
                "applying migration {}: {}",
                migration.version, migration.description
            );
            let start = Instant::now();
            match conn.apply(migration).await {
                Ok(_) => {
                    let elapsed = start.elapsed();
                    info!(
                        "migration {}: \"{}\" applied in {:?}",
                        migration.version, migration.description, elapsed,
                    );
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "migration {} failed: {}",
                        migration.version,
                        e
                    ));
                }
            }
        }
    }

    Ok(())
}
