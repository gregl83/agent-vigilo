//! Shard placement table access.
//!
//! Shard placements are the durable routing decisions from a logical
//! `run_id + run_shard` pair to a database placement alias.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::shard_placement::ShardPlacement;

pub(crate) async fn select_shard_placement(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<Option<ShardPlacement>> {
    let placement = sqlx::query_as::<_, ShardPlacement>(
        r#"
        SELECT run_id, run_shard, database_alias, status, created_at, updated_at
        FROM shard_placements
        WHERE run_id = $1
          AND run_shard = $2
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .fetch_optional(db)
    .await?;

    Ok(placement)
}

pub(crate) async fn list_shard_placements_for_run(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<Vec<ShardPlacement>> {
    let placements = sqlx::query_as::<_, ShardPlacement>(
        r#"
        SELECT run_id, run_shard, database_alias, status, created_at, updated_at
        FROM shard_placements
        WHERE run_id = $1
        ORDER BY run_shard
        "#,
    )
    .bind(run_id)
    .fetch_all(db)
    .await?;

    Ok(placements)
}
