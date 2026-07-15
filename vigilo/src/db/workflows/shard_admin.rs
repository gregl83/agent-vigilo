//! Shard placement administration workflows.
//!
//! These helpers keep shard topology guardrails behind the database workflow
//! boundary. CLI commands call them to list database placements, add shard
//! databases, disable shard databases, and assign empty run shards.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    context::database,
    db::tables::{
        database_placements,
        shard_placements,
    },
    models::{
        database_placement::{
            DATABASE_PLACEMENT_ROLE_SHARD,
            DATABASE_PLACEMENT_STATUS_ACTIVE,
            DATABASE_PLACEMENT_STATUS_DISABLED,
            DatabasePlacement,
        },
        run_chunk::RUN_SHARD_COUNT,
        shard_placement::{
            SHARD_PLACEMENT_STATUS_ACTIVE,
            ShardPlacement,
        },
    },
};

#[derive(Debug, Clone)]
pub(crate) struct ShardPlacementSetOutcome {
    pub(crate) placement: ShardPlacement,
    pub(crate) previous_database_alias: Option<String>,
    pub(crate) changed: bool,
}

pub(crate) async fn list_database_placements(
    db: &PgPool,
) -> anyhow::Result<Vec<DatabasePlacement>> {
    database_placements::list_database_placements(db).await
}

pub(crate) async fn add_shard_database_placement(
    db: &PgPool,
    alias: &str,
    database_url_env: &str,
    defer_env_validation: bool,
) -> anyhow::Result<DatabasePlacement> {
    validate_non_empty(alias, "database alias")?;
    validate_non_empty(database_url_env, "database_url_env")?;

    if database_placements::select_database_placement(db, alias)
        .await?
        .is_some()
    {
        anyhow::bail!("database placement alias {} already exists", alias);
    }

    if !defer_env_validation {
        std::env::var(database_url_env).map_err(|_| {
            anyhow::anyhow!(
                "database_url_env {} is not set in the current process; set it or pass --defer-env-validation",
                database_url_env
            )
        })?;
    }

    database_placements::insert_database_placement(
        db,
        alias,
        database_url_env,
        DATABASE_PLACEMENT_ROLE_SHARD,
        DATABASE_PLACEMENT_STATUS_ACTIVE,
    )
    .await
}

pub(crate) async fn disable_database_placement(
    db: &PgPool,
    alias: &str,
) -> anyhow::Result<DatabasePlacement> {
    validate_non_empty(alias, "database alias")?;

    let Some(existing) = database_placements::select_database_placement(db, alias).await? else {
        anyhow::bail!("database placement alias {} was not found", alias);
    };

    if existing.is_control_capable() && existing.status == DATABASE_PLACEMENT_STATUS_ACTIVE {
        anyhow::bail!(
            "database placement alias {} is control-capable and active; disabling it would remove the control plane",
            alias
        );
    }

    if existing.status == DATABASE_PLACEMENT_STATUS_DISABLED {
        return Ok(existing);
    }

    database_placements::disable_database_placement(db, alias)
        .await?
        .ok_or_else(|| anyhow::anyhow!("database placement alias {} was not found", alias))
}

pub(crate) async fn list_shard_placements(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<Vec<ShardPlacement>> {
    shard_placements::list_shard_placements_for_run(db, run_id).await
}

pub(crate) async fn select_shard_placement(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<Option<ShardPlacement>> {
    validate_run_shard(run_shard)?;
    shard_placements::select_shard_placement(db, run_id, run_shard).await
}

pub(crate) async fn set_shard_placement(
    database: &database::Db,
    run_id: Uuid,
    run_shard: i16,
    database_alias: &str,
) -> anyhow::Result<ShardPlacementSetOutcome> {
    validate_run_shard(run_shard)?;
    validate_non_empty(database_alias, "database alias")?;

    let control_db = database.control().await?;
    let target = database_placements::select_database_placement(control_db, database_alias)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("database placement alias {} was not found", database_alias)
        })?;

    if target.status != DATABASE_PLACEMENT_STATUS_ACTIVE {
        anyhow::bail!(
            "database placement alias {} has status {}, which cannot receive shard placements",
            target.alias,
            target.status
        );
    }

    if !target.is_shard_capable() {
        anyhow::bail!(
            "database placement alias {} has role {}, which is not shard-capable",
            target.alias,
            target.role
        );
    }

    let existing = shard_placements::select_shard_placement(control_db, run_id, run_shard).await?;
    if let Some(existing) = &existing {
        let changing_alias = existing.database_alias != database_alias;
        if existing.status == SHARD_PLACEMENT_STATUS_ACTIVE && changing_alias {
            let source_db = database.placement(&existing.database_alias).await?;
            let row_count = count_shard_owned_rows(source_db, run_id, run_shard).await?;
            if row_count > 0 {
                anyhow::bail!(
                    "run {} shard {} already has {} shard-owned row(s) on {}; use the shard move workflow to change its database placement",
                    run_id,
                    run_shard,
                    row_count,
                    existing.database_alias
                );
            }
        }
    }

    let previous_database_alias = existing
        .as_ref()
        .map(|placement| placement.database_alias.clone());
    let changed = previous_database_alias
        .as_deref()
        .is_none_or(|previous| previous != database_alias);
    let placement = shard_placements::upsert_active_shard_placement(
        control_db,
        run_id,
        run_shard,
        database_alias,
    )
    .await?;

    Ok(ShardPlacementSetOutcome {
        placement,
        previous_database_alias,
        changed,
    })
}

async fn count_shard_owned_rows(db: &PgPool, run_id: Uuid, run_shard: i16) -> anyhow::Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(SUM(row_count), 0)::bigint
        FROM (
            SELECT COUNT(*)::bigint AS row_count FROM run_chunks WHERE run_id = $1::uuid AND run_shard = $2
            UNION ALL
            SELECT COUNT(*)::bigint FROM run_snapshots WHERE run_id = $1::uuid AND run_shard = $2
            UNION ALL
            SELECT COUNT(*)::bigint FROM run_shard_summaries WHERE run_id = $1::uuid AND run_shard = $2
            UNION ALL
            SELECT COUNT(*)::bigint FROM executions WHERE run_id = $1::uuid AND run_shard = $2
            UNION ALL
            SELECT COUNT(*)::bigint FROM execution_attempts WHERE run_id = $1::uuid AND run_shard = $2
            UNION ALL
            SELECT COUNT(*)::bigint FROM execution_aggregates WHERE run_id = $1::uuid AND run_shard = $2
            UNION ALL
            SELECT COUNT(*)::bigint FROM evaluator_results WHERE run_id = $1::uuid AND run_shard = $2
        ) counts
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .fetch_one(db)
    .await?;

    Ok(count)
}

fn validate_run_shard(run_shard: i16) -> anyhow::Result<()> {
    if !(0..RUN_SHARD_COUNT).contains(&run_shard) {
        anyhow::bail!(
            "run_shard {} is outside the supported range 0..{}",
            run_shard,
            RUN_SHARD_COUNT
        );
    }

    Ok(())
}

fn validate_non_empty(value: &str, label: &str) -> anyhow::Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("{} must not be empty", label);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::models::database_placement::DEFAULT_DATABASE_ALIAS;

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn add_shard_database_placement_requires_env_unless_deferred(pool: PgPool) {
        let error = add_shard_database_placement(
            &pool,
            "shard_001",
            "VIGILO_TEST_MISSING_SHARD_URL",
            false,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("is not set"));

        let placement =
            add_shard_database_placement(&pool, "shard_001", "VIGILO_TEST_MISSING_SHARD_URL", true)
                .await
                .unwrap();
        assert_eq!(placement.alias, "shard_001");
        assert_eq!(placement.role, DATABASE_PLACEMENT_ROLE_SHARD);
        assert_eq!(placement.status, DATABASE_PLACEMENT_STATUS_ACTIVE);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn disable_database_placement_rejects_active_control(pool: PgPool) {
        let error = disable_database_placement(&pool, DEFAULT_DATABASE_ALIAS)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("control-capable and active"));
    }

    #[test]
    fn validate_run_shard_rejects_out_of_range_values() {
        assert!(validate_run_shard(0).is_ok());
        assert!(validate_run_shard(127).is_ok());
        assert!(validate_run_shard(-1).is_err());
        assert!(validate_run_shard(128).is_err());
    }
}
