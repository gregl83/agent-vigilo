// PostgreSQL-backed workflow scenarios and fixtures.

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    tests::terminal_summary,
    *,
};

#[derive(sqlx::FromRow)]
struct PersistedRun {
    status: String,
    gate_status: String,
    expected_execution_count: i32,
    terminal_execution_count: i32,
    passed_execution_count: i32,
    failed_execution_count: i32,
    errored_execution_count: i32,
    summary: Value,
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx finalization tests"]
async fn finalize_claimed_run_from_summaries_combines_terminal_shards(pool: PgPool) {
    let run_id = Uuid::now_v7();
    let coordinator_id = Uuid::now_v7();
    let dataset_id = Uuid::now_v7();
    let dataset_version_id = Uuid::now_v7();

    seed_run(&pool, run_id, dataset_id, dataset_version_id, "finalizing").await;
    set_finalization_lease(&pool, run_id, coordinator_id, "60 seconds").await;
    seed_dispatch_cursor(&pool, run_id, 3, "open").await;

    let mut first_summary = terminal_summary(run_id, 3, "completed");
    first_summary.expected_execution_count = 2;
    first_summary.terminal_execution_count = 2;
    first_summary.passed_execution_count = 2;

    let mut second_summary = terminal_summary(run_id, 7, "failed");
    second_summary.failed_execution_count = 1;

    let summaries = vec![first_summary, second_summary];

    let finalized = finalize_claimed_run_from_summaries(
        &pool,
        run_id,
        coordinator_id,
        "aggregation-hash",
        &summaries,
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(finalized.id, run_id);
    assert_eq!(finalized.gate_status, "fail");
    assert_eq!(finalized.terminal_execution_count, 3);
    assert_eq!(finalized.passed_execution_count, 2);
    assert_eq!(finalized.failed_execution_count, 1);
    assert_eq!(finalized.errored_execution_count, 0);

    let row = sqlx::query_as::<_, PersistedRun>(
        r#"
            SELECT
                status::text AS status,
                gate_status::text AS gate_status,
                expected_execution_count,
                terminal_execution_count,
                passed_execution_count,
                failed_execution_count,
                errored_execution_count,
                summary
            FROM runs
            WHERE id = $1::uuid
            "#,
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(row.status, "completed");
    assert_eq!(row.gate_status, "fail");
    assert_eq!(row.expected_execution_count, 3);
    assert_eq!(row.terminal_execution_count, 3);
    assert_eq!(row.passed_execution_count, 2);
    assert_eq!(row.failed_execution_count, 1);
    assert_eq!(row.errored_execution_count, 0);
    assert_eq!(row.summary["shard_summary_count"], Value::from(2));
    assert_eq!(row.summary["coverage_complete"], Value::from(true));

    let (policy_hash, shard_count, scorecard_passed) =
        sqlx::query_as::<_, (String, i32, bool)>(
            "SELECT aggregation_policy_hash, shard_count, passed FROM run_scorecards WHERE run_id = $1::uuid",
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(policy_hash, "aggregation-hash");
    assert_eq!(shard_count, 2);
    assert!(scorecard_passed);

    let cursor_status = sqlx::query_scalar::<_, String>(
        r#"
            SELECT status
            FROM run_shard_dispatch_cursors
            WHERE run_id = $1::uuid
              AND run_shard = 3
            "#,
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cursor_status, "drained");

    let event_count = sqlx::query_scalar::<_, i64>(
        r#"
            SELECT COUNT(*)
            FROM outbox_events
            WHERE aggregate_id = $1::uuid
              AND event_type = 'run.completed'
            "#,
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(event_count, 1);
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx finalization tests"]
async fn finalize_claimed_run_from_summaries_waits_for_running_summary(pool: PgPool) {
    let run_id = Uuid::now_v7();
    let coordinator_id = Uuid::now_v7();
    let dataset_id = Uuid::now_v7();
    let dataset_version_id = Uuid::now_v7();

    seed_run(&pool, run_id, dataset_id, dataset_version_id, "finalizing").await;
    set_finalization_lease(&pool, run_id, coordinator_id, "60 seconds").await;
    let mut summary = terminal_summary(run_id, 3, "running");
    summary.expected_execution_count = 2;
    summary.terminal_execution_count = 1;
    let summaries = vec![summary];

    let finalized = finalize_claimed_run_from_summaries(
        &pool,
        run_id,
        coordinator_id,
        "aggregation-hash",
        &summaries,
    )
    .await
    .unwrap();

    assert!(finalized.is_none());

    let status = sqlx::query_scalar::<_, String>(
        r#"
            SELECT status::text
            FROM runs
            WHERE id = $1::uuid
            "#,
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "finalizing");
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx finalization tests"]
async fn stale_or_expired_finalization_owner_cannot_commit(pool: PgPool) {
    let run_id = Uuid::now_v7();
    let stale_coordinator_id = Uuid::now_v7();
    let current_coordinator_id = Uuid::now_v7();

    seed_run(&pool, run_id, Uuid::now_v7(), Uuid::now_v7(), "finalizing").await;
    set_finalization_lease(&pool, run_id, current_coordinator_id, "60 seconds").await;
    let summaries = [terminal_summary(run_id, 0, "completed")];

    let finalized = finalize_claimed_run_from_summaries(
        &pool,
        run_id,
        stale_coordinator_id,
        "aggregation-hash",
        &summaries,
    )
    .await
    .unwrap();

    assert!(finalized.is_none());
    set_finalization_lease(&pool, run_id, stale_coordinator_id, "-1 second").await;
    let expired = finalize_claimed_run_from_summaries(
        &pool,
        run_id,
        stale_coordinator_id,
        "aggregation-hash",
        &summaries,
    )
    .await
    .unwrap();
    assert!(expired.is_none());

    let status =
        sqlx::query_scalar::<_, String>("SELECT status::text FROM runs WHERE id = $1::uuid")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "finalizing");
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx finalization tests"]
async fn select_finalization_candidate_backlog_matches_control_cursor_gate(pool: PgPool) {
    let ready_run_id = Uuid::now_v7();
    let open_run_id = Uuid::now_v7();
    let leased_run_id = Uuid::now_v7();

    seed_run(
        &pool,
        ready_run_id,
        Uuid::now_v7(),
        Uuid::now_v7(),
        "running",
    )
    .await;
    seed_dispatch_cursor(&pool, ready_run_id, 3, "drained").await;

    seed_run(
        &pool,
        open_run_id,
        Uuid::now_v7(),
        Uuid::now_v7(),
        "running",
    )
    .await;
    seed_dispatch_cursor(&pool, open_run_id, 5, "open").await;

    seed_run(
        &pool,
        leased_run_id,
        Uuid::now_v7(),
        Uuid::now_v7(),
        "finalizing",
    )
    .await;
    seed_dispatch_cursor(&pool, leased_run_id, 7, "drained").await;
    sqlx::query(
        r#"
            UPDATE runs
            SET coordinator_leased_until = now() + interval '60 seconds'
            WHERE id = $1::uuid
            "#,
    )
    .bind(leased_run_id)
    .execute(&pool)
    .await
    .unwrap();

    let backlog = select_finalization_candidate_backlog(&pool).await.unwrap();

    assert_eq!(backlog.candidate_count, 1);
    assert!(backlog.oldest_candidate_lag_seconds.unwrap() >= 0);
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx finalization tests"]
async fn checked_finalization_candidate_rotates_behind_unchecked_candidate(pool: PgPool) {
    let older_run_id = Uuid::now_v7();
    let newer_run_id = Uuid::now_v7();

    for run_id in [older_run_id, newer_run_id] {
        seed_run(&pool, run_id, Uuid::now_v7(), Uuid::now_v7(), "running").await;
        seed_dispatch_cursor(&pool, run_id, 0, "drained").await;
    }

    sqlx::query(
        r#"
            UPDATE runs
            SET coordinator_heartbeat_at = CASE id
                WHEN $1::uuid THEN now() - interval '2 hours'
                WHEN $2::uuid THEN now() - interval '1 hour'
            END
            WHERE id IN ($1::uuid, $2::uuid)
            "#,
    )
    .bind(older_run_id)
    .bind(newer_run_id)
    .execute(&pool)
    .await
    .unwrap();

    let selected = select_next_finalization_candidate(&pool, &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(selected.id, older_run_id);

    assert!(
        mark_finalization_candidate_checked(&pool, older_run_id)
            .await
            .unwrap()
    );

    let rotated = select_next_finalization_candidate(&pool, &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rotated.id, newer_run_id);

    let excluded = select_next_finalization_candidate(&pool, &[newer_run_id])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(excluded.id, older_run_id);
}

async fn seed_run(
    pool: &PgPool,
    run_id: Uuid,
    dataset_id: Uuid,
    dataset_version_id: Uuid,
    status: &str,
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
                $5::run_status,
                3
            )
            "#,
    )
    .bind(run_id)
    .bind(format!("run-{run_id}"))
    .bind(dataset_id)
    .bind(dataset_version_id)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_dispatch_cursor(pool: &PgPool, run_id: Uuid, run_shard: i16, status: &str) {
    sqlx::query(
        r#"
            INSERT INTO run_shard_dispatch_cursors (run_id, run_shard, status)
            VALUES ($1::uuid, $2, $3)
            "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

async fn set_finalization_lease(
    pool: &PgPool,
    run_id: Uuid,
    coordinator_id: Uuid,
    lease_interval: &str,
) {
    sqlx::query(
        r#"
            UPDATE runs
            SET coordinator_id = $2::uuid,
                coordinator_leased_until = now() + $3::text::interval
            WHERE id = $1::uuid
            "#,
    )
    .bind(run_id)
    .bind(coordinator_id)
    .bind(lease_interval)
    .execute(pool)
    .await
    .unwrap();
}
