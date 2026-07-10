//! Database placement table access.
//!
//! The database placement catalog is authoritative for logical database
//! targets. Runtime configuration supplies secret URL values by resolving each
//! row's `database_url_env`.

use sqlx::PgPool;

use crate::models::database_placement::DatabasePlacement;

/// Lists active database placements ordered by alias for deterministic
/// validation and diagnostics.
pub(crate) async fn list_active_database_placements(
    db: &PgPool,
) -> anyhow::Result<Vec<DatabasePlacement>> {
    let placements = sqlx::query_as::<_, DatabasePlacement>(
        r#"
        SELECT alias, database_url_env, role, status, created_at, updated_at
        FROM database_placements
        WHERE status = 'active'
        ORDER BY alias
        "#,
    )
    .fetch_all(db)
    .await?;

    Ok(placements)
}

/// Lists active shard-capable aliases that currently own active shard rows.
///
/// Recovery scans run per execution placement, so aliases without active shard
/// placements do not need a recovery pass.
pub(crate) async fn list_active_shard_database_aliases(db: &PgPool) -> anyhow::Result<Vec<String>> {
    let aliases = sqlx::query_scalar::<_, String>(
        r#"
        SELECT DISTINCT dp.alias
        FROM database_placements dp
        JOIN shard_placements sp
          ON sp.database_alias = dp.alias
        WHERE dp.status = 'active'
          AND dp.role IN ('shard', 'control_and_shard')
          AND sp.status = 'active'
        ORDER BY dp.alias
        "#,
    )
    .fetch_all(db)
    .await?;

    Ok(aliases)
}

/// Counts shard placement rows that route to a disabled database placement.
///
/// The foreign key prevents missing aliases, but it intentionally permits
/// disabling a database placement while historical rows still point at it. The
/// router must reject those rows for normal dispatch.
pub(crate) async fn count_shard_placements_on_disabled_databases(
    db: &PgPool,
) -> anyhow::Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM shard_placements sp
        JOIN database_placements dp
          ON dp.alias = sp.database_alias
        WHERE dp.status <> 'active'
        "#,
    )
    .fetch_one(db)
    .await?;

    Ok(count)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx placement tests"]
    async fn list_active_shard_database_aliases_filters_to_active_routed_shards(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES
                ('shard_001', 'VIGILO_SHARD_001_DATABASE_URL', 'shard', 'active'),
                ('shard_002', 'VIGILO_SHARD_002_DATABASE_URL', 'shard', 'disabled'),
                ('shard_003', 'VIGILO_SHARD_003_DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES
                ($1::uuid, 0, 'primary', 'active'),
                ($1::uuid, 1, 'primary', 'moving'),
                ($1::uuid, 2, 'shard_001', 'active'),
                ($1::uuid, 3, 'shard_002', 'active')
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();

        let aliases = list_active_shard_database_aliases(&pool).await.unwrap();

        assert_eq!(
            aliases,
            vec!["primary".to_string(), "shard_001".to_string()]
        );
    }
}
