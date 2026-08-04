//! PostgreSQL operations for shard-local run summaries.

use sqlx::{
    Executor,
    PgPool,
    Postgres,
};
use uuid::Uuid;

use super::RunShardSummary;

/// Reads the shard-local summary for one routed run shard.
pub(crate) async fn select_run_shard_summary(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<Option<RunShardSummary>> {
    let summary = sqlx::query_as::<_, RunShardSummary>(
        r#"
        SELECT
            run_id,
            run_shard,
            expected_execution_count,
            execution_count,
            terminal_execution_count,
            aggregate_count,
            passed_execution_count,
            failed_execution_count,
            errored_execution_count,
            skipped_execution_count,
            missing_aggregate_count,
            evaluator_result_count,
            blocking_failure_count,
            score_count,
            score_sum,
            min_score,
            max_score,
            failed_chunk_count,
            cancelled_chunk_count,
            status
        FROM run_shard_summaries
        WHERE run_id = $1::uuid
          AND run_shard = $2
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .fetch_optional(db)
    .await?;

    Ok(summary)
}

#[cfg(test)]
pub(crate) async fn refresh_run_shard_summary(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<Option<RunShardSummary>> {
    refresh_run_shard_summary_with(db, run_id, run_shard).await
}

/// Recomputes a summary on the caller's connection or transaction.
pub(crate) async fn refresh_run_shard_summary_with<'e, E>(
    executor: E,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<Option<RunShardSummary>>
where
    E: Executor<'e, Database = Postgres>,
{
    let summary = sqlx::query_as::<_, RunShardSummary>(
        r#"
        WITH snapshot AS (
            SELECT run_id, run_shard, expected_execution_count
            FROM run_snapshots
            WHERE run_id = $1::uuid
              AND run_shard = $2
        ),
        execution_counts AS (
            SELECT
                snapshot.run_id,
                snapshot.run_shard,
                COUNT(e.id)::int AS execution_count,
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                )::int AS terminal_execution_count,
                COUNT(ea.execution_id)::int AS aggregate_count,
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                      AND ea.overall_status = 'passed'::evaluation_status
                )::int AS passed_execution_count,
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                      AND ea.overall_status = 'failed'::evaluation_status
                )::int AS failed_execution_count,
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                      AND ea.overall_status = 'error'::evaluation_status
                )::int AS errored_execution_count,
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                      AND ea.overall_status = 'skipped'::evaluation_status
                )::int AS skipped_execution_count,
                COUNT(e.id) FILTER (
                    WHERE e.status IN (
                        'completed'::execution_status,
                        'failed'::execution_status,
                        'timed_out'::execution_status,
                        'cancelled'::execution_status
                    )
                      AND ea.execution_id IS NULL
                )::int AS missing_aggregate_count,
                COALESCE(SUM(ea.evaluator_result_count), 0)::bigint AS evaluator_result_count,
                COALESCE(SUM(
                    CASE
                        WHEN ea.blocking_failures IS NULL THEN 0
                        ELSE jsonb_array_length(ea.blocking_failures)
                    END
                ), 0)::bigint AS blocking_failure_count,
                COUNT(ea.aggregate_score)::bigint AS score_count,
                COALESCE(SUM(ea.aggregate_score), 0.0)::double precision AS score_sum,
                MIN(ea.aggregate_score) AS min_score,
                MAX(ea.aggregate_score) AS max_score
            FROM snapshot
            LEFT JOIN executions e
              ON e.run_id = snapshot.run_id
             AND e.run_shard = snapshot.run_shard
            LEFT JOIN execution_aggregates ea
              ON ea.run_id = e.run_id
             AND ea.run_shard = e.run_shard
             AND ea.execution_id = e.id
             AND ea.attempt_id = e.current_attempt_id
            GROUP BY snapshot.run_id, snapshot.run_shard
        ),
        chunk_counts AS (
            SELECT
                snapshot.run_id,
                snapshot.run_shard,
                COUNT(rc.id) FILTER (WHERE rc.status = 'failed')::int AS failed_chunk_count,
                COUNT(rc.id) FILTER (WHERE rc.status = 'cancelled')::int AS cancelled_chunk_count,
                COUNT(rc.id) FILTER (WHERE rc.status IN ('pending', 'leased'))::int AS open_chunk_count
            FROM snapshot
            LEFT JOIN run_chunks rc
              ON rc.run_id = snapshot.run_id
             AND rc.run_shard = snapshot.run_shard
            GROUP BY snapshot.run_id, snapshot.run_shard
        ),
        computed AS (
            SELECT
                snapshot.run_id,
                snapshot.run_shard,
                snapshot.expected_execution_count,
                execution_counts.execution_count,
                execution_counts.terminal_execution_count,
                execution_counts.aggregate_count,
                execution_counts.passed_execution_count,
                execution_counts.failed_execution_count,
                execution_counts.errored_execution_count,
                execution_counts.skipped_execution_count,
                execution_counts.missing_aggregate_count,
                execution_counts.evaluator_result_count,
                execution_counts.blocking_failure_count,
                execution_counts.score_count,
                execution_counts.score_sum,
                execution_counts.min_score,
                execution_counts.max_score,
                chunk_counts.failed_chunk_count,
                chunk_counts.cancelled_chunk_count,
                CASE
                    WHEN chunk_counts.failed_chunk_count > 0
                      OR chunk_counts.cancelled_chunk_count > 0
                      OR execution_counts.failed_execution_count > 0
                      OR execution_counts.errored_execution_count > 0
                      OR execution_counts.missing_aggregate_count > 0
                    THEN 'failed'
                    WHEN chunk_counts.open_chunk_count = 0
                      AND execution_counts.terminal_execution_count >= snapshot.expected_execution_count
                    THEN 'completed'
                    ELSE 'running'
                END AS status
            FROM snapshot
            JOIN execution_counts
              ON execution_counts.run_id = snapshot.run_id
             AND execution_counts.run_shard = snapshot.run_shard
            JOIN chunk_counts
              ON chunk_counts.run_id = snapshot.run_id
             AND chunk_counts.run_shard = snapshot.run_shard
        ),
        upserted AS (
            INSERT INTO run_shard_summaries (
                run_id,
                run_shard,
                expected_execution_count,
                execution_count,
                terminal_execution_count,
                aggregate_count,
                passed_execution_count,
                failed_execution_count,
                errored_execution_count,
                skipped_execution_count,
                missing_aggregate_count,
                evaluator_result_count,
                blocking_failure_count,
                score_count,
                score_sum,
                min_score,
                max_score,
                failed_chunk_count,
                cancelled_chunk_count,
                status
            )
            SELECT
                run_id,
                run_shard,
                expected_execution_count,
                execution_count,
                terminal_execution_count,
                aggregate_count,
                passed_execution_count,
                failed_execution_count,
                errored_execution_count,
                skipped_execution_count,
                missing_aggregate_count,
                evaluator_result_count,
                blocking_failure_count,
                score_count,
                score_sum,
                min_score,
                max_score,
                failed_chunk_count,
                cancelled_chunk_count,
                status
            FROM computed
            ON CONFLICT (run_id, run_shard) DO UPDATE
            SET expected_execution_count = EXCLUDED.expected_execution_count,
                execution_count = EXCLUDED.execution_count,
                terminal_execution_count = EXCLUDED.terminal_execution_count,
                aggregate_count = EXCLUDED.aggregate_count,
                passed_execution_count = EXCLUDED.passed_execution_count,
                failed_execution_count = EXCLUDED.failed_execution_count,
                errored_execution_count = EXCLUDED.errored_execution_count,
                skipped_execution_count = EXCLUDED.skipped_execution_count,
                missing_aggregate_count = EXCLUDED.missing_aggregate_count,
                evaluator_result_count = EXCLUDED.evaluator_result_count,
                blocking_failure_count = EXCLUDED.blocking_failure_count,
                score_count = EXCLUDED.score_count,
                score_sum = EXCLUDED.score_sum,
                min_score = EXCLUDED.min_score,
                max_score = EXCLUDED.max_score,
                failed_chunk_count = EXCLUDED.failed_chunk_count,
                cancelled_chunk_count = EXCLUDED.cancelled_chunk_count,
                status = EXCLUDED.status,
                updated_at = now()
            RETURNING
                run_id,
                run_shard,
                expected_execution_count,
                execution_count,
                terminal_execution_count,
                aggregate_count,
                passed_execution_count,
                failed_execution_count,
                errored_execution_count,
                skipped_execution_count,
                missing_aggregate_count,
                evaluator_result_count,
                blocking_failure_count,
                score_count,
                score_sum,
                min_score,
                max_score,
                failed_chunk_count,
                cancelled_chunk_count,
                status
        )
        SELECT *
        FROM upserted
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .fetch_optional(executor)
    .await?;

    Ok(summary)
}
