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
            SELECT
                id,
                run_key,
                expected_execution_count,
                terminal_execution_count,
                passed_execution_count,
                failed_execution_count,
                errored_execution_count
            FROM runs
            WHERE id = $1::uuid
              AND status = 'finalizing'::run_status
            FOR UPDATE
        ),
        terminal_chunk_failure AS (
            SELECT
                EXISTS (
                    SELECT 1
                    FROM run_chunks
                    WHERE run_id = $1::uuid
                      AND status IN ('failed', 'cancelled')
                    LIMIT 1
                ) AS exists
        ),
        open_chunk_exists AS (
            SELECT
                EXISTS (
                    SELECT 1
                    FROM run_chunks
                    WHERE run_id = $1::uuid
                      AND status IN ('pending', 'leased')
                    LIMIT 1
                ) AS exists
        ),
        finalized AS (
            UPDATE runs r
            SET status = 'completed'::run_status,
                gate_status = CASE
                    WHEN tcf.exists
                      OR rr.failed_execution_count > 0
                      OR rr.errored_execution_count > 0
                      OR rr.terminal_execution_count < rr.expected_execution_count
                    THEN 'fail'::gate_status
                    ELSE 'pass'::gate_status
                END,
                summary = jsonb_build_object(
                    'expected_execution_count', rr.expected_execution_count,
                    'terminal_execution_count', rr.terminal_execution_count,
                    'passed_execution_count', rr.passed_execution_count,
                    'failed_execution_count', rr.failed_execution_count,
                    'errored_execution_count', rr.errored_execution_count,
                    'coverage_complete', rr.terminal_execution_count >= rr.expected_execution_count,
                    'has_terminal_chunk_failure', tcf.exists
                ),
                finalized_at = COALESCE(r.finalized_at, now()),
                completed_at = COALESCE(r.completed_at, now()),
                coordinator_leased_until = NULL,
                coordinator_heartbeat_at = now(),
                updated_at = now()
            FROM run_row rr, terminal_chunk_failure tcf, open_chunk_exists oce
            WHERE r.id = rr.id
              AND NOT oce.exists
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
