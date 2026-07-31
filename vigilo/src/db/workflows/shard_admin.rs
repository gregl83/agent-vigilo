//! Shard placement administration workflows.
//!
//! These helpers keep shard topology guardrails behind the database workflow
//! boundary. CLI commands call them to list database placements, add shard
//! databases, drain or disable shard databases, assign empty run shards, and
//! move or restore shard-owned routes between placements.

use std::collections::BTreeMap;

use chrono::{
    DateTime,
    Utc,
};
use futures_util::TryStreamExt;
use serde::Serialize;
use serde_json::Value;
use sqlx::{
    Executor,
    PgPool,
    Postgres,
    Transaction,
    types::Json,
};
use tracing::{
    info,
    warn,
};
use uuid::Uuid;

use crate::{
    context::database,
    db::tables::{
        database_placements,
        outbox_events,
        shard_placements,
    },
    models::{
        database_placement::{
            DATABASE_PLACEMENT_ROLE_SHARD,
            DATABASE_PLACEMENT_STATUS_ACTIVE,
            DATABASE_PLACEMENT_STATUS_DISABLED,
            DATABASE_PLACEMENT_STATUS_DRAINING,
            DatabasePlacement,
        },
        run_chunk::RUN_SHARD_COUNT,
        shard_placement::{
            SHARD_PLACEMENT_STATUS_ACTIVE,
            SHARD_PLACEMENT_STATUS_COPYING,
            SHARD_PLACEMENT_STATUS_DRAINING,
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
    pub(crate) source_row_count: Option<i64>,
    pub(crate) target_row_count: Option<i64>,
    pub(crate) copied_row_count: u64,
    pub(crate) source_checksum: Option<String>,
    pub(crate) target_checksum: Option<String>,
    pub(crate) verification_mode: &'static str,
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
pub(crate) struct ShardMoveAbortOutcome {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) source_database_alias: String,
    pub(crate) target_database_alias: String,
    pub(crate) aborted: bool,
    pub(crate) placement: ShardPlacement,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShardRouteInspection {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) database_alias: String,
    pub(crate) shard_placement_status: String,
    pub(crate) move_target_database_alias: Option<String>,
    pub(crate) route_version: i64,
    pub(crate) database_role: String,
    pub(crate) database_status: String,
    pub(crate) database_url_env: String,
    pub(crate) database_url_env_resolved: bool,
    pub(crate) dispatchable: bool,
    pub(crate) readable: bool,
    pub(crate) routing_decision: &'static str,
    pub(crate) move_operation_id: Option<Uuid>,
    pub(crate) move_phase: Option<String>,
    pub(crate) move_completed_page_count: Option<i64>,
    pub(crate) move_copied_row_count: Option<i64>,
    pub(crate) move_copied_byte_count: Option<i64>,
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
    pub(crate) claim_token: Option<Uuid>,
    pub(crate) claimed_by: Option<Uuid>,
    pub(crate) claimed_until: Option<DateTime<Utc>>,
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
    pub(crate) lease_seconds: i32,
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

#[derive(Debug, sqlx::FromRow)]
struct ClaimedShardRebalanceItem {
    #[sqlx(flatten)]
    item: ShardRebalanceItem,
    reclaimed: bool,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "planned route is stale for run {run_id} shard {run_shard}; expected {expected_alias} version {expected_version}, found {actual_alias} status {actual_status} version {actual_version}"
)]
struct StaleRebalancePlanError {
    run_id: Uuid,
    run_shard: i16,
    expected_alias: String,
    expected_version: i64,
    actual_alias: String,
    actual_status: String,
    actual_version: i64,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "run {run_id} shard {run_shard} has {active_work_count} leased/running row(s); the route remains draining until work finishes, and --force cannot bypass the shard write fence"
)]
struct ShardMoveDrainPending {
    run_id: Uuid,
    run_shard: i16,
    active_work_count: i64,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "run {run_id} shard {run_shard} retains {remaining_dirty_key_count} dirty key(s) after bounded online catch-up; the route remains copying and the move can be resumed"
)]
struct ShardMoveCatchUpPending {
    run_id: Uuid,
    run_shard: i16,
    remaining_dirty_key_count: i64,
}

#[derive(Debug, Clone, Copy)]
struct ShardTable {
    name: &'static str,
    key_columns: &'static [&'static str],
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct TableFingerprint {
    row_count: i64,
    checksum: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ShardMoveOperation {
    id: Uuid,
    run_id: Uuid,
    run_shard: i16,
    source_database_alias: String,
    target_database_alias: String,
    status: String,
    phase: String,
    target_reset_at: Option<DateTime<Utc>>,
}

#[derive(Debug, sqlx::FromRow)]
struct MoveSourceRow {
    row: Value,
    row_key: String,
    row_bytes: i32,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct DirtyShardKey {
    table_name: String,
    row_key: Value,
    change_version: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct ShardMoveInspection {
    id: Uuid,
    phase: String,
    completed_page_count: i64,
    copied_row_count: i64,
    copied_byte_count: i64,
}

const PREREQUISITE_TABLES: &[&str] = &["case_blobs", "dataset_versions", "runs"];
const SHARD_TABLES: &[ShardTable] = &[
    ShardTable {
        name: "run_shard_cases",
        key_columns: &["run_id", "run_shard", "case_id"],
    },
    ShardTable {
        name: "run_chunks",
        key_columns: &["run_id", "run_shard", "id"],
    },
    ShardTable {
        name: "run_snapshots",
        key_columns: &["run_id", "run_shard"],
    },
    ShardTable {
        name: "executions",
        key_columns: &["run_id", "run_shard", "id"],
    },
    ShardTable {
        name: "execution_attempts",
        key_columns: &["run_id", "run_shard", "id"],
    },
    ShardTable {
        name: "execution_aggregates",
        key_columns: &["run_id", "run_shard", "execution_id"],
    },
    ShardTable {
        name: "evaluator_results",
        key_columns: &["run_id", "run_shard", "id"],
    },
    ShardTable {
        name: "run_shard_summaries",
        key_columns: &["run_id", "run_shard"],
    },
];
const SHARD_MOVE_COPY_BATCH_SIZE: usize = 1_000;
const SHARD_MOVE_COPY_BATCH_BYTES: usize = 4 * 1024 * 1024;
const SHARD_MOVE_ONLINE_REPLAY_BATCHES: usize = 100;
const SHARD_MOVE_FINAL_DIRTY_KEY_LIMIT: i64 = SHARD_MOVE_COPY_BATCH_SIZE as i64;

fn bounded_page_len(row_bytes: &[usize], max_rows: usize, max_bytes: usize) -> usize {
    if row_bytes.is_empty() || max_rows == 0 {
        return 0;
    }
    let mut bytes = 0usize;
    let mut rows = 0;
    for row_bytes in row_bytes.iter().copied().take(max_rows) {
        if rows > 0 && bytes.saturating_add(row_bytes) > max_bytes {
            break;
        }
        bytes = bytes.saturating_add(row_bytes);
        rows += 1;
    }
    rows
}

const REBALANCE_STRATEGY_DRAIN_SOURCE: &str = "drain-source";
const REBALANCE_STRATEGY_FILL_TARGET: &str = "fill-target";
const REBALANCE_OPERATION_STATUS_RUNNING: &str = "running";
const REBALANCE_OPERATION_STATUS_COMPLETED: &str = "completed";
const REBALANCE_OPERATION_STATUS_CANCELLED: &str = "cancelled";
const REBALANCE_OPERATION_STATUS_FAILED: &str = "failed";
const REBALANCE_ITEM_STATUS_PENDING: &str = "pending";
const REBALANCE_ITEM_STATUS_CANCELLED: &str = "cancelled";
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
    database_router: &database::DatabaseRouter,
    alias: &str,
) -> anyhow::Result<DatabasePlacement> {
    validate_non_empty(alias, "database alias")?;

    let db = database_router.control().await?;
    let mut tx = db.begin().await?;
    let Some(existing) =
        database_placements::select_database_placement_for_update(&mut tx, alias).await?
    else {
        anyhow::bail!("database placement alias {} was not found", alias);
    };

    if existing.is_control_capable() && existing.status != DATABASE_PLACEMENT_STATUS_DISABLED {
        anyhow::bail!(
            "database placement alias {} is control-capable and {}; disabling it would remove the control plane",
            alias,
            existing.status
        );
    }

    if existing.status == DATABASE_PLACEMENT_STATUS_DISABLED {
        tx.commit().await?;
        return Ok(existing);
    }

    if existing.status != DATABASE_PLACEMENT_STATUS_DRAINING {
        anyhow::bail!(
            "database placement alias {} has status {}; it must be draining before it can be disabled",
            alias,
            existing.status
        );
    }

    reject_inflight_move_target(&mut *tx, alias).await?;

    let route_count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM shard_placements
        WHERE database_alias = $1
        "#,
    )
    .bind(alias)
    .fetch_one(&mut *tx)
    .await?;
    if route_count > 0 {
        anyhow::bail!(
            "database placement alias {} still owns {} shard route(s); move every route before disabling it",
            alias,
            route_count
        );
    }

    let creation_count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM run_creation_placements creation
        JOIN runs run ON run.id = creation.run_id
        WHERE creation.database_alias = $1
          AND run.status = 'creating'::run_status
        "#,
    )
    .bind(alias)
    .fetch_one(&mut *tx)
    .await?;
    if creation_count > 0 {
        anyhow::bail!(
            "database placement alias {} is referenced by {} creating run placement(s)",
            alias,
            creation_count
        );
    }

    let rebalance_count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM shard_rebalance_items item
        JOIN shard_rebalance_operations operation
          ON operation.id = item.operation_id
        WHERE operation.status IN ('planned', 'running')
          AND item.status IN ('pending', 'running')
          AND (
                item.source_database_alias = $1
                OR item.target_database_alias = $1
          )
        "#,
    )
    .bind(alias)
    .fetch_one(&mut *tx)
    .await?;
    if rebalance_count > 0 {
        anyhow::bail!(
            "database placement alias {} is referenced by {} unfinished rebalance item(s)",
            alias,
            rebalance_count
        );
    }

    let placement_db = database_router.execution_database(alias).await?;
    let pending_outbox_count = outbox_events::count_pending_outbox_deliveries(placement_db).await?;
    if pending_outbox_count > 0 {
        anyhow::bail!(
            "database placement alias {} still has {} pending outbox delivery row(s)",
            alias,
            pending_outbox_count
        );
    }

    let placement = database_placements::update_database_placement_status(
        &mut tx,
        alias,
        DATABASE_PLACEMENT_STATUS_DISABLED,
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("database placement alias {} was not found", alias))?;
    tx.commit().await?;
    Ok(placement)
}

pub(crate) async fn drain_database_placement(
    database_router: &database::DatabaseRouter,
    alias: &str,
) -> anyhow::Result<DatabasePlacement> {
    validate_non_empty(alias, "database alias")?;

    let db = database_router.control().await?;
    let mut tx = db.begin().await?;
    let Some(existing) =
        database_placements::select_database_placement_for_update(&mut tx, alias).await?
    else {
        anyhow::bail!("database placement alias {} was not found", alias);
    };

    if existing.is_control_capable() {
        anyhow::bail!(
            "database placement alias {} is control-capable; draining it would remove active control-plane admission",
            alias
        );
    }

    if existing.status == DATABASE_PLACEMENT_STATUS_DISABLED {
        anyhow::bail!(
            "database placement alias {} is disabled and cannot transition back to draining",
            alias
        );
    }

    reject_inflight_move_target(&mut *tx, alias).await?;

    if existing.status == DATABASE_PLACEMENT_STATUS_DRAINING {
        tx.commit().await?;
        return Ok(existing);
    }

    let placement = database_placements::update_database_placement_status(
        &mut tx,
        alias,
        DATABASE_PLACEMENT_STATUS_DRAINING,
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("database placement alias {} was not found", alias))?;
    tx.commit().await?;
    Ok(placement)
}

async fn reject_inflight_move_target<'e, E>(executor: E, database_alias: &str) -> anyhow::Result<()>
where
    E: Executor<'e, Database = Postgres>,
{
    let move_count =
        shard_placements::count_inflight_moves_to_database(executor, database_alias).await?;
    if move_count > 0 {
        anyhow::bail!(
            "database placement alias {} is the target of {} in-flight shard move(s); complete or abort those moves first",
            database_alias,
            move_count
        );
    }

    Ok(())
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
/// This reads only control-database metadata. It reports whether the current
/// process can resolve the placement URL env var, but never returns the URL
/// value.
pub(crate) async fn inspect_shard_route(
    database_router: &database::DatabaseRouter,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<ShardRouteInspection> {
    validate_run_shard(run_shard)?;

    let control_db = database_router.control().await?;
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

    let database_serviceable = database_placement.can_serve_owned_shards();
    let shard_capable = database_placement.is_shard_capable();
    let dispatchable = placement.is_dispatchable() && database_serviceable && shard_capable;
    let readable = database_serviceable && shard_capable;
    let routing_decision = if dispatchable {
        "dispatchable"
    } else if readable {
        "read_only"
    } else {
        "blocked"
    };
    let move_progress = sqlx::query_as::<_, ShardMoveInspection>(
        r#"
        SELECT
            operation.id,
            operation.phase,
            COALESCE(SUM(page.completed_page_count), 0)::bigint AS completed_page_count,
            operation.copied_row_count,
            operation.copied_byte_count
        FROM shard_move_operations operation
        LEFT JOIN shard_move_table_progress page ON page.move_id = operation.id
        WHERE operation.run_id = $1::uuid
          AND operation.run_shard = $2
          AND operation.status = 'active'
        GROUP BY operation.id
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .fetch_optional(control_db)
    .await?;

    Ok(ShardRouteInspection {
        run_id,
        run_shard,
        database_alias: placement.database_alias,
        shard_placement_status: placement.status,
        move_target_database_alias: placement.move_target_database_alias,
        route_version: placement.route_version,
        database_role: database_placement.role,
        database_status: database_placement.status,
        database_url_env_resolved: database_router
            .database_url_env_is_resolved(&database_placement.database_url_env),
        database_url_env: database_placement.database_url_env,
        dispatchable,
        readable,
        routing_decision,
        move_operation_id: move_progress.as_ref().map(|progress| progress.id),
        move_phase: move_progress
            .as_ref()
            .map(|progress| progress.phase.clone()),
        move_completed_page_count: move_progress
            .as_ref()
            .map(|progress| progress.completed_page_count),
        move_copied_row_count: move_progress
            .as_ref()
            .map(|progress| progress.copied_row_count),
        move_copied_byte_count: move_progress.map(|progress| progress.copied_byte_count),
    })
}

pub(crate) async fn set_shard_placement(
    database_router: &database::DatabaseRouter,
    run_id: Uuid,
    run_shard: i16,
    database_alias: &str,
) -> anyhow::Result<ShardPlacementSetOutcome> {
    validate_run_shard(run_shard)?;
    validate_non_empty(database_alias, "database alias")?;

    let control_db = database_router.control().await?;
    ensure_run_creation_is_inactive(control_db, run_id).await?;
    let existing = shard_placements::select_shard_placement(control_db, run_id, run_shard).await?;
    if let Some(existing) = &existing
        && existing.status != SHARD_PLACEMENT_STATUS_ACTIVE
    {
        anyhow::bail!(
            "run {} shard {} has placement status {}; only the shard move workflow may recover or change a non-active route",
            run_id,
            run_shard,
            existing.status
        );
    }

    let previous_database_alias = existing
        .as_ref()
        .map(|placement| placement.database_alias.clone());
    let changed = previous_database_alias
        .as_deref()
        .is_none_or(|previous| previous != database_alias);
    let placement = match &existing {
        Some(existing) if existing.database_alias == database_alias => {
            validate_existing_ownership(control_db, database_alias).await?;
            existing.clone()
        }
        Some(existing) => {
            change_empty_shard_placement(database_router, control_db, existing, database_alias)
                .await?
        }
        None => {
            let mut control_tx = control_db.begin().await?;
            validate_new_ownership_target(&mut control_tx, database_alias).await?;
            let placement = shard_placements::insert_active_shard_placement(
                &mut *control_tx,
                run_id,
                run_shard,
                database_alias,
            )
            .await?;
            control_tx.commit().await?;
            placement
        }
    };
    database_router
        .invalidate_execution_placement(run_id, run_shard)
        .await;

    Ok(ShardPlacementSetOutcome {
        placement,
        previous_database_alias,
        changed,
    })
}

async fn change_empty_shard_placement(
    database_router: &database::DatabaseRouter,
    control_db: &PgPool,
    expected: &ShardPlacement,
    target_database_alias: &str,
) -> anyhow::Result<ShardPlacement> {
    let source_db = database_router
        .execution_database(&expected.database_alias)
        .await?;
    let source_is_control = expected.database_alias == database_router.control_database_alias();
    let mut source_tx = source_db.begin().await?;
    crate::db::shard_write_fence::lock_exclusive(
        &mut source_tx,
        expected.run_id,
        expected.run_shard,
    )
    .await?;

    let current = if source_is_control {
        shard_placements::select_shard_placement_with(
            &mut *source_tx,
            expected.run_id,
            expected.run_shard,
        )
        .await?
    } else {
        shard_placements::select_shard_placement(control_db, expected.run_id, expected.run_shard)
            .await?
    };
    if current
        .as_ref()
        .is_none_or(|current| !expected.same_route_fence(current))
    {
        anyhow::bail!(
            "run {} shard {} route changed while its empty placement was being updated",
            expected.run_id,
            expected.run_shard
        );
    }

    let row_count =
        count_shard_owned_rows_with(&mut *source_tx, expected.run_id, expected.run_shard).await?;
    if row_count > 0 {
        anyhow::bail!(
            "run {} shard {} already has {} shard-owned row(s) on {}; use the shard move workflow to change its database placement",
            expected.run_id,
            expected.run_shard,
            row_count,
            expected.database_alias
        );
    }

    let placement = if source_is_control {
        validate_new_ownership_target(&mut source_tx, target_database_alias).await?;
        shard_placements::change_empty_active_shard_placement(
            &mut *source_tx,
            expected.run_id,
            expected.run_shard,
            &expected.database_alias,
            expected.route_version,
            target_database_alias,
        )
        .await?
    } else {
        let mut control_tx = control_db.begin().await?;
        validate_new_ownership_target(&mut control_tx, target_database_alias).await?;
        let placement = shard_placements::change_empty_active_shard_placement(
            &mut *control_tx,
            expected.run_id,
            expected.run_shard,
            &expected.database_alias,
            expected.route_version,
            target_database_alias,
        )
        .await?;
        control_tx.commit().await?;
        placement
    }
    .ok_or_else(|| {
        anyhow::anyhow!(
            "run {} shard {} route changed while its empty placement was being updated",
            expected.run_id,
            expected.run_shard
        )
    })?;
    source_tx.commit().await?;
    Ok(placement)
}

/// Creates a persisted rebalance plan or returns the same plan without writing.
///
/// The plan records only intended moves. Applying the plan still uses
/// `move_shard_placement` for each item, so copy, verification, and route
/// fencing remain centralized in the single-shard move workflow.
pub(crate) async fn plan_shard_rebalance(
    database_router: &database::DatabaseRouter,
    options: ShardRebalancePlanOptions,
) -> anyhow::Result<ShardRebalancePlanOutcome> {
    validate_non_empty(&options.target_database_alias, "target database alias")?;
    if options.max_items == 0 {
        anyhow::bail!("max_items must be greater than zero");
    }

    let control_db = database_router.control().await?;
    validate_target_placement(control_db, &options.target_database_alias).await?;
    if let Some(source_alias) = &options.source_database_alias {
        validate_source_placement(control_db, source_alias).await?;
        if source_alias == &options.target_database_alias {
            anyhow::bail!("source and target database aliases must differ");
        }
    }

    let active_aliases = database_router
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

/// Applies up to `max_items` leased moves from a persisted rebalance plan.
///
/// Each move is claimed immediately before it starts. Re-running apply resumes
/// pending items and running items whose prior claim expired. An item whose
/// shard is still draining is released back to pending. All settlement writes
/// are fenced by the opaque token returned with the claim.
pub(crate) async fn apply_shard_rebalance(
    database_router: &database::DatabaseRouter,
    operation_id: Uuid,
    options: ShardRebalanceApplyOptions,
) -> anyhow::Result<ShardRebalanceApplyOutcome> {
    if options.max_items == 0 {
        anyhow::bail!("max_items must be greater than zero");
    }
    if options.lease_seconds <= 0 {
        anyhow::bail!("lease_seconds must be greater than zero");
    }

    let control_db = database_router.control().await?;
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

    let claimed_by = Uuid::now_v7();
    let mut processed_items = Vec::with_capacity(options.max_items);

    for _ in 0..options.max_items {
        let Some(claim) = claim_next_rebalance_apply_item(
            control_db,
            operation_id,
            claimed_by,
            options.lease_seconds,
        )
        .await?
        else {
            break;
        };
        let item = claim.item;
        let claim_token = item.claim_token.ok_or_else(|| {
            anyhow::anyhow!(
                "claimed rebalance item {} shard {} did not return a claim token",
                item.run_id,
                item.run_shard
            )
        })?;

        if claim.reclaimed {
            info!(
                operation_id = %operation_id,
                sequence_no = item.sequence_no,
                run_id = %item.run_id,
                run_shard = item.run_shard,
                claim_token = %claim_token,
                claimed_by = %claimed_by,
                claimed_until = ?item.claimed_until,
                "re-claimed expired shard rebalance apply item"
            );
        } else {
            info!(
                operation_id = %operation_id,
                sequence_no = item.sequence_no,
                run_id = %item.run_id,
                run_shard = item.run_shard,
                claim_token = %claim_token,
                claimed_by = %claimed_by,
                claimed_until = ?item.claimed_until,
                "claimed shard rebalance apply item"
            );
        }

        if rebalance_operation_is_cancelled(control_db, operation_id).await? {
            let settled = mark_rebalance_item_cancelled(control_db, &item, claim_token).await?;
            record_rebalance_item_settlement(
                &mut processed_items,
                settled,
                &item,
                claim_token,
                claimed_by,
                REBALANCE_ITEM_STATUS_CANCELLED,
            );
            continue;
        }

        let result = apply_rebalance_item(database_router, &item, options.force).await;
        match result {
            Ok(()) => {
                let settled = mark_rebalance_item_completed(control_db, &item, claim_token).await?;
                record_rebalance_item_settlement(
                    &mut processed_items,
                    settled,
                    &item,
                    claim_token,
                    claimed_by,
                    REBALANCE_ITEM_STATUS_COMPLETED,
                );
            }
            Err(error) => {
                if let Some(catch_up) = error.downcast_ref::<ShardMoveCatchUpPending>() {
                    let settled =
                        defer_rebalance_item(control_db, &item, claim_token, &error.to_string())
                            .await?;
                    if settled.is_some() {
                        info!(
                            operation_id = %operation_id,
                            sequence_no = item.sequence_no,
                            run_id = %catch_up.run_id,
                            run_shard = catch_up.run_shard,
                            remaining_dirty_key_count = catch_up.remaining_dirty_key_count,
                            claim_token = %claim_token,
                            claimed_by = %claimed_by,
                            "deferred shard rebalance item for another online catch-up cycle"
                        );
                    }
                    record_rebalance_item_settlement(
                        &mut processed_items,
                        settled,
                        &item,
                        claim_token,
                        claimed_by,
                        REBALANCE_ITEM_STATUS_PENDING,
                    );
                    break;
                }
                if let Some(drain) = error.downcast_ref::<ShardMoveDrainPending>() {
                    let settled =
                        defer_rebalance_item(control_db, &item, claim_token, &error.to_string())
                            .await?;
                    if settled.is_some() {
                        info!(
                            operation_id = %operation_id,
                            sequence_no = item.sequence_no,
                            run_id = %drain.run_id,
                            run_shard = drain.run_shard,
                            active_work_count = drain.active_work_count,
                            claim_token = %claim_token,
                            claimed_by = %claimed_by,
                            "deferred shard rebalance item until active work drains"
                        );
                    }
                    record_rebalance_item_settlement(
                        &mut processed_items,
                        settled,
                        &item,
                        claim_token,
                        claimed_by,
                        REBALANCE_ITEM_STATUS_PENDING,
                    );
                    break;
                }
                if let Some(stale) = error.downcast_ref::<StaleRebalancePlanError>() {
                    warn!(
                        operation_id = %operation_id,
                        sequence_no = item.sequence_no,
                        run_id = %item.run_id,
                        run_shard = item.run_shard,
                        claim_token = %claim_token,
                        claimed_by = %claimed_by,
                        expected_database_alias = %stale.expected_alias,
                        expected_route_version = stale.expected_version,
                        actual_database_alias = %stale.actual_alias,
                        actual_route_status = %stale.actual_status,
                        actual_route_version = stale.actual_version,
                        "shard rebalance apply item failed stale-plan check"
                    );
                }
                let settled =
                    mark_rebalance_item_failed(control_db, &item, claim_token, &error.to_string())
                        .await?;
                record_rebalance_item_settlement(
                    &mut processed_items,
                    settled,
                    &item,
                    claim_token,
                    claimed_by,
                    REBALANCE_ITEM_STATUS_FAILED,
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
    database_router: &database::DatabaseRouter,
    operation_id: Uuid,
    max_items: usize,
) -> anyhow::Result<ShardRebalanceVerifyOutcome> {
    if max_items == 0 {
        anyhow::bail!("max_items must be greater than zero");
    }

    let control_db = database_router.control().await?;
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
        let source_db = database_router
            .execution_database(&item.source_database_alias)
            .await?;
        let target_db = database_router
            .execution_database(&item.target_database_alias)
            .await?;
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
    database_router: &database::DatabaseRouter,
    operation_id: Uuid,
) -> anyhow::Result<ShardRebalanceOperation> {
    let control_db = database_router.control().await?;
    let mut tx = control_db.begin().await?;
    let operation = sqlx::query_as::<_, ShardRebalanceOperation>(
        r#"
        SELECT *
        FROM shard_rebalance_operations
        WHERE id = $1::uuid
        FOR UPDATE
        "#,
    )
    .bind(operation_id)
    .fetch_optional(tx.as_mut())
    .await?
    .ok_or_else(|| anyhow::anyhow!("shard rebalance operation {} was not found", operation_id))?;

    if matches!(
        operation.status.as_str(),
        REBALANCE_OPERATION_STATUS_COMPLETED | REBALANCE_OPERATION_STATUS_FAILED
    ) {
        tx.commit().await?;
        return Ok(operation);
    }

    sqlx::query(
        r#"
        UPDATE shard_rebalance_operations
        SET status = 'cancelled',
            cancelled_at = COALESCE(cancelled_at, now()),
            updated_at = now()
        WHERE id = $1::uuid
        "#,
    )
    .bind(operation_id)
    .execute(tx.as_mut())
    .await?;

    sqlx::query(
        r#"
        WITH cancellable AS (
            SELECT
                operation_id,
                run_id,
                run_shard,
                status,
                claim_token
            FROM shard_rebalance_items
            WHERE operation_id = $1::uuid
              AND (
                    status = 'pending'
                    OR (
                        status = 'running'
                        AND claimed_until <= now()
                    )
              )
            FOR UPDATE
        )
        UPDATE shard_rebalance_items item
        SET status = 'cancelled',
            error_message = NULL,
            claim_token = NULL,
            claimed_by = NULL,
            claimed_until = NULL,
            completed_at = now(),
            updated_at = now()
        FROM cancellable
        WHERE item.operation_id = cancellable.operation_id
          AND item.run_id = cancellable.run_id
          AND item.run_shard = cancellable.run_shard
          AND item.status = cancellable.status
          AND item.claim_token IS NOT DISTINCT FROM cancellable.claim_token
        "#,
    )
    .bind(operation_id)
    .execute(tx.as_mut())
    .await?;

    tx.commit().await?;
    refresh_rebalance_operation_status(control_db, operation_id).await
}

/// Backfills, drains, and moves one shard under fenced write admission.
///
/// `copying` stays dispatchable while durable pages and captured mutations are
/// replayed. `draining` rejects new work while admitted leases finish.
/// `moving` holds the exclusive fence only for final replay and activation.
/// The persisted operation, target, checkpoints, and route versions make
/// retries resumable and prevent concurrent redirection.
pub(crate) async fn move_shard_placement(
    database_router: &database::DatabaseRouter,
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

    let control_db = database_router.control().await?;
    ensure_run_creation_is_inactive(control_db, run_id).await?;
    let current = shard_placements::select_shard_placement(control_db, run_id, run_shard)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "shard placement for run {} shard {} was not found",
                run_id,
                run_shard
            )
        })?;

    let completed_operation = if !options.dry_run
        && !options.verify_only
        && current.database_alias == target_database_alias
        && current.status == SHARD_PLACEMENT_STATUS_ACTIVE
    {
        sqlx::query_as::<_, ShardMoveOperation>(
            r#"
            SELECT id, run_id, run_shard, source_database_alias,
                   target_database_alias, starting_route_version, status, phase,
                   target_reset_at, copied_row_count, copied_byte_count, claim_token
            FROM shard_move_operations
            WHERE run_id = $1::uuid
              AND run_shard = $2
              AND target_database_alias = $3
              AND status IN ('active', 'completed')
            ORDER BY
                CASE status WHEN 'active' THEN 0 ELSE 1 END,
                completed_at DESC NULLS LAST
            LIMIT 1
            "#,
        )
        .bind(run_id)
        .bind(run_shard)
        .bind(target_database_alias)
        .fetch_optional(control_db)
        .await?
    } else {
        None
    };
    let retrying_completed_move = completed_operation.is_some();

    if options.verify_only {
        validate_source_placement(control_db, target_database_alias).await?;
    } else if !retrying_completed_move {
        validate_target_placement(control_db, target_database_alias).await?;
    }

    if current.database_alias == target_database_alias
        && !options.verify_only
        && !retrying_completed_move
    {
        anyhow::bail!(
            "run {} shard {} already routes to database placement {}",
            run_id,
            run_shard,
            target_database_alias
        );
    }

    let source_db = database_router
        .execution_database(&current.database_alias)
        .await?;
    let target_db = if options.verify_only || retrying_completed_move {
        database_router
            .execution_database(target_database_alias)
            .await?
    } else {
        database_router
            .execution_target_database(target_database_alias)
            .await?
    };
    let copied_rows_by_table = std::collections::BTreeMap::<&'static str, u64>::new();

    if options.dry_run || options.verify_only {
        let active_work_count = count_active_shard_work(source_db, run_id, run_shard).await?;
        let reports = verify_move_tables(
            source_db,
            target_db,
            run_id,
            run_shard,
            &copied_rows_by_table,
        )
        .await?;

        return Ok(ShardMoveOutcome {
            run_id,
            run_shard,
            source_database_alias: current.database_alias.clone(),
            target_database_alias: target_database_alias.to_string(),
            dry_run: options.dry_run,
            verify_only: options.verify_only,
            forced: options.force,
            active_work_count,
            moved: false,
            placement: current,
            tables: reports,
        });
    }

    if retrying_completed_move {
        let operation = completed_operation.expect("completed move retry has an operation");
        if operation.status == "active" {
            let old_source = database_router
                .execution_database(&operation.source_database_alias)
                .await?;
            let mut cleanup_tx = old_source.begin().await?;
            crate::db::shard_write_fence::lock_exclusive(&mut cleanup_tx, run_id, run_shard)
                .await?;
            sqlx::query("DELETE FROM shard_move_captures WHERE move_id = $1::uuid")
                .bind(operation.id)
                .execute(cleanup_tx.as_mut())
                .await?;
            cleanup_tx.commit().await?;
            settle_completed_move_operation(control_db, operation.id, None).await?;
        }
        let reports = checkpoint_move_reports(control_db, operation.id).await?;
        let active_work_count = count_active_shard_work(source_db, run_id, run_shard).await?;
        return Ok(ShardMoveOutcome {
            run_id,
            run_shard,
            source_database_alias: operation.source_database_alias,
            target_database_alias: target_database_alias.to_string(),
            dry_run: false,
            verify_only: false,
            forced: options.force,
            active_work_count,
            moved: false,
            placement: current,
            tables: reports,
        });
    }

    if !matches!(
        current.status.as_str(),
        SHARD_PLACEMENT_STATUS_ACTIVE
            | SHARD_PLACEMENT_STATUS_COPYING
            | SHARD_PLACEMENT_STATUS_DRAINING
            | SHARD_PLACEMENT_STATUS_MOVING
    ) {
        anyhow::bail!(
            "run {} shard {} has placement status {}, which cannot be moved",
            run_id,
            run_shard,
            current.status
        );
    }
    if current.status != SHARD_PLACEMENT_STATUS_ACTIVE
        && current.move_target_database_alias.as_deref() != Some(target_database_alias)
    {
        anyhow::bail!(
            "run {} shard {} is already {} toward database placement {}; retry with the persisted target",
            run_id,
            run_shard,
            current.status,
            current
                .move_target_database_alias
                .as_deref()
                .unwrap_or("<missing>")
        );
    }

    let claim_token = Uuid::now_v7();
    let operation =
        claim_shard_move_operation(control_db, &current, target_database_alias, claim_token)
            .await?;
    let current = match prepare_online_shard_move(
        database_router,
        source_db,
        target_db,
        current,
        &operation,
        claim_token,
    )
    .await
    {
        Ok(current) => current,
        Err(error) => {
            release_shard_move_claim(control_db, operation.id, claim_token).await?;
            return Err(error);
        }
    };
    let initial_route = current.clone();
    let source_database_alias = current.database_alias.clone();
    let source_is_control = source_database_alias == database_router.control_database_alias();
    let active_work_count = count_active_shard_work(source_db, run_id, run_shard).await?;
    if active_work_count > 0 {
        release_shard_move_claim(control_db, operation.id, claim_token).await?;
        return Err(ShardMoveDrainPending {
            run_id,
            run_shard,
            active_work_count,
        }
        .into());
    }
    let mut source_tx = source_db.begin().await?;
    crate::db::shard_write_fence::lock_exclusive(&mut source_tx, run_id, run_shard).await?;

    // Re-read after acquiring the barrier. Another mover may have completed
    // while this caller waited for previously admitted writes to finish.
    let current = if source_is_control {
        shard_placements::select_shard_placement_with(&mut *source_tx, run_id, run_shard).await?
    } else {
        shard_placements::select_shard_placement(control_db, run_id, run_shard).await?
    }
    .ok_or_else(|| {
        anyhow::anyhow!(
            "shard placement for run {} shard {} was not found",
            run_id,
            run_shard
        )
    })?;

    if current.database_alias == target_database_alias
        && current.status == SHARD_PLACEMENT_STATUS_ACTIVE
    {
        let active_work_count =
            count_active_shard_work_with(&mut *source_tx, run_id, run_shard).await?;
        let reports = verify_move_tables_with_source(
            &mut source_tx,
            target_db,
            run_id,
            run_shard,
            &copied_rows_by_table,
        )
        .await?;
        if !reports.iter().all(|report| report.verified) {
            anyhow::bail!(
                "run {} shard {} reached target {} while the completed move could not be verified",
                run_id,
                run_shard,
                target_database_alias
            );
        }
        source_tx.commit().await?;

        return Ok(ShardMoveOutcome {
            run_id,
            run_shard,
            source_database_alias,
            target_database_alias: target_database_alias.to_string(),
            dry_run: false,
            verify_only: false,
            forced: options.force,
            active_work_count,
            moved: false,
            placement: current,
            tables: reports,
        });
    }

    if !initial_route.same_route_fence(&current) {
        anyhow::bail!(
            "run {} shard {} route changed while waiting for move admission; expected {} status {} version {}, found {} status {} version {}",
            run_id,
            run_shard,
            initial_route.database_alias,
            initial_route.status,
            initial_route.route_version,
            current.database_alias,
            current.status,
            current.route_version
        );
    }

    if current.database_alias != source_database_alias
        || !matches!(
            current.status.as_str(),
            SHARD_PLACEMENT_STATUS_ACTIVE
                | SHARD_PLACEMENT_STATUS_COPYING
                | SHARD_PLACEMENT_STATUS_DRAINING
                | SHARD_PLACEMENT_STATUS_MOVING
        )
    {
        anyhow::bail!(
            "run {} shard {} route changed while waiting for move admission; found {} status {} version {}",
            run_id,
            run_shard,
            current.database_alias,
            current.status,
            current.route_version
        );
    }

    if current.status != SHARD_PLACEMENT_STATUS_ACTIVE
        && current.move_target_database_alias.as_deref() != Some(target_database_alias)
    {
        anyhow::bail!(
            "run {} shard {} is already {} toward database placement {}; retry with the persisted target",
            run_id,
            run_shard,
            current.status,
            current
                .move_target_database_alias
                .as_deref()
                .unwrap_or("<missing>")
        );
    }

    let draining = current;
    database_router
        .invalidate_execution_placement(run_id, run_shard)
        .await;

    let active_work_count =
        count_active_shard_work_with(&mut *source_tx, run_id, run_shard).await?;
    if active_work_count > 0 {
        source_tx.commit().await?;
        release_shard_move_claim(control_db, operation.id, claim_token).await?;
        return Err(ShardMoveDrainPending {
            run_id,
            run_shard,
            active_work_count,
        }
        .into());
    }

    let moving = if draining.status == SHARD_PLACEMENT_STATUS_DRAINING {
        let placement = if source_is_control {
            shard_placements::mark_shard_placement_moving(
                &mut *source_tx,
                run_id,
                run_shard,
                &source_database_alias,
                draining.route_version,
                target_database_alias,
            )
            .await?
        } else {
            shard_placements::mark_shard_placement_moving(
                control_db,
                run_id,
                run_shard,
                &source_database_alias,
                draining.route_version,
                target_database_alias,
            )
            .await?
        };
        placement.ok_or_else(|| {
            anyhow::anyhow!(
                "run {} shard {} route changed before it could be frozen for copying",
                run_id,
                run_shard
            )
        })?
    } else {
        draining
    };
    database_router
        .invalidate_execution_placement(run_id, run_shard)
        .await;

    sqlx::query(
        r#"
        UPDATE shard_move_operations
        SET phase = 'cutover',
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'active'
          AND claim_token = $2::uuid
        "#,
    )
    .bind(operation.id)
    .bind(claim_token)
    .execute(control_db)
    .await?;
    let remaining_dirty =
        replay_dirty_shard_keys(source_db, target_db, operation.id, usize::MAX).await?;
    if remaining_dirty != 0 {
        anyhow::bail!(
            "run {} shard {} final replay retained {} dirty key(s)",
            run_id,
            run_shard,
            remaining_dirty
        );
    }
    let reports = checkpoint_move_reports(control_db, operation.id).await?;

    let placement = if source_is_control {
        activate_moved_shard_placement_on_target_with(
            &mut source_tx,
            run_id,
            run_shard,
            &source_database_alias,
            moving.route_version,
            target_database_alias,
        )
        .await?
    } else {
        activate_moved_shard_placement_on_target(
            control_db,
            run_id,
            run_shard,
            &source_database_alias,
            moving.route_version,
            target_database_alias,
        )
        .await?
    }
    .ok_or_else(|| {
        anyhow::anyhow!(
            "run {} shard {} route changed before target activation",
            run_id,
            run_shard
        )
    })?;
    sqlx::query("DELETE FROM shard_move_captures WHERE move_id = $1::uuid")
        .bind(operation.id)
        .execute(source_tx.as_mut())
        .await?;
    source_tx.commit().await?;
    settle_completed_move_operation(control_db, operation.id, Some(claim_token)).await?;
    database_router
        .invalidate_execution_placement(run_id, run_shard)
        .await;

    Ok(ShardMoveOutcome {
        run_id,
        run_shard,
        source_database_alias,
        target_database_alias: target_database_alias.to_string(),
        dry_run: false,
        verify_only: false,
        forced: options.force,
        active_work_count,
        moved: true,
        placement,
        tables: reports,
    })
}

/// Restores an in-progress shard move to active ownership on its source.
///
/// Partial target rows remain non-authoritative. The source admission lock and
/// route-version compare-and-swap ensure the abort cannot race a copy or route
/// activation, and stale movers cannot restart after the route fence advances.
pub(crate) async fn abort_shard_move(
    database_router: &database::DatabaseRouter,
    run_id: Uuid,
    run_shard: i16,
    source_database_alias: &str,
    target_database_alias: &str,
) -> anyhow::Result<ShardMoveAbortOutcome> {
    validate_run_shard(run_shard)?;
    validate_non_empty(source_database_alias, "source database alias")?;
    validate_non_empty(target_database_alias, "target database alias")?;
    if source_database_alias == target_database_alias {
        anyhow::bail!("source and target database aliases must differ");
    }

    let control_db = database_router.control().await?;
    ensure_run_creation_is_inactive(control_db, run_id).await?;
    let initial = shard_placements::select_shard_placement(control_db, run_id, run_shard)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "shard placement for run {} shard {} was not found",
                run_id,
                run_shard
            )
        })?;
    let prepared_move_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM shard_move_operations
        WHERE run_id = $1::uuid
          AND run_shard = $2
          AND source_database_alias = $3
          AND target_database_alias = $4
          AND status = 'active'
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(source_database_alias)
    .bind(target_database_alias)
    .fetch_optional(control_db)
    .await?;
    if prepared_move_id.is_none()
        && let Some(outcome) = completed_abort_outcome(
            &initial,
            run_id,
            run_shard,
            source_database_alias,
            target_database_alias,
        )?
    {
        return Ok(outcome);
    }

    let source_db = database_router
        .execution_database(source_database_alias)
        .await?;
    let source_is_control = source_database_alias == database_router.control_database_alias();
    let mut source_tx = source_db.begin().await?;
    crate::db::shard_write_fence::lock_exclusive(&mut source_tx, run_id, run_shard).await?;

    let current = if source_is_control {
        shard_placements::select_shard_placement_with(&mut *source_tx, run_id, run_shard).await?
    } else {
        shard_placements::select_shard_placement(control_db, run_id, run_shard).await?
    }
    .ok_or_else(|| {
        anyhow::anyhow!(
            "shard placement for run {} shard {} was not found",
            run_id,
            run_shard
        )
    })?;
    if prepared_move_id.is_none()
        && let Some(outcome) = completed_abort_outcome(
            &current,
            run_id,
            run_shard,
            source_database_alias,
            target_database_alias,
        )?
    {
        source_tx.commit().await?;
        return Ok(outcome);
    }
    if !initial.same_route_fence(&current) {
        anyhow::bail!(
            "run {} shard {} route changed while waiting for move-abort admission; expected {} status {} version {}, found {} status {} version {}",
            run_id,
            run_shard,
            initial.database_alias,
            initial.status,
            initial.route_version,
            current.database_alias,
            current.status,
            current.route_version
        );
    }
    if current.status == SHARD_PLACEMENT_STATUS_ACTIVE {
        let placement = if source_is_control {
            shard_placements::fence_active_shard_placement(
                &mut *source_tx,
                run_id,
                run_shard,
                source_database_alias,
                current.route_version,
            )
            .await?
        } else {
            shard_placements::fence_active_shard_placement(
                control_db,
                run_id,
                run_shard,
                source_database_alias,
                current.route_version,
            )
            .await?
        }
        .ok_or_else(|| {
            anyhow::anyhow!(
                "run {} shard {} route changed before its prepared move could be aborted",
                run_id,
                run_shard
            )
        })?;
        let move_id = prepared_move_id.expect("active abort requires a prepared move");
        sqlx::query("DELETE FROM shard_move_captures WHERE move_id = $1::uuid")
            .bind(move_id)
            .execute(source_tx.as_mut())
            .await?;
        source_tx.commit().await?;
        settle_aborted_move_operation(control_db, move_id).await?;
        database_router
            .invalidate_execution_placement(run_id, run_shard)
            .await;
        return Ok(ShardMoveAbortOutcome {
            run_id,
            run_shard,
            source_database_alias: source_database_alias.to_string(),
            target_database_alias: target_database_alias.to_string(),
            aborted: true,
            placement,
        });
    }

    validate_abort_route(
        &current,
        run_id,
        run_shard,
        source_database_alias,
        target_database_alias,
    )?;

    let placement = if source_is_control {
        shard_placements::abort_shard_placement_move(
            &mut *source_tx,
            run_id,
            run_shard,
            source_database_alias,
            current.route_version,
            target_database_alias,
        )
        .await?
    } else {
        shard_placements::abort_shard_placement_move(
            control_db,
            run_id,
            run_shard,
            source_database_alias,
            current.route_version,
            target_database_alias,
        )
        .await?
    }
    .ok_or_else(|| {
        anyhow::anyhow!(
            "run {} shard {} route changed before its move could be aborted",
            run_id,
            run_shard
        )
    })?;
    if let Some(move_id) = prepared_move_id {
        sqlx::query("DELETE FROM shard_move_captures WHERE move_id = $1::uuid")
            .bind(move_id)
            .execute(source_tx.as_mut())
            .await?;
    }
    source_tx.commit().await?;
    if let Some(move_id) = prepared_move_id {
        settle_aborted_move_operation(control_db, move_id).await?;
    }
    database_router
        .invalidate_execution_placement(run_id, run_shard)
        .await;

    Ok(ShardMoveAbortOutcome {
        run_id,
        run_shard,
        source_database_alias: source_database_alias.to_string(),
        target_database_alias: target_database_alias.to_string(),
        aborted: true,
        placement,
    })
}

fn completed_abort_outcome(
    placement: &ShardPlacement,
    run_id: Uuid,
    run_shard: i16,
    source_database_alias: &str,
    target_database_alias: &str,
) -> anyhow::Result<Option<ShardMoveAbortOutcome>> {
    if placement.status != SHARD_PLACEMENT_STATUS_ACTIVE {
        return Ok(None);
    }
    if placement.database_alias == target_database_alias {
        anyhow::bail!(
            "run {} shard {} move already completed on database placement {}",
            run_id,
            run_shard,
            target_database_alias
        );
    }
    if placement.database_alias != source_database_alias
        || placement.move_target_database_alias.is_some()
    {
        anyhow::bail!(
            "run {} shard {} has no matching move from {} to {}; found {} status {} version {}",
            run_id,
            run_shard,
            source_database_alias,
            target_database_alias,
            placement.database_alias,
            placement.status,
            placement.route_version
        );
    }

    Ok(Some(ShardMoveAbortOutcome {
        run_id,
        run_shard,
        source_database_alias: source_database_alias.to_string(),
        target_database_alias: target_database_alias.to_string(),
        aborted: false,
        placement: placement.clone(),
    }))
}

fn validate_abort_route(
    placement: &ShardPlacement,
    run_id: Uuid,
    run_shard: i16,
    source_database_alias: &str,
    target_database_alias: &str,
) -> anyhow::Result<()> {
    if placement.database_alias != source_database_alias
        || !matches!(
            placement.status.as_str(),
            SHARD_PLACEMENT_STATUS_COPYING
                | SHARD_PLACEMENT_STATUS_DRAINING
                | SHARD_PLACEMENT_STATUS_MOVING
        )
        || placement.move_target_database_alias.as_deref() != Some(target_database_alias)
    {
        anyhow::bail!(
            "run {} shard {} has no matching move from {} to {}; found {} status {} target {} version {}",
            run_id,
            run_shard,
            source_database_alias,
            target_database_alias,
            placement.database_alias,
            placement.status,
            placement
                .move_target_database_alias
                .as_deref()
                .unwrap_or("<none>"),
            placement.route_version
        );
    }

    Ok(())
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
        LEFT JOIN runs run ON run.id = sp.run_id
        WHERE sp.status = 'active'
          AND dp.status IN ('active', 'draining')
          AND dp.role IN ('shard', 'control_and_shard')
          AND (run.id IS NULL OR run.status <> 'creating'::run_status)
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

async fn claim_next_rebalance_apply_item(
    db: &PgPool,
    operation_id: Uuid,
    claimed_by: Uuid,
    lease_seconds: i32,
) -> anyhow::Result<Option<ClaimedShardRebalanceItem>> {
    let item = sqlx::query_as::<_, ClaimedShardRebalanceItem>(
        r#"
        WITH active_operation AS (
            UPDATE shard_rebalance_operations
            SET status = 'running',
                started_at = COALESCE(started_at, now()),
                updated_at = now()
            WHERE id = $1::uuid
              AND status IN ('planned', 'running')
            RETURNING id
        ),
        candidate AS (
            SELECT
                i.operation_id,
                i.run_id,
                i.run_shard,
                i.status = 'running' AS reclaimed
            FROM shard_rebalance_items i
            JOIN active_operation operation
              ON operation.id = i.operation_id
            WHERE i.status = 'pending'
               OR (
                    i.status = 'running'
                    AND i.claimed_until <= now()
               )
            ORDER BY i.sequence_no
            FOR UPDATE OF i SKIP LOCKED
            LIMIT 1
        ),
        claimed AS (
            UPDATE shard_rebalance_items i
            SET status = 'running',
                error_message = NULL,
                claim_token = gen_random_uuid(),
                claimed_by = $2::uuid,
                claimed_until = now() + ($3::int * interval '1 second'),
                started_at = COALESCE(i.started_at, now()),
                completed_at = NULL,
                updated_at = now()
            FROM candidate
            WHERE i.operation_id = candidate.operation_id
              AND i.run_id = candidate.run_id
              AND i.run_shard = candidate.run_shard
            RETURNING i.*, candidate.reclaimed
        )
        SELECT *
        FROM claimed
        "#,
    )
    .bind(operation_id)
    .bind(claimed_by)
    .bind(lease_seconds)
    .fetch_optional(db)
    .await?;

    Ok(item)
}

#[cfg(test)]
fn rebalance_item_is_claim_eligible(
    status: &str,
    claimed_until: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    status == "pending"
        || (status == "running" && claimed_until.is_some_and(|deadline| deadline <= now))
}

async fn mark_rebalance_item_completed(
    db: &PgPool,
    item: &ShardRebalanceItem,
    claim_token: Uuid,
) -> anyhow::Result<Option<ShardRebalanceItem>> {
    mark_rebalance_item_status(db, item, claim_token, REBALANCE_ITEM_STATUS_COMPLETED, None).await
}

async fn defer_rebalance_item(
    db: &PgPool,
    item: &ShardRebalanceItem,
    claim_token: Uuid,
    reason: &str,
) -> anyhow::Result<Option<ShardRebalanceItem>> {
    let item = sqlx::query_as::<_, ShardRebalanceItem>(
        r#"
        UPDATE shard_rebalance_items
        SET status = 'pending',
            error_message = $5,
            claim_token = NULL,
            claimed_by = NULL,
            claimed_until = NULL,
            completed_at = NULL,
            updated_at = now()
        WHERE operation_id = $1::uuid
          AND run_id = $2::uuid
          AND run_shard = $3
          AND status = 'running'
          AND claim_token = $4::uuid
        RETURNING *
        "#,
    )
    .bind(item.operation_id)
    .bind(item.run_id)
    .bind(item.run_shard)
    .bind(claim_token)
    .bind(reason)
    .fetch_optional(db)
    .await?;

    Ok(item)
}

async fn mark_rebalance_item_failed(
    db: &PgPool,
    item: &ShardRebalanceItem,
    claim_token: Uuid,
    error_message: &str,
) -> anyhow::Result<Option<ShardRebalanceItem>> {
    mark_rebalance_item_status(
        db,
        item,
        claim_token,
        REBALANCE_ITEM_STATUS_FAILED,
        Some(error_message),
    )
    .await
}

async fn mark_rebalance_item_cancelled(
    db: &PgPool,
    item: &ShardRebalanceItem,
    claim_token: Uuid,
) -> anyhow::Result<Option<ShardRebalanceItem>> {
    mark_rebalance_item_status(db, item, claim_token, REBALANCE_ITEM_STATUS_CANCELLED, None).await
}

async fn mark_rebalance_item_status(
    db: &PgPool,
    item: &ShardRebalanceItem,
    claim_token: Uuid,
    status: &str,
    error_message: Option<&str>,
) -> anyhow::Result<Option<ShardRebalanceItem>> {
    let item = sqlx::query_as::<_, ShardRebalanceItem>(
        r#"
        UPDATE shard_rebalance_items
        SET status = $5,
            error_message = $6,
            claim_token = NULL,
            claimed_by = NULL,
            claimed_until = NULL,
            completed_at = now(),
            updated_at = now()
        WHERE operation_id = $1::uuid
          AND run_id = $2::uuid
          AND run_shard = $3
          AND status = 'running'
          AND claim_token = $4::uuid
        RETURNING *
        "#,
    )
    .bind(item.operation_id)
    .bind(item.run_id)
    .bind(item.run_shard)
    .bind(claim_token)
    .bind(status)
    .bind(error_message)
    .fetch_optional(db)
    .await?;

    Ok(item)
}

async fn rebalance_operation_is_cancelled(db: &PgPool, operation_id: Uuid) -> anyhow::Result<bool> {
    let cancelled = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT status = 'cancelled'
        FROM shard_rebalance_operations
        WHERE id = $1::uuid
        "#,
    )
    .bind(operation_id)
    .fetch_one(db)
    .await?;

    Ok(cancelled)
}

fn record_rebalance_item_settlement(
    processed_items: &mut Vec<ShardRebalanceItem>,
    settled: Option<ShardRebalanceItem>,
    claimed_item: &ShardRebalanceItem,
    claim_token: Uuid,
    claimed_by: Uuid,
    status: &str,
) {
    let Some(settled) = settled else {
        warn!(
            operation_id = %claimed_item.operation_id,
            sequence_no = claimed_item.sequence_no,
            run_id = %claimed_item.run_id,
            run_shard = claimed_item.run_shard,
            claim_token = %claim_token,
            claimed_by = %claimed_by,
            attempted_status = status,
            "stale shard rebalance apply worker was fenced from settling item"
        );
        return;
    };

    match status {
        REBALANCE_ITEM_STATUS_COMPLETED => info!(
            operation_id = %settled.operation_id,
            sequence_no = settled.sequence_no,
            run_id = %settled.run_id,
            run_shard = settled.run_shard,
            claim_token = %claim_token,
            claimed_by = %claimed_by,
            "completed shard rebalance apply item"
        ),
        REBALANCE_ITEM_STATUS_FAILED => warn!(
            operation_id = %settled.operation_id,
            sequence_no = settled.sequence_no,
            run_id = %settled.run_id,
            run_shard = settled.run_shard,
            claim_token = %claim_token,
            claimed_by = %claimed_by,
            error = ?settled.error_message,
            "failed shard rebalance apply item"
        ),
        REBALANCE_ITEM_STATUS_CANCELLED => info!(
            operation_id = %settled.operation_id,
            sequence_no = settled.sequence_no,
            run_id = %settled.run_id,
            run_shard = settled.run_shard,
            claim_token = %claim_token,
            claimed_by = %claimed_by,
            "cancelled claimed shard rebalance apply item"
        ),
        _ => {}
    }
    processed_items.push(settled);
}

async fn apply_rebalance_item(
    database_router: &database::DatabaseRouter,
    item: &ShardRebalanceItem,
    force: bool,
) -> anyhow::Result<()> {
    let control_db = database_router.control().await?;
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

    let planned_route_is_current = current.database_alias == item.source_database_alias
        && current.status == SHARD_PLACEMENT_STATUS_ACTIVE
        && current.route_version == item.planned_route_version;
    let copying_move_is_resumable = current.database_alias == item.source_database_alias
        && current.status == SHARD_PLACEMENT_STATUS_COPYING
        && current.move_target_database_alias.as_deref() == Some(&item.target_database_alias)
        && item.planned_route_version.checked_add(1) == Some(current.route_version);
    let draining_move_is_resumable = current.database_alias == item.source_database_alias
        && current.status == SHARD_PLACEMENT_STATUS_DRAINING
        && current.move_target_database_alias.as_deref() == Some(&item.target_database_alias)
        && item.planned_route_version.checked_add(2) == Some(current.route_version);
    let moving_move_is_resumable = current.database_alias == item.source_database_alias
        && current.status == SHARD_PLACEMENT_STATUS_MOVING
        && current.move_target_database_alias.as_deref() == Some(&item.target_database_alias)
        && item.planned_route_version.checked_add(3) == Some(current.route_version);

    if !planned_route_is_current
        && !copying_move_is_resumable
        && !draining_move_is_resumable
        && !moving_move_is_resumable
    {
        return Err(StaleRebalancePlanError {
            run_id: item.run_id,
            run_shard: item.run_shard,
            expected_alias: item.source_database_alias.clone(),
            expected_version: item.planned_route_version,
            actual_alias: current.database_alias,
            actual_status: current.status,
            actual_version: current.route_version,
        }
        .into());
    }

    move_shard_placement(
        database_router,
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
    let mut tx = db.begin().await?;
    let current_status = sqlx::query_scalar::<_, String>(
        r#"
        SELECT status
        FROM shard_rebalance_operations
        WHERE id = $1::uuid
        FOR UPDATE
        "#,
    )
    .bind(operation_id)
    .fetch_one(tx.as_mut())
    .await?;
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
        .fetch_one(tx.as_mut())
        .await?;
    let status = match current_status.as_str() {
        REBALANCE_OPERATION_STATUS_CANCELLED => REBALANCE_OPERATION_STATUS_CANCELLED,
        REBALANCE_OPERATION_STATUS_COMPLETED => REBALANCE_OPERATION_STATUS_COMPLETED,
        REBALANCE_OPERATION_STATUS_FAILED => REBALANCE_OPERATION_STATUS_FAILED,
        _ if pending == 0 && running == 0 && failed == 0 => REBALANCE_OPERATION_STATUS_COMPLETED,
        _ if pending == 0 && running == 0 && failed > 0 => REBALANCE_OPERATION_STATUS_FAILED,
        _ => REBALANCE_OPERATION_STATUS_RUNNING,
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
            completed_at = CASE
                WHEN $2 IN ('completed', 'failed') THEN COALESCE(completed_at, now())
                ELSE completed_at
            END,
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
    .fetch_one(tx.as_mut())
    .await?;
    tx.commit().await?;

    Ok(operation)
}

async fn count_shard_owned_rows_with<'e, E>(
    executor: E,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<i64>
where
    E: Executor<'e, Database = Postgres>,
{
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(SUM(row_count), 0)::bigint
        FROM (
            SELECT COUNT(*)::bigint AS row_count FROM run_shard_cases WHERE run_id = $1::uuid AND run_shard = $2
            UNION ALL
            SELECT COUNT(*)::bigint FROM run_chunks WHERE run_id = $1::uuid AND run_shard = $2
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
    .fetch_one(executor)
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

/// Reserves an active move target while persisting the durable target reference.
///
/// Database drain and disable lock the same placement row `FOR UPDATE` before
/// checking target references. Holding `FOR SHARE` through this route update
/// makes either the move reservation or the lifecycle transition win first.
/// Holds a shared lifecycle lock until the caller commits its ownership write.
///
/// Drain and disable take `FOR UPDATE` on the same row, so either admission
/// commits first or the lifecycle transition wins and admission observes the
/// non-active status.
async fn validate_new_ownership_target(
    tx: &mut Transaction<'_, Postgres>,
    alias: &str,
) -> anyhow::Result<()> {
    let target = database_placements::select_database_placement_for_share(tx, alias)
        .await?
        .ok_or_else(|| anyhow::anyhow!("database placement alias {} was not found", alias))?;

    if !target.accepts_new_shards() {
        anyhow::bail!(
            "database placement alias {} has status {}, which cannot receive new shard ownership",
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

async fn validate_existing_ownership(db: &PgPool, alias: &str) -> anyhow::Result<()> {
    let placement = database_placements::select_database_placement(db, alias)
        .await?
        .ok_or_else(|| anyhow::anyhow!("database placement alias {} was not found", alias))?;

    if !placement.can_serve_owned_shards() {
        anyhow::bail!(
            "database placement alias {} has status {}, which cannot serve existing shard ownership",
            placement.alias,
            placement.status
        );
    }
    if !placement.is_shard_capable() {
        anyhow::bail!(
            "database placement alias {} has role {}, which is not shard-capable",
            placement.alias,
            placement.role
        );
    }

    Ok(())
}

async fn activate_moved_shard_placement_on_target(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    expected_database_alias: &str,
    expected_route_version: i64,
    target_database_alias: &str,
) -> anyhow::Result<Option<ShardPlacement>> {
    let mut tx = db.begin().await?;
    let placement = activate_moved_shard_placement_on_target_with(
        &mut tx,
        run_id,
        run_shard,
        expected_database_alias,
        expected_route_version,
        target_database_alias,
    )
    .await?;
    tx.commit().await?;
    Ok(placement)
}

async fn activate_moved_shard_placement_on_target_with(
    tx: &mut Transaction<'_, Postgres>,
    run_id: Uuid,
    run_shard: i16,
    expected_database_alias: &str,
    expected_route_version: i64,
    target_database_alias: &str,
) -> anyhow::Result<Option<ShardPlacement>> {
    validate_new_ownership_target(tx, target_database_alias).await?;
    shard_placements::activate_moved_shard_placement(
        &mut **tx,
        run_id,
        run_shard,
        expected_database_alias,
        expected_route_version,
        target_database_alias,
    )
    .await
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
    if !source.can_serve_owned_shards() {
        anyhow::bail!(
            "database placement alias {} has status {}, which cannot serve source shard ownership",
            source.alias,
            source.status
        );
    }

    Ok(())
}

const SHARD_MOVE_CLAIM_SECONDS: i32 = 300;

async fn claim_shard_move_operation(
    control_db: &PgPool,
    current: &ShardPlacement,
    target_database_alias: &str,
    claim_token: Uuid,
) -> anyhow::Result<ShardMoveOperation> {
    let mut tx = control_db.begin().await?;
    validate_new_ownership_target(&mut tx, target_database_alias).await?;

    let existing = sqlx::query_as::<_, ShardMoveOperation>(
        r#"
        SELECT id, run_id, run_shard, source_database_alias,
               target_database_alias, starting_route_version, status, phase,
               target_reset_at, copied_row_count, copied_byte_count, claim_token
        FROM shard_move_operations
        WHERE run_id = $1::uuid
          AND run_shard = $2
          AND status = 'active'
        FOR UPDATE
        "#,
    )
    .bind(current.run_id)
    .bind(current.run_shard)
    .fetch_optional(tx.as_mut())
    .await?;

    let operation = if let Some(operation) = existing {
        if operation.source_database_alias != current.database_alias
            || operation.target_database_alias != target_database_alias
        {
            anyhow::bail!(
                "run {} shard {} already has an active move from {} to {}",
                current.run_id,
                current.run_shard,
                operation.source_database_alias,
                operation.target_database_alias
            );
        }
        operation
    } else {
        sqlx::query_as::<_, ShardMoveOperation>(
            r#"
            INSERT INTO shard_move_operations (
                run_id, run_shard, source_database_alias,
                target_database_alias, starting_route_version
            )
            VALUES ($1::uuid, $2, $3, $4, $5)
            RETURNING id, run_id, run_shard, source_database_alias,
                      target_database_alias, starting_route_version, status, phase,
                      target_reset_at, copied_row_count, copied_byte_count, claim_token
            "#,
        )
        .bind(current.run_id)
        .bind(current.run_shard)
        .bind(&current.database_alias)
        .bind(target_database_alias)
        .bind(current.route_version)
        .fetch_one(tx.as_mut())
        .await?
    };

    let claimed = sqlx::query_as::<_, ShardMoveOperation>(
        r#"
        UPDATE shard_move_operations
        SET claim_token = $2::uuid,
            claimed_until = now() + make_interval(secs => $3),
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'active'
          AND (
              claim_token IS NULL
              OR claimed_until < now()
              OR claim_token = $2::uuid
          )
        RETURNING id, run_id, run_shard, source_database_alias,
                  target_database_alias, starting_route_version, status, phase,
                  target_reset_at, copied_row_count, copied_byte_count, claim_token
        "#,
    )
    .bind(operation.id)
    .bind(claim_token)
    .bind(SHARD_MOVE_CLAIM_SECONDS)
    .fetch_optional(tx.as_mut())
    .await?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "run {} shard {} move is currently claimed by another worker",
            current.run_id,
            current.run_shard
        )
    })?;
    tx.commit().await?;
    Ok(claimed)
}

async fn renew_shard_move_claim(
    control_db: &PgPool,
    move_id: Uuid,
    claim_token: Uuid,
) -> anyhow::Result<()> {
    let updated = sqlx::query(
        r#"
        UPDATE shard_move_operations
        SET claimed_until = now() + make_interval(secs => $3),
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'active'
          AND claim_token = $2::uuid
        "#,
    )
    .bind(move_id)
    .bind(claim_token)
    .bind(SHARD_MOVE_CLAIM_SECONDS)
    .execute(control_db)
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!("shard move {} lost its operation claim", move_id);
    }
    Ok(())
}

async fn release_shard_move_claim(
    control_db: &PgPool,
    move_id: Uuid,
    claim_token: Uuid,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        UPDATE shard_move_operations
        SET claim_token = NULL,
            claimed_until = NULL,
            updated_at = now()
        WHERE id = $1::uuid
          AND claim_token = $2::uuid
        "#,
    )
    .bind(move_id)
    .bind(claim_token)
    .execute(control_db)
    .await?;
    Ok(())
}

async fn settle_aborted_move_operation(control_db: &PgPool, move_id: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        UPDATE shard_move_operations
        SET status = 'aborted',
            phase = 'aborted',
            claim_token = NULL,
            claimed_until = NULL,
            completed_at = now(),
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'active'
        "#,
    )
    .bind(move_id)
    .execute(control_db)
    .await?;
    Ok(())
}

async fn settle_completed_move_operation(
    control_db: &PgPool,
    move_id: Uuid,
    claim_token: Option<Uuid>,
) -> anyhow::Result<()> {
    let updated = sqlx::query(
        r#"
        UPDATE shard_move_operations
        SET status = 'completed',
            phase = 'completed',
            claim_token = NULL,
            claimed_until = NULL,
            completed_at = COALESCE(completed_at, now()),
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'active'
          AND ($2::uuid IS NULL OR claim_token = $2::uuid)
        "#,
    )
    .bind(move_id)
    .bind(claim_token)
    .execute(control_db)
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!("shard move {} could not be marked completed", move_id);
    }
    Ok(())
}

async fn enable_shard_move_capture(
    source_db: &PgPool,
    move_id: Uuid,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<()> {
    let mut tx = source_db.begin().await?;
    crate::db::shard_write_fence::lock_exclusive(&mut tx, run_id, run_shard).await?;
    let captured_move = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO shard_move_captures (move_id, run_id, run_shard, active)
        VALUES ($1::uuid, $2::uuid, $3, true)
        ON CONFLICT (run_id, run_shard) DO UPDATE
        SET active = true
        WHERE shard_move_captures.move_id = EXCLUDED.move_id
        RETURNING move_id
        "#,
    )
    .bind(move_id)
    .bind(run_id)
    .bind(run_shard)
    .fetch_optional(tx.as_mut())
    .await?;
    if captured_move != Some(move_id) {
        anyhow::bail!(
            "run {} shard {} already has a different source capture",
            run_id,
            run_shard
        );
    }
    tx.commit().await?;
    Ok(())
}

async fn reserve_active_move_target_and_mark_copying(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    expected_database_alias: &str,
    expected_route_version: i64,
    target_database_alias: &str,
) -> anyhow::Result<Option<ShardPlacement>> {
    let mut tx = db.begin().await?;
    validate_new_ownership_target(&mut tx, target_database_alias).await?;
    let placement = shard_placements::mark_shard_placement_copying(
        &mut *tx,
        run_id,
        run_shard,
        expected_database_alias,
        expected_route_version,
        target_database_alias,
    )
    .await?;
    tx.commit().await?;
    Ok(placement)
}

fn move_table_key_columns(table: &str) -> anyhow::Result<&'static [&'static str]> {
    match table {
        "case_blobs" => Ok(&["case_hash"]),
        "dataset_versions" => Ok(&["dataset_version_id"]),
        "runs" => Ok(&["id"]),
        _ => SHARD_TABLES
            .iter()
            .find(|candidate| candidate.name == table)
            .map(|candidate| candidate.key_columns)
            .ok_or_else(|| anyhow::anyhow!("unsupported shard move table {}", table)),
    }
}

fn move_table_names() -> impl Iterator<Item = &'static str> {
    PREREQUISITE_TABLES
        .iter()
        .copied()
        .chain(SHARD_TABLES.iter().map(|table| table.name))
}

fn move_key_expression(table_alias: &str, key_columns: &[&str]) -> String {
    let columns = key_columns
        .iter()
        .map(|column| format!("to_jsonb({table_alias}.{column})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("jsonb_build_array({columns})::text")
}

async fn select_move_source_page(
    source_db: &PgPool,
    table: &str,
    run_id: Uuid,
    run_shard: i16,
    start_after_key: Option<&str>,
) -> anyhow::Result<Vec<MoveSourceRow>> {
    let key_expression = move_key_expression("source_row", move_table_key_columns(table)?);
    let filter = match table {
        "case_blobs" => {
            "EXISTS (
                SELECT 1 FROM run_shard_cases projected
                WHERE projected.run_id = $1::uuid
                  AND projected.run_shard = $2
                  AND projected.case_hash = source_row.case_hash
            )"
        }
        "dataset_versions" => {
            "EXISTS (
                SELECT 1 FROM runs run
                WHERE run.id = $1::uuid
                  AND run.dataset_version_id = source_row.dataset_version_id
            ) AND $2::smallint = $2"
        }
        "runs" => "source_row.id = $1::uuid AND $2::smallint = $2",
        _ => "source_row.run_id = $1::uuid AND source_row.run_shard = $2",
    };
    let sql = format!(
        r#"
        SELECT
            to_jsonb(source_row) AS row,
            {key_expression} AS row_key,
            octet_length(to_jsonb(source_row)::text)::integer AS row_bytes
        FROM {table} source_row
        WHERE {filter}
          AND ($3::text IS NULL OR {key_expression} > $3)
        ORDER BY {key_expression}
        LIMIT $4
        "#
    );
    let mut candidates = sqlx::query_as::<_, MoveSourceRow>(&sql)
        .bind(run_id)
        .bind(run_shard)
        .bind(start_after_key)
        .bind(SHARD_MOVE_COPY_BATCH_SIZE as i64)
        .fetch(source_db);
    let mut page = Vec::new();
    let mut row_bytes = Vec::new();
    while let Some(row) = candidates.try_next().await? {
        row_bytes.push(row.row_bytes.max(0) as usize);
        if bounded_page_len(
            &row_bytes,
            SHARD_MOVE_COPY_BATCH_SIZE,
            SHARD_MOVE_COPY_BATCH_BYTES,
        ) < row_bytes.len()
        {
            break;
        }
        page.push(row);
    }
    Ok(page)
}

async fn select_last_move_page(
    control_db: &PgPool,
    move_id: Uuid,
    table: &str,
) -> anyhow::Result<(i64, Option<String>)> {
    let page = sqlx::query_as::<_, (i64, String)>(
        r#"
        SELECT completed_page_count, last_end_key
        FROM shard_move_table_progress
        WHERE move_id = $1::uuid
          AND table_name = $2
        "#,
    )
    .bind(move_id)
    .bind(table)
    .fetch_optional(control_db)
    .await?;
    Ok(page
        .map(|(page_count, end_key)| (page_count, Some(end_key)))
        .unwrap_or((0, None)))
}

async fn record_completed_move_page(
    control_db: &PgPool,
    move_id: Uuid,
    claim_token: Uuid,
    table: &str,
    page_number: i64,
    start_after_key: Option<&str>,
    rows: &[MoveSourceRow],
) -> anyhow::Result<()> {
    let end_key = &rows.last().expect("move pages are non-empty").row_key;
    let row_count = rows.len() as i64;
    let byte_count = rows
        .iter()
        .map(|row| i64::from(row.row_bytes.max(0)))
        .sum::<i64>();
    let mut hasher = blake3::Hasher::new();
    for row in rows {
        let encoded = serde_json::to_vec(&row.row)?;
        hasher.update(&(encoded.len() as u64).to_be_bytes());
        hasher.update(&encoded);
    }
    let checksum = hasher.finalize().to_hex().to_string();

    let mut tx = control_db.begin().await?;
    let advanced = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO shard_move_table_progress (
            move_id, table_name, completed_page_count,
            last_start_after_key, last_end_key,
            copied_row_count, copied_byte_count, last_page_checksum
        )
        VALUES ($1::uuid, $2, 1, $4, $5, $6, $7, $8)
        ON CONFLICT (move_id, table_name) DO UPDATE
        SET completed_page_count =
                shard_move_table_progress.completed_page_count + 1,
            last_start_after_key = EXCLUDED.last_start_after_key,
            last_end_key = EXCLUDED.last_end_key,
            copied_row_count =
                shard_move_table_progress.copied_row_count
                + EXCLUDED.copied_row_count,
            copied_byte_count =
                shard_move_table_progress.copied_byte_count
                + EXCLUDED.copied_byte_count,
            last_page_checksum = EXCLUDED.last_page_checksum,
            updated_at = now()
        WHERE shard_move_table_progress.completed_page_count = $3
          AND shard_move_table_progress.last_end_key IS NOT DISTINCT FROM $4
        RETURNING completed_page_count
        "#,
    )
    .bind(move_id)
    .bind(table)
    .bind(page_number)
    .bind(start_after_key)
    .bind(end_key)
    .bind(row_count)
    .bind(byte_count)
    .bind(&checksum)
    .fetch_optional(tx.as_mut())
    .await?;
    if advanced.is_some_and(|page_count| page_count != page_number + 1) {
        anyhow::bail!(
            "shard move {} table {} cannot checkpoint page {} without its predecessor",
            move_id,
            table,
            page_number
        );
    }
    if advanced.is_none() {
        let existing = sqlx::query_as::<_, (i64, Option<String>, String)>(
            r#"
            SELECT completed_page_count, last_start_after_key, last_end_key
            FROM shard_move_table_progress
            WHERE move_id = $1::uuid
              AND table_name = $2
            "#,
        )
        .bind(move_id)
        .bind(table)
        .fetch_optional(tx.as_mut())
        .await?;
        if existing.as_ref()
            != Some(&(
                page_number + 1,
                start_after_key.map(str::to_string),
                end_key.clone(),
            ))
        {
            anyhow::bail!(
                "shard move {} table {} has a non-contiguous page checkpoint",
                move_id,
                table
            );
        }
    }
    let acknowledged_row_count = if advanced.is_some() { row_count } else { 0 };
    let acknowledged_byte_count = if advanced.is_some() { byte_count } else { 0 };
    let updated = sqlx::query(
        r#"
        UPDATE shard_move_operations
        SET phase = 'backfill',
            copied_row_count = copied_row_count + $3,
            copied_byte_count = copied_byte_count + $4,
            claimed_until = now() + make_interval(secs => $5),
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'active'
          AND claim_token = $2::uuid
        "#,
    )
    .bind(move_id)
    .bind(claim_token)
    .bind(acknowledged_row_count)
    .bind(acknowledged_byte_count)
    .bind(SHARD_MOVE_CLAIM_SECONDS)
    .execute(tx.as_mut())
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!(
            "shard move {} lost its claim before page acknowledgement",
            move_id
        );
    }
    tx.commit().await?;
    Ok(())
}

async fn backfill_shard_move(
    control_db: &PgPool,
    source_db: &PgPool,
    target_db: &PgPool,
    move_id: Uuid,
    claim_token: Uuid,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<()> {
    for table in move_table_names() {
        let (mut page_number, mut cursor) =
            select_last_move_page(control_db, move_id, table).await?;
        loop {
            let rows =
                select_move_source_page(source_db, table, run_id, run_shard, cursor.as_deref())
                    .await?;
            if rows.is_empty() {
                break;
            }
            let payload = rows.iter().map(|row| row.row.clone()).collect::<Vec<_>>();
            if PREREQUISITE_TABLES.contains(&table) {
                copy_json_rows(target_db, table, payload).await?;
            } else {
                upsert_json_rows(target_db, table, move_table_key_columns(table)?, payload).await?;
            }
            record_completed_move_page(
                control_db,
                move_id,
                claim_token,
                table,
                page_number,
                cursor.as_deref(),
                &rows,
            )
            .await?;
            cursor = rows.last().map(|row| row.row_key.clone());
            page_number += 1;
        }
    }
    let updated = sqlx::query(
        r#"
        UPDATE shard_move_operations
        SET phase = 'catch_up',
            claimed_until = now() + make_interval(secs => $3),
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'active'
          AND claim_token = $2::uuid
        "#,
    )
    .bind(move_id)
    .bind(claim_token)
    .bind(SHARD_MOVE_CLAIM_SECONDS)
    .execute(control_db)
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!("shard move {} lost its claim after backfill", move_id);
    }
    Ok(())
}

async fn select_dirty_shard_keys_for_table(
    source_db: &PgPool,
    move_id: Uuid,
    table: &str,
    key_columns: &[&str],
    current_row_exists: bool,
    limit: usize,
) -> anyhow::Result<Vec<DirtyShardKey>> {
    let predicate = key_join_predicate("source_row", "journal", key_columns);
    let existence = if current_row_exists {
        "EXISTS"
    } else {
        "NOT EXISTS"
    };
    let sql = format!(
        r#"
        SELECT journal.table_name, journal.row_key, journal.change_version
        FROM shard_move_dirty_keys journal
        WHERE journal.move_id = $1::uuid
          AND journal.table_name = $2
          AND {existence} (
              SELECT 1
              FROM {table} source_row
              WHERE {predicate}
          )
        ORDER BY journal.last_changed_at, journal.row_key
        LIMIT $3
        "#
    );
    Ok(sqlx::query_as::<_, DirtyShardKey>(&sql)
        .bind(move_id)
        .bind(table)
        .bind(limit as i64)
        .fetch_all(source_db)
        .await?)
}

fn key_join_predicate(table_alias: &str, key_alias: &str, key_columns: &[&str]) -> String {
    key_columns
        .iter()
        .map(|column| {
            let cast = if *column == "run_shard" {
                "smallint"
            } else if *column == "case_hash" {
                "text"
            } else {
                "uuid"
            };
            format!("{table_alias}.{column} = ({key_alias}.row_key->>'{column}')::{cast}")
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

async fn select_current_rows_for_dirty_keys(
    source_db: &PgPool,
    table: &str,
    key_columns: &[&str],
    keys: &[DirtyShardKey],
) -> anyhow::Result<BTreeMap<String, Value>> {
    if keys.is_empty() {
        return Ok(BTreeMap::new());
    }
    let predicate = key_join_predicate("source_row", "dirty", key_columns);
    let sql = format!(
        r#"
        WITH dirty AS (
            SELECT value AS row_key
            FROM jsonb_array_elements($1::jsonb)
        )
        SELECT dirty.row_key, to_jsonb(source_row)
        FROM dirty
        JOIN {table} source_row ON {predicate}
        "#
    );
    let rows = sqlx::query_as::<_, (Value, Value)>(&sql)
        .bind(Json(Value::Array(
            keys.iter().map(|key| key.row_key.clone()).collect(),
        )))
        .fetch_all(source_db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(key, row)| (key.to_string(), row))
        .collect())
}

async fn delete_target_rows_for_dirty_keys(
    target_db: &PgPool,
    table: &str,
    key_columns: &[&str],
    keys: &[Value],
) -> anyhow::Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let predicate = key_join_predicate("target_row", "dirty", key_columns);
    let sql = format!(
        r#"
        WITH dirty AS (
            SELECT value AS row_key
            FROM jsonb_array_elements($1::jsonb)
        )
        DELETE FROM {table} target_row
        USING dirty
        WHERE {predicate}
        "#
    );
    sqlx::query(&sql)
        .bind(Json(Value::Array(keys.to_vec())))
        .execute(target_db)
        .await?;
    Ok(())
}

async fn settle_replayed_dirty_keys(
    source_db: &PgPool,
    move_id: Uuid,
    keys: &[DirtyShardKey],
) -> anyhow::Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let replayed = keys
        .iter()
        .map(|key| {
            serde_json::json!({
                "table_name": key.table_name,
                "row_key": key.row_key,
                "change_version": key.change_version,
            })
        })
        .collect::<Vec<_>>();
    sqlx::query(
        r#"
        WITH replayed AS (
            SELECT table_name, row_key, change_version
            FROM jsonb_to_recordset($2::jsonb) AS key(
                table_name text,
                row_key jsonb,
                change_version bigint
            )
        )
        DELETE FROM shard_move_dirty_keys journal
        USING replayed
        WHERE journal.move_id = $1::uuid
          AND journal.table_name = replayed.table_name
          AND journal.row_key = replayed.row_key
          AND journal.change_version = replayed.change_version
        "#,
    )
    .bind(move_id)
    .bind(Json(replayed))
    .execute(source_db)
    .await?;
    Ok(())
}

async fn replay_dirty_shard_keys(
    source_db: &PgPool,
    target_db: &PgPool,
    move_id: Uuid,
    max_batches: usize,
) -> anyhow::Result<i64> {
    let mut completed_batches = 0;
    for table in SHARD_TABLES {
        while completed_batches < max_batches {
            let keys = select_dirty_shard_keys_for_table(
                source_db,
                move_id,
                table.name,
                table.key_columns,
                true,
                SHARD_MOVE_COPY_BATCH_SIZE,
            )
            .await?;
            if keys.is_empty() {
                break;
            }
            let current =
                select_current_rows_for_dirty_keys(source_db, table.name, table.key_columns, &keys)
                    .await?;
            let rows = current.values().cloned().collect::<Vec<_>>();
            upsert_json_rows(target_db, table.name, table.key_columns, rows).await?;
            let replayed = keys
                .into_iter()
                .filter(|key| current.contains_key(&key.row_key.to_string()))
                .collect::<Vec<_>>();
            settle_replayed_dirty_keys(source_db, move_id, &replayed).await?;
            completed_batches += 1;
        }
    }

    for table in SHARD_TABLES.iter().rev() {
        while completed_batches < max_batches {
            let keys = select_dirty_shard_keys_for_table(
                source_db,
                move_id,
                table.name,
                table.key_columns,
                false,
                SHARD_MOVE_COPY_BATCH_SIZE,
            )
            .await?;
            if keys.is_empty() {
                break;
            }
            let missing = keys
                .iter()
                .map(|key| key.row_key.clone())
                .collect::<Vec<_>>();
            delete_target_rows_for_dirty_keys(target_db, table.name, table.key_columns, &missing)
                .await?;
            settle_replayed_dirty_keys(source_db, move_id, &keys).await?;
            completed_batches += 1;
        }
    }

    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::bigint FROM shard_move_dirty_keys WHERE move_id = $1::uuid",
    )
    .bind(move_id)
    .fetch_one(source_db)
    .await?)
}

async fn ensure_move_target_reset(
    control_db: &PgPool,
    source_db: &PgPool,
    target_db: &PgPool,
    operation: &ShardMoveOperation,
    claim_token: Uuid,
) -> anyhow::Result<()> {
    if operation.target_reset_at.is_some() {
        return Ok(());
    }
    let mut source_tx = source_db.begin().await?;
    let same_database = databases_share_identity(&mut source_tx, target_db).await?;
    source_tx.commit().await?;
    if !same_database {
        reset_target_shard_rows(target_db, operation.run_id, operation.run_shard).await?;
    }
    let updated = sqlx::query(
        r#"
        UPDATE shard_move_operations
        SET target_reset_at = now(),
            phase = 'backfill',
            claimed_until = now() + make_interval(secs => $3),
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'active'
          AND claim_token = $2::uuid
          AND target_reset_at IS NULL
        "#,
    )
    .bind(operation.id)
    .bind(claim_token)
    .bind(SHARD_MOVE_CLAIM_SECONDS)
    .execute(control_db)
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!(
            "shard move {} lost its claim during target reset",
            operation.id
        );
    }
    Ok(())
}

async fn prepare_online_shard_move(
    database_router: &database::DatabaseRouter,
    source_db: &PgPool,
    target_db: &PgPool,
    current: ShardPlacement,
    operation: &ShardMoveOperation,
    claim_token: Uuid,
) -> anyhow::Result<ShardPlacement> {
    let control_db = database_router.control().await?;
    let mut identity_tx = source_db.begin().await?;
    let same_database = databases_share_identity(&mut identity_tx, target_db).await?;
    identity_tx.commit().await?;
    let mut route = current;
    if route.status == SHARD_PLACEMENT_STATUS_ACTIVE {
        if !same_database {
            enable_shard_move_capture(
                source_db,
                operation.id,
                operation.run_id,
                operation.run_shard,
            )
            .await?;
        }
        reserve_active_move_target_and_mark_copying(
            control_db,
            operation.run_id,
            operation.run_shard,
            &operation.source_database_alias,
            route.route_version,
            &operation.target_database_alias,
        )
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "run {} shard {} route changed before online copy reservation",
                operation.run_id,
                operation.run_shard
            )
        })?;
        database_router
            .invalidate_execution_placement(operation.run_id, operation.run_shard)
            .await;
    } else if !same_database {
        enable_shard_move_capture(
            source_db,
            operation.id,
            operation.run_id,
            operation.run_shard,
        )
        .await?;
    }

    if same_database {
        sqlx::query(
            r#"
            UPDATE shard_move_operations
            SET target_reset_at = COALESCE(target_reset_at, now()),
                phase = 'catch_up',
                updated_at = now()
            WHERE id = $1::uuid
              AND status = 'active'
              AND claim_token = $2::uuid
            "#,
        )
        .bind(operation.id)
        .bind(claim_token)
        .execute(control_db)
        .await?;
    } else {
        ensure_move_target_reset(control_db, source_db, target_db, operation, claim_token).await?;
        if !matches!(
            operation.phase.as_str(),
            "catch_up" | "draining" | "cutover"
        ) {
            backfill_shard_move(
                control_db,
                source_db,
                target_db,
                operation.id,
                claim_token,
                operation.run_id,
                operation.run_shard,
            )
            .await?;
        }
        let remaining_dirty = replay_dirty_shard_keys(
            source_db,
            target_db,
            operation.id,
            SHARD_MOVE_ONLINE_REPLAY_BATCHES,
        )
        .await?;
        if remaining_dirty > SHARD_MOVE_FINAL_DIRTY_KEY_LIMIT {
            return Err(ShardMoveCatchUpPending {
                run_id: operation.run_id,
                run_shard: operation.run_shard,
                remaining_dirty_key_count: remaining_dirty,
            }
            .into());
        }
    }
    renew_shard_move_claim(control_db, operation.id, claim_token).await?;

    route =
        shard_placements::select_shard_placement(control_db, operation.run_id, operation.run_shard)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "run {} shard {} route disappeared during online copy",
                    operation.run_id,
                    operation.run_shard
                )
            })?;
    if route.status == SHARD_PLACEMENT_STATUS_COPYING {
        route = shard_placements::mark_shard_placement_draining(
            control_db,
            operation.run_id,
            operation.run_shard,
            &operation.source_database_alias,
            route.route_version,
            &operation.target_database_alias,
        )
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "run {} shard {} route changed before drain cutover",
                operation.run_id,
                operation.run_shard
            )
        })?;
        database_router
            .invalidate_execution_placement(operation.run_id, operation.run_shard)
            .await;
    }
    sqlx::query(
        r#"
        UPDATE shard_move_operations
        SET phase = 'draining',
            claimed_until = now() + make_interval(secs => $3),
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'active'
          AND claim_token = $2::uuid
        "#,
    )
    .bind(operation.id)
    .bind(claim_token)
    .bind(SHARD_MOVE_CLAIM_SECONDS)
    .execute(control_db)
    .await?;
    Ok(route)
}

async fn checkpoint_move_reports(
    control_db: &PgPool,
    move_id: Uuid,
) -> anyhow::Result<Vec<ShardMoveTableReport>> {
    let page_rows = sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT
            table_name,
            copied_row_count
        FROM shard_move_table_progress
        WHERE move_id = $1::uuid
        "#,
    )
    .bind(move_id)
    .fetch_all(control_db)
    .await?;
    let pages = page_rows.into_iter().collect::<BTreeMap<_, _>>();
    Ok(move_table_names()
        .map(|table| {
            let rows = pages.get(table).copied().unwrap_or_default();
            ShardMoveTableReport {
                table,
                source_row_count: None,
                target_row_count: None,
                copied_row_count: rows as u64,
                source_checksum: None,
                target_checksum: None,
                verification_mode: "checkpoint_and_replay",
                verified: true,
            }
        })
        .collect())
}

async fn count_active_shard_work(db: &PgPool, run_id: Uuid, run_shard: i16) -> anyhow::Result<i64> {
    count_active_shard_work_with(db, run_id, run_shard).await
}

async fn count_active_shard_work_with<'e, E>(
    executor: E,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<i64>
where
    E: Executor<'e, Database = Postgres>,
{
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
    .fetch_one(executor)
    .await?;

    Ok(count)
}

async fn databases_share_identity(
    source: &mut Transaction<'_, Postgres>,
    target: &PgPool,
) -> anyhow::Result<bool> {
    type DatabaseIdentity = (String, Option<String>, Option<i32>, DateTime<Utc>);

    let sql = r#"
        SELECT
            current_database(),
            inet_server_addr()::text,
            inet_server_port(),
            pg_postmaster_start_time()
    "#;
    let source_identity = sqlx::query_as::<_, DatabaseIdentity>(sql)
        .fetch_one(&mut **source)
        .await?;
    let target_identity = sqlx::query_as::<_, DatabaseIdentity>(sql)
        .fetch_one(target)
        .await?;

    Ok(source_identity == target_identity)
}

/// Removes only the target's non-authoritative rows for one run shard.
///
/// Source ownership remains unchanged while this commits. Deletes run in
/// reverse dependency order and take target-side exclusive admission so stale
/// routed transactions cannot race the reset.
async fn reset_target_shard_rows(
    target: &PgPool,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<()> {
    let mut tx = target.begin().await?;
    crate::db::shard_write_fence::lock_exclusive(&mut tx, run_id, run_shard).await?;

    for table in SHARD_TABLES.iter().rev() {
        let sql = format!(
            "DELETE FROM {} WHERE run_id = $1::uuid AND run_shard = $2",
            table.name
        );
        sqlx::query(&sql)
            .bind(run_id)
            .bind(run_shard)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(())
}

async fn copy_json_rows(db: &PgPool, table: &str, rows: Vec<Value>) -> anyhow::Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }

    if table == "case_blobs" {
        let copied = copy_case_blob_rows(db, rows.clone()).await?;
        verify_prerequisite_rows(db, table, &rows).await?;
        return Ok(copied);
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
        .bind(Json(Value::Array(rows.clone())))
        .execute(db)
        .await?;

    verify_prerequisite_rows(db, table, &rows).await?;
    Ok(result.rows_affected())
}

fn normalized_prerequisite_row(table: &str, mut row: Value) -> anyhow::Result<Value> {
    let object = row
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} prerequisite row is not an object", table))?;
    let ignored: &[&str] = match table {
        "case_blobs" => &["created_at"],
        "dataset_versions" => &["created_at", "updated_at"],
        "runs" => &[
            "status",
            "gate_status",
            "coordinator_id",
            "coordinator_leased_until",
            "coordinator_heartbeat_at",
            "terminal_execution_count",
            "passed_execution_count",
            "failed_execution_count",
            "errored_execution_count",
            "summary",
            "error_message",
            "created_at",
            "started_at",
            "dispatched_at",
            "finalized_at",
            "completed_at",
            "updated_at",
        ],
        _ => anyhow::bail!("unsupported prerequisite table {}", table),
    };
    for field in ignored {
        object.remove(*field);
    }
    Ok(row)
}

async fn verify_prerequisite_rows(
    db: &PgPool,
    table: &str,
    source_rows: &[Value],
) -> anyhow::Result<()> {
    let (key_column, key_cast) = match table {
        "case_blobs" => ("case_hash", "text"),
        "dataset_versions" => ("dataset_version_id", "uuid"),
        "runs" => ("id", "uuid"),
        _ => anyhow::bail!("unsupported prerequisite table {}", table),
    };
    let sql = format!(
        r#"
        WITH expected AS (
            SELECT value AS row
            FROM jsonb_array_elements($1::jsonb)
        )
        SELECT to_jsonb(target_row)
        FROM expected
        JOIN {table} target_row
          ON target_row.{key_column} = (expected.row->>'{key_column}')::{key_cast}
        "#
    );
    let target_rows = sqlx::query_scalar::<_, Value>(&sql)
        .bind(Json(Value::Array(source_rows.to_vec())))
        .fetch_all(db)
        .await?;
    let normalize = |row: &Value| -> anyhow::Result<(String, Value)> {
        let key = row
            .get(key_column)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("{} prerequisite row is missing {}", table, key_column))?
            .to_string();
        Ok((key, normalized_prerequisite_row(table, row.clone())?))
    };
    let expected = source_rows
        .iter()
        .map(normalize)
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let actual = target_rows
        .iter()
        .map(normalize)
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    if expected != actual {
        anyhow::bail!(
            "{} prerequisite rows conflict with immutable target data",
            table
        );
    }
    Ok(())
}

async fn upsert_json_rows(
    db: &PgPool,
    table: &str,
    key_columns: &[&str],
    rows: Vec<Value>,
) -> anyhow::Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let columns = sqlx::query_scalar::<_, String>(
        r#"
        SELECT column_name
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = $1
        ORDER BY ordinal_position
        "#,
    )
    .bind(table)
    .fetch_all(db)
    .await?;
    if columns.is_empty() {
        anyhow::bail!("shard move target table {} was not found", table);
    }
    let quote = |identifier: &str| format!("\"{}\"", identifier.replace('"', "\"\""));
    let conflict_columns = key_columns
        .iter()
        .map(|column| quote(column))
        .collect::<Vec<_>>()
        .join(", ");
    let updates = columns
        .iter()
        .filter(|column| !key_columns.contains(&column.as_str()))
        .map(|column| {
            let quoted = quote(column);
            format!("{quoted} = EXCLUDED.{quoted}")
        })
        .collect::<Vec<_>>();
    let conflict_action = if updates.is_empty() {
        "DO NOTHING".to_string()
    } else {
        format!("DO UPDATE SET {}", updates.join(", "))
    };
    let sql = format!(
        r#"
        INSERT INTO {table}
        SELECT *
        FROM jsonb_populate_recordset(NULL::{table}, $1::jsonb)
        ON CONFLICT ({conflict_columns}) {conflict_action}
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
    let mut source_tx = source_db.begin().await?;
    let reports = verify_move_tables_with_source(
        &mut source_tx,
        target_db,
        run_id,
        run_shard,
        copied_rows_by_table,
    )
    .await?;
    source_tx.commit().await?;
    Ok(reports)
}

async fn verify_move_tables_with_source(
    source_tx: &mut Transaction<'_, Postgres>,
    target_db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    copied_rows_by_table: &std::collections::BTreeMap<&'static str, u64>,
) -> anyhow::Result<Vec<ShardMoveTableReport>> {
    let mut reports = Vec::with_capacity(PREREQUISITE_TABLES.len() + SHARD_TABLES.len());

    for table in PREREQUISITE_TABLES {
        let source =
            prerequisite_table_fingerprint_with(&mut **source_tx, table, run_id, run_shard).await?;
        let target = prerequisite_table_fingerprint(target_db, table, run_id, run_shard).await?;
        let verified = source.row_count == target.row_count && source.checksum == target.checksum;
        reports.push(ShardMoveTableReport {
            table,
            source_row_count: Some(source.row_count),
            target_row_count: Some(target.row_count),
            copied_row_count: copied_rows_by_table.get(table).copied().unwrap_or_default(),
            source_checksum: Some(source.checksum),
            target_checksum: Some(target.checksum),
            verification_mode: "full_fingerprint",
            verified,
        });
    }

    for table in SHARD_TABLES {
        let source =
            table_fingerprint_with(&mut **source_tx, table.name, run_id, run_shard).await?;
        let target = table_fingerprint(target_db, table.name, run_id, run_shard).await?;
        let verified = source.row_count == target.row_count && source.checksum == target.checksum;
        reports.push(ShardMoveTableReport {
            table: table.name,
            source_row_count: Some(source.row_count),
            target_row_count: Some(target.row_count),
            copied_row_count: copied_rows_by_table
                .get(table.name)
                .copied()
                .unwrap_or_default(),
            source_checksum: Some(source.checksum),
            target_checksum: Some(target.checksum),
            verification_mode: "full_fingerprint",
            verified,
        });
    }

    Ok(reports)
}

async fn prerequisite_table_fingerprint(
    db: &PgPool,
    table: &str,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<TableFingerprint> {
    prerequisite_table_fingerprint_with(db, table, run_id, run_shard).await
}

async fn prerequisite_table_fingerprint_with<'e, E>(
    executor: E,
    table: &str,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<TableFingerprint>
where
    E: Executor<'e, Database = Postgres>,
{
    let sql = match table {
        "case_blobs" => {
            r#"
            SELECT (to_jsonb(cb) - 'created_at')::text AS row_json
            FROM case_blobs cb
            WHERE EXISTS (
                SELECT 1
                FROM run_shard_cases projected
                WHERE projected.run_id = $1::uuid
                  AND projected.run_shard = $2
                  AND projected.case_hash = cb.case_hash
            )
            ORDER BY row_json
            "#
        }
        "dataset_versions" => {
            r#"
            SELECT (to_jsonb(dv) - 'created_at' - 'updated_at')::text AS row_json
            FROM dataset_versions dv
            JOIN runs r
              ON r.dataset_version_id = dv.dataset_version_id
            WHERE r.id = $1::uuid
              AND $2::smallint = $2
            ORDER BY row_json
            "#
        }
        "runs" => {
            r#"
            SELECT (
                to_jsonb(r)
                - 'status'
                - 'gate_status'
                - 'coordinator_id'
                - 'coordinator_leased_until'
                - 'coordinator_heartbeat_at'
                - 'terminal_execution_count'
                - 'passed_execution_count'
                - 'failed_execution_count'
                - 'errored_execution_count'
                - 'summary'
                - 'error_message'
                - 'created_at'
                - 'started_at'
                - 'dispatched_at'
                - 'finalized_at'
                - 'completed_at'
                - 'updated_at'
            )::text AS row_json
            FROM runs r
            WHERE r.id = $1::uuid
              AND $2::smallint = $2
            ORDER BY row_json
            "#
        }
        _ => anyhow::bail!("unsupported prerequisite table {}", table),
    };

    let rows = sqlx::query_scalar::<_, String>(sql)
        .bind(run_id)
        .bind(run_shard)
        .fetch(executor);
    fingerprint_ordered_rows(rows).await
}

async fn table_fingerprint(
    db: &PgPool,
    table: &str,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<TableFingerprint> {
    table_fingerprint_with(db, table, run_id, run_shard).await
}

async fn table_fingerprint_with<'e, E>(
    executor: E,
    table: &str,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<TableFingerprint>
where
    E: Executor<'e, Database = Postgres>,
{
    let sql = format!(
        r#"
        SELECT to_jsonb(t)::text AS row_json
        FROM {table} t
        WHERE run_id = $1::uuid
          AND run_shard = $2
        ORDER BY row_json
        "#
    );

    let rows = sqlx::query_scalar::<_, String>(&sql)
        .bind(run_id)
        .bind(run_shard)
        .fetch(executor);
    fingerprint_ordered_rows(rows).await
}

async fn fingerprint_ordered_rows<'a>(
    mut rows: impl futures_util::TryStream<Ok = String, Error = sqlx::Error> + Unpin + 'a,
) -> anyhow::Result<TableFingerprint> {
    let mut hasher = blake3::Hasher::new();
    let mut row_count = 0_i64;

    while let Some(row) = rows.try_next().await? {
        let bytes = row.as_bytes();
        hasher.update(&(bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
        row_count += 1;
    }

    Ok(TableFingerprint {
        row_count,
        checksum: hasher.finalize().to_hex().to_string(),
    })
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

async fn ensure_run_creation_is_inactive(db: &PgPool, run_id: Uuid) -> anyhow::Result<()> {
    let status =
        sqlx::query_scalar::<_, String>("SELECT status::text FROM runs WHERE id = $1::uuid")
            .bind(run_id)
            .fetch_optional(db)
            .await?;
    if status.as_deref() == Some("creating") {
        anyhow::bail!(
            "run {} is still creating; shard routes cannot change until creation finishes",
            run_id
        );
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
            DatabaseRouter,
            PlacementConfig,
            new_shard_placement_cache,
        },
        models::database_placement::{
            DEFAULT_DATABASE_ALIAS,
            DEFAULT_DATABASE_URL_ENV,
        },
    };

    #[test]
    fn move_page_is_bounded_by_rows_and_bytes() {
        assert_eq!(bounded_page_len(&[4, 4, 4, 4], 3, 100), 3);
        assert_eq!(bounded_page_len(&[4, 4, 4, 4], 10, 9), 2);
    }

    #[test]
    fn move_page_allows_one_oversized_row_to_make_progress() {
        assert_eq!(bounded_page_len(&[20, 2], 10, 10), 1);
        assert_eq!(bounded_page_len(&[], 10, 10), 0);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn move_table_progress_is_compact_and_idempotent(pool: PgPool) {
        sqlx::query(
            "INSERT INTO database_placements (alias, database_url_env, role, status) VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        let claim_token = Uuid::now_v7();
        let move_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO shard_move_operations (
                run_id, run_shard, source_database_alias,
                target_database_alias, starting_route_version,
                claim_token, claimed_until
            )
            VALUES (
                $1, 3, 'primary', 'shard_001', 1,
                $2, now() + interval '5 minutes'
            )
            RETURNING id
            "#,
        )
        .bind(run_id)
        .bind(claim_token)
        .fetch_one(&pool)
        .await
        .unwrap();
        let first = MoveSourceRow {
            row: serde_json::json!({"id": 1}),
            row_key: "key-1".to_string(),
            row_bytes: 16,
        };
        let second = MoveSourceRow {
            row: serde_json::json!({"id": 2}),
            row_key: "key-2".to_string(),
            row_bytes: 16,
        };

        record_completed_move_page(
            &pool,
            move_id,
            claim_token,
            "run_chunks",
            0,
            None,
            std::slice::from_ref(&first),
        )
        .await
        .unwrap();
        record_completed_move_page(
            &pool,
            move_id,
            claim_token,
            "run_chunks",
            0,
            None,
            std::slice::from_ref(&first),
        )
        .await
        .unwrap();
        record_completed_move_page(
            &pool,
            move_id,
            claim_token,
            "run_chunks",
            1,
            Some(&first.row_key),
            &[second],
        )
        .await
        .unwrap();

        let (progress_rows, completed_pages, copied_rows, operation_rows) =
            sqlx::query_as::<_, (i64, i64, i64, i64)>(
                r#"
                SELECT
                    COUNT(*)::bigint,
                    MAX(completed_page_count),
                    MAX(copied_row_count),
                    (
                        SELECT copied_row_count
                        FROM shard_move_operations
                        WHERE id = $1
                    )
                FROM shard_move_table_progress
                WHERE move_id = $1
                  AND table_name = 'run_chunks'
                "#,
            )
            .bind(move_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(progress_rows, 1);
        assert_eq!(completed_pages, 2);
        assert_eq!(copied_rows, 2);
        assert_eq!(operation_rows, 2);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn move_capture_coalesces_repeated_shard_mutations(pool: PgPool) {
        let run_id = Uuid::now_v7();
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        let chunk_id = Uuid::now_v7();
        let move_id = Uuid::now_v7();
        seed_run(&pool, run_id, dataset_id, dataset_version_id).await;
        sqlx::query(
            "INSERT INTO shard_move_captures (move_id, run_id, run_shard) VALUES ($1, $2, 3)",
        )
        .bind(move_id)
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO run_chunks (
                id, run_id, run_shard, dataset_version_id,
                profile_group_id, ordinal_start, ordinal_end
            )
            VALUES ($1, $2, 3, $3, 'default', 0, 1)
            "#,
        )
        .bind(chunk_id)
        .bind(run_id)
        .bind(dataset_version_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            UPDATE run_chunks
            SET recovery_count = recovery_count + 1
            WHERE run_id = $1 AND run_shard = 3 AND id = $2
            "#,
        )
        .bind(run_id)
        .bind(chunk_id)
        .execute(&pool)
        .await
        .unwrap();

        let (table_name, row_key, version) = sqlx::query_as::<_, (String, Value, i64)>(
            r#"
                SELECT table_name, row_key, change_version
                FROM shard_move_dirty_keys
                WHERE move_id = $1
                "#,
        )
        .bind(move_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(table_name, "run_chunks");
        assert_eq!(row_key["run_id"], run_id.to_string());
        assert_eq!(row_key["run_shard"], 3);
        assert_eq!(row_key["id"], chunk_id.to_string());
        assert_eq!(version, 2);

        let stale_key = DirtyShardKey {
            table_name: table_name.clone(),
            row_key: row_key.clone(),
            change_version: version - 1,
        };
        settle_replayed_dirty_keys(&pool, move_id, &[stale_key])
            .await
            .unwrap();
        let retained_version = sqlx::query_scalar::<_, i64>(
            "SELECT change_version FROM shard_move_dirty_keys WHERE move_id = $1",
        )
        .bind(move_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(retained_version, version);

        let current_key = DirtyShardKey {
            table_name,
            row_key,
            change_version: version,
        };
        settle_replayed_dirty_keys(&pool, move_id, &[current_key])
            .await
            .unwrap();
        let settled_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::bigint FROM shard_move_dirty_keys WHERE move_id = $1",
        )
        .bind(move_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(settled_count, 0);

        sqlx::query(
            "UPDATE run_chunks SET recovery_count = recovery_count + 1 WHERE run_id = $1 AND run_shard = 3 AND id = $2",
        )
        .bind(run_id)
        .bind(chunk_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM run_chunks WHERE run_id = $1 AND run_shard = 3 AND id = $2")
            .bind(run_id)
            .bind(chunk_id)
            .execute(&pool)
            .await
            .unwrap();
        let existing = select_dirty_shard_keys_for_table(
            &pool,
            move_id,
            "run_chunks",
            &["run_id", "run_shard", "id"],
            true,
            10,
        )
        .await
        .unwrap();
        let deleted = select_dirty_shard_keys_for_table(
            &pool,
            move_id,
            "run_chunks",
            &["run_id", "run_shard", "id"],
            false,
            10,
        )
        .await
        .unwrap();
        assert!(existing.is_empty());
        assert_eq!(deleted.len(), 1);

        sqlx::query("DELETE FROM shard_move_captures WHERE move_id = $1")
            .bind(move_id)
            .execute(&pool)
            .await
            .unwrap();
        let dirty_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::bigint FROM shard_move_dirty_keys WHERE move_id = $1",
        )
        .bind(move_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(dirty_count, 0);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn abort_fences_a_prepared_move_before_copying_starts(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO shard_placements (run_id, run_shard, database_alias, status) VALUES ($1, 3, 'primary', 'active')",
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let move_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO shard_move_operations (
                run_id, run_shard, source_database_alias,
                target_database_alias, starting_route_version
            )
            VALUES ($1, 3, 'primary', 'shard_001', 1)
            RETURNING id
            "#,
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO shard_move_captures (move_id, run_id, run_shard) VALUES ($1, $2, 3)",
        )
        .bind(move_id)
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool.clone()).await;

        let outcome = abort_shard_move(&database_router, run_id, 3, "primary", "shard_001")
            .await
            .unwrap();

        assert!(outcome.aborted);
        assert_eq!(outcome.placement.status, SHARD_PLACEMENT_STATUS_ACTIVE);
        assert_eq!(outcome.placement.route_version, 2);
        let operation_status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM shard_move_operations WHERE id = $1",
        )
        .bind(move_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(operation_status, "aborted");
        assert!(
            shard_placements::mark_shard_placement_copying(
                &pool,
                run_id,
                3,
                "primary",
                1,
                "shard_001",
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn completed_route_retry_settles_ambiguous_cutover_state(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO shard_placements (run_id, run_shard, database_alias, status, route_version) VALUES ($1, 3, 'shard_001', 'active', 5)",
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let move_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO shard_move_operations (
                run_id, run_shard, source_database_alias,
                target_database_alias, starting_route_version, phase
            )
            VALUES ($1, 3, 'primary', 'shard_001', 1, 'cutover')
            RETURNING id
            "#,
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO shard_move_captures (move_id, run_id, run_shard) VALUES ($1, $2, 3)",
        )
        .bind(move_id)
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool.clone()).await;

        let outcome = move_shard_placement(
            &database_router,
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

        assert!(!outcome.moved);
        let (operation_status, capture_count) = sqlx::query_as::<_, (String, i64)>(
            r#"
                SELECT
                    status,
                    (SELECT COUNT(*)::bigint FROM shard_move_captures WHERE move_id = $1)
                FROM shard_move_operations
                WHERE id = $1
                "#,
        )
        .bind(move_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(operation_status, "completed");
        assert_eq!(capture_count, 0);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn move_to_current_database_is_not_treated_as_a_completed_retry(pool: PgPool) {
        let run_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO shard_placements (run_id, run_shard, database_alias, status) VALUES ($1, 3, 'primary', 'active')",
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool).await;

        let error = move_shard_placement(
            &database_router,
            run_id,
            3,
            "primary",
            ShardMoveOptions {
                dry_run: false,
                verify_only: false,
                force: false,
            },
        )
        .await
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("already routes to database placement primary")
        );
    }

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
        let database_router = database_router_with_isolated_control_pool(pool).await;
        let error = disable_database_placement(&database_router, DEFAULT_DATABASE_ALIAS)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("control-capable and active"));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn draining_database_placement_rejects_new_routes_but_keeps_existing_routes(
        pool: PgPool,
    ) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1::uuid, 3, 'shard_001', 'active')
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();

        let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
        let placement = drain_database_placement(&database_router, "shard_001")
            .await
            .unwrap();
        assert_eq!(placement.status, DATABASE_PLACEMENT_STATUS_DRAINING);
        let repeated = drain_database_placement(&database_router, "shard_001")
            .await
            .unwrap();
        assert_eq!(repeated.updated_at, placement.updated_at);

        let persisted_route = shard_placements::select_shard_placement(&pool, run_id, 3)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted_route.database_alias, "shard_001");

        let error = set_shard_placement(&database_router, Uuid::now_v7(), 4, "shard_001")
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot receive new shard ownership")
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn projection_rows_prevent_direct_empty_shard_reassignment(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        seed_run(&pool, run_id, Uuid::now_v7(), dataset_version_id).await;
        let case_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO case_blobs (case_hash, task_type, input_payload, expected_output) VALUES ('projection-only', 'test', '{}'::jsonb, 'null'::jsonb)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO run_shard_cases (
                run_id, run_shard, dataset_version_id,
                case_id, case_ordinal, case_hash
            )
            VALUES ($1, 3, $2, $3, 0, 'projection-only')
            "#,
        )
        .bind(run_id)
        .bind(dataset_version_id)
        .bind(case_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO shard_placements (run_id, run_shard, database_alias, status) VALUES ($1, 3, 'primary', 'active')",
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool).await;

        let error = set_shard_placement(&database_router, run_id, 3, "shard_001")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("already has 1 shard-owned row"));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn disable_requires_draining_placement_without_owned_routes(pool: PgPool) {
        let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1::uuid, 3, 'shard_001', 'active')
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();

        let active_error = disable_database_placement(&database_router, "shard_001")
            .await
            .unwrap_err();
        assert!(active_error.to_string().contains("must be draining"));

        drain_database_placement(&database_router, "shard_001")
            .await
            .unwrap();
        let owned_error = disable_database_placement(&database_router, "shard_001")
            .await
            .unwrap_err();
        assert!(owned_error.to_string().contains("still owns 1 shard route"));

        sqlx::query("DELETE FROM shard_placements WHERE database_alias = 'shard_001'")
            .execute(&pool)
            .await
            .unwrap();
        let disabled = disable_database_placement(&database_router, "shard_001")
            .await
            .unwrap();
        assert_eq!(disabled.status, DATABASE_PLACEMENT_STATUS_DISABLED);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn move_activation_rejects_target_changed_outside_lifecycle_workflow(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (
                run_id,
                run_shard,
                database_alias,
                status,
                move_target_database_alias,
                route_version
            )
            VALUES ($1::uuid, 3, 'primary', 'moving', 'shard_001', 3)
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            UPDATE database_placements
            SET status = 'draining',
                updated_at = now()
            WHERE alias = 'shard_001'
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        let error =
            activate_moved_shard_placement_on_target(&pool, run_id, 3, "primary", 3, "shard_001")
                .await
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot receive new shard ownership"),
            "{error:#}"
        );

        let route = shard_placements::select_shard_placement(&pool, run_id, 3)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.database_alias, "primary");
        assert_eq!(route.status, SHARD_PLACEMENT_STATUS_MOVING);
        assert_eq!(route.route_version, 3);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn drain_rejects_database_referenced_by_inflight_move(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (
                run_id,
                run_shard,
                database_alias,
                status,
                move_target_database_alias,
                route_version
            )
            VALUES ($1::uuid, 3, 'primary', 'moving', 'shard_001', 3)
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool.clone()).await;

        let error = drain_database_placement(&database_router, "shard_001")
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("target of 1 in-flight shard move"),
            "{error:#}"
        );
        let placement = database_placements::select_database_placement(&pool, "shard_001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(placement.status, DATABASE_PLACEMENT_STATUS_ACTIVE);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn disable_rejects_database_referenced_by_inflight_move(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'draining')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (
                run_id,
                run_shard,
                database_alias,
                status,
                move_target_database_alias,
                route_version
            )
            VALUES ($1::uuid, 3, 'primary', 'moving', 'shard_001', 3)
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool).await;

        let error = disable_database_placement(&database_router, "shard_001")
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("target of 1 in-flight shard move"),
            "{error:#}"
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn concurrent_database_drain_wins_before_move_target_reservation(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
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

        let mut lifecycle_tx = pool.begin().await.unwrap();
        database_placements::select_database_placement_for_update(&mut lifecycle_tx, "shard_001")
            .await
            .unwrap()
            .unwrap();
        database_placements::update_database_placement_status(
            &mut lifecycle_tx,
            "shard_001",
            DATABASE_PLACEMENT_STATUS_DRAINING,
        )
        .await
        .unwrap()
        .unwrap();

        let reservation_pool = pool.clone();
        let mut reservation = tokio::spawn(async move {
            reserve_active_move_target_and_mark_copying(
                &reservation_pool,
                run_id,
                3,
                DEFAULT_DATABASE_ALIAS,
                1,
                "shard_001",
            )
            .await
        });
        tokio::select! {
            result = &mut reservation => {
                panic!("move target reservation bypassed the placement lifecycle lock: {result:?}");
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        lifecycle_tx.commit().await.unwrap();
        let error = reservation.await.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot receive new shard ownership"),
            "{error:#}"
        );

        let route = shard_placements::select_shard_placement(&pool, run_id, 3)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.status, SHARD_PLACEMENT_STATUS_ACTIVE);
        assert!(route.move_target_database_alias.is_none());
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn concurrent_move_target_reservation_wins_before_database_drain(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
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

        let mut reservation_tx = pool.begin().await.unwrap();
        validate_new_ownership_target(&mut reservation_tx, "shard_001")
            .await
            .unwrap();
        shard_placements::mark_shard_placement_copying(
            &mut *reservation_tx,
            run_id,
            3,
            DEFAULT_DATABASE_ALIAS,
            1,
            "shard_001",
        )
        .await
        .unwrap()
        .unwrap();

        let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
        let mut drain =
            tokio::spawn(
                async move { drain_database_placement(&database_router, "shard_001").await },
            );
        tokio::select! {
            result = &mut drain => {
                panic!("database drain bypassed the move target reservation: {result:?}");
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        reservation_tx.commit().await.unwrap();
        let error = drain.await.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("target of 1 in-flight shard move"),
            "{error:#}"
        );

        let placement = database_placements::select_database_placement(&pool, "shard_001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(placement.status, DATABASE_PLACEMENT_STATUS_ACTIVE);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn abort_draining_shard_move_restores_source_route_idempotently(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (
                run_id,
                run_shard,
                database_alias,
                status,
                move_target_database_alias,
                route_version
            )
            VALUES ($1::uuid, 3, 'primary', 'draining', 'shard_001', 2)
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool).await;

        let first = abort_shard_move(
            &database_router,
            run_id,
            3,
            DEFAULT_DATABASE_ALIAS,
            "shard_001",
        )
        .await
        .unwrap();
        assert!(first.aborted);
        assert_eq!(first.placement.database_alias, DEFAULT_DATABASE_ALIAS);
        assert_eq!(first.placement.status, SHARD_PLACEMENT_STATUS_ACTIVE);
        assert!(first.placement.move_target_database_alias.is_none());
        assert_eq!(first.placement.route_version, 3);

        let repeated = abort_shard_move(
            &database_router,
            run_id,
            3,
            DEFAULT_DATABASE_ALIAS,
            "shard_001",
        )
        .await
        .unwrap();
        assert!(!repeated.aborted);
        assert_eq!(repeated.placement.route_version, 3);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn abort_moving_shard_move_restores_source_route(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (
                run_id,
                run_shard,
                database_alias,
                status,
                move_target_database_alias,
                route_version
            )
            VALUES ($1::uuid, 3, 'primary', 'moving', 'shard_001', 3)
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool).await;

        let outcome = abort_shard_move(
            &database_router,
            run_id,
            3,
            DEFAULT_DATABASE_ALIAS,
            "shard_001",
        )
        .await
        .unwrap();

        assert!(outcome.aborted);
        assert_eq!(outcome.placement.database_alias, DEFAULT_DATABASE_ALIAS);
        assert_eq!(outcome.placement.status, SHARD_PLACEMENT_STATUS_ACTIVE);
        assert!(outcome.placement.move_target_database_alias.is_none());
        assert_eq!(outcome.placement.route_version, 4);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn abort_rejects_route_that_completed_on_target(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (
                run_id,
                run_shard,
                database_alias,
                status,
                route_version
            )
            VALUES ($1::uuid, 3, 'shard_001', 'active', 4)
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool).await;

        let error = abort_shard_move(
            &database_router,
            run_id,
            3,
            DEFAULT_DATABASE_ALIAS,
            "shard_001",
        )
        .await
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("already completed on database placement shard_001"),
            "{error:#}"
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn stale_mover_cannot_restart_after_abort_advances_route_fence(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (
                run_id,
                run_shard,
                database_alias,
                status,
                move_target_database_alias,
                route_version
            )
            VALUES ($1::uuid, 3, 'primary', 'draining', 'shard_001', 2)
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();

        let mut abort_tx = pool.begin().await.unwrap();
        crate::db::shard_write_fence::lock_exclusive(&mut abort_tx, run_id, 3)
            .await
            .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
        let mover = tokio::spawn(async move {
            move_shard_placement(
                &database_router,
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
        });

        wait_for_waiting_advisory_lock(&pool, "mover").await;

        shard_placements::abort_shard_placement_move(
            &mut *abort_tx,
            run_id,
            3,
            DEFAULT_DATABASE_ALIAS,
            2,
            "shard_001",
        )
        .await
        .unwrap()
        .unwrap();
        abort_tx.commit().await.unwrap();

        let error = mover.await.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("route changed while waiting for move admission"),
            "{error:#}"
        );
        let route = shard_placements::select_shard_placement(&pool, run_id, 3)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.database_alias, DEFAULT_DATABASE_ALIAS);
        assert_eq!(route.status, SHARD_PLACEMENT_STATUS_ACTIVE);
        assert!(route.move_target_database_alias.is_none());
        assert_eq!(route.route_version, 3);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn stale_abort_cannot_cancel_newer_move_route_version(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (
                run_id,
                run_shard,
                database_alias,
                status,
                move_target_database_alias,
                route_version
            )
            VALUES ($1::uuid, 3, 'primary', 'draining', 'shard_001', 2)
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();

        let mut move_tx = pool.begin().await.unwrap();
        crate::db::shard_write_fence::lock_exclusive(&mut move_tx, run_id, 3)
            .await
            .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
        let abort = tokio::spawn(async move {
            abort_shard_move(
                &database_router,
                run_id,
                3,
                DEFAULT_DATABASE_ALIAS,
                "shard_001",
            )
            .await
        });

        wait_for_waiting_advisory_lock(&pool, "abort").await;

        shard_placements::mark_shard_placement_moving(
            &mut *move_tx,
            run_id,
            3,
            DEFAULT_DATABASE_ALIAS,
            2,
            "shard_001",
        )
        .await
        .unwrap()
        .unwrap();
        move_tx.commit().await.unwrap();

        let error = abort.await.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("route changed while waiting for move-abort admission"),
            "{error:#}"
        );
        let route = shard_placements::select_shard_placement(&pool, run_id, 3)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.status, SHARD_PLACEMENT_STATUS_MOVING);
        assert_eq!(route.route_version, 3);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn concurrent_drain_serializes_before_move_activation(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (
                run_id,
                run_shard,
                database_alias,
                status,
                move_target_database_alias,
                route_version
            )
            VALUES ($1::uuid, 3, 'primary', 'moving', 'shard_001', 3)
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();

        let mut lifecycle_tx = pool.begin().await.unwrap();
        database_placements::select_database_placement_for_update(&mut lifecycle_tx, "shard_001")
            .await
            .unwrap()
            .unwrap();

        let activation_pool = pool.clone();
        let mut activation = tokio::spawn(async move {
            activate_moved_shard_placement_on_target(
                &activation_pool,
                run_id,
                3,
                "primary",
                3,
                "shard_001",
            )
            .await
        });
        tokio::select! {
            result = &mut activation => {
                panic!("move activation bypassed the placement lifecycle lock: {result:?}");
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        database_placements::update_database_placement_status(
            &mut lifecycle_tx,
            "shard_001",
            DATABASE_PLACEMENT_STATUS_DRAINING,
        )
        .await
        .unwrap()
        .unwrap();
        lifecycle_tx.commit().await.unwrap();

        let error = activation.await.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot receive new shard ownership"),
            "{error:#}"
        );

        let route = shard_placements::select_shard_placement(&pool, run_id, 3)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.database_alias, "primary");
        assert_eq!(route.status, SHARD_PLACEMENT_STATUS_MOVING);
        assert_eq!(route.route_version, 3);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn disable_rejects_pending_outbox_delivery(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'draining')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO outbox_events (
                event_type,
                aggregate_type,
                aggregate_id,
                dedupe_key
            )
            VALUES ('test.event', 'test', $1::uuid, $2)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(format!("drain-test-{}", Uuid::now_v7()))
        .execute(&pool)
        .await
        .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool).await;

        let error = disable_database_placement(&database_router, "shard_001")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("pending outbox delivery"));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn inspect_shard_route_keeps_draining_owner_dispatchable(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'draining')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1::uuid, 4, 'shard_001', 'active')
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let database_router = database_router_with_isolated_control_pool(pool).await;

        let route = inspect_shard_route(&database_router, run_id, 4)
            .await
            .unwrap();

        assert!(route.dispatchable);
        assert!(route.readable);
        assert_eq!(route.routing_decision, "dispatchable");
        assert_eq!(route.database_status, DATABASE_PLACEMENT_STATUS_DRAINING);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn inspect_shard_route_reports_dispatchable_primary_route(pool: PgPool) {
        let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
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

        let route = inspect_shard_route(&database_router, run_id, 4)
            .await
            .unwrap();

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
        let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
        let run_id = Uuid::now_v7();

        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (
                run_id,
                run_shard,
                database_alias,
                status,
                move_target_database_alias
            )
            VALUES ($1::uuid, 4, 'primary', 'moving', 'shard_001')
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let move_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO shard_move_operations (
                run_id, run_shard, source_database_alias,
                target_database_alias, starting_route_version,
                phase, copied_row_count, copied_byte_count
            )
            VALUES ($1, 4, 'primary', 'shard_001', 1, 'cutover', 3, 512)
            RETURNING id
            "#,
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO shard_move_table_progress (
                move_id, table_name, completed_page_count,
                last_end_key, copied_row_count, copied_byte_count,
                last_page_checksum
            )
            VALUES ($1, 'run_chunks', 1, 'end', 3, 512, 'checksum')
            "#,
        )
        .bind(move_id)
        .execute(&pool)
        .await
        .unwrap();

        let route = inspect_shard_route(&database_router, run_id, 4)
            .await
            .unwrap();

        assert!(!route.dispatchable);
        assert!(route.readable);
        assert_eq!(route.routing_decision, "read_only");
        assert_eq!(route.move_operation_id, Some(move_id));
        assert_eq!(route.move_phase.as_deref(), Some("cutover"));
        assert_eq!(route.move_completed_page_count, Some(1));
        assert_eq!(route.move_copied_row_count, Some(3));
        assert_eq!(route.move_copied_byte_count, Some(512));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn inspect_shard_route_reports_disabled_placement_as_blocked(pool: PgPool) {
        let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
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

        let route = inspect_shard_route(&database_router, run_id, 4)
            .await
            .unwrap();

        assert_eq!(route.database_alias, "shard_disabled");
        assert_eq!(route.database_status, "disabled");
        assert!(!route.database_url_env_resolved);
        assert!(!route.dispatchable);
        assert!(!route.readable);
        assert_eq!(route.routing_decision, "blocked");
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn creating_run_rejects_route_changes(pool: PgPool) {
        let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
        let run_id = Uuid::now_v7();

        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        seed_run(&pool, run_id, Uuid::now_v7(), Uuid::now_v7()).await;
        sqlx::query("UPDATE runs SET status = 'creating'::run_status WHERE id = $1::uuid")
            .bind(run_id)
            .execute(&pool)
            .await
            .unwrap();
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

        let set_error = set_shard_placement(&database_router, run_id, 3, "shard_001")
            .await
            .unwrap_err();
        let move_error = move_shard_placement(
            &database_router,
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
        .unwrap_err();

        assert!(set_error.to_string().contains("still creating"));
        assert!(move_error.to_string().contains("still creating"));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn move_shard_placement_switches_alias_after_verification(pool: PgPool) {
        let database_url = isolated_database_url(&pool).await;
        let database_router = database_router_with_control_pool(pool.clone(), database_url);
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
            &database_router,
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
        assert!(outcome.placement.move_target_database_alias.is_none());
        assert_eq!(outcome.placement.route_version, 5);

        let retried = move_shard_placement(
            &database_router,
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

        assert!(!retried.moved);
        assert!(retried.tables.iter().all(|table| table.verified));
        assert_eq!(retried.placement.database_alias, "shard_001");
        assert_eq!(retried.placement.route_version, 5);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn force_cannot_move_a_shard_with_active_work(pool: PgPool) {
        let database_url = isolated_database_url(&pool).await;
        let database_router = database_router_with_control_pool(pool.clone(), database_url);
        let run_id = Uuid::now_v7();
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();

        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES
                ('shard_001', 'DATABASE_URL', 'shard', 'active'),
                ('shard_002', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        seed_run(&pool, run_id, dataset_id, dataset_version_id).await;
        seed_run_snapshot(&pool, run_id, 3, dataset_id, dataset_version_id).await;
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
        sqlx::query(
            r#"
            INSERT INTO run_chunks (
                id, run_id, run_shard, dataset_version_id, profile_group_id,
                ordinal_start, ordinal_end, status, lease_token, leased_until
            )
            VALUES (
                $1::uuid, $2::uuid, 3, $3::uuid, 'default',
                0, 1, 'leased', gen_random_uuid(), now() + interval '1 minute'
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(run_id)
        .bind(dataset_version_id)
        .execute(&pool)
        .await
        .unwrap();

        let error = move_shard_placement(
            &database_router,
            run_id,
            3,
            "shard_001",
            ShardMoveOptions {
                dry_run: false,
                verify_only: false,
                force: true,
            },
        )
        .await
        .unwrap_err();
        let placement = shard_placements::select_shard_placement(&pool, run_id, 3)
            .await
            .unwrap()
            .unwrap();

        assert!(error.to_string().contains("--force cannot bypass"));
        assert_eq!(placement.database_alias, "primary");
        assert_eq!(placement.status, SHARD_PLACEMENT_STATUS_DRAINING);
        assert_eq!(
            placement.move_target_database_alias.as_deref(),
            Some("shard_001")
        );
        assert_eq!(placement.route_version, 3);

        let redirect_error = move_shard_placement(
            &database_router,
            run_id,
            3,
            "shard_002",
            ShardMoveOptions {
                dry_run: false,
                verify_only: false,
                force: false,
            },
        )
        .await
        .unwrap_err();
        assert!(
            redirect_error
                .to_string()
                .contains("retry with the persisted target")
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn rebalance_defers_a_draining_shard_until_active_work_drains(pool: PgPool) {
        let database_url = isolated_database_url(&pool).await;
        let database_router = database_router_with_control_pool(pool.clone(), database_url);
        let (operation_id, items) = seed_rebalance_operation(&pool, 1).await;
        let (run_id, run_shard) = items[0];
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();

        seed_run(&pool, run_id, dataset_id, dataset_version_id).await;
        seed_run_snapshot(&pool, run_id, run_shard, dataset_id, dataset_version_id).await;
        sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1::uuid, $2, 'primary', 'active')
            "#,
        )
        .bind(run_id)
        .bind(run_shard)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO run_chunks (
                id, run_id, run_shard, dataset_version_id, profile_group_id,
                ordinal_start, ordinal_end, status, lease_token, leased_until
            )
            VALUES (
                $1::uuid, $2::uuid, $3, $4::uuid, 'default',
                0, 1, 'leased', gen_random_uuid(), now() + interval '1 minute'
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(run_id)
        .bind(run_shard)
        .bind(dataset_version_id)
        .execute(&pool)
        .await
        .unwrap();

        let outcome = apply_shard_rebalance(
            &database_router,
            operation_id,
            ShardRebalanceApplyOptions {
                max_items: 1,
                lease_seconds: 60,
                force: false,
            },
        )
        .await
        .unwrap();
        let placement = shard_placements::select_shard_placement(&pool, run_id, run_shard)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(outcome.operation.status, REBALANCE_OPERATION_STATUS_RUNNING);
        assert_eq!(outcome.processed_items[0].status, "pending");
        assert_eq!(placement.status, SHARD_PLACEMENT_STATUS_DRAINING);
        assert_eq!(
            placement.move_target_database_alias.as_deref(),
            Some("shard_001")
        );
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

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn prerequisite_fingerprints_ignore_local_timestamps_and_run_lifecycle(pool: PgPool) {
        let run_id = Uuid::now_v7();
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        let case_id = Uuid::now_v7();
        let case_hash = format!("case-{}", Uuid::now_v7());

        seed_run(&pool, run_id, dataset_id, dataset_version_id).await;
        seed_case(&pool, dataset_version_id, case_id, &case_hash).await;

        let before = prerequisite_fingerprints(&pool, run_id).await;

        sqlx::query(
            r#"
            UPDATE case_blobs
            SET created_at = created_at + interval '1 second'
            WHERE case_hash = $1
            "#,
        )
        .bind(&case_hash)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            UPDATE dataset_versions
            SET created_at = created_at + interval '1 second',
                updated_at = updated_at + interval '1 second'
            WHERE dataset_version_id = $1::uuid
            "#,
        )
        .bind(dataset_version_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            UPDATE dataset_version_cases
            SET created_at = created_at + interval '1 second',
                updated_at = updated_at + interval '1 second'
            WHERE dataset_version_id = $1::uuid
              AND case_id = $2::uuid
            "#,
        )
        .bind(dataset_version_id)
        .bind(case_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            UPDATE runs
            SET status = 'completed'::run_status,
                gate_status = 'pass'::gate_status,
                terminal_execution_count = 1,
                passed_execution_count = 1,
                summary = '{"local":"changed"}'::jsonb,
                completed_at = now(),
                updated_at = now()
            WHERE id = $1::uuid
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();

        let after = prerequisite_fingerprints(&pool, run_id).await;

        assert_eq!(before, after);
    }

    #[test]
    fn rebalance_claim_eligibility_distinguishes_fresh_and_expired_running_items() {
        let now = Utc::now();

        assert!(rebalance_item_is_claim_eligible("pending", None, now));
        assert!(!rebalance_item_is_claim_eligible(
            "running",
            Some(now + chrono::Duration::seconds(1)),
            now,
        ));
        assert!(rebalance_item_is_claim_eligible(
            "running",
            Some(now - chrono::Duration::seconds(1)),
            now,
        ));
        assert!(!rebalance_item_is_claim_eligible("completed", None, now));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn rebalance_claims_pending_and_expired_items_but_skips_fresh_claims(pool: PgPool) {
        let (operation_id, items) = seed_rebalance_operation(&pool, 3).await;
        let fresh_token = Uuid::now_v7();
        let fresh_owner = Uuid::now_v7();
        let expired_token = Uuid::now_v7();
        let expired_owner = Uuid::now_v7();

        sqlx::query(
            r#"
            UPDATE shard_rebalance_items
            SET status = 'running',
                claim_token = $2::uuid,
                claimed_by = $3::uuid,
                claimed_until = now() + interval '1 hour'
            WHERE operation_id = $1::uuid
              AND sequence_no = 1
            "#,
        )
        .bind(operation_id)
        .bind(fresh_token)
        .bind(fresh_owner)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            UPDATE shard_rebalance_items
            SET status = 'running',
                claim_token = $2::uuid,
                claimed_by = $3::uuid,
                claimed_until = now() - interval '1 hour'
            WHERE operation_id = $1::uuid
              AND sequence_no = 2
            "#,
        )
        .bind(operation_id)
        .bind(expired_token)
        .bind(expired_owner)
        .execute(&pool)
        .await
        .unwrap();

        let claimant = Uuid::now_v7();
        let pending = claim_next_rebalance_apply_item(&pool, operation_id, claimant, 60)
            .await
            .unwrap()
            .unwrap();
        let expired = claim_next_rebalance_apply_item(&pool, operation_id, claimant, 60)
            .await
            .unwrap()
            .unwrap();
        let none = claim_next_rebalance_apply_item(&pool, operation_id, claimant, 60)
            .await
            .unwrap();

        assert_eq!(pending.item.run_id, items[0].0);
        assert!(!pending.reclaimed);
        assert_eq!(expired.item.run_id, items[2].0);
        assert!(expired.reclaimed);
        assert!(none.is_none());

        let fresh = sqlx::query_as::<_, ShardRebalanceItem>(
            r#"
            SELECT *
            FROM shard_rebalance_items
            WHERE operation_id = $1::uuid
              AND sequence_no = 1
            "#,
        )
        .bind(operation_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fresh.claim_token, Some(fresh_token));
        assert_eq!(fresh.claimed_by, Some(fresh_owner));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn expired_rebalance_claim_can_be_reclaimed_and_fences_stale_settlement(pool: PgPool) {
        let (operation_id, _) = seed_rebalance_operation(&pool, 1).await;
        let first = claim_next_rebalance_apply_item(&pool, operation_id, Uuid::now_v7(), 60)
            .await
            .unwrap()
            .unwrap();
        let first_token = first.item.claim_token.unwrap();

        sqlx::query(
            r#"
            UPDATE shard_rebalance_items
            SET claimed_until = now() - interval '1 second'
            WHERE operation_id = $1::uuid
            "#,
        )
        .bind(operation_id)
        .execute(&pool)
        .await
        .unwrap();

        let second = claim_next_rebalance_apply_item(&pool, operation_id, Uuid::now_v7(), 60)
            .await
            .unwrap()
            .unwrap();
        let second_token = second.item.claim_token.unwrap();
        assert!(second.reclaimed);
        assert_ne!(first_token, second_token);

        let stale = mark_rebalance_item_completed(&pool, &first.item, first_token)
            .await
            .unwrap();
        assert!(stale.is_none());

        let completed = mark_rebalance_item_completed(&pool, &second.item, second_token)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completed.status, REBALANCE_ITEM_STATUS_COMPLETED);
        assert!(completed.claim_token.is_none());
        assert!(completed.claimed_by.is_none());
        assert!(completed.claimed_until.is_none());
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn rebalance_cancellation_preserves_fresh_claim_and_fences_claimed_cancel(pool: PgPool) {
        let database_url = isolated_database_url(&pool).await;
        let database_router = database_router_with_control_pool(pool.clone(), database_url);
        let (operation_id, _) = seed_rebalance_operation(&pool, 3).await;
        let fresh = claim_next_rebalance_apply_item(&pool, operation_id, Uuid::now_v7(), 60)
            .await
            .unwrap()
            .unwrap();
        let expired = claim_next_rebalance_apply_item(&pool, operation_id, Uuid::now_v7(), 60)
            .await
            .unwrap()
            .unwrap();
        sqlx::query(
            r#"
            UPDATE shard_rebalance_items
            SET claimed_until = now() - interval '1 second'
            WHERE operation_id = $1::uuid
              AND run_id = $2::uuid
              AND run_shard = $3
            "#,
        )
        .bind(operation_id)
        .bind(expired.item.run_id)
        .bind(expired.item.run_shard)
        .execute(&pool)
        .await
        .unwrap();

        let cancelled = cancel_shard_rebalance(&database_router, operation_id)
            .await
            .unwrap();
        assert_eq!(cancelled.status, REBALANCE_OPERATION_STATUS_CANCELLED);
        assert_eq!(cancelled.cancelled_item_count, 2);

        let fresh_state = sqlx::query_as::<_, ShardRebalanceItem>(
            r#"
            SELECT *
            FROM shard_rebalance_items
            WHERE operation_id = $1::uuid
              AND run_id = $2::uuid
              AND run_shard = $3
            "#,
        )
        .bind(operation_id)
        .bind(fresh.item.run_id)
        .bind(fresh.item.run_shard)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fresh_state.status, "running");
        assert_eq!(fresh_state.claim_token, fresh.item.claim_token);

        let claim_token = fresh.item.claim_token.unwrap();
        let settled = mark_rebalance_item_cancelled(&pool, &fresh.item, claim_token)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(settled.status, REBALANCE_ITEM_STATUS_CANCELLED);
        let operation = refresh_rebalance_operation_status(&pool, operation_id)
            .await
            .unwrap();
        assert_eq!(operation.status, REBALANCE_OPERATION_STATUS_CANCELLED);
        assert_eq!(operation.cancelled_item_count, 3);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
    async fn concurrent_rebalance_apply_workers_move_each_item_once(pool: PgPool) {
        let database_url = isolated_database_url(&pool).await;
        let database_router = database_router_with_control_pool(pool.clone(), database_url);
        let (operation_id, items) = seed_rebalance_operation(&pool, 6).await;
        for (run_id, run_shard) in &items {
            sqlx::query(
                r#"
                INSERT INTO shard_placements (
                    run_id,
                    run_shard,
                    database_alias,
                    status
                )
                VALUES ($1::uuid, $2, 'primary', 'active')
                "#,
            )
            .bind(run_id)
            .bind(run_shard)
            .execute(&pool)
            .await
            .unwrap();
        }

        let options = ShardRebalanceApplyOptions {
            max_items: items.len(),
            lease_seconds: 60,
            force: false,
        };
        let (first, second) = tokio::join!(
            apply_shard_rebalance(&database_router, operation_id, options),
            apply_shard_rebalance(&database_router, operation_id, options),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        let processed = first
            .processed_items
            .iter()
            .chain(&second.processed_items)
            .map(|item| (item.run_id, item.run_shard))
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            first.processed_items.len() + second.processed_items.len(),
            items.len()
        );
        assert_eq!(processed.len(), items.len());
        let operation = select_rebalance_operation(&pool, operation_id)
            .await
            .unwrap()
            .unwrap();
        let item_states = sqlx::query_as::<_, (i32, String, Option<String>)>(
            r#"
            SELECT sequence_no, status, error_message
            FROM shard_rebalance_items
            WHERE operation_id = $1::uuid
            ORDER BY sequence_no
            "#,
        )
        .bind(operation_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            operation.status, REBALANCE_OPERATION_STATUS_COMPLETED,
            "unexpected item states: {item_states:?}"
        );
        assert_eq!(operation.completed_item_count, items.len() as i32);

        let route_versions = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT route_version
            FROM shard_placements
            WHERE run_id = ANY($1::uuid[])
            ORDER BY run_id
            "#,
        )
        .bind(items.iter().map(|(run_id, _)| *run_id).collect::<Vec<_>>())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(route_versions.len(), items.len());
        assert!(route_versions.into_iter().all(|version| version == 5));
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

    async fn wait_for_waiting_advisory_lock(pool: &PgPool, operation: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let waiting = sqlx::query_scalar::<_, bool>(
                    r#"
                    SELECT EXISTS (
                        SELECT 1
                        FROM pg_locks
                        WHERE locktype = 'advisory'
                          AND database = (
                              SELECT oid
                              FROM pg_database
                              WHERE datname = current_database()
                          )
                          AND NOT granted
                    )
                    "#,
                )
                .fetch_one(pool)
                .await
                .unwrap();
                if waiting {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{operation} did not wait behind source admission"));
    }

    async fn database_router_with_isolated_control_pool(pool: PgPool) -> DatabaseRouter {
        let database_url = isolated_database_url(&pool).await;
        database_router_with_control_pool(pool, database_url)
    }

    fn database_router_with_control_pool(pool: PgPool, uri: String) -> DatabaseRouter {
        let database_router = DatabaseRouter {
            uri,
            max_connections: 5,
            placement_config: PlacementConfig::default_single_database(),
            control_pool: OnceCell::new(),
            placement_pools: OnceCell::new(),
            shard_placement_cache: new_shard_placement_cache(),
        };
        assert!(database_router.control_pool.set(pool).is_ok());
        database_router
    }

    async fn isolated_database_url(pool: &PgPool) -> String {
        let database_name = sqlx::query_scalar::<_, String>("SELECT current_database()")
            .fetch_one(pool)
            .await
            .unwrap();
        let mut database_url =
            url::Url::parse(&std::env::var(DEFAULT_DATABASE_URL_ENV).unwrap()).unwrap();
        database_url.set_path(&database_name);
        database_url.to_string()
    }

    async fn seed_rebalance_operation(
        pool: &PgPool,
        item_count: usize,
    ) -> (Uuid, Vec<(Uuid, i16)>) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (
                alias,
                database_url_env,
                role,
                status
            )
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(pool)
        .await
        .unwrap();

        let operation_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO shard_rebalance_operations (
                id,
                strategy,
                source_database_alias,
                target_database_alias,
                planned_item_count
            )
            VALUES ($1::uuid, 'drain-source', 'primary', 'shard_001', $2)
            "#,
        )
        .bind(operation_id)
        .bind(item_count as i32)
        .execute(pool)
        .await
        .unwrap();

        let mut items = Vec::with_capacity(item_count);
        for sequence_no in 0..item_count {
            let run_id = Uuid::now_v7();
            let run_shard = sequence_no as i16;
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
                VALUES ($1::uuid, $2, $3::uuid, $4, 'primary', 'shard_001', 1)
                "#,
            )
            .bind(operation_id)
            .bind(sequence_no as i32)
            .bind(run_id)
            .bind(run_shard)
            .execute(pool)
            .await
            .unwrap();
            items.push((run_id, run_shard));
        }

        (operation_id, items)
    }

    async fn seed_case(pool: &PgPool, dataset_version_id: Uuid, case_id: Uuid, case_hash: &str) {
        sqlx::query(
            r#"
            INSERT INTO case_blobs (
                case_hash,
                task_type,
                input_payload,
                expected_output
            )
            VALUES ($1, 'classification', '{"text":"hello"}'::jsonb, 'null'::jsonb)
            "#,
        )
        .bind(case_hash)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO dataset_version_cases (
                dataset_version_id,
                case_id,
                case_ordinal,
                case_hash
            )
            VALUES ($1::uuid, $2::uuid, 0, $3)
            "#,
        )
        .bind(dataset_version_id)
        .bind(case_id)
        .bind(case_hash)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn prerequisite_fingerprints(pool: &PgPool, run_id: Uuid) -> Vec<(i64, String)> {
        let mut fingerprints = Vec::new();

        for table in PREREQUISITE_TABLES {
            let fingerprint = prerequisite_table_fingerprint(pool, table, run_id, 0)
                .await
                .unwrap();
            fingerprints.push((fingerprint.row_count, fingerprint.checksum));
        }

        fingerprints
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
