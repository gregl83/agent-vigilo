// PostgreSQL-backed workflow scenarios and fixtures.

use sqlx::PgPool;
use uuid::Uuid;

use super::*;

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

async fn seed_control_run(pool: &PgPool, run_id: Uuid, dataset_id: Uuid, dataset_version_id: Uuid) {
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
