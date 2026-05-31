//! Run dispatch workflow helpers.
//!
//! Coordinators use this module to atomically start pending runs and enqueue
//! bounded windows of outbox events that make chunks visible to workers.
//! Dispatch cursors keep chunk scans scoped to one run shard at a time while
//! row locks prevent multiple coordinators from dispatching the same window.

use sqlx::PgPool;
use uuid::Uuid;

/// Run projection returned after a coordinator dispatches one shard window.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct DispatchedRun {
    pub(crate) id: Uuid,
    pub(crate) run_key: String,
    pub(crate) run_shard: i16,
    pub(crate) chunk_events_enqueued: i64,
    pub(crate) chunks_marked_dispatched: i64,
    pub(crate) run_started_events_enqueued: i64,
}

/// Counts returned after one expired chunk lease recovery pass.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ChunkLeaseRecoveryStats {
    pub(crate) recovered: i64,
    pub(crate) failed: i64,
}

/// Recovers expired worker chunk leases for running runs.
///
/// Recovered chunks are moved back to `pending` and receive a fresh
/// `run.chunk.ready` event with a recovery-scoped dedupe key. Chunks that have
/// already reached the recovery limit are marked failed so finalization can
/// terminate the run instead of leaving it blocked by a dead lease forever.
///
/// Query behavior:
/// - Runs in one transaction so recovery and poison-chunk failure see a
///   consistent lease snapshot.
/// - First query locks a bounded oldest-expired set below `max_recoveries`,
///   resets those chunks to `pending`, increments recovery metadata, and emits
///   idempotent recovery-scoped chunk-ready events.
/// - Second query locks a bounded oldest-expired set already at the recovery
///   limit and marks them `failed`.
/// - `SKIP LOCKED` lets multiple coordinators run recovery without blocking
///   each other on the same chunk rows.
pub(crate) async fn recover_expired_chunk_leases(
    db: &PgPool,
    max_recoveries: i32,
    batch_size: i64,
) -> anyhow::Result<ChunkLeaseRecoveryStats> {
    let mut tx = db.begin().await?;

    // Query outline:
    //
    // expired         - recoverable leased chunks whose lease is past due.
    // recovered       - clear lease, increment recovery_count, return rows.
    // stale_recovered_attempts
    //                 - mark current running attempts stale before requeue.
    // recovery_events - publish a fresh, recovery-scoped chunk-ready event.
    let recovered = sqlx::query_scalar::<_, i64>(
        r#"
        WITH expired AS (
            SELECT
                rc.run_id,
                rc.run_shard,
                rc.id,
                rc.recovery_count + 1 AS next_recovery_count
            FROM run_chunks rc
            JOIN runs r
              ON r.id = rc.run_id
            WHERE rc.status = 'leased'
              AND rc.leased_until < now()
              AND rc.recovery_count < $1
              AND r.status = 'running'::run_status
            ORDER BY rc.leased_until ASC, rc.run_id ASC, rc.id ASC
            FOR UPDATE OF rc SKIP LOCKED
            LIMIT $2
        ),
        recovered AS (
            UPDATE run_chunks rc
            SET status = 'pending',
                leased_until = NULL,
                recovery_count = expired.next_recovery_count,
                last_recovered_at = now(),
                updated_at = now()
            FROM expired
            WHERE rc.run_id = expired.run_id
              AND rc.run_shard = expired.run_shard
              AND rc.id = expired.id
            RETURNING rc.run_id, rc.run_shard, rc.id, rc.recovery_count
        ),
        stale_recovered_attempts AS (
            UPDATE execution_attempts ea
            SET status = 'stale'::attempt_status,
                error_message = COALESCE(
                    ea.error_message,
                    'attempt lease expired with recovered chunk'
                ),
                completed_at = COALESCE(ea.completed_at, now()),
                leased_until = NULL,
                updated_at = now()
            FROM recovered
            JOIN executions e
              ON e.run_id = recovered.run_id
             AND e.run_shard = recovered.run_shard
             AND e.chunk_id = recovered.id
            WHERE ea.run_id = recovered.run_id
              AND ea.run_shard = recovered.run_shard
              AND ea.execution_id = e.id
              AND ea.id = e.current_attempt_id
              AND ea.status = 'running'::attempt_status
            RETURNING ea.id
        ),
        recovery_events AS (
            INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload)
            SELECT
                'run.chunk.ready',
                'run',
                recovered.run_id,
                format(
                    'run:%s:chunk:%s:ready:recovery:%s',
                    recovered.run_id,
                    recovered.id,
                    recovered.recovery_count
                ),
                jsonb_build_object(
                    'run_id', recovered.run_id,
                    'run_shard', recovered.run_shard,
                    'chunk_id', recovered.id,
                    'recovery_count', recovered.recovery_count
                )
            FROM recovered
            ON CONFLICT (dedupe_key) DO NOTHING
            RETURNING id
        )
        SELECT COUNT(*)::bigint
        FROM recovered
        "#,
    )
    .bind(max_recoveries)
    .bind(batch_size)
    .fetch_one(&mut *tx)
    .await?;

    // Query outline:
    //
    // expired - leased chunks past due that have exhausted recovery attempts.
    // failed  - clear the dead lease and make the chunk terminal.
    // stale_failed_attempts
    //         - mark current running attempts stale with the failed chunk.
    let failed = sqlx::query_scalar::<_, i64>(
        r#"
        WITH expired AS (
            SELECT rc.run_id, rc.run_shard, rc.id
            FROM run_chunks rc
            JOIN runs r
              ON r.id = rc.run_id
            WHERE rc.status = 'leased'
              AND rc.leased_until < now()
              AND rc.recovery_count >= $1
              AND r.status = 'running'::run_status
            ORDER BY rc.leased_until ASC, rc.run_id ASC, rc.id ASC
            FOR UPDATE OF rc SKIP LOCKED
            LIMIT $2
        ),
        failed AS (
            UPDATE run_chunks rc
            SET status = 'failed',
                leased_until = NULL,
                updated_at = now()
            FROM expired
            WHERE rc.run_id = expired.run_id
              AND rc.run_shard = expired.run_shard
              AND rc.id = expired.id
            RETURNING 1
        ),
        stale_failed_attempts AS (
            UPDATE execution_attempts ea
            SET status = 'stale'::attempt_status,
                error_message = COALESCE(
                    ea.error_message,
                    'attempt lease expired with failed chunk'
                ),
                completed_at = COALESCE(ea.completed_at, now()),
                leased_until = NULL,
                updated_at = now()
            FROM expired
            JOIN executions e
              ON e.run_id = expired.run_id
             AND e.run_shard = expired.run_shard
             AND e.chunk_id = expired.id
            WHERE ea.run_id = expired.run_id
              AND ea.run_shard = expired.run_shard
              AND ea.execution_id = e.id
              AND ea.id = e.current_attempt_id
              AND ea.status = 'running'::attempt_status
            RETURNING ea.id
        )
        SELECT COUNT(*)::bigint
        FROM failed
        "#,
    )
    .bind(max_recoveries)
    .bind(batch_size)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(ChunkLeaseRecoveryStats { recovered, failed })
}

/// Claims one dispatchable run shard and enqueues a bounded chunk-ready window.
///
/// Pending runs are first marked running and receive a `run.started` event. A
/// coordinator claims one open `(run_id, run_shard)` dispatch cursor, scans only
/// that shard's pending chunks, and releases or drains the cursor in the same
/// statement so a rollback does not lose undispatched work.
///
/// Query behavior:
/// - Selects one open shard cursor for a pending or running run with
///   `FOR UPDATE SKIP LOCKED` so coordinators can compete safely.
/// - Orders cursors by their own `updated_at` so large runs rotate across
///   shards instead of draining the lowest shard first.
/// - Marks newly selected pending runs as `running`; already-running runs take
///   only a shared lifecycle lock so dispatchers can run together while
///   cancellation/finalization updates still serialize.
/// - Selects a bounded chunk window inside one `run_id + run_shard`, emits
///   idempotent `run.chunk.ready` events, and advances each chunk's
///   `dispatched_at` cursor in the same statement.
/// - Releases the shard cursor if more undispatched chunks remain, or marks it
///   drained when that shard has no undispatched pending chunks left.
/// - Emits `run.started` only when the run transitioned from `pending`.
pub(crate) async fn dispatch_next_run_window(
    db: &PgPool,
    coordinator_id: Uuid,
    lease_seconds: i32,
    chunk_window_size: i64,
) -> anyhow::Result<Option<DispatchedRun>> {
    // Query outline:
    //
    // cursor_candidate - one shard cursor that can dispatch more chunks.
    // claimed_cursor   - locked cursor identity for this dispatch statement.
    // started_run      - running state/coordinator metadata for pending runs.
    // running_run      - shared lifecycle guard for already-running runs.
    // claimed          - common run projection for the claimed shard cursor.
    // selected_chunks  - bounded undispatched pending chunk window for one shard.
    // chunk_events     - idempotent queue-visible chunk-ready ledger rows.
    // marked_chunks    - cursor update proving those chunks were dispatched.
    // cursor_update    - release or drain the dispatch cursor.
    // started_event    - one idempotent run.started event for new runs.
    let dispatched = sqlx::query_as::<_, DispatchedRun>(
        r#"
        WITH cursor_candidate AS (
            SELECT
                c.run_id,
                c.run_shard,
                r.status AS previous_status
            FROM run_shard_dispatch_cursors c
            JOIN runs r
              ON r.id = c.run_id
            WHERE c.status = 'open'
              AND (
                  r.status = 'running'::run_status
                  OR (
                      r.status = 'pending'::run_status
                      AND (
                          r.coordinator_leased_until IS NULL
                          OR r.coordinator_leased_until < now()
                      )
                  )
              )
            ORDER BY c.updated_at ASC, c.run_id ASC, c.run_shard ASC
            FOR UPDATE OF c SKIP LOCKED
            LIMIT 1
        ),
        claimed_cursor AS (
            SELECT run_id, run_shard
            FROM cursor_candidate
        ),
        started_run AS (
            UPDATE runs AS r
            SET status = 'running'::run_status,
                coordinator_id = CASE
                    WHEN r.status = 'pending'::run_status THEN $1
                    ELSE r.coordinator_id
                END,
                coordinator_leased_until = CASE
                    WHEN r.status = 'pending'::run_status
                    THEN now() + ($2::int * interval '1 second')
                    ELSE r.coordinator_leased_until
                END,
                coordinator_heartbeat_at = CASE
                    WHEN r.status = 'pending'::run_status THEN now()
                    ELSE r.coordinator_heartbeat_at
                END,
                started_at = COALESCE(r.started_at, now()),
                dispatched_at = COALESCE(r.dispatched_at, now()),
                updated_at = CASE
                    WHEN r.status = 'pending'::run_status THEN now()
                    ELSE r.updated_at
                END
            FROM cursor_candidate
            JOIN claimed_cursor
              ON claimed_cursor.run_id = cursor_candidate.run_id
             AND claimed_cursor.run_shard = cursor_candidate.run_shard
            WHERE r.id = cursor_candidate.run_id
              AND cursor_candidate.previous_status = 'pending'::run_status
              AND r.status IN ('pending'::run_status, 'running'::run_status)
            RETURNING
                r.id,
                r.run_key,
                claimed_cursor.run_shard,
                cursor_candidate.previous_status
        ),
        running_run AS (
            SELECT
                r.id,
                r.run_key,
                claimed_cursor.run_shard,
                cursor_candidate.previous_status
            FROM runs r
            JOIN cursor_candidate
              ON cursor_candidate.run_id = r.id
            JOIN claimed_cursor
              ON claimed_cursor.run_id = cursor_candidate.run_id
             AND claimed_cursor.run_shard = cursor_candidate.run_shard
            WHERE cursor_candidate.previous_status = 'running'::run_status
              AND r.status = 'running'::run_status
            FOR SHARE OF r
        ),
        claimed AS (
            SELECT id, run_key, run_shard, previous_status
            FROM started_run
            UNION ALL
            SELECT id, run_key, run_shard, previous_status
            FROM running_run
        ),
        selected_chunks AS (
            SELECT rc.run_id, rc.run_shard, rc.id
            FROM run_chunks rc
            JOIN claimed_cursor
              ON claimed_cursor.run_id = rc.run_id
             AND claimed_cursor.run_shard = rc.run_shard
            JOIN claimed
              ON claimed.id = rc.run_id
            WHERE rc.status = 'pending'
              AND rc.dispatched_at IS NULL
            ORDER BY rc.ordinal_start ASC, rc.id ASC
            LIMIT $3
        ),
        chunk_events AS (
            INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload)
            SELECT
                'run.chunk.ready',
                'run',
                selected_chunks.run_id,
                format('run:%s:chunk:%s:ready', selected_chunks.run_id, selected_chunks.id),
                jsonb_build_object(
                    'run_id', selected_chunks.run_id,
                    'run_shard', selected_chunks.run_shard,
                    'chunk_id', selected_chunks.id
                )
            FROM selected_chunks
            ON CONFLICT (dedupe_key) DO NOTHING
            RETURNING id
        ),
        marked_chunks AS (
            UPDATE run_chunks rc
            SET dispatched_at = COALESCE(rc.dispatched_at, now()),
                updated_at = now()
            FROM selected_chunks
            WHERE rc.run_id = selected_chunks.run_id
              AND rc.run_shard = selected_chunks.run_shard
              AND rc.id = selected_chunks.id
            RETURNING rc.id
        ),
        remaining_chunks AS (
            SELECT EXISTS (
                SELECT 1
                FROM run_chunks rc
                JOIN claimed_cursor
                  ON claimed_cursor.run_id = rc.run_id
                 AND claimed_cursor.run_shard = rc.run_shard
                JOIN claimed
                  ON claimed.id = rc.run_id
                WHERE rc.status = 'pending'
                  AND rc.dispatched_at IS NULL
                  AND NOT EXISTS (
                      SELECT 1
                      FROM selected_chunks
                      WHERE selected_chunks.run_id = rc.run_id
                        AND selected_chunks.run_shard = rc.run_shard
                        AND selected_chunks.id = rc.id
                  )
            ) AS has_remaining
        ),
        cursor_update AS (
            UPDATE run_shard_dispatch_cursors c
            SET status = CASE
                    WHEN remaining_chunks.has_remaining THEN 'open'
                    ELSE 'drained'
                END,
                updated_at = now()
            FROM claimed_cursor, remaining_chunks, claimed
            WHERE c.run_id = claimed_cursor.run_id
              AND c.run_shard = claimed_cursor.run_shard
              AND claimed.id = claimed_cursor.run_id
              AND claimed.run_shard = claimed_cursor.run_shard
            RETURNING c.run_id, c.run_shard, c.status
        ),
        started_event AS (
            INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload)
            SELECT
                'run.started',
                'run',
                claimed.id,
                format('run:%s:started', claimed.id),
                jsonb_build_object('run_id', claimed.id)
            FROM claimed
            WHERE claimed.previous_status = 'pending'::run_status
            ON CONFLICT (dedupe_key) DO NOTHING
            RETURNING id
        )
        SELECT
            claimed.id,
            claimed.run_key,
            claimed.run_shard,
            (SELECT COUNT(*)::bigint FROM chunk_events) AS chunk_events_enqueued,
            (SELECT COUNT(*)::bigint FROM marked_chunks) AS chunks_marked_dispatched,
            (SELECT COUNT(*)::bigint FROM started_event) AS run_started_events_enqueued
        FROM claimed
        JOIN cursor_update
          ON cursor_update.run_id = claimed.id
         AND cursor_update.run_shard = claimed.run_shard
        "#,
    )
    .bind(coordinator_id)
    .bind(lease_seconds)
    .bind(chunk_window_size)
    .fetch_optional(db)
    .await?;

    Ok(dispatched)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::PgPool;

    use super::*;

    async fn seed_pending_run(pool: &PgPool, shard_chunk_counts: &[(i16, i32)]) -> Uuid {
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        let run_id = Uuid::now_v7();
        let expected_execution_count = shard_chunk_counts
            .iter()
            .map(|(_, count)| *count)
            .sum::<i32>();

        sqlx::query(
            r#"
            INSERT INTO dataset_versions (dataset_version_id, dataset_id, dataset_version)
            VALUES ($1::uuid, $2::uuid, 'test')
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
                expected_execution_count
            )
            VALUES (
                $1::uuid,
                $2,
                $3::uuid,
                $4::uuid,
                'test',
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
        .bind(format!("run-{run_id}"))
        .bind(dataset_id)
        .bind(dataset_version_id)
        .bind(expected_execution_count)
        .execute(pool)
        .await
        .unwrap();

        let mut ordinal = 0;
        for (run_shard, chunk_count) in shard_chunk_counts {
            sqlx::query(
                r#"
                INSERT INTO run_shard_dispatch_cursors (run_id, run_shard, status)
                VALUES ($1::uuid, $2, 'open')
                "#,
            )
            .bind(run_id)
            .bind(run_shard)
            .execute(pool)
            .await
            .unwrap();

            for _ in 0..*chunk_count {
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
                    VALUES (
                        $1::uuid,
                        $2::uuid,
                        $3,
                        $4::uuid,
                        'default',
                        $5,
                        $6,
                        'pending'
                    )
                    "#,
                )
                .bind(Uuid::now_v7())
                .bind(run_id)
                .bind(run_shard)
                .bind(dataset_version_id)
                .bind(ordinal)
                .bind(ordinal + 1)
                .execute(pool)
                .await
                .unwrap();

                ordinal += 1;
            }
        }

        run_id
    }

    async fn mark_run_running(pool: &PgPool, run_id: Uuid) {
        sqlx::query(
            r#"
            UPDATE runs
            SET status = 'running'::run_status,
                started_at = now(),
                dispatched_at = now(),
                updated_at = now()
            WHERE id = $1::uuid
            "#,
        )
        .bind(run_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn dispatch_scans_one_run_shard_at_a_time(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 2), (1, 2)]).await;

        let first = dispatch_next_run_window(&pool, Uuid::now_v7(), 60, 10)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.id, run_id);
        assert_eq!(first.run_shard, 0);
        assert_eq!(first.chunks_marked_dispatched, 2);

        let shard_1_dispatched = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)::bigint
            FROM run_chunks
            WHERE run_id = $1::uuid
              AND run_shard = 1
              AND dispatched_at IS NOT NULL
            "#,
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(shard_1_dispatched, 0);

        let cursor_status = sqlx::query_scalar::<_, String>(
            r#"
            SELECT status
            FROM run_shard_dispatch_cursors
            WHERE run_id = $1::uuid
              AND run_shard = 0
            "#,
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cursor_status, "drained");
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn dispatch_releases_open_cursor_when_shard_has_more_chunks(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 3)]).await;

        let first = dispatch_next_run_window(&pool, Uuid::now_v7(), 60, 2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.run_shard, 0);
        assert_eq!(first.chunks_marked_dispatched, 2);

        let cursor_status = sqlx::query_scalar::<_, String>(
            r#"
            SELECT status
            FROM run_shard_dispatch_cursors
            WHERE run_id = $1::uuid
              AND run_shard = 0
            "#,
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cursor_status, "open");

        let second = dispatch_next_run_window(&pool, Uuid::now_v7(), 60, 2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.run_shard, 0);
        assert_eq!(second.chunks_marked_dispatched, 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn running_dispatch_does_not_wait_on_parent_run_share_lock(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 1)]).await;
        mark_run_running(&pool, run_id).await;

        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            r#"
            SELECT id
            FROM runs
            WHERE id = $1::uuid
            FOR SHARE
            "#,
        )
        .bind(run_id)
        .execute(&mut *tx)
        .await
        .unwrap();

        let dispatched = tokio::time::timeout(
            Duration::from_secs(1),
            dispatch_next_run_window(&pool, Uuid::now_v7(), 60, 10),
        )
        .await
        .expect("dispatch should not wait on a compatible parent run share lock")
        .unwrap()
        .unwrap();

        assert_eq!(dispatched.id, run_id);
        assert_eq!(dispatched.run_shard, 0);
        assert_eq!(dispatched.chunks_marked_dispatched, 1);
        assert_eq!(dispatched.run_started_events_enqueued, 0);

        tx.rollback().await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn running_dispatch_waits_on_parent_run_update_lock(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 1)]).await;
        mark_run_running(&pool, run_id).await;

        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            r#"
            SELECT id
            FROM runs
            WHERE id = $1::uuid
            FOR UPDATE
            "#,
        )
        .bind(run_id)
        .execute(&mut *tx)
        .await
        .unwrap();

        let dispatch_result = tokio::time::timeout(
            Duration::from_millis(100),
            dispatch_next_run_window(&pool, Uuid::now_v7(), 60, 10),
        )
        .await;
        assert!(
            dispatch_result.is_err(),
            "dispatch should wait behind an exclusive lifecycle update lock"
        );

        tx.rollback().await.unwrap();
    }
}
