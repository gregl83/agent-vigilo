use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct ClaimedRunForFinalization {
    pub(crate) id: Uuid,
    pub(crate) run_key: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct FinalizedRun {
    pub(crate) id: Uuid,
    pub(crate) run_key: String,
    pub(crate) gate_status: String,
    pub(crate) terminal_execution_count: i32,
    pub(crate) passed_execution_count: i32,
    pub(crate) failed_execution_count: i32,
    pub(crate) errored_execution_count: i32,
}

pub(crate) async fn claim_next_finalizable_run(
    db: &PgPool,
    coordinator_id: &str,
    lease_seconds: i32,
) -> anyhow::Result<Option<ClaimedRunForFinalization>> {
    let claimed = sqlx::query_as::<_, ClaimedRunForFinalization>(
        r#"
        WITH candidate AS (
            SELECT r.id
            FROM runs r
            WHERE r.status IN ('running'::run_status, 'finalizing'::run_status)
              AND (
                  r.status <> 'finalizing'::run_status
                  OR r.coordinator_leased_until IS NULL
                  OR r.coordinator_leased_until < now()
              )
              AND NOT EXISTS (
                  SELECT 1
                  FROM run_chunks rc
                  WHERE rc.run_id = r.id
                    AND rc.status IN ('pending', 'leased')
              )
            ORDER BY r.updated_at ASC
            FOR UPDATE SKIP LOCKED
            LIMIT 1
        )
        UPDATE runs r
        SET status = 'finalizing'::run_status,
            coordinator_id = $1,
            coordinator_leased_until = now() + ($2::int * interval '1 second'),
            coordinator_heartbeat_at = now(),
            updated_at = now()
        FROM candidate
        WHERE r.id = candidate.id
        RETURNING r.id, r.run_key
        "#,
    )
    .bind(coordinator_id)
    .bind(lease_seconds)
    .fetch_optional(db)
    .await?;

    Ok(claimed)
}

pub(crate) async fn finalize_claimed_run(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<Option<FinalizedRun>> {
    let finalized = sqlx::query_as::<_, FinalizedRun>(
        r#"
        WITH run_row AS (
            SELECT id, run_key, expected_execution_count
            FROM runs
            WHERE id = $1::uuid
              AND status = 'finalizing'::run_status
            FOR UPDATE
        ),
        execution_stats AS (
            SELECT
                COALESCE(COUNT(*)::int, 0) AS terminal_execution_count,
                COALESCE(SUM(CASE WHEN overall_status = 'passed'::evaluation_status THEN 1 ELSE 0 END)::int, 0) AS passed_execution_count,
                COALESCE(SUM(CASE WHEN overall_status = 'failed'::evaluation_status THEN 1 ELSE 0 END)::int, 0) AS failed_execution_count,
                COALESCE(SUM(CASE WHEN overall_status = 'error'::evaluation_status THEN 1 ELSE 0 END)::int, 0) AS errored_execution_count,
                AVG(aggregate_score) AS avg_aggregate_score
            FROM execution_aggregates
            WHERE run_id = $1::uuid
        ),
        chunk_stats AS (
            SELECT
                COALESCE(COUNT(*)::int, 0) AS total_chunk_count,
                COALESCE(SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END)::int, 0) AS completed_chunk_count,
                COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END)::int, 0) AS failed_chunk_count,
                COALESCE(SUM(CASE WHEN status = 'cancelled' THEN 1 ELSE 0 END)::int, 0) AS cancelled_chunk_count,
                COALESCE(SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END)::int, 0) AS pending_chunk_count,
                COALESCE(SUM(CASE WHEN status = 'leased' THEN 1 ELSE 0 END)::int, 0) AS leased_chunk_count
            FROM run_chunks
            WHERE run_id = $1::uuid
        ),
        finalized AS (
            UPDATE runs r
            SET status = 'completed'::run_status,
                gate_status = CASE
                    WHEN cs.failed_chunk_count > 0
                      OR cs.cancelled_chunk_count > 0
                      OR es.failed_execution_count > 0
                      OR es.errored_execution_count > 0
                      OR es.terminal_execution_count < rr.expected_execution_count
                    THEN 'fail'::gate_status
                    ELSE 'pass'::gate_status
                END,
                terminal_execution_count = es.terminal_execution_count,
                passed_execution_count = es.passed_execution_count,
                failed_execution_count = es.failed_execution_count,
                errored_execution_count = es.errored_execution_count,
                summary = jsonb_build_object(
                    'expected_execution_count', rr.expected_execution_count,
                    'terminal_execution_count', es.terminal_execution_count,
                    'passed_execution_count', es.passed_execution_count,
                    'failed_execution_count', es.failed_execution_count,
                    'errored_execution_count', es.errored_execution_count,
                    'coverage_complete', es.terminal_execution_count >= rr.expected_execution_count,
                    'total_chunk_count', cs.total_chunk_count,
                    'completed_chunk_count', cs.completed_chunk_count,
                    'failed_chunk_count', cs.failed_chunk_count,
                    'cancelled_chunk_count', cs.cancelled_chunk_count,
                    'avg_aggregate_score', es.avg_aggregate_score
                ),
                finalized_at = COALESCE(r.finalized_at, now()),
                completed_at = COALESCE(r.completed_at, now()),
                coordinator_leased_until = NULL,
                coordinator_heartbeat_at = now(),
                updated_at = now()
            FROM run_row rr, execution_stats es, chunk_stats cs
            WHERE r.id = rr.id
              AND cs.pending_chunk_count = 0
              AND cs.leased_chunk_count = 0
            RETURNING
                r.id,
                r.run_key,
                r.gate_status::text as gate_status,
                r.terminal_execution_count,
                r.passed_execution_count,
                r.failed_execution_count,
                r.errored_execution_count
        ),
        inserted_event AS (
            INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload)
            SELECT
                'run.completed',
                'run',
                f.id,
                format('run:%s:completed', f.id),
                jsonb_build_object(
                    'run_id', f.id,
                    'run_key', f.run_key,
                    'gate_status', f.gate_status,
                    'terminal_execution_count', f.terminal_execution_count,
                    'passed_execution_count', f.passed_execution_count,
                    'failed_execution_count', f.failed_execution_count,
                    'errored_execution_count', f.errored_execution_count
                )
            FROM finalized f
            ON CONFLICT (dedupe_key) DO NOTHING
            RETURNING id
        )
        SELECT
            f.id,
            f.run_key,
            f.gate_status,
            f.terminal_execution_count,
            f.passed_execution_count,
            f.failed_execution_count,
            f.errored_execution_count
        FROM finalized f
        "#,
    )
    .bind(run_id)
    .fetch_optional(db)
    .await?;

    Ok(finalized)
}
