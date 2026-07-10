//! Run finalization workflow helpers.
//!
//! Finalization is coordinator-owned and guarded by leases. Workers write
//! execution and aggregate state into shard-local storage. This workflow uses
//! `run_shard_summaries` read by the coordinator from execution placements to
//! complete the authoritative control `runs` row.

use sqlx::PgPool;
use uuid::Uuid;

use super::run_shard_summary::RunShardSummary;

/// Minimal run projection returned when a coordinator claims finalization.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct ClaimedRunForFinalization {
    pub(crate) id: Uuid,
    pub(crate) run_key: String,
}

/// Run projection returned after final gate status is persisted.
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

/// Selects the next run whose dispatch cursors are drained.
///
/// Query behavior:
/// - Does not inspect execution-owned rows.
/// - Requires all control dispatch cursors for the run to be drained.
/// - Returns `finalizing` runs only when their coordinator lease has expired.
pub(crate) async fn select_next_finalization_candidate(
    db: &PgPool,
) -> anyhow::Result<Option<ClaimedRunForFinalization>> {
    let candidate = sqlx::query_as::<_, ClaimedRunForFinalization>(
        r#"
        SELECT r.id, r.run_key
        FROM runs r
        WHERE r.status IN ('running'::run_status, 'finalizing'::run_status)
          AND (
              r.status <> 'finalizing'::run_status
              OR r.coordinator_leased_until IS NULL
              OR r.coordinator_leased_until < now()
          )
          AND EXISTS (
              SELECT 1
              FROM run_shard_dispatch_cursors c
              WHERE c.run_id = r.id
          )
          AND NOT EXISTS (
              SELECT 1
              FROM run_shard_dispatch_cursors c
              WHERE c.run_id = r.id
                AND c.status = 'open'
          )
        ORDER BY r.updated_at ASC, r.id ASC
        LIMIT 1
        "#,
    )
    .fetch_optional(db)
    .await?;

    Ok(candidate)
}

/// Claims a finalization candidate in control storage.
///
/// The coordinator reads shard summaries before calling this. Claiming only
/// serializes the final control write and protects retries if the process dies
/// after the claim but before completion.
pub(crate) async fn claim_finalization_candidate(
    db: &PgPool,
    run_id: Uuid,
    coordinator_id: Uuid,
    lease_seconds: i32,
) -> anyhow::Result<Option<ClaimedRunForFinalization>> {
    let claimed = sqlx::query_as::<_, ClaimedRunForFinalization>(
        r#"
        UPDATE runs r
        SET status = 'finalizing'::run_status,
            coordinator_id = $2,
            coordinator_leased_until = now() + ($3::int * interval '1 second'),
            coordinator_heartbeat_at = now(),
            updated_at = now()
        WHERE r.id = $1::uuid
          AND r.status IN ('running'::run_status, 'finalizing'::run_status)
          AND (
              r.status <> 'finalizing'::run_status
              OR r.coordinator_leased_until IS NULL
              OR r.coordinator_leased_until < now()
          )
        RETURNING r.id, r.run_key
        "#,
    )
    .bind(run_id)
    .bind(coordinator_id)
    .bind(lease_seconds)
    .fetch_optional(db)
    .await?;

    Ok(claimed)
}

/// Claims the next control candidate without reading execution storage.
///
/// Retained for existing workflow tests; production finalization uses
/// [`select_next_finalization_candidate`] plus routed shard summary reads.
#[cfg(test)]
pub(crate) async fn claim_next_finalizable_run(
    db: &PgPool,
    coordinator_id: Uuid,
    lease_seconds: i32,
) -> anyhow::Result<Option<ClaimedRunForFinalization>> {
    let Some(candidate) = select_next_finalization_candidate(db).await? else {
        return Ok(None);
    };

    claim_finalization_candidate(db, candidate.id, coordinator_id, lease_seconds).await
}

/// Finalizes a claimed run from routed shard summaries.
///
/// Query behavior:
/// - Locks the claimed run row.
/// - Uses precomputed shard summary counters supplied by the coordinator.
/// - Marks the run `completed`, sets the final gate status, persists the
///   global summary, drains leftover cursors, and emits `run.completed` in the
///   control outbox.
pub(crate) async fn finalize_claimed_run_from_summaries(
    db: &PgPool,
    run_id: Uuid,
    summaries: &[RunShardSummary],
) -> anyhow::Result<Option<FinalizedRun>> {
    if summaries.is_empty() || summaries.iter().any(|summary| !summary.is_terminal()) {
        return Ok(None);
    }

    let expected_execution_count = summaries
        .iter()
        .map(|summary| summary.expected_execution_count)
        .sum::<i32>();
    let terminal_execution_count = summaries
        .iter()
        .map(|summary| summary.terminal_execution_count)
        .sum::<i32>();
    let passed_execution_count = summaries
        .iter()
        .map(|summary| summary.passed_execution_count)
        .sum::<i32>();
    let failed_execution_count = summaries
        .iter()
        .map(|summary| summary.failed_execution_count)
        .sum::<i32>();
    let errored_execution_count = summaries
        .iter()
        .map(|summary| summary.errored_execution_count)
        .sum::<i32>();
    let missing_aggregate_count = summaries
        .iter()
        .map(|summary| summary.missing_aggregate_count)
        .sum::<i32>();
    let failed_chunk_count = summaries
        .iter()
        .map(|summary| summary.failed_chunk_count)
        .sum::<i32>();
    let cancelled_chunk_count = summaries
        .iter()
        .map(|summary| summary.cancelled_chunk_count)
        .sum::<i32>();
    let shard_summary_count = i32::try_from(summaries.len())?;
    let coverage_complete = terminal_execution_count >= expected_execution_count;
    let has_terminal_chunk_failure = failed_chunk_count > 0 || cancelled_chunk_count > 0;
    let gate_status = if has_terminal_chunk_failure
        || failed_execution_count > 0
        || errored_execution_count > 0
        || missing_aggregate_count > 0
        || !coverage_complete
    {
        "fail"
    } else {
        "pass"
    };

    let finalized = sqlx::query_as::<_, FinalizedRun>(
        r#"
        WITH run_row AS (
            SELECT
                id,
                run_key,
                expected_execution_count
            FROM runs
            WHERE id = $1::uuid
              AND status = 'finalizing'::run_status
            FOR UPDATE
        ),
        finalized AS (
            UPDATE runs r
            SET status = 'completed'::run_status,
                gate_status = $2::gate_status,
                terminal_execution_count = $4,
                passed_execution_count = $5,
                failed_execution_count = $6,
                errored_execution_count = $7,
                summary = jsonb_build_object(
                    'expected_execution_count', $3::int,
                    'terminal_execution_count', $4::int,
                    'passed_execution_count', $5::int,
                    'failed_execution_count', $6::int,
                    'errored_execution_count', $7::int,
                    'missing_aggregate_count', $8::int,
                    'failed_chunk_count', $9::int,
                    'cancelled_chunk_count', $10::int,
                    'coverage_complete', $11::bool,
                    'has_terminal_chunk_failure', $12::bool,
                    'shard_summary_count', $13::int
                ),
                finalized_at = COALESCE(r.finalized_at, now()),
                completed_at = COALESCE(r.completed_at, now()),
                coordinator_leased_until = NULL,
                coordinator_heartbeat_at = now(),
                updated_at = now()
            FROM run_row rr
            WHERE r.id = rr.id
            RETURNING
                r.id,
                r.run_key,
                r.gate_status::text as gate_status,
                r.terminal_execution_count,
                r.passed_execution_count,
                r.failed_execution_count,
                r.errored_execution_count
        ),
        drained_cursors AS (
            UPDATE run_shard_dispatch_cursors c
            SET status = 'drained',
                updated_at = now()
            FROM finalized f
            WHERE c.run_id = f.id
            RETURNING 1
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
    .bind(gate_status)
    .bind(expected_execution_count)
    .bind(terminal_execution_count)
    .bind(passed_execution_count)
    .bind(failed_execution_count)
    .bind(errored_execution_count)
    .bind(missing_aggregate_count)
    .bind(failed_chunk_count)
    .bind(cancelled_chunk_count)
    .bind(coverage_complete)
    .bind(has_terminal_chunk_failure)
    .bind(shard_summary_count)
    .fetch_optional(db)
    .await?;

    Ok(finalized)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx finalization tests"]
    async fn finalize_claimed_run_from_summaries_combines_terminal_shards(pool: PgPool) {
        let run_id = Uuid::now_v7();
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();

        seed_run(&pool, run_id, dataset_id, dataset_version_id, "finalizing").await;
        seed_dispatch_cursor(&pool, run_id, 3, "open").await;

        let mut first_summary = terminal_summary(run_id, 3, "completed");
        first_summary.expected_execution_count = 2;
        first_summary.terminal_execution_count = 2;
        first_summary.passed_execution_count = 2;

        let mut second_summary = terminal_summary(run_id, 7, "failed");
        second_summary.failed_execution_count = 1;

        let summaries = vec![first_summary, second_summary];

        let finalized = finalize_claimed_run_from_summaries(&pool, run_id, &summaries)
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
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();

        seed_run(&pool, run_id, dataset_id, dataset_version_id, "finalizing").await;
        let mut summary = terminal_summary(run_id, 3, "running");
        summary.expected_execution_count = 2;
        summary.terminal_execution_count = 1;
        let summaries = vec![summary];

        let finalized = finalize_claimed_run_from_summaries(&pool, run_id, &summaries)
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

    fn terminal_summary(run_id: Uuid, run_shard: i16, status: &str) -> RunShardSummary {
        RunShardSummary {
            run_id,
            run_shard,
            expected_execution_count: 1,
            execution_count: 1,
            terminal_execution_count: 1,
            aggregate_count: 1,
            passed_execution_count: 0,
            failed_execution_count: 0,
            errored_execution_count: 0,
            skipped_execution_count: 0,
            missing_aggregate_count: 0,
            evaluator_result_count: 0,
            blocking_failure_count: 0,
            score_count: 0,
            score_sum: 0.0,
            min_score: None,
            max_score: None,
            failed_chunk_count: 0,
            cancelled_chunk_count: 0,
            status: status.to_owned(),
        }
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
}
