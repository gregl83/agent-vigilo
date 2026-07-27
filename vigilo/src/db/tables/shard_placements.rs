//! Shard placement table access.
//!
//! Shard placements are the durable routing decisions from a logical
//! `run_id + run_shard` pair to a database placement alias.

use sqlx::{
    Executor,
    PgPool,
    Postgres,
};
use uuid::Uuid;

use crate::models::shard_placement::ShardPlacement;

pub(crate) async fn select_shard_placement(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<Option<ShardPlacement>> {
    select_shard_placement_with(db, run_id, run_shard).await
}

pub(crate) async fn select_shard_placement_with<'e, E>(
    executor: E,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<Option<ShardPlacement>>
where
    E: Executor<'e, Database = Postgres>,
{
    let placement = sqlx::query_as::<_, ShardPlacement>(
        r#"
        SELECT run_id, run_shard, database_alias, status, route_version, created_at, updated_at
        FROM shard_placements
        WHERE run_id = $1
          AND run_shard = $2
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .fetch_optional(executor)
    .await?;

    Ok(placement)
}

pub(crate) async fn mark_shard_placement_moving<'e, E>(
    executor: E,
    run_id: Uuid,
    run_shard: i16,
    expected_database_alias: &str,
    expected_route_version: i64,
) -> anyhow::Result<Option<ShardPlacement>>
where
    E: Executor<'e, Database = Postgres>,
{
    let placement = sqlx::query_as::<_, ShardPlacement>(
        r#"
        UPDATE shard_placements
        SET status = 'moving',
            route_version = route_version + 1,
            updated_at = now()
        WHERE run_id = $1::uuid
          AND run_shard = $2
          AND database_alias = $3
          AND status = 'active'
          AND route_version = $4
        RETURNING run_id, run_shard, database_alias, status, route_version, created_at, updated_at
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(expected_database_alias)
    .bind(expected_route_version)
    .fetch_optional(executor)
    .await?;

    Ok(placement)
}

pub(crate) async fn activate_moved_shard_placement<'e, E>(
    executor: E,
    run_id: Uuid,
    run_shard: i16,
    expected_database_alias: &str,
    expected_route_version: i64,
    target_database_alias: &str,
) -> anyhow::Result<Option<ShardPlacement>>
where
    E: Executor<'e, Database = Postgres>,
{
    let placement = sqlx::query_as::<_, ShardPlacement>(
        r#"
        UPDATE shard_placements
        SET database_alias = $5,
            status = 'active',
            route_version = route_version + 1,
            updated_at = now()
        WHERE run_id = $1::uuid
          AND run_shard = $2
          AND database_alias = $3
          AND status = 'moving'
          AND route_version = $4
        RETURNING run_id, run_shard, database_alias, status, route_version, created_at, updated_at
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(expected_database_alias)
    .bind(expected_route_version)
    .bind(target_database_alias)
    .fetch_optional(executor)
    .await?;

    Ok(placement)
}

pub(crate) async fn change_empty_active_shard_placement<'e, E>(
    executor: E,
    run_id: Uuid,
    run_shard: i16,
    expected_database_alias: &str,
    expected_route_version: i64,
    target_database_alias: &str,
) -> anyhow::Result<Option<ShardPlacement>>
where
    E: Executor<'e, Database = Postgres>,
{
    let placement = sqlx::query_as::<_, ShardPlacement>(
        r#"
        UPDATE shard_placements
        SET database_alias = $5,
            route_version = route_version + 1,
            updated_at = now()
        WHERE run_id = $1::uuid
          AND run_shard = $2
          AND database_alias = $3
          AND status = 'active'
          AND route_version = $4
        RETURNING run_id, run_shard, database_alias, status, route_version, created_at, updated_at
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(expected_database_alias)
    .bind(expected_route_version)
    .bind(target_database_alias)
    .fetch_optional(executor)
    .await?;

    Ok(placement)
}

pub(crate) async fn list_shard_placements_for_run(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<Vec<ShardPlacement>> {
    let placements = sqlx::query_as::<_, ShardPlacement>(
        r#"
        SELECT run_id, run_shard, database_alias, status, route_version, created_at, updated_at
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

pub(crate) async fn insert_active_shard_placement<'e, E>(
    executor: E,
    run_id: Uuid,
    run_shard: i16,
    database_alias: &str,
) -> anyhow::Result<ShardPlacement>
where
    E: Executor<'e, Database = Postgres>,
{
    let placement = sqlx::query_as::<_, ShardPlacement>(
        r#"
        INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
        VALUES ($1::uuid, $2, $3, 'active')
        RETURNING run_id, run_shard, database_alias, status, route_version, created_at, updated_at
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(database_alias)
    .fetch_one(executor)
    .await?;

    Ok(placement)
}

pub(crate) async fn update_shard_placement_status(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    status: &str,
) -> anyhow::Result<Option<ShardPlacement>> {
    let placement = sqlx::query_as::<_, ShardPlacement>(
        r#"
        UPDATE shard_placements
        SET status = $3,
            route_version = CASE
                WHEN status IS DISTINCT FROM $3
                THEN route_version + 1
                ELSE route_version
            END,
            updated_at = now()
        WHERE run_id = $1::uuid
          AND run_shard = $2
        RETURNING run_id, run_shard, database_alias, status, route_version, created_at, updated_at
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(status)
    .fetch_optional(db)
    .await?;

    Ok(placement)
}

pub(crate) async fn update_shard_placement_alias_and_status(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    database_alias: &str,
    status: &str,
) -> anyhow::Result<Option<ShardPlacement>> {
    let placement = sqlx::query_as::<_, ShardPlacement>(
        r#"
        UPDATE shard_placements
        SET database_alias = $3,
            status = $4,
            route_version = CASE
                WHEN database_alias IS DISTINCT FROM $3
                  OR status IS DISTINCT FROM $4
                THEN route_version + 1
                ELSE route_version
            END,
            updated_at = now()
        WHERE run_id = $1::uuid
          AND run_shard = $2
        RETURNING run_id, run_shard, database_alias, status, route_version, created_at, updated_at
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(database_alias)
    .bind(status)
    .fetch_optional(db)
    .await?;

    Ok(placement)
}
