//! Run cancellation workflow helpers.
//!
//! Cancellation is a terminal, user-requested state transition.
//!
//! The control database owns the run lifecycle and cancellation event. Execution
//! placements own chunks, attempts, and executions, so routed cancellation first
//! marks the control run cancelled and drains dispatch cursors, then fans out
//! idempotent execution-local cancellation to each routed placement.

use std::collections::BTreeMap;

use futures_util::{
    StreamExt,
    stream,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::run_shard_summary::{
    RunShardSummary,
    refresh_run_shard_summary,
};
use crate::{
    context::database,
    models::run::Run,
};

const CANCEL_REASON: &str = "run cancelled by user";
const ROUTED_CANCEL_PARALLELISM: usize = 16;

/// Result of a cancel request for a run.
#[derive(Debug, Clone)]
pub(crate) struct CancelRunOutcome {
    pub(crate) run: Run,
    pub(crate) cancelled: bool,
    pub(crate) already_cancelled: bool,
    pub(crate) chunks_cancelled: i64,
    pub(crate) executions_cancelled: i64,
    pub(crate) attempts_cancelled: i64,
    pub(crate) outbox_events_enqueued: i64,
}

#[derive(Debug, Clone, Default)]
struct CancellationCounts {
    chunks_cancelled: i64,
    executions_cancelled: i64,
    attempts_cancelled: i64,
}

impl CancellationCounts {
    fn add(&mut self, other: &Self) {
        self.chunks_cancelled += other.chunks_cancelled;
        self.executions_cancelled += other.executions_cancelled;
        self.attempts_cancelled += other.attempts_cancelled;
    }
}

#[derive(Debug, Clone, Default, sqlx::FromRow)]
struct ExecutionProgressCounts {
    expected_execution_count: i64,
    execution_count: i64,
    terminal_execution_count: i64,
    passed_execution_count: i64,
    failed_execution_count: i64,
    errored_execution_count: i64,
    skipped_execution_count: i64,
    missing_aggregate_count: i64,
    chunk_count: i64,
    completed_chunk_count: i64,
    failed_chunk_count: i64,
    cancelled_chunk_count: i64,
    cancelled_execution_count: i64,
}

impl ExecutionProgressCounts {
    fn add(&mut self, other: &Self) {
        self.expected_execution_count += other.expected_execution_count;
        self.execution_count += other.execution_count;
        self.terminal_execution_count += other.terminal_execution_count;
        self.passed_execution_count += other.passed_execution_count;
        self.failed_execution_count += other.failed_execution_count;
        self.errored_execution_count += other.errored_execution_count;
        self.skipped_execution_count += other.skipped_execution_count;
        self.missing_aggregate_count += other.missing_aggregate_count;
        self.chunk_count += other.chunk_count;
        self.completed_chunk_count += other.completed_chunk_count;
        self.failed_chunk_count += other.failed_chunk_count;
        self.cancelled_chunk_count += other.cancelled_chunk_count;
        self.cancelled_execution_count += other.cancelled_execution_count;
    }

    fn from_control_run(run: &Run) -> Self {
        Self {
            expected_execution_count: i64::from(run.expected_execution_count),
            terminal_execution_count: i64::from(run.terminal_execution_count),
            passed_execution_count: i64::from(run.passed_execution_count),
            failed_execution_count: i64::from(run.failed_execution_count),
            errored_execution_count: i64::from(run.errored_execution_count),
            ..Self::default()
        }
    }
}

#[derive(Debug)]
struct ControlCancellationState {
    run: Run,
    cancelled: bool,
    already_cancelled: bool,
}

#[derive(Debug)]
struct ExecutionCancellationTarget {
    db: PgPool,
    run_shards: Vec<i16>,
}

#[derive(Debug, Default)]
struct ExecutionCancellationOutcome {
    counts: CancellationCounts,
    progress: ExecutionProgressCounts,
    summaries: Vec<RunShardSummary>,
}

impl ExecutionCancellationOutcome {
    fn add(&mut self, other: Self) {
        self.counts.add(&other.counts);
        self.progress.add(&other.progress);
        self.summaries.extend(other.summaries);
    }
}

fn is_cancelable_status(status: &str) -> bool {
    matches!(status, "pending" | "running" | "finalizing")
}

fn is_already_cancelled_status(status: &str) -> bool {
    status == "cancelled"
}

fn is_uncancelable_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed")
}

async fn select_run_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
) -> anyhow::Result<Option<Run>> {
    // Query behavior: read the run projection inside the caller's transaction.
    // Used for idempotent already-cancelled responses after the status row has
    // been locked by `cancel_run`.
    let run = sqlx::query_as::<_, Run>(
        r#"
        SELECT
            id, run_key, name, description,
            dataset_id, dataset_version,
            evaluation_profile_id, evaluation_profile_version,
            aggregation_policy_id, aggregation_policy_version,
            agent_provider, agent_name, agent_version,
            prompt_config_id, prompt_config_version,
            config_snapshot,
            status::text as status,
            gate_status::text as gate_status,
            coordinator_id,
            coordinator_leased_until,
            coordinator_heartbeat_at,
            expected_execution_count,
            terminal_execution_count,
            passed_execution_count,
            failed_execution_count,
            errored_execution_count,
            summary,
            error_message,
            created_at,
            started_at,
            dispatched_at,
            finalized_at,
            completed_at,
            updated_at
        FROM runs
        WHERE id = $1::uuid
        "#,
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await?;

    Ok(run)
}

async fn cancel_open_attempts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
) -> anyhow::Result<i64> {
    // Query behavior: close only pending/running attempts for the run, clear
    // lease ownership, preserve any existing error text, and count affected
    // rows for the cancellation summary.
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        WITH updated AS (
            UPDATE execution_attempts
            SET status = 'cancelled'::attempt_status,
                leased_until = NULL,
                error_message = COALESCE(error_message, $2),
                completed_at = COALESCE(completed_at, now()),
                updated_at = now()
            WHERE run_id = $1::uuid
              AND status IN ('pending'::attempt_status, 'running'::attempt_status)
            RETURNING 1
        )
        SELECT COUNT(*)::bigint
        FROM updated
        "#,
    )
    .bind(run_id)
    .bind(CANCEL_REASON)
    .fetch_one(&mut **tx)
    .await?;

    Ok(count)
}

async fn cancel_open_executions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
) -> anyhow::Result<i64> {
    // Query behavior: move every non-terminal execution state to cancelled,
    // including retry_scheduled rows, so no worker can later allocate or
    // finalize additional attempts for this run.
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        WITH updated AS (
            UPDATE executions
            SET status = 'cancelled'::execution_status,
                last_error_message = COALESCE(last_error_message, $2),
                completed_at = COALESCE(completed_at, now()),
                updated_at = now()
            WHERE run_id = $1::uuid
              AND status IN (
                  'pending'::execution_status,
                  'running'::execution_status,
                  'awaiting_evaluators'::execution_status,
                  'retry_scheduled'::execution_status
              )
            RETURNING 1
        )
        SELECT COUNT(*)::bigint
        FROM updated
        "#,
    )
    .bind(run_id)
    .bind(CANCEL_REASON)
    .fetch_one(&mut **tx)
    .await?;

    Ok(count)
}

async fn cancel_open_chunks(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
) -> anyhow::Result<i64> {
    // Query behavior: move only open chunks to cancelled, clear their worker
    // lease, and drain shard dispatch cursors so no later coordinator cycle
    // tries to dispatch stale open cursor rows. Completed/failed chunks remain
    // as historical terminal outcomes.
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        WITH updated AS (
            UPDATE run_chunks
            SET status = 'cancelled',
                leased_until = NULL,
                updated_at = now()
            WHERE run_id = $1::uuid
              AND status IN ('pending', 'leased')
            RETURNING 1
        ),
        drained_cursors AS (
            UPDATE run_shard_dispatch_cursors
            SET status = 'drained',
                updated_at = now()
            WHERE run_id = $1::uuid
            RETURNING 1
        )
        SELECT COUNT(*)::bigint
        FROM updated
        "#,
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await?;

    Ok(count)
}

async fn drain_control_dispatch_cursors(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        UPDATE run_shard_dispatch_cursors
        SET status = 'drained',
            updated_at = now()
        WHERE run_id = $1::uuid
        "#,
    )
    .bind(run_id)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

async fn mark_control_cancellation_requested(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<Option<ControlCancellationState>> {
    let mut tx = db.begin().await?;

    let Some(status) = sqlx::query_scalar::<_, String>(
        r#"
        SELECT status::text
        FROM runs
        WHERE id = $1::uuid
        FOR UPDATE
        "#,
    )
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.commit().await?;
        return Ok(None);
    };

    if is_uncancelable_terminal_status(&status) {
        anyhow::bail!(
            "run '{}' is already terminal with status '{}' and cannot be cancelled",
            run_id,
            status
        );
    }

    if is_already_cancelled_status(&status) {
        drain_control_dispatch_cursors(&mut tx, run_id).await?;
        let run = select_run_in_tx(&mut tx, run_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("run '{}' disappeared during cancellation", run_id))?;
        tx.commit().await?;
        return Ok(Some(ControlCancellationState {
            run,
            cancelled: false,
            already_cancelled: true,
        }));
    }

    if !is_cancelable_status(&status) {
        anyhow::bail!(
            "run '{}' has unsupported status '{}' and cannot be cancelled",
            run_id,
            status
        );
    }

    drain_control_dispatch_cursors(&mut tx, run_id).await?;
    let run = mark_run_cancelled_pending(&mut tx, run_id).await?;
    tx.commit().await?;

    Ok(Some(ControlCancellationState {
        run,
        cancelled: true,
        already_cancelled: false,
    }))
}

async fn mark_run_cancelled_pending(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
) -> anyhow::Result<Run> {
    let run = sqlx::query_as::<_, Run>(
        r#"
        UPDATE runs r
        SET status = 'cancelled'::run_status,
            gate_status = 'fail'::gate_status,
            summary = jsonb_build_object(
                'cancelled', true,
                'reason', $2,
                'cancellation_pending', true,
                'expected_execution_count', r.expected_execution_count
            ),
            error_message = COALESCE(r.error_message, $2),
            completed_at = COALESCE(r.completed_at, now()),
            coordinator_id = NULL,
            coordinator_leased_until = NULL,
            coordinator_heartbeat_at = now(),
            updated_at = now()
        WHERE r.id = $1::uuid
        RETURNING
            r.id, r.run_key, r.name, r.description,
            r.dataset_id, r.dataset_version,
            r.evaluation_profile_id, r.evaluation_profile_version,
            r.aggregation_policy_id, r.aggregation_policy_version,
            r.agent_provider, r.agent_name, r.agent_version,
            r.prompt_config_id, r.prompt_config_version,
            r.config_snapshot,
            r.status::text as status,
            r.gate_status::text as gate_status,
            r.coordinator_id,
            r.coordinator_leased_until,
            r.coordinator_heartbeat_at,
            r.expected_execution_count,
            r.terminal_execution_count,
            r.passed_execution_count,
            r.failed_execution_count,
            r.errored_execution_count,
            r.summary,
            r.error_message,
            r.created_at,
            r.started_at,
            r.dispatched_at,
            r.finalized_at,
            r.completed_at,
            r.updated_at
        "#,
    )
    .bind(run_id)
    .bind(CANCEL_REASON)
    .fetch_one(&mut **tx)
    .await?;

    Ok(run)
}

async fn update_run_cancelled(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
    progress: &ExecutionProgressCounts,
    counts: &CancellationCounts,
    execution_route_count: usize,
    shard_summary_count: usize,
) -> anyhow::Result<Run> {
    let run = sqlx::query_as::<_, Run>(
        r#"
        UPDATE runs r
        SET status = 'cancelled'::run_status,
            gate_status = 'fail'::gate_status,
            terminal_execution_count = $3::int,
            passed_execution_count = $4::int,
            failed_execution_count = $5::int,
            errored_execution_count = $6::int,
            summary = jsonb_build_object(
                'cancelled', true,
                'reason', $2,
                'cancellation_pending', false,
                'expected_execution_count', r.expected_execution_count,
                'execution_expected_count', $7::bigint,
                'execution_count', $8::bigint,
                'terminal_execution_count', $9::bigint,
                'passed_execution_count', $10::bigint,
                'failed_execution_count', $11::bigint,
                'errored_execution_count', $12::bigint,
                'skipped_execution_count', $13::bigint,
                'missing_aggregate_count', $14::bigint,
                'cancelled_execution_count', $15::bigint,
                'chunk_count', $16::bigint,
                'completed_chunk_count', $17::bigint,
                'failed_chunk_count', $18::bigint,
                'cancelled_chunk_count', $19::bigint,
                'chunks_cancelled_by_request', $20::bigint,
                'executions_cancelled_by_request', $21::bigint,
                'attempts_cancelled_by_request', $22::bigint,
                'execution_route_count', $23::int,
                'shard_summary_count', $24::int
            ),
            error_message = COALESCE(r.error_message, $2),
            completed_at = COALESCE(r.completed_at, now()),
            coordinator_id = NULL,
            coordinator_leased_until = NULL,
            coordinator_heartbeat_at = now(),
            updated_at = now()
        WHERE r.id = $1::uuid
        RETURNING
            r.id, r.run_key, r.name, r.description,
            r.dataset_id, r.dataset_version,
            r.evaluation_profile_id, r.evaluation_profile_version,
            r.aggregation_policy_id, r.aggregation_policy_version,
            r.agent_provider, r.agent_name, r.agent_version,
            r.prompt_config_id, r.prompt_config_version,
            r.config_snapshot,
            r.status::text as status,
            r.gate_status::text as gate_status,
            r.coordinator_id,
            r.coordinator_leased_until,
            r.coordinator_heartbeat_at,
            r.expected_execution_count,
            r.terminal_execution_count,
            r.passed_execution_count,
            r.failed_execution_count,
            r.errored_execution_count,
            r.summary,
            r.error_message,
            r.created_at,
            r.started_at,
            r.dispatched_at,
            r.finalized_at,
            r.completed_at,
            r.updated_at
        "#,
    )
    .bind(run_id)
    .bind(CANCEL_REASON)
    .bind(i32::try_from(progress.terminal_execution_count)?)
    .bind(i32::try_from(progress.passed_execution_count)?)
    .bind(i32::try_from(progress.failed_execution_count)?)
    .bind(i32::try_from(progress.errored_execution_count)?)
    .bind(progress.expected_execution_count)
    .bind(progress.execution_count)
    .bind(progress.terminal_execution_count)
    .bind(progress.passed_execution_count)
    .bind(progress.failed_execution_count)
    .bind(progress.errored_execution_count)
    .bind(progress.skipped_execution_count)
    .bind(progress.missing_aggregate_count)
    .bind(progress.cancelled_execution_count)
    .bind(progress.chunk_count)
    .bind(progress.completed_chunk_count)
    .bind(progress.failed_chunk_count)
    .bind(progress.cancelled_chunk_count)
    .bind(counts.chunks_cancelled)
    .bind(counts.executions_cancelled)
    .bind(counts.attempts_cancelled)
    .bind(i32::try_from(execution_route_count)?)
    .bind(i32::try_from(shard_summary_count)?)
    .fetch_one(&mut **tx)
    .await?;

    Ok(run)
}

async fn select_run_execution_progress(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<ExecutionProgressCounts> {
    let counts = sqlx::query_as::<_, ExecutionProgressCounts>(
        r#"
        WITH execution_counts AS (
            SELECT
                COUNT(e.id)::bigint AS execution_count,
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                )::bigint AS terminal_execution_count,
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                      AND ea.overall_status = 'passed'::evaluation_status
                )::bigint AS passed_execution_count,
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                      AND ea.overall_status = 'failed'::evaluation_status
                )::bigint AS failed_execution_count,
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                      AND ea.overall_status = 'error'::evaluation_status
                )::bigint AS errored_execution_count,
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                      AND ea.overall_status = 'skipped'::evaluation_status
                )::bigint AS skipped_execution_count,
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                      AND ea.execution_id IS NULL
                )::bigint AS missing_aggregate_count,
                COUNT(e.id) FILTER (WHERE e.status = 'cancelled'::execution_status)::bigint AS cancelled_execution_count
            FROM executions e
            LEFT JOIN execution_aggregates ea
              ON ea.run_id = e.run_id
             AND ea.run_shard = e.run_shard
             AND ea.execution_id = e.id
             AND ea.attempt_id = e.current_attempt_id
            WHERE e.run_id = $1::uuid
        ),
        chunk_counts AS (
            SELECT
                COALESCE(SUM(ordinal_end - ordinal_start), 0)::bigint AS expected_execution_count,
                COUNT(*)::bigint AS chunk_count,
                COUNT(*) FILTER (WHERE status = 'completed')::bigint AS completed_chunk_count,
                COUNT(*) FILTER (WHERE status = 'failed')::bigint AS failed_chunk_count,
                COUNT(*) FILTER (WHERE status = 'cancelled')::bigint AS cancelled_chunk_count
            FROM run_chunks
            WHERE run_id = $1::uuid
        )
        SELECT
            chunk_counts.expected_execution_count,
            execution_counts.execution_count,
            execution_counts.terminal_execution_count,
            execution_counts.passed_execution_count,
            execution_counts.failed_execution_count,
            execution_counts.errored_execution_count,
            execution_counts.skipped_execution_count,
            execution_counts.missing_aggregate_count,
            chunk_counts.chunk_count,
            chunk_counts.completed_chunk_count,
            chunk_counts.failed_chunk_count,
            chunk_counts.cancelled_chunk_count,
            execution_counts.cancelled_execution_count
        FROM execution_counts, chunk_counts
        "#,
    )
    .bind(run_id)
    .fetch_one(db)
    .await?;

    Ok(counts)
}

#[allow(dead_code)]
async fn select_run_shards(db: &PgPool, run_id: Uuid) -> anyhow::Result<Vec<i16>> {
    let shards = sqlx::query_scalar::<_, i16>(
        r#"
        SELECT DISTINCT run_shard
        FROM run_chunks
        WHERE run_id = $1::uuid
        ORDER BY run_shard
        "#,
    )
    .bind(run_id)
    .fetch_all(db)
    .await?;

    Ok(shards)
}

async fn cancel_execution_target(
    run_id: Uuid,
    target: ExecutionCancellationTarget,
) -> anyhow::Result<ExecutionCancellationOutcome> {
    let mut tx = target.db.begin().await?;
    let counts = CancellationCounts {
        attempts_cancelled: cancel_open_attempts(&mut tx, run_id).await?,
        executions_cancelled: cancel_open_executions(&mut tx, run_id).await?,
        chunks_cancelled: cancel_open_chunks(&mut tx, run_id).await?,
    };
    tx.commit().await?;

    let progress = select_run_execution_progress(&target.db, run_id).await?;
    let mut summaries = Vec::with_capacity(target.run_shards.len());
    for run_shard in target.run_shards {
        if let Some(summary) = refresh_run_shard_summary(&target.db, run_id, run_shard).await? {
            summaries.push(summary);
        }
    }

    Ok(ExecutionCancellationOutcome {
        counts,
        progress,
        summaries,
    })
}

fn group_execution_routes(routes: Vec<(i16, String, PgPool)>) -> Vec<ExecutionCancellationTarget> {
    let mut grouped = BTreeMap::<String, ExecutionCancellationTarget>::new();

    for (run_shard, alias, db) in routes {
        grouped
            .entry(alias)
            .and_modify(|target| target.run_shards.push(run_shard))
            .or_insert_with(|| ExecutionCancellationTarget {
                db,
                run_shards: vec![run_shard],
            });
    }

    grouped.into_values().collect()
}

async fn cancel_execution_targets(
    run_id: Uuid,
    targets: Vec<ExecutionCancellationTarget>,
) -> anyhow::Result<ExecutionCancellationOutcome> {
    let mut stream = stream::iter(
        targets
            .into_iter()
            .map(|target| async move { cancel_execution_target(run_id, target).await }),
    )
    .buffer_unordered(ROUTED_CANCEL_PARALLELISM);

    let mut combined = ExecutionCancellationOutcome::default();
    while let Some(outcome) = stream.next().await {
        combined.add(outcome?);
    }

    Ok(combined)
}

fn fallback_progress(run: &Run, mut progress: ExecutionProgressCounts) -> ExecutionProgressCounts {
    if progress.expected_execution_count == 0 {
        progress.expected_execution_count = i64::from(run.expected_execution_count);
    }

    if progress.execution_count == 0 && progress.terminal_execution_count == 0 {
        let control = ExecutionProgressCounts::from_control_run(run);
        progress.execution_count = control.execution_count;
        progress.terminal_execution_count = control.terminal_execution_count;
        progress.passed_execution_count = control.passed_execution_count;
        progress.failed_execution_count = control.failed_execution_count;
        progress.errored_execution_count = control.errored_execution_count;
    }

    progress
}

async fn finalize_control_cancellation(
    db: &PgPool,
    state: ControlCancellationState,
    execution_outcome: ExecutionCancellationOutcome,
    execution_route_count: usize,
) -> anyhow::Result<CancelRunOutcome> {
    let counts = execution_outcome.counts;
    let progress = fallback_progress(&state.run, execution_outcome.progress);
    let shard_summary_count = execution_outcome.summaries.len();

    let mut tx = db.begin().await?;
    let run = update_run_cancelled(
        &mut tx,
        state.run.id,
        &progress,
        &counts,
        execution_route_count,
        shard_summary_count,
    )
    .await?;
    let mut outcome = CancelRunOutcome {
        run,
        cancelled: state.cancelled,
        already_cancelled: state.already_cancelled,
        chunks_cancelled: counts.chunks_cancelled,
        executions_cancelled: counts.executions_cancelled,
        attempts_cancelled: counts.attempts_cancelled,
        outbox_events_enqueued: 0,
    };
    outcome.outbox_events_enqueued = enqueue_cancelled_event(&mut tx, &outcome).await?;

    tx.commit().await?;
    Ok(outcome)
}

async fn enqueue_cancelled_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    outcome: &CancelRunOutcome,
) -> anyhow::Result<i64> {
    // Query behavior: insert the durable run.cancelled event with a deterministic
    // dedupe key. Repeated cancellation calls return zero inserted events.
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        WITH inserted AS (
            INSERT INTO outbox_events (
                event_type,
                aggregate_type,
                aggregate_id,
                dedupe_key,
                payload
            )
            VALUES (
                'run.cancelled',
                'run',
                $1::uuid,
                format('run:%s:cancelled', $1::uuid),
                jsonb_build_object(
                    'run_id', $1::uuid,
                    'run_key', $2::text,
                    'status', $3::text,
                    'gate_status', $4::text,
                    'expected_execution_count', $5::int,
                    'terminal_execution_count', $6::int,
                    'chunks_cancelled', $7::bigint,
                    'executions_cancelled', $8::bigint,
                    'attempts_cancelled', $9::bigint
                )
            )
            ON CONFLICT (dedupe_key) DO NOTHING
            RETURNING 1
        )
        SELECT COUNT(*)::bigint
        FROM inserted
        "#,
    )
    .bind(outcome.run.id)
    .bind(&outcome.run.run_key)
    .bind(&outcome.run.status)
    .bind(&outcome.run.gate_status)
    .bind(outcome.run.expected_execution_count)
    .bind(outcome.run.terminal_execution_count)
    .bind(outcome.chunks_cancelled)
    .bind(outcome.executions_cancelled)
    .bind(outcome.attempts_cancelled)
    .fetch_one(&mut **tx)
    .await?;

    Ok(count)
}

/// Cancels a pending/running/finalizing run through routed execution storage.
///
/// Returns `Ok(None)` when the run does not exist. Repeated cancellation of an
/// already-cancelled run is idempotent; completed or failed runs are rejected.
///
/// Workflow behavior:
/// - Mark the control run cancelled and drain control dispatch cursors under a
///   short control transaction.
/// - Resolve execution routes and cancel each placement once, grouped by
///   database alias and bounded by `ROUTED_CANCEL_PARALLELISM`.
/// - Refresh shard summaries where snapshots exist and count execution-local
///   progress directly for shards that were never dispatched.
/// - Finalize the control run summary and emit one idempotent control outbox
///   event. If fanout fails, retrying the command resumes from the already
///   cancelled control state and repeats idempotent execution cleanup.
pub(crate) async fn cancel_run_routed(
    database: &database::Db,
    run_id: Uuid,
) -> anyhow::Result<Option<CancelRunOutcome>> {
    let control_db = database.control().await?;
    let Some(state) = mark_control_cancellation_requested(control_db, run_id).await? else {
        return Ok(None);
    };

    let routes = database.execution_read_routes_for_run(run_id).await?;
    let execution_route_count = routes.len();
    let targets = group_execution_routes(routes);
    let execution_outcome = cancel_execution_targets(run_id, targets).await?;

    finalize_control_cancellation(control_db, state, execution_outcome, execution_route_count)
        .await
        .map(Some)
}

/// Cancels a run whose control and execution rows live in one database.
///
/// This remains for single-pool workflow tests and local single-database use.
/// CLI callers should use `cancel_run_routed` so execution-owned rows are
/// cancelled in their stored placements.
#[allow(dead_code)]
pub(crate) async fn cancel_run(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<Option<CancelRunOutcome>> {
    let Some(state) = mark_control_cancellation_requested(db, run_id).await? else {
        return Ok(None);
    };

    let run_shards = select_run_shards(db, run_id).await?;
    let execution_outcome = cancel_execution_target(
        run_id,
        ExecutionCancellationTarget {
            db: db.clone(),
            run_shards,
        },
    )
    .await?;

    finalize_control_cancellation(db, state, execution_outcome, 1)
        .await
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::{
        ExecutionProgressCounts,
        is_already_cancelled_status,
        is_cancelable_status,
        is_uncancelable_terminal_status,
    };

    #[test]
    fn cancelable_statuses_are_open_run_states() {
        assert!(is_cancelable_status("pending"));
        assert!(is_cancelable_status("running"));
        assert!(is_cancelable_status("finalizing"));
        assert!(!is_cancelable_status("completed"));
        assert!(!is_cancelable_status("failed"));
        assert!(!is_cancelable_status("cancelled"));
    }

    #[test]
    fn cancelled_status_is_idempotent() {
        assert!(is_already_cancelled_status("cancelled"));
        assert!(!is_already_cancelled_status("running"));
    }

    #[test]
    fn completed_and_failed_runs_are_not_cancelable() {
        assert!(is_uncancelable_terminal_status("completed"));
        assert!(is_uncancelable_terminal_status("failed"));
        assert!(!is_uncancelable_terminal_status("cancelled"));
    }

    #[test]
    fn execution_progress_counts_adds_all_routed_totals() {
        let mut first = ExecutionProgressCounts {
            expected_execution_count: 10,
            execution_count: 7,
            terminal_execution_count: 6,
            passed_execution_count: 4,
            failed_execution_count: 1,
            errored_execution_count: 1,
            skipped_execution_count: 0,
            missing_aggregate_count: 1,
            chunk_count: 2,
            completed_chunk_count: 1,
            failed_chunk_count: 0,
            cancelled_chunk_count: 1,
            cancelled_execution_count: 2,
        };
        let second = ExecutionProgressCounts {
            expected_execution_count: 5,
            execution_count: 5,
            terminal_execution_count: 5,
            passed_execution_count: 3,
            failed_execution_count: 1,
            errored_execution_count: 1,
            skipped_execution_count: 0,
            missing_aggregate_count: 0,
            chunk_count: 1,
            completed_chunk_count: 0,
            failed_chunk_count: 0,
            cancelled_chunk_count: 1,
            cancelled_execution_count: 1,
        };

        first.add(&second);

        assert_eq!(first.expected_execution_count, 15);
        assert_eq!(first.terminal_execution_count, 11);
        assert_eq!(first.passed_execution_count, 7);
        assert_eq!(first.cancelled_chunk_count, 2);
        assert_eq!(first.cancelled_execution_count, 3);
    }
}
