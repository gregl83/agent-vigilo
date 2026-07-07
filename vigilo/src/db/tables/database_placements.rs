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
