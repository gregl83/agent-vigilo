//! Schema migration runner.
//!
//! The runtime uses this module to apply the SQL migrations in order while
//! logging skipped and newly applied versions. All schema shape remains in the
//! migration files; this module only coordinates sqlx migration execution.

use std::{
    collections::HashSet,
    path::PathBuf,
    time::Instant,
};

use sqlx::{
    PgPool,
    migrate::{
        Migrate,
        Migrator,
    },
};
use tracing::{
    debug,
    info,
};

/// Applies all migrations from `migrations_dir` that are not already recorded.
///
/// The function acquires a database connection, ensures sqlx's migration table
/// exists, checks applied versions, and applies each missing migration with
/// per-migration logging.
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

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::models::{
        database_placement::{
            DEFAULT_DATABASE_ALIAS,
            DEFAULT_DATABASE_URL_ENV,
            DatabasePlacement,
        },
        shard_placement::ShardPlacement,
    };

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn migrations_seed_primary_database_placement(pool: PgPool) {
        let placement = sqlx::query_as::<_, DatabasePlacement>(
            r#"
            SELECT alias, database_url_env, role, status, created_at, updated_at
            FROM database_placements
            WHERE alias = $1
            "#,
        )
        .bind(DEFAULT_DATABASE_ALIAS)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(placement.alias, DEFAULT_DATABASE_ALIAS);
        assert_eq!(placement.database_url_env, DEFAULT_DATABASE_URL_ENV);
        assert_eq!(placement.role, "control_and_shard");
        assert_eq!(placement.status, "active");
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn database_placements_allow_only_one_active_control_placement(pool: PgPool) {
        let second_active_control = sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('control_002', 'VIGILO_CONTROL_002_DATABASE_URL', 'control', 'active')
            "#,
        )
        .execute(&pool)
        .await;
        assert!(second_active_control.is_err());

        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('control_disabled', 'VIGILO_DISABLED_CONTROL_DATABASE_URL', 'control', 'disabled')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'VIGILO_SHARD_001_DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn shard_placements_enforce_alias_status_and_shard_range(pool: PgPool) {
        let run_id = Uuid::now_v7();

        let placement = sqlx::query_as::<_, ShardPlacement>(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1::uuid, 42, 'primary', 'active')
            RETURNING run_id, run_shard, database_alias, status, created_at, updated_at
            "#,
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(placement.run_id, run_id);
        assert_eq!(placement.run_shard, 42);
        assert_eq!(placement.database_alias, DEFAULT_DATABASE_ALIAS);
        assert_eq!(placement.status, "active");

        let invalid_shard = sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1::uuid, 128, 'primary', 'active')
            "#,
        )
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await;
        assert!(invalid_shard.is_err());

        let invalid_alias = sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1::uuid, 1, 'missing', 'active')
            "#,
        )
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await;
        assert!(invalid_alias.is_err());

        let invalid_status = sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1::uuid, 2, 'primary', 'paused')
            "#,
        )
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await;
        assert!(invalid_status.is_err());
    }
}
