//! Shard placement administration workflows.
//!
//! These helpers keep shard topology guardrails behind the database workflow
//! boundary. CLI commands call them to list database placements, add shard
//! databases, disable shard databases, assign empty run shards, and move
//! shard-owned rows between placements.

use std::collections::BTreeMap;

use chrono::{
    DateTime,
    Utc,
};
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
    pub(crate) route_version: i64,
    pub(crate) database_role: String,
    pub(crate) database_status: String,
    pub(crate) database_url_env: String,
    pub(crate) database_url_env_resolved: bool,
    pub(crate) dispatchable: bool,
    pub(crate) readable: bool,
    pub(crate) routing_decision: &'static str,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub(crate) struct ShardRebalanceOperation {
    pub(crate) id: Uuid,
    pub(crate) strategy: String,
    pub(crate) source_database_alias: Option<String>,
    pub(crate) target_database_alias: String,
    pub(crate) status: String,
    pub(crate) planned_item_count: i32,
    pub(crate) completed_item_count: i32,
    pub(crate) failed_item_count: i32,
    pub(crate) cancelled_item_count: i32,
    pub(crate) error_message: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) started_at: Option<DateTime<Utc>>,
    pub(crate) completed_at: Option<DateTime<Utc>>,
    pub(crate) cancelled_at: Option<DateTime<Utc>>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub(crate) struct ShardRebalanceItem {
    pub(crate) operation_id: Uuid,
    pub(crate) sequence_no: i32,
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) source_database_alias: String,
    pub(crate) target_database_alias: String,
    pub(crate) planned_route_version: i64,
    pub(crate) status: String,
    pub(crate) error_message: Option<String>,
    pub(crate) started_at: Option<DateTime<Utc>>,
    pub(crate) completed_at: Option<DateTime<Utc>>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PlannedShardRebalanceItem {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) source_database_alias: String,
    pub(crate) target_database_alias: String,
    pub(crate) planned_route_version: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct ShardRebalancePlanOptions {
    pub(crate) source_database_alias: Option<String>,
    pub(crate) target_database_alias: String,
    pub(crate) max_items: usize,
    pub(crate) dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShardRebalancePlanOutcome {
    pub(crate) operation: Option<ShardRebalanceOperation>,
    pub(crate) items: Vec<PlannedShardRebalanceItem>,
    pub(crate) dry_run: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ShardRebalanceApplyOptions {
    pub(crate) max_items: usize,
    pub(crate) force: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShardRebalanceApplyOutcome {
    pub(crate) operation: ShardRebalanceOperation,
    pub(crate) processed_items: Vec<ShardRebalanceItem>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShardRebalanceVerifyItem {
    pub(crate) item: ShardRebalanceItem,
    pub(crate) verified: bool,
    pub(crate) tables: Vec<ShardMoveTableReport>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShardRebalanceVerifyOutcome {
    pub(crate) operation: ShardRebalanceOperation,
    pub(crate) items: Vec<ShardRebalanceVerifyItem>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ShardMoveOptions {
    pub(crate) dry_run: bool,
    pub(crate) verify_only: bool,
    pub(crate) force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShardRebalanceCandidate {
    run_id: Uuid,
    run_shard: i16,
    source_database_alias: String,
    route_version: i64,
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

const PREREQUISITE_TABLES: &[&str] = &[
    "case_blobs",
    "dataset_versions",
    "dataset_version_cases",
    "runs",
];
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

const REBALANCE_STRATEGY_DRAIN_SOURCE: &str = "drain-source";
const REBALANCE_STRATEGY_FILL_TARGET: &str = "fill-target";
const REBALANCE_OPERATION_STATUS_RUNNING: &str = "running";
const REBALANCE_OPERATION_STATUS_COMPLETED: &str = "completed";
const REBALANCE_OPERATION_STATUS_CANCELLED: &str = "cancelled";
const REBALANCE_OPERATION_STATUS_FAILED: &str = "failed";
const REBALANCE_ITEM_STATUS_COMPLETED: &str = "completed";
const REBALANCE_ITEM_STATUS_FAILED: &str = "failed";

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
        route_version: placement.route_version,
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

/// Creates a persisted rebalance plan or returns the same plan without writing.
///
/// The plan records only intended moves. Applying the plan still uses
/// `move_shard_placement` for each item, so copy, verification, and route
/// fencing remain centralized in the single-shard move workflow.
pub(crate) async fn plan_shard_rebalance(
    database: &database::Db,
    options: ShardRebalancePlanOptions,
) -> anyhow::Result<ShardRebalancePlanOutcome> {
    validate_non_empty(&options.target_database_alias, "target database alias")?;
    if options.max_items == 0 {
        anyhow::bail!("max_items must be greater than zero");
    }

    let control_db = database.control().await?;
    validate_target_placement(control_db, &options.target_database_alias).await?;
    if let Some(source_alias) = &options.source_database_alias {
        validate_source_placement(control_db, source_alias).await?;
        if source_alias == &options.target_database_alias {
            anyhow::bail!("source and target database aliases must differ");
        }
    }

    let active_aliases = database
        .active_shard_capable_database_aliases()
        .await?
        .into_iter()
        .filter(|alias| options.source_database_alias.as_ref() != Some(alias))
        .collect::<Vec<_>>();
    let candidates = select_rebalance_candidates(
        list_active_rebalance_candidates(control_db).await?,
        &active_aliases,
        options.source_database_alias.as_deref(),
        &options.target_database_alias,
        options.max_items,
    );
    let items = candidates
        .into_iter()
        .map(|candidate| PlannedShardRebalanceItem {
            run_id: candidate.run_id,
            run_shard: candidate.run_shard,
            source_database_alias: candidate.source_database_alias,
            target_database_alias: options.target_database_alias.clone(),
            planned_route_version: candidate.route_version,
        })
        .collect::<Vec<_>>();

    if options.dry_run {
        return Ok(ShardRebalancePlanOutcome {
            operation: None,
            items,
            dry_run: true,
        });
    }

    let strategy = if options.source_database_alias.is_some() {
        REBALANCE_STRATEGY_DRAIN_SOURCE
    } else {
        REBALANCE_STRATEGY_FILL_TARGET
    };
    let operation = insert_rebalance_operation(
        control_db,
        strategy,
        options.source_database_alias.as_deref(),
        &options.target_database_alias,
        &items,
    )
    .await?;

    Ok(ShardRebalancePlanOutcome {
        operation: Some(operation),
        items,
        dry_run: false,
    })
}

/// Applies up to `max_items` pending moves from a persisted rebalance plan.
///
/// Re-running apply resumes from remaining pending items. Failed items retain
/// their error in the item ledger; pending items are left untouched.
pub(crate) async fn apply_shard_rebalance(
    database: &database::Db,
    operation_id: Uuid,
    options: ShardRebalanceApplyOptions,
) -> anyhow::Result<ShardRebalanceApplyOutcome> {
    if options.max_items == 0 {
        anyhow::bail!("max_items must be greater than zero");
    }

    let control_db = database.control().await?;
    let operation = select_rebalance_operation(control_db, operation_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("shard rebalance operation {} was not found", operation_id)
        })?;

    if matches!(
        operation.status.as_str(),
        REBALANCE_OPERATION_STATUS_COMPLETED
            | REBALANCE_OPERATION_STATUS_CANCELLED
            | REBALANCE_OPERATION_STATUS_FAILED
    ) {
        anyhow::bail!(
            "shard rebalance operation {} has terminal status {}",
            operation_id,
            operation.status
        );
    }

    mark_rebalance_operation_running(control_db, operation_id).await?;
    let pending_items =
        list_rebalance_apply_items(control_db, operation_id, options.max_items).await?;
    let mut processed_items = Vec::with_capacity(pending_items.len());

    for item in pending_items {
        mark_rebalance_item_running(control_db, &item).await?;
        let result = apply_rebalance_item(database, &item, options.force).await;
        match result {
            Ok(()) => {
                processed_items.push(
                    mark_rebalance_item_completed(
                        control_db,
                        operation_id,
                        item.run_id,
                        item.run_shard,
                    )
                    .await?,
                );
            }
            Err(error) => {
                processed_items.push(
                    mark_rebalance_item_failed(
                        control_db,
                        operation_id,
                        item.run_id,
                        item.run_shard,
                        &error.to_string(),
                    )
                    .await?,
                );
            }
        }
    }

    let operation = refresh_rebalance_operation_status(control_db, operation_id).await?;

    Ok(ShardRebalanceApplyOutcome {
        operation,
        processed_items,
    })
}

pub(crate) async fn verify_shard_rebalance(
    database: &database::Db,
    operation_id: Uuid,
    max_items: usize,
) -> anyhow::Result<ShardRebalanceVerifyOutcome> {
    if max_items == 0 {
        anyhow::bail!("max_items must be greater than zero");
    }

    let control_db = database.control().await?;
    let operation = select_rebalance_operation(control_db, operation_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("shard rebalance operation {} was not found", operation_id)
        })?;
    let items = list_rebalance_items_by_status(
        control_db,
        operation_id,
        REBALANCE_ITEM_STATUS_COMPLETED,
        max_items,
    )
    .await?;
    let mut verified_items = Vec::with_capacity(items.len());

    for item in items {
        let source_db = database.placement(&item.source_database_alias).await?;
        let target_db = database.placement(&item.target_database_alias).await?;
        let reports = verify_move_tables(
            source_db,
            target_db,
            item.run_id,
            item.run_shard,
            &BTreeMap::new(),
        )
        .await?;
        let verified = reports.iter().all(|report| report.verified);
        verified_items.push(ShardRebalanceVerifyItem {
            item,
            verified,
            tables: reports,
        });
    }

    Ok(ShardRebalanceVerifyOutcome {
        operation,
        items: verified_items,
    })
}

pub(crate) async fn cancel_shard_rebalance(
    database: &database::Db,
    operation_id: Uuid,
) -> anyhow::Result<ShardRebalanceOperation> {
    let control_db = database.control().await?;
    let operation = select_rebalance_operation(control_db, operation_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("shard rebalance operation {} was not found", operation_id)
        })?;

    if matches!(
        operation.status.as_str(),
        REBALANCE_OPERATION_STATUS_COMPLETED
            | REBALANCE_OPERATION_STATUS_CANCELLED
            | REBALANCE_OPERATION_STATUS_FAILED
    ) {
        return Ok(operation);
    }

    sqlx::query(
        r#"
        WITH cancelled AS (
            UPDATE shard_rebalance_items
            SET status = 'cancelled',
                completed_at = now(),
                updated_at = now()
            WHERE operation_id = $1::uuid
              AND status IN ('pending', 'running')
            RETURNING 1
        )
        UPDATE shard_rebalance_operations
        SET status = 'cancelled',
            cancelled_item_count = (
                SELECT COUNT(*)::int
                FROM shard_rebalance_items
                WHERE operation_id = $1::uuid
                  AND status = 'cancelled'
            ),
            cancelled_at = now(),
            updated_at = now()
        WHERE id = $1::uuid
        RETURNING *
        "#,
    )
    .bind(operation_id)
    .fetch_one(control_db)
    .await?;

    Ok(operation)
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
    let active_work_count = count_active_shard_work(source_db, run_id, run_shard).await?;

    if active_work_count > 0 && !options.force && !options.verify_only {
        anyhow::bail!(
            "run {} shard {} has {} leased/running row(s); wait for work to drain or pass --force",
            run_id,
            run_shard,
            active_work_count
        );
    }

    let marked_moving =
        !options.dry_run && !options.verify_only && current.status == SHARD_PLACEMENT_STATUS_ACTIVE;

    if marked_moving {
        mark_shard_moving(control_db, run_id, run_shard).await?;
        database
            .invalidate_execution_placement(run_id, run_shard)
            .await;
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

    let reports = verify_move_tables(
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

fn select_rebalance_candidates(
    candidates: Vec<ShardRebalanceCandidate>,
    active_shard_aliases: &[String],
    source_database_alias: Option<&str>,
    target_database_alias: &str,
    max_items: usize,
) -> Vec<ShardRebalanceCandidate> {
    if max_items == 0 {
        return Vec::new();
    }

    let mut counts = active_shard_aliases
        .iter()
        .map(|alias| (alias.as_str(), 0usize))
        .collect::<BTreeMap<_, _>>();
    for candidate in &candidates {
        if let Some(count) = counts.get_mut(candidate.source_database_alias.as_str()) {
            *count += 1;
        }
    }

    let mut candidates = candidates
        .into_iter()
        .filter(|candidate| candidate.source_database_alias != target_database_alias)
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.source_database_alias
            .cmp(&right.source_database_alias)
            .then_with(|| left.run_id.cmp(&right.run_id))
            .then_with(|| left.run_shard.cmp(&right.run_shard))
    });

    if let Some(source_alias) = source_database_alias {
        return candidates
            .into_iter()
            .filter(|candidate| candidate.source_database_alias == source_alias)
            .take(max_items)
            .collect();
    }

    let alias_count = active_shard_aliases.len();
    if alias_count < 2 {
        return Vec::new();
    }

    let target_count = counts
        .get(target_database_alias)
        .copied()
        .unwrap_or_default();
    let total = counts.values().sum::<usize>();
    let desired_per_alias = total.div_ceil(alias_count);
    let needed = desired_per_alias.saturating_sub(target_count);
    if needed == 0 {
        return Vec::new();
    }

    candidates
        .into_iter()
        .filter(|candidate| {
            counts
                .get(candidate.source_database_alias.as_str())
                .is_some_and(|count| *count > desired_per_alias)
        })
        .take(max_items.min(needed))
        .collect()
}

async fn list_active_rebalance_candidates(
    db: &PgPool,
) -> anyhow::Result<Vec<ShardRebalanceCandidate>> {
    let candidates = sqlx::query_as::<_, (Uuid, i16, String, i64)>(
        r#"
        SELECT sp.run_id, sp.run_shard, sp.database_alias, sp.route_version
        FROM shard_placements sp
        JOIN database_placements dp
          ON dp.alias = sp.database_alias
        WHERE sp.status = 'active'
          AND dp.status = 'active'
          AND dp.role IN ('shard', 'control_and_shard')
        ORDER BY sp.database_alias, sp.run_id, sp.run_shard
        "#,
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(
        |(run_id, run_shard, source_database_alias, route_version)| ShardRebalanceCandidate {
            run_id,
            run_shard,
            source_database_alias,
            route_version,
        },
    )
    .collect();

    Ok(candidates)
}

async fn insert_rebalance_operation(
    db: &PgPool,
    strategy: &str,
    source_database_alias: Option<&str>,
    target_database_alias: &str,
    items: &[PlannedShardRebalanceItem],
) -> anyhow::Result<ShardRebalanceOperation> {
    let mut tx = db.begin().await?;
    let operation = sqlx::query_as::<_, ShardRebalanceOperation>(
        r#"
        INSERT INTO shard_rebalance_operations (
            strategy,
            source_database_alias,
            target_database_alias,
            planned_item_count
        )
        VALUES ($1, $2, $3, $4)
        RETURNING *
        "#,
    )
    .bind(strategy)
    .bind(source_database_alias)
    .bind(target_database_alias)
    .bind(items.len() as i32)
    .fetch_one(tx.as_mut())
    .await?;

    for (idx, item) in items.iter().enumerate() {
        sqlx::query(
            r#"
            INSERT INTO shard_rebalance_items (
                operation_id,
                sequence_no,
                run_id,
                run_shard,
                source_database_alias,
                target_database_alias,
                planned_route_version
            )
            VALUES ($1::uuid, $2, $3::uuid, $4, $5, $6, $7)
            "#,
        )
        .bind(operation.id)
        .bind(idx as i32)
        .bind(item.run_id)
        .bind(item.run_shard)
        .bind(&item.source_database_alias)
        .bind(&item.target_database_alias)
        .bind(item.planned_route_version)
        .execute(tx.as_mut())
        .await?;
    }

    tx.commit().await?;
    Ok(operation)
}

async fn select_rebalance_operation(
    db: &PgPool,
    operation_id: Uuid,
) -> anyhow::Result<Option<ShardRebalanceOperation>> {
    let operation = sqlx::query_as::<_, ShardRebalanceOperation>(
        r#"
        SELECT *
        FROM shard_rebalance_operations
        WHERE id = $1::uuid
        "#,
    )
    .bind(operation_id)
    .fetch_optional(db)
    .await?;

    Ok(operation)
}

async fn mark_rebalance_operation_running(db: &PgPool, operation_id: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        UPDATE shard_rebalance_operations
        SET status = 'running',
            started_at = COALESCE(started_at, now()),
            updated_at = now()
        WHERE id = $1::uuid
          AND status IN ('planned', 'running')
        "#,
    )
    .bind(operation_id)
    .execute(db)
    .await?;

    Ok(())
}

async fn list_rebalance_items_by_status(
    db: &PgPool,
    operation_id: Uuid,
    status: &str,
    limit: usize,
) -> anyhow::Result<Vec<ShardRebalanceItem>> {
    let items = sqlx::query_as::<_, ShardRebalanceItem>(
        r#"
        SELECT *
        FROM shard_rebalance_items
        WHERE operation_id = $1::uuid
          AND status = $2
        ORDER BY sequence_no
        LIMIT $3
        "#,
    )
    .bind(operation_id)
    .bind(status)
    .bind(limit as i64)
    .fetch_all(db)
    .await?;

    Ok(items)
}

async fn list_rebalance_apply_items(
    db: &PgPool,
    operation_id: Uuid,
    limit: usize,
) -> anyhow::Result<Vec<ShardRebalanceItem>> {
    let items = sqlx::query_as::<_, ShardRebalanceItem>(
        r#"
        SELECT *
        FROM shard_rebalance_items
        WHERE operation_id = $1::uuid
          AND status IN ('pending', 'running')
        ORDER BY sequence_no
        LIMIT $2
        "#,
    )
    .bind(operation_id)
    .bind(limit as i64)
    .fetch_all(db)
    .await?;

    Ok(items)
}

async fn mark_rebalance_item_running(db: &PgPool, item: &ShardRebalanceItem) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        UPDATE shard_rebalance_items
        SET status = 'running',
            started_at = COALESCE(started_at, now()),
            updated_at = now()
        WHERE operation_id = $1::uuid
          AND run_id = $2::uuid
          AND run_shard = $3
          AND status = 'pending'
        "#,
    )
    .bind(item.operation_id)
    .bind(item.run_id)
    .bind(item.run_shard)
    .execute(db)
    .await?;

    Ok(())
}

async fn mark_rebalance_item_completed(
    db: &PgPool,
    operation_id: Uuid,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<ShardRebalanceItem> {
    mark_rebalance_item_status(
        db,
        operation_id,
        run_id,
        run_shard,
        REBALANCE_ITEM_STATUS_COMPLETED,
        None,
    )
    .await
}

async fn mark_rebalance_item_failed(
    db: &PgPool,
    operation_id: Uuid,
    run_id: Uuid,
    run_shard: i16,
    error_message: &str,
) -> anyhow::Result<ShardRebalanceItem> {
    mark_rebalance_item_status(
        db,
        operation_id,
        run_id,
        run_shard,
        REBALANCE_ITEM_STATUS_FAILED,
        Some(error_message),
    )
    .await
}

async fn mark_rebalance_item_status(
    db: &PgPool,
    operation_id: Uuid,
    run_id: Uuid,
    run_shard: i16,
    status: &str,
    error_message: Option<&str>,
) -> anyhow::Result<ShardRebalanceItem> {
    let item = sqlx::query_as::<_, ShardRebalanceItem>(
        r#"
        UPDATE shard_rebalance_items
        SET status = $4,
            error_message = $5,
            completed_at = CASE WHEN $4 IN ('completed', 'failed', 'cancelled') THEN now() ELSE completed_at END,
            updated_at = now()
        WHERE operation_id = $1::uuid
          AND run_id = $2::uuid
          AND run_shard = $3
        RETURNING *
        "#,
    )
    .bind(operation_id)
    .bind(run_id)
    .bind(run_shard)
    .bind(status)
    .bind(error_message)
    .fetch_one(db)
    .await?;

    Ok(item)
}

async fn apply_rebalance_item(
    database: &database::Db,
    item: &ShardRebalanceItem,
    force: bool,
) -> anyhow::Result<()> {
    let control_db = database.control().await?;
    let current = shard_placements::select_shard_placement(control_db, item.run_id, item.run_shard)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "shard placement for run {} shard {} was not found",
                item.run_id,
                item.run_shard
            )
        })?;

    if current.database_alias == item.target_database_alias
        && current.status == SHARD_PLACEMENT_STATUS_ACTIVE
    {
        return Ok(());
    }

    if current.database_alias != item.source_database_alias
        || current.route_version != item.planned_route_version
        || current.status != SHARD_PLACEMENT_STATUS_ACTIVE
    {
        anyhow::bail!(
            "planned route is stale for run {} shard {}; expected {} version {}, found {} status {} version {}",
            item.run_id,
            item.run_shard,
            item.source_database_alias,
            item.planned_route_version,
            current.database_alias,
            current.status,
            current.route_version
        );
    }

    move_shard_placement(
        database,
        item.run_id,
        item.run_shard,
        &item.target_database_alias,
        ShardMoveOptions {
            dry_run: false,
            verify_only: false,
            force,
        },
    )
    .await?;

    Ok(())
}

async fn refresh_rebalance_operation_status(
    db: &PgPool,
    operation_id: Uuid,
) -> anyhow::Result<ShardRebalanceOperation> {
    let (pending, running, completed, failed, cancelled) =
        sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
            r#"
            SELECT
                COUNT(*) FILTER (WHERE status = 'pending')::bigint,
                COUNT(*) FILTER (WHERE status = 'running')::bigint,
                COUNT(*) FILTER (WHERE status = 'completed')::bigint,
                COUNT(*) FILTER (WHERE status = 'failed')::bigint,
                COUNT(*) FILTER (WHERE status = 'cancelled')::bigint
            FROM shard_rebalance_items
            WHERE operation_id = $1::uuid
            "#,
        )
        .bind(operation_id)
        .fetch_one(db)
        .await?;
    let status = if pending == 0 && running == 0 && failed == 0 {
        REBALANCE_OPERATION_STATUS_COMPLETED
    } else if pending == 0 && running == 0 && failed > 0 {
        REBALANCE_OPERATION_STATUS_FAILED
    } else {
        REBALANCE_OPERATION_STATUS_RUNNING
    };
    let error_message = if status == REBALANCE_OPERATION_STATUS_FAILED {
        Some("one or more rebalance items failed")
    } else {
        None
    };

    let operation = sqlx::query_as::<_, ShardRebalanceOperation>(
        r#"
        UPDATE shard_rebalance_operations
        SET status = $2,
            completed_item_count = $3,
            failed_item_count = $4,
            cancelled_item_count = $5,
            error_message = $6,
            completed_at = CASE WHEN $2 IN ('completed', 'failed') THEN now() ELSE completed_at END,
            updated_at = now()
        WHERE id = $1::uuid
        RETURNING *
        "#,
    )
    .bind(operation_id)
    .bind(status)
    .bind(completed as i32)
    .bind(failed as i32)
    .bind(cancelled as i32)
    .bind(error_message)
    .fetch_one(db)
    .await?;

    Ok(operation)
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

async fn validate_source_placement(db: &PgPool, alias: &str) -> anyhow::Result<()> {
    let source = database_placements::select_database_placement(db, alias)
        .await?
        .ok_or_else(|| anyhow::anyhow!("database placement alias {} was not found", alias))?;

    if !source.is_shard_capable() {
        anyhow::bail!(
            "database placement alias {} has role {}, which is not shard-capable",
            source.alias,
            source.role
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
        "case_blobs" => {
            r#"
            SELECT to_jsonb(cb) AS row
            FROM case_blobs cb
            WHERE EXISTS (
                SELECT 1
                FROM runs r
                JOIN dataset_version_cases cvc
                  ON cvc.dataset_version_id = r.dataset_version_id
                WHERE r.id = $1::uuid
                  AND cvc.case_hash = cb.case_hash
            )
            ORDER BY cb.case_hash
            "#
        }
        "dataset_versions" => {
            r#"
            SELECT to_jsonb(dv) AS row
            FROM dataset_versions dv
            JOIN runs r
              ON r.dataset_version_id = dv.dataset_version_id
            WHERE r.id = $1::uuid
            "#
        }
        "dataset_version_cases" => {
            r#"
            SELECT to_jsonb(cvc) AS row
            FROM dataset_version_cases cvc
            JOIN runs r
              ON r.dataset_version_id = cvc.dataset_version_id
            WHERE r.id = $1::uuid
            ORDER BY cvc.case_ordinal, cvc.case_id
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

    if table == "case_blobs" {
        return copy_case_blob_rows(db, rows).await;
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

async fn copy_case_blob_rows(db: &PgPool, rows: Vec<Value>) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        INSERT INTO case_blobs (
            case_hash,
            task_type,
            case_group,
            input_payload,
            expected_output,
            context_payload,
            tags,
            metadata,
            created_at
        )
        SELECT
            row->>'case_hash',
            row->>'task_type',
            row->>'case_group',
            COALESCE(row->'input_payload', 'null'::jsonb),
            COALESCE(row->'expected_output', 'null'::jsonb),
            COALESCE(row->'context_payload', 'null'::jsonb),
            COALESCE(row->'tags', '[]'::jsonb),
            COALESCE(row->'metadata', '{}'::jsonb),
            COALESCE((row->>'created_at')::timestamptz, now())
        FROM jsonb_array_elements($1::jsonb) AS source(row)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(Json(Value::Array(rows)))
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

async fn verify_move_tables(
    source_db: &PgPool,
    target_db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    copied_rows_by_table: &std::collections::BTreeMap<&'static str, u64>,
) -> anyhow::Result<Vec<ShardMoveTableReport>> {
    let mut reports = Vec::with_capacity(PREREQUISITE_TABLES.len() + SHARD_TABLES.len());

    for table in PREREQUISITE_TABLES {
        let source = prerequisite_table_fingerprint(source_db, table, run_id).await?;
        let target = prerequisite_table_fingerprint(target_db, table, run_id).await?;
        let verified = source.row_count == target.row_count && source.checksum == target.checksum;
        reports.push(ShardMoveTableReport {
            table,
            source_row_count: source.row_count,
            target_row_count: target.row_count,
            copied_row_count: copied_rows_by_table.get(table).copied().unwrap_or_default(),
            source_checksum: source.checksum,
            target_checksum: target.checksum,
            verified,
        });
    }

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

async fn prerequisite_table_fingerprint(
    db: &PgPool,
    table: &str,
    run_id: Uuid,
) -> anyhow::Result<TableFingerprint> {
    let sql = match table {
        "case_blobs" => {
            r#"
            SELECT
                COUNT(*)::bigint AS row_count,
                COALESCE(md5(string_agg(row_json, E'\n' ORDER BY row_json)), '') AS checksum
            FROM (
                SELECT to_jsonb(cb)::text AS row_json
                FROM case_blobs cb
                WHERE EXISTS (
                    SELECT 1
                    FROM runs r
                    JOIN dataset_version_cases cvc
                      ON cvc.dataset_version_id = r.dataset_version_id
                    WHERE r.id = $1::uuid
                      AND cvc.case_hash = cb.case_hash
                )
            ) rows
            "#
        }
        "dataset_versions" => {
            r#"
            SELECT
                COUNT(*)::bigint AS row_count,
                COALESCE(md5(string_agg(row_json, E'\n' ORDER BY row_json)), '') AS checksum
            FROM (
                SELECT to_jsonb(dv)::text AS row_json
                FROM dataset_versions dv
                JOIN runs r
                  ON r.dataset_version_id = dv.dataset_version_id
                WHERE r.id = $1::uuid
            ) rows
            "#
        }
        "dataset_version_cases" => {
            r#"
            SELECT
                COUNT(*)::bigint AS row_count,
                COALESCE(md5(string_agg(row_json, E'\n' ORDER BY row_json)), '') AS checksum
            FROM (
                SELECT to_jsonb(cvc)::text AS row_json
                FROM dataset_version_cases cvc
                JOIN runs r
                  ON r.dataset_version_id = cvc.dataset_version_id
                WHERE r.id = $1::uuid
            ) rows
            "#
        }
        "runs" => {
            r#"
            SELECT
                COUNT(*)::bigint AS row_count,
                COALESCE(md5(string_agg(row_json, E'\n' ORDER BY row_json)), '') AS checksum
            FROM (
                SELECT to_jsonb(r)::text AS row_json
                FROM runs r
                WHERE r.id = $1::uuid
            ) rows
            "#
        }
        _ => anyhow::bail!("unsupported prerequisite table {}", table),
    };

    let fingerprint = sqlx::query_as::<_, TableFingerprint>(sql)
        .bind(run_id)
        .fetch_one(db)
        .await?;

    Ok(fingerprint)
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

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn copy_case_blob_rows_preserves_json_null_context(pool: PgPool) {
        let case_hash = format!("case-{}", Uuid::now_v7());
        let source_rows = vec![serde_json::json!({
            "case_hash": case_hash,
            "task_type": "classification",
            "case_group": null,
            "input_payload": {"text": "hello"},
            "expected_output": null,
            "context_payload": null,
            "tags": [],
            "metadata": {},
            "created_at": Utc::now(),
        })];

        copy_json_rows(&pool, "case_blobs", source_rows)
            .await
            .unwrap();

        let context_payload = sqlx::query_scalar::<_, Value>(
            r#"
            SELECT context_payload
            FROM case_blobs
            WHERE case_hash = $1
            "#,
        )
        .bind(case_hash)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(context_payload, Value::Null);
    }

    #[test]
    fn validate_run_shard_rejects_out_of_range_values() {
        assert!(validate_run_shard(0).is_ok());
        assert!(validate_run_shard(127).is_ok());
        assert!(validate_run_shard(-1).is_err());
        assert!(validate_run_shard(128).is_err());
    }

    #[test]
    fn rebalance_candidates_drain_source_in_route_order() {
        let run_a = Uuid::now_v7();
        let run_b = Uuid::now_v7();
        let candidates = vec![
            rebalance_candidate(run_b, 2, "primary", 1),
            rebalance_candidate(run_a, 1, "primary", 1),
            rebalance_candidate(run_a, 0, "shard_001", 1),
        ];

        let selected = select_rebalance_candidates(
            candidates,
            &["primary".to_string(), "shard_001".to_string()],
            Some("primary"),
            "shard_001",
            10,
        );

        assert_eq!(
            selected,
            vec![
                rebalance_candidate(run_a, 1, "primary", 1),
                rebalance_candidate(run_b, 2, "primary", 1),
            ]
        );
    }

    #[test]
    fn rebalance_candidates_fill_target_to_even_distribution() {
        let run_id = Uuid::now_v7();
        let candidates = vec![
            rebalance_candidate(run_id, 0, "primary", 1),
            rebalance_candidate(run_id, 1, "primary", 1),
            rebalance_candidate(run_id, 2, "primary", 1),
            rebalance_candidate(run_id, 3, "primary", 1),
            rebalance_candidate(run_id, 4, "shard_001", 1),
        ];

        let selected = select_rebalance_candidates(
            candidates,
            &["primary".to_string(), "shard_001".to_string()],
            None,
            "shard_001",
            10,
        );

        assert_eq!(
            selected,
            vec![
                rebalance_candidate(run_id, 0, "primary", 1),
                rebalance_candidate(run_id, 1, "primary", 1),
            ]
        );
    }

    fn rebalance_candidate(
        run_id: Uuid,
        run_shard: i16,
        source_database_alias: &str,
        route_version: i64,
    ) -> ShardRebalanceCandidate {
        ShardRebalanceCandidate {
            run_id,
            run_shard,
            source_database_alias: source_database_alias.to_string(),
            route_version,
        }
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
