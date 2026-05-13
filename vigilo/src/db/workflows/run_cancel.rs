//! Run cancellation workflow helpers.
//!
//! Cancellation is a terminal, user-requested state transition. It stops future
//! work claims, closes open work leases, marks in-flight execution state
//! cancelled, and emits one idempotent domain event.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::run::Run;

const CANCEL_REASON: &str = "run cancelled by user";

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

async fn update_run_cancelled(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
    chunks_cancelled: i64,
    executions_cancelled: i64,
    attempts_cancelled: i64,
) -> anyhow::Result<Run> {
    let run = sqlx::query_as::<_, Run>(
        r#"
        WITH execution_counts AS (
            SELECT
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                )::int AS terminal_execution_count,
                COUNT(e.id) FILTER (WHERE ea.overall_status = 'passed'::evaluation_status)::int AS passed_execution_count,
                COUNT(e.id) FILTER (WHERE ea.overall_status = 'failed'::evaluation_status)::int AS failed_execution_count,
                COUNT(e.id) FILTER (WHERE ea.overall_status = 'error'::evaluation_status)::int AS errored_execution_count,
                COUNT(e.id) FILTER (WHERE e.status = 'cancelled'::execution_status)::int AS cancelled_execution_count
            FROM executions e
            LEFT JOIN execution_aggregates ea
              ON ea.run_id = e.run_id
             AND ea.execution_id = e.id
            WHERE e.run_id = $1::uuid
        ),
        chunk_counts AS (
            SELECT
                COUNT(*)::int AS chunk_count,
                COUNT(*) FILTER (WHERE status = 'cancelled')::int AS cancelled_chunk_count,
                COUNT(*) FILTER (WHERE status = 'completed')::int AS completed_chunk_count,
                COUNT(*) FILTER (WHERE status = 'failed')::int AS failed_chunk_count
            FROM run_chunks
            WHERE run_id = $1::uuid
        )
        UPDATE runs r
        SET status = 'cancelled'::run_status,
            gate_status = 'fail'::gate_status,
            terminal_execution_count = ec.terminal_execution_count,
            passed_execution_count = ec.passed_execution_count,
            failed_execution_count = ec.failed_execution_count,
            errored_execution_count = ec.errored_execution_count,
            summary = jsonb_build_object(
                'cancelled', true,
                'reason', $2,
                'expected_execution_count', r.expected_execution_count,
                'terminal_execution_count', ec.terminal_execution_count,
                'passed_execution_count', ec.passed_execution_count,
                'failed_execution_count', ec.failed_execution_count,
                'errored_execution_count', ec.errored_execution_count,
                'cancelled_execution_count', ec.cancelled_execution_count,
                'chunk_count', cc.chunk_count,
                'completed_chunk_count', cc.completed_chunk_count,
                'failed_chunk_count', cc.failed_chunk_count,
                'cancelled_chunk_count', cc.cancelled_chunk_count,
                'chunks_cancelled_by_request', $3::bigint,
                'executions_cancelled_by_request', $4::bigint,
                'attempts_cancelled_by_request', $5::bigint
            ),
            error_message = COALESCE(r.error_message, $2),
            completed_at = COALESCE(r.completed_at, now()),
            coordinator_id = NULL,
            coordinator_leased_until = NULL,
            coordinator_heartbeat_at = now(),
            updated_at = now()
        FROM execution_counts ec, chunk_counts cc
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
    .bind(chunks_cancelled)
    .bind(executions_cancelled)
    .bind(attempts_cancelled)
    .fetch_one(&mut **tx)
    .await?;

    Ok(run)
}

async fn enqueue_cancelled_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    outcome: &CancelRunOutcome,
) -> anyhow::Result<i64> {
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

/// Cancels a pending/running/finalizing run in one transactional workflow.
///
/// Returns `Ok(None)` when the run does not exist. Repeated cancellation of an
/// already-cancelled run is idempotent; completed or failed runs are rejected.
pub(crate) async fn cancel_run(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<Option<CancelRunOutcome>> {
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
        let run = select_run_in_tx(&mut tx, run_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("run '{}' disappeared during cancellation", run_id))?;
        tx.commit().await?;
        return Ok(Some(CancelRunOutcome {
            run,
            cancelled: false,
            already_cancelled: true,
            chunks_cancelled: 0,
            executions_cancelled: 0,
            attempts_cancelled: 0,
            outbox_events_enqueued: 0,
        }));
    }

    if !is_cancelable_status(&status) {
        anyhow::bail!(
            "run '{}' has unsupported status '{}' and cannot be cancelled",
            run_id,
            status
        );
    }

    let attempts_cancelled = cancel_open_attempts(&mut tx, run_id).await?;
    let executions_cancelled = cancel_open_executions(&mut tx, run_id).await?;
    let chunks_cancelled = cancel_open_chunks(&mut tx, run_id).await?;
    let run = update_run_cancelled(
        &mut tx,
        run_id,
        chunks_cancelled,
        executions_cancelled,
        attempts_cancelled,
    )
    .await?;
    let mut outcome = CancelRunOutcome {
        run,
        cancelled: true,
        already_cancelled: false,
        chunks_cancelled,
        executions_cancelled,
        attempts_cancelled,
        outbox_events_enqueued: 0,
    };
    outcome.outbox_events_enqueued = enqueue_cancelled_event(&mut tx, &outcome).await?;

    tx.commit().await?;
    Ok(Some(outcome))
}

#[cfg(test)]
mod tests {
    use super::{
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
}
