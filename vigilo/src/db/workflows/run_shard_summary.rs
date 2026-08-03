//! Shard-local run summary workflow helpers.
//!
//! Summaries roll chunk and execution progress up to one row per
//! `run_id + run_shard` so later control-database finalization can combine shard
//! results without scanning all execution rows directly.

use sqlx::{
    Executor,
    PgPool,
    Postgres,
};
use uuid::Uuid;

/// Current shard-local summary for one run shard.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct RunShardSummary {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) expected_execution_count: i32,
    pub(crate) execution_count: i32,
    pub(crate) terminal_execution_count: i32,
    pub(crate) aggregate_count: i32,
    pub(crate) passed_execution_count: i32,
    pub(crate) failed_execution_count: i32,
    pub(crate) errored_execution_count: i32,
    pub(crate) skipped_execution_count: i32,
    pub(crate) missing_aggregate_count: i32,
    pub(crate) evaluator_result_count: i64,
    pub(crate) blocking_failure_count: i64,
    pub(crate) score_count: i64,
    pub(crate) score_sum: f64,
    pub(crate) min_score: Option<f64>,
    pub(crate) max_score: Option<f64>,
    pub(crate) failed_chunk_count: i32,
    pub(crate) cancelled_chunk_count: i32,
    pub(crate) status: String,
}

impl RunShardSummary {
    pub(crate) fn is_terminal(&self) -> bool {
        is_terminal_summary_status(&self.status)
    }
}

fn is_terminal_summary_status(status: &str) -> bool {
    matches!(status, "completed" | "failed")
}

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

/// Recomputes and upserts the shard-local summary for a prepared run shard.
///
/// Query behavior:
/// - Reads expected count from `run_snapshots`.
/// - Counts terminal executions and current-attempt aggregate outcomes.
/// - Counts failed/cancelled chunks.
/// - Marks the shard `completed` only when all expected executions are
///   terminal, all chunks are terminal, and no failure/missing aggregate
///   counters are present.
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

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn terminal_summary_statuses_are_strict_and_exhaustive() {
        for terminal in ["completed", "failed"] {
            assert!(is_terminal_summary_status(terminal));
        }
        for nonterminal in ["", "running", "pending", "cancelled", "Completed"] {
            assert!(!is_terminal_summary_status(nonterminal));
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx summary tests"]
    async fn refresh_run_shard_summary_counts_terminal_outcomes(pool: PgPool) {
        let run_id = Uuid::now_v7();
        let run_shard = 4i16;
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        let completed_execution_id = Uuid::now_v7();
        let completed_attempt_id = Uuid::now_v7();
        let failed_execution_id = Uuid::now_v7();
        let failed_attempt_id = Uuid::now_v7();

        seed_control_run(&pool, run_id, dataset_id, dataset_version_id).await;
        seed_run_snapshot(&pool, run_id, run_shard, dataset_id, dataset_version_id, 2).await;
        let chunk_id = seed_chunk(&pool, run_id, run_shard, dataset_version_id, "completed").await;
        seed_execution(
            &pool,
            run_id,
            run_shard,
            chunk_id,
            completed_execution_id,
            completed_attempt_id,
            "completed",
        )
        .await;
        seed_execution(
            &pool,
            run_id,
            run_shard,
            chunk_id,
            failed_execution_id,
            failed_attempt_id,
            "failed",
        )
        .await;
        seed_aggregate(
            &pool,
            run_id,
            run_shard,
            completed_execution_id,
            completed_attempt_id,
            "passed",
        )
        .await;
        seed_aggregate(
            &pool,
            run_id,
            run_shard,
            failed_execution_id,
            failed_attempt_id,
            "failed",
        )
        .await;

        let summary = refresh_run_shard_summary(&pool, run_id, run_shard)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(summary.expected_execution_count, 2);
        assert_eq!(summary.execution_count, 2);
        assert_eq!(summary.terminal_execution_count, 2);
        assert_eq!(summary.aggregate_count, 2);
        assert_eq!(summary.passed_execution_count, 1);
        assert_eq!(summary.failed_execution_count, 1);
        assert_eq!(summary.errored_execution_count, 0);
        assert_eq!(summary.skipped_execution_count, 0);
        assert_eq!(summary.missing_aggregate_count, 0);
        assert_eq!(summary.evaluator_result_count, 2);
        assert_eq!(summary.blocking_failure_count, 0);
        assert_eq!(summary.score_count, 0);
        assert_eq!(summary.score_sum, 0.0);
        assert_eq!(summary.status, "failed");
    }

    async fn seed_control_run(
        pool: &PgPool,
        run_id: Uuid,
        dataset_id: Uuid,
        dataset_version_id: Uuid,
    ) {
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
                'run-key',
                $2::uuid,
                $3::uuid,
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
                2
            )
            "#,
        )
        .bind(run_id)
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
        expected_execution_count: i32,
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
                $5
            )
            "#,
        )
        .bind(run_id)
        .bind(run_shard)
        .bind(dataset_id)
        .bind(dataset_version_id)
        .bind(expected_execution_count)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_chunk(
        pool: &PgPool,
        run_id: Uuid,
        run_shard: i16,
        dataset_version_id: Uuid,
        status: &str,
    ) -> Uuid {
        let chunk_id = Uuid::now_v7();
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
                status,
                dispatched_at
            )
            VALUES ($1::uuid, $2::uuid, $3, $4::uuid, 'default', 0, 2, $5, now())
            "#,
        )
        .bind(chunk_id)
        .bind(run_id)
        .bind(run_shard)
        .bind(dataset_version_id)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();

        chunk_id
    }

    async fn seed_execution(
        pool: &PgPool,
        run_id: Uuid,
        run_shard: i16,
        chunk_id: Uuid,
        execution_id: Uuid,
        attempt_id: Uuid,
        status: &str,
    ) {
        sqlx::query(
            r#"
            INSERT INTO executions (
                id,
                run_id,
                run_shard,
                chunk_id,
                case_id,
                case_hash,
                profile_group_id,
                task_type,
                evaluation_profile_id,
                evaluation_profile_version,
                expected_evaluator_count,
                status,
                current_attempt_id,
                current_attempt_no
            )
            VALUES (
                $1::uuid,
                $2::uuid,
                $3,
                $4::uuid,
                $5::uuid,
                'case-hash',
                'default',
                'classification',
                'profile',
                '1.0.0',
                1,
                $6::execution_status,
                $7::uuid,
                1
            )
            "#,
        )
        .bind(execution_id)
        .bind(run_id)
        .bind(run_shard)
        .bind(chunk_id)
        .bind(Uuid::now_v7())
        .bind(status)
        .bind(attempt_id)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO execution_attempts (
                id,
                run_id,
                run_shard,
                execution_id,
                attempt_no,
                status,
                completed_at
            )
            VALUES ($1::uuid, $2::uuid, $3, $4::uuid, 1, 'completed'::attempt_status, now())
            "#,
        )
        .bind(attempt_id)
        .bind(run_id)
        .bind(run_shard)
        .bind(execution_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_aggregate(
        pool: &PgPool,
        run_id: Uuid,
        run_shard: i16,
        execution_id: Uuid,
        attempt_id: Uuid,
        overall_status: &str,
    ) {
        sqlx::query(
            r#"
            INSERT INTO execution_aggregates (
                execution_id,
                run_id,
                run_shard,
                attempt_id,
                overall_status,
                evaluator_result_count
            )
            VALUES ($1::uuid, $2::uuid, $3, $4::uuid, $5::evaluation_status, 1)
            "#,
        )
        .bind(execution_id)
        .bind(run_id)
        .bind(run_shard)
        .bind(attempt_id)
        .bind(overall_status)
        .execute(pool)
        .await
        .unwrap();
    }
}
