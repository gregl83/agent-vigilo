//! Shard placement administration workflows.
//!
//! These helpers keep shard topology guardrails behind the database workflow
//! boundary. CLI commands call them to list database placements, add shard
//! databases, disable shard databases, assign empty run shards, and move
//! shard-owned rows between placements.

use serde::Serialize;
use serde_json::Value;
use sqlx::{
    PgPool,
    types::Json,
};
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
            SHARD_PLACEMENT_STATUS_MOVING,
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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShardMoveTableReport {
    pub(crate) table: &'static str,
    pub(crate) source_row_count: i64,
    pub(crate) target_row_count: i64,
    pub(crate) copied_row_count: u64,
    pub(crate) source_checksum: String,
    pub(crate) target_checksum: String,
    pub(crate) verified: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShardMoveOutcome {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) source_database_alias: String,
    pub(crate) target_database_alias: String,
    pub(crate) dry_run: bool,
    pub(crate) verify_only: bool,
    pub(crate) forced: bool,
    pub(crate) active_work_count: i64,
    pub(crate) moved: bool,
    pub(crate) placement: ShardPlacement,
    pub(crate) tables: Vec<ShardMoveTableReport>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShardRouteInspection {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) database_alias: String,
    pub(crate) shard_placement_status: String,
    pub(crate) database_role: String,
    pub(crate) database_status: String,
    pub(crate) database_url_env: String,
    pub(crate) database_url_env_resolved: bool,
    pub(crate) dispatchable: bool,
    pub(crate) readable: bool,
    pub(crate) routing_decision: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ShardMoveOptions {
    pub(crate) dry_run: bool,
    pub(crate) verify_only: bool,
    pub(crate) force: bool,
}

#[derive(Debug, Clone, Copy)]
struct ShardTable {
    name: &'static str,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct TableFingerprint {
    row_count: i64,
    checksum: String,
}

const PREREQUISITE_TABLES: &[&str] = &["dataset_versions", "runs"];
const SHARD_TABLES: &[ShardTable] = &[
    ShardTable { name: "run_chunks" },
    ShardTable {
        name: "run_snapshots",
    },
    ShardTable { name: "executions" },
    ShardTable {
        name: "execution_attempts",
    },
    ShardTable {
        name: "execution_aggregates",
    },
    ShardTable {
        name: "evaluator_results",
    },
    ShardTable {
        name: "run_shard_summaries",
    },
];

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

/// Inspects the persisted route for one run shard.
///
/// This reads only control-plane metadata. It reports whether the current
/// process can resolve the placement URL env var, but never returns the URL
/// value.
pub(crate) async fn inspect_shard_route(
    database: &database::Db,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<ShardRouteInspection> {
    validate_run_shard(run_shard)?;

    let control_db = database.control().await?;
    let placement = shard_placements::select_shard_placement(control_db, run_id, run_shard)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "shard placement for run {} shard {} was not found",
                run_id,
                run_shard
            )
        })?;
    let database_placement =
        database_placements::select_database_placement(control_db, &placement.database_alias)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "database placement alias {} was not found",
                    placement.database_alias
                )
            })?;

    let database_active = database_placement.status == DATABASE_PLACEMENT_STATUS_ACTIVE;
    let shard_capable = database_placement.is_shard_capable();
    let dispatchable = placement.is_dispatchable() && database_active && shard_capable;
    let readable = database_active && shard_capable;
    let routing_decision = if dispatchable {
        "dispatchable"
    } else if readable {
        "read_only"
    } else {
        "blocked"
    };

    Ok(ShardRouteInspection {
        run_id,
        run_shard,
        database_alias: placement.database_alias,
        shard_placement_status: placement.status,
        database_role: database_placement.role,
        database_status: database_placement.status,
        database_url_env_resolved: database
            .database_url_env_is_resolved(&database_placement.database_url_env),
        database_url_env: database_placement.database_url_env,
        dispatchable,
        readable,
        routing_decision,
    })
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
    database
        .invalidate_execution_placement(run_id, run_shard)
        .await;

    Ok(ShardPlacementSetOutcome {
        placement,
        previous_database_alias,
        changed,
    })
}

pub(crate) async fn move_shard_placement(
    database: &database::Db,
    run_id: Uuid,
    run_shard: i16,
    target_database_alias: &str,
    options: ShardMoveOptions,
) -> anyhow::Result<ShardMoveOutcome> {
    validate_run_shard(run_shard)?;
    validate_non_empty(target_database_alias, "target database alias")?;

    if options.dry_run && options.verify_only {
        anyhow::bail!("--dry-run and --verify-only cannot be used together");
    }

    let control_db = database.control().await?;
    let current = shard_placements::select_shard_placement(control_db, run_id, run_shard)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "shard placement for run {} shard {} was not found",
                run_id,
                run_shard
            )
        })?;

    validate_target_placement(control_db, target_database_alias).await?;

    if current.database_alias == target_database_alias && !options.verify_only {
        anyhow::bail!(
            "run {} shard {} already routes to database placement {}",
            run_id,
            run_shard,
            target_database_alias
        );
    }

    let source_db = database.placement(&current.database_alias).await?;
    let target_db = database.placement(target_database_alias).await?;
    let marked_moving =
        !options.dry_run && !options.verify_only && current.status == SHARD_PLACEMENT_STATUS_ACTIVE;

    if marked_moving {
        mark_shard_moving(control_db, run_id, run_shard).await?;
        database
            .invalidate_execution_placement(run_id, run_shard)
            .await;
    }

    let active_work_count = count_active_shard_work(source_db, run_id, run_shard).await?;

    if active_work_count > 0 && !options.force && !options.verify_only {
        anyhow::bail!(
            "run {} shard {} has {} leased/running row(s); wait for work to drain or pass --force",
            run_id,
            run_shard,
            active_work_count
        );
    }

    let mut copied_rows_by_table = std::collections::BTreeMap::<&'static str, u64>::new();

    if !options.dry_run && !options.verify_only {
        for table in PREREQUISITE_TABLES {
            let rows = select_prerequisite_rows(source_db, table, run_id).await?;
            let copied = copy_json_rows(target_db, table, rows).await?;
            copied_rows_by_table.insert(table, copied);
        }

        for table in SHARD_TABLES {
            let rows = select_shard_rows(source_db, table.name, run_id, run_shard).await?;
            let copied = copy_json_rows(target_db, table.name, rows).await?;
            copied_rows_by_table.insert(table.name, copied);
        }
    }

    let reports = verify_shard_tables(
        source_db,
        target_db,
        run_id,
        run_shard,
        &copied_rows_by_table,
    )
    .await?;
    let verified = reports.iter().all(|report| report.verified);

    if !verified && !options.dry_run {
        anyhow::bail!(
            "run {} shard {} copy verification failed; source rows were retained and placement was not switched",
            run_id,
            run_shard
        );
    }

    let placement = if options.dry_run || options.verify_only {
        current.clone()
    } else {
        let placement = shard_placements::update_shard_placement_alias_and_status(
            control_db,
            run_id,
            run_shard,
            target_database_alias,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "shard placement for run {} shard {} was not found",
                run_id,
                run_shard
            )
        })?;
        database
            .invalidate_execution_placement(run_id, run_shard)
            .await;
        placement
    };

    Ok(ShardMoveOutcome {
        run_id,
        run_shard,
        source_database_alias: current.database_alias,
        target_database_alias: target_database_alias.to_string(),
        dry_run: options.dry_run,
        verify_only: options.verify_only,
        forced: options.force,
        active_work_count,
        moved: !(options.dry_run || options.verify_only),
        placement,
        tables: reports,
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

async fn validate_target_placement(db: &PgPool, alias: &str) -> anyhow::Result<()> {
    let target = database_placements::select_database_placement(db, alias)
        .await?
        .ok_or_else(|| anyhow::anyhow!("database placement alias {} was not found", alias))?;

    if target.status != DATABASE_PLACEMENT_STATUS_ACTIVE {
        anyhow::bail!(
            "database placement alias {} has status {}, which cannot receive moved shard rows",
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

    Ok(())
}

async fn mark_shard_moving(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<ShardPlacement> {
    shard_placements::update_shard_placement_status(
        db,
        run_id,
        run_shard,
        SHARD_PLACEMENT_STATUS_MOVING,
    )
    .await?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "shard placement for run {} shard {} was not found",
            run_id,
            run_shard
        )
    })
}

async fn count_active_shard_work(db: &PgPool, run_id: Uuid, run_shard: i16) -> anyhow::Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT (
            SELECT COUNT(*)::bigint
            FROM run_chunks
            WHERE run_id = $1::uuid
              AND run_shard = $2
              AND status = 'leased'
        ) + (
            SELECT COUNT(*)::bigint
            FROM execution_attempts
            WHERE run_id = $1::uuid
              AND run_shard = $2
              AND status = 'running'::attempt_status
        )
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .fetch_one(db)
    .await?;

    Ok(count)
}

async fn select_prerequisite_rows(
    db: &PgPool,
    table: &str,
    run_id: Uuid,
) -> anyhow::Result<Vec<Value>> {
    let sql = match table {
        "dataset_versions" => {
            r#"
            SELECT to_jsonb(dv) AS row
            FROM dataset_versions dv
            JOIN runs r
              ON r.dataset_version_id = dv.dataset_version_id
            WHERE r.id = $1::uuid
            "#
        }
        "runs" => {
            r#"
            SELECT to_jsonb(r) AS row
            FROM runs r
            WHERE r.id = $1::uuid
            "#
        }
        _ => anyhow::bail!("unsupported prerequisite table {}", table),
    };

    let rows = sqlx::query_scalar::<_, Value>(sql)
        .bind(run_id)
        .fetch_all(db)
        .await?;

    Ok(rows)
}

async fn select_shard_rows(
    db: &PgPool,
    table: &str,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<Vec<Value>> {
    let sql = format!(
        r#"
        SELECT to_jsonb(t) AS row
        FROM {table} t
        WHERE run_id = $1::uuid
          AND run_shard = $2
        "#
    );

    let rows = sqlx::query_scalar::<_, Value>(&sql)
        .bind(run_id)
        .bind(run_shard)
        .fetch_all(db)
        .await?;

    Ok(rows)
}

async fn copy_json_rows(db: &PgPool, table: &str, rows: Vec<Value>) -> anyhow::Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }

    let sql = format!(
        r#"
        INSERT INTO {table}
        SELECT *
        FROM jsonb_populate_recordset(NULL::{table}, $1::jsonb)
        ON CONFLICT DO NOTHING
        "#
    );

    let result = sqlx::query(&sql)
        .bind(Json(Value::Array(rows)))
        .execute(db)
        .await?;

    Ok(result.rows_affected())
}

async fn verify_shard_tables(
    source_db: &PgPool,
    target_db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    copied_rows_by_table: &std::collections::BTreeMap<&'static str, u64>,
) -> anyhow::Result<Vec<ShardMoveTableReport>> {
    let mut reports = Vec::with_capacity(SHARD_TABLES.len());

    for table in SHARD_TABLES {
        let source = table_fingerprint(source_db, table.name, run_id, run_shard).await?;
        let target = table_fingerprint(target_db, table.name, run_id, run_shard).await?;
        let verified = source.row_count == target.row_count && source.checksum == target.checksum;
        reports.push(ShardMoveTableReport {
            table: table.name,
            source_row_count: source.row_count,
            target_row_count: target.row_count,
            copied_row_count: copied_rows_by_table
                .get(table.name)
                .copied()
                .unwrap_or_default(),
            source_checksum: source.checksum,
            target_checksum: target.checksum,
            verified,
        });
    }

    Ok(reports)
}

async fn table_fingerprint(
    db: &PgPool,
    table: &str,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<TableFingerprint> {
    let sql = format!(
        r#"
        SELECT
            COUNT(*)::bigint AS row_count,
            COALESCE(md5(string_agg(row_json, E'\n' ORDER BY row_json)), '') AS checksum
        FROM (
            SELECT to_jsonb(t)::text AS row_json
            FROM {table} t
            WHERE run_id = $1::uuid
              AND run_shard = $2
        ) rows
        "#
    );

    let fingerprint = sqlx::query_as::<_, TableFingerprint>(&sql)
        .bind(run_id)
        .bind(run_shard)
        .fetch_one(db)
        .await?;

    Ok(fingerprint)
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
    use tokio::sync::OnceCell;

    use super::*;
    use crate::{
        context::database::{
            Db,
            PlacementConfig,
            new_shard_placement_cache,
        },
        models::database_placement::{
            DEFAULT_DATABASE_ALIAS,
            DEFAULT_DATABASE_URL_ENV,
        },
    };

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

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn inspect_shard_route_reports_dispatchable_primary_route(pool: PgPool) {
        let database_url = std::env::var(DEFAULT_DATABASE_URL_ENV).unwrap();
        let database = context_with_control_pool(pool.clone(), database_url);
        let run_id = Uuid::now_v7();

        sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1::uuid, 4, 'primary', 'active')
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();

        let route = inspect_shard_route(&database, run_id, 4).await.unwrap();

        assert_eq!(route.database_alias, "primary");
        assert_eq!(route.database_role, "control_and_shard");
        assert_eq!(route.database_url_env, DEFAULT_DATABASE_URL_ENV);
        assert!(route.database_url_env_resolved);
        assert!(route.dispatchable);
        assert!(route.readable);
        assert_eq!(route.routing_decision, "dispatchable");
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn inspect_shard_route_reports_moving_route_as_read_only(pool: PgPool) {
        let database_url = std::env::var(DEFAULT_DATABASE_URL_ENV).unwrap();
        let database = context_with_control_pool(pool.clone(), database_url);
        let run_id = Uuid::now_v7();

        sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1::uuid, 4, 'primary', 'moving')
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();

        let route = inspect_shard_route(&database, run_id, 4).await.unwrap();

        assert!(!route.dispatchable);
        assert!(route.readable);
        assert_eq!(route.routing_decision, "read_only");
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn inspect_shard_route_reports_disabled_placement_as_blocked(pool: PgPool) {
        let database_url = std::env::var(DEFAULT_DATABASE_URL_ENV).unwrap();
        let database = context_with_control_pool(pool.clone(), database_url);
        let run_id = Uuid::now_v7();

        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_disabled', 'VIGILO_TEST_MISSING_SHARD_URL', 'shard', 'disabled')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1::uuid, 4, 'shard_disabled', 'active')
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();

        let route = inspect_shard_route(&database, run_id, 4).await.unwrap();

        assert_eq!(route.database_alias, "shard_disabled");
        assert_eq!(route.database_status, "disabled");
        assert!(!route.database_url_env_resolved);
        assert!(!route.dispatchable);
        assert!(!route.readable);
        assert_eq!(route.routing_decision, "blocked");
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn move_shard_placement_switches_alias_after_verification(pool: PgPool) {
        let database_url = std::env::var(DEFAULT_DATABASE_URL_ENV).unwrap();
        let database = context_with_control_pool(pool.clone(), database_url);
        let run_id = Uuid::now_v7();
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        let chunk_id = Uuid::now_v7();

        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        seed_run(&pool, run_id, dataset_id, dataset_version_id).await;
        sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1::uuid, 3, 'primary', 'active')
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        seed_run_snapshot(&pool, run_id, 3, dataset_id, dataset_version_id).await;
        sqlx::query(
            r#"
            INSERT INTO run_chunks (
                id,
                run_id,
                run_shard,
                dataset_version_id,
                profile_group_id,
                ordinal_start,
                ordinal_end,
                status
            )
            VALUES ($1::uuid, $2::uuid, 3, $3::uuid, 'default', 0, 1, 'completed')
            "#,
        )
        .bind(chunk_id)
        .bind(run_id)
        .bind(dataset_version_id)
        .execute(&pool)
        .await
        .unwrap();

        let outcome = move_shard_placement(
            &database,
            run_id,
            3,
            "shard_001",
            ShardMoveOptions {
                dry_run: false,
                verify_only: false,
                force: false,
            },
        )
        .await
        .unwrap();

        assert!(outcome.moved);
        assert!(outcome.tables.iter().all(|table| table.verified));
        assert_eq!(outcome.placement.database_alias, "shard_001");
        assert_eq!(outcome.placement.status, SHARD_PLACEMENT_STATUS_ACTIVE);
    }

    #[test]
    fn validate_run_shard_rejects_out_of_range_values() {
        assert!(validate_run_shard(0).is_ok());
        assert!(validate_run_shard(127).is_ok());
        assert!(validate_run_shard(-1).is_err());
        assert!(validate_run_shard(128).is_err());
    }

    fn context_with_control_pool(pool: PgPool, uri: String) -> Db {
        let context = Db {
            uri,
            max_connections: 5,
            placement_config: PlacementConfig::default_single_database(),
            cell: OnceCell::new(),
            placement_catalog: OnceCell::new(),
            shard_placement_cache: new_shard_placement_cache(),
        };
        assert!(context.cell.set(pool).is_ok());
        context
    }

    async fn seed_run(pool: &PgPool, run_id: Uuid, dataset_id: Uuid, dataset_version_id: Uuid) {
        sqlx::query(
            r#"
            INSERT INTO dataset_versions (dataset_version_id, dataset_id, dataset_version)
            VALUES ($1::uuid, $2::uuid, 'dataset')
            "#,
        )
        .bind(dataset_version_id)
        .bind(dataset_id)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO runs (
                id,
                run_key,
                dataset_id,
                dataset_version_id,
                dataset_version,
                evaluation_profile_id,
                evaluation_profile_version,
                profile_version_id,
                profile_hash,
                aggregation_policy_id,
                aggregation_policy_version,
                aggregation_policy_hash,
                agent_provider,
                agent_name,
                prompt_config_id,
                prompt_config_version,
                status,
                expected_execution_count
            )
            VALUES (
                $1::uuid,
                $2,
                $3::uuid,
                $4::uuid,
                'dataset',
                'profile',
                '1.0.0',
                'profile-version',
                'profile-hash',
                'aggregation',
                '1.0.0',
                'aggregation-hash',
                'example',
                'agent',
                'prompt',
                '1.0.0',
                'running'::run_status,
                1
            )
            "#,
        )
        .bind(run_id)
        .bind(format!("run-{run_id}"))
        .bind(dataset_id)
        .bind(dataset_version_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_run_snapshot(
        pool: &PgPool,
        run_id: Uuid,
        run_shard: i16,
        dataset_id: Uuid,
        dataset_version_id: Uuid,
    ) {
        sqlx::query(
            r#"
            INSERT INTO run_snapshots (
                run_id,
                run_shard,
                run_key,
                dataset_id,
                dataset_version_id,
                dataset_version,
                evaluation_profile_id,
                evaluation_profile_version,
                profile_version_id,
                profile_hash,
                aggregation_policy_id,
                aggregation_policy_version,
                aggregation_policy_hash,
                agent_provider,
                agent_name,
                prompt_config_id,
                prompt_config_version,
                expected_execution_count
            )
            VALUES (
                $1::uuid,
                $2,
                'run-key',
                $3::uuid,
                $4::uuid,
                'dataset',
                'profile',
                '1.0.0',
                'profile-version',
                'profile-hash',
                'aggregation',
                '1.0.0',
                'aggregation-hash',
                'example',
                'agent',
                'prompt',
                '1.0.0',
                1
            )
            "#,
        )
        .bind(run_id)
        .bind(run_shard)
        .bind(dataset_id)
        .bind(dataset_version_id)
        .execute(pool)
        .await
        .unwrap();
    }
}
