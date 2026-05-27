//! Run dispatch workflow helpers.
//!
//! Coordinators use this module to atomically start pending runs and enqueue
//! bounded windows of outbox events that make chunks visible to workers. Run
//! and chunk row locks keep multiple coordinators from dispatching the same
//! window concurrently.

use sqlx::PgPool;
use uuid::Uuid;

/// Run projection returned after a coordinator dispatches a pending run.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct DispatchedRun {
    pub(crate) id: Uuid,
    pub(crate) run_key: String,
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

/// Claims one dispatchable run and enqueues a bounded chunk-ready window.
///
/// Pending runs are first marked running and receive a `run.started` event.
/// Running runs are eligible while they still have pending chunks whose
/// `dispatched_at` cursor is open. Chunk-ready events and cursor updates happen
/// in the same statement so a rollback does not lose undispatched work.
///
/// Query behavior:
/// - Selects one pending run or one running run with undispatched pending
///   chunks, using `FOR UPDATE SKIP LOCKED` so coordinators can compete safely.
/// - Marks newly selected pending runs as `running` and records coordinator
///   lease metadata.
/// - Selects a bounded chunk window, emits idempotent `run.chunk.ready` events,
///   and advances each chunk's `dispatched_at` cursor in the same statement.
/// - Emits `run.started` only when the run transitioned from `pending`.
pub(crate) async fn dispatch_next_run_window(
    db: &PgPool,
    coordinator_id: Uuid,
    lease_seconds: i32,
    chunk_window_size: i64,
) -> anyhow::Result<Option<DispatchedRun>> {
    // Query outline:
    //
    // candidate       - one run that can either start or dispatch more chunks.
    // claimed         - running state/coordinator metadata for that run.
    // selected_chunks - bounded undispatched pending chunk window.
    // chunk_events    - idempotent queue-visible chunk-ready ledger rows.
    // marked_chunks   - cursor update proving those chunks were dispatched.
    // started_event   - one idempotent run.started event for new runs.
    let dispatched = sqlx::query_as::<_, DispatchedRun>(
        r#"
        WITH candidate AS (
            SELECT r.id, r.status AS previous_status
            FROM runs r
            WHERE (
                    r.status = 'pending'::run_status
                    AND (
                        r.coordinator_leased_until IS NULL
                        OR r.coordinator_leased_until < now()
                    )
                )
               OR (
                    r.status = 'running'::run_status
                    AND EXISTS (
                        SELECT 1
                        FROM run_chunks rc
                        WHERE rc.run_id = r.id
                          AND rc.status = 'pending'
                          AND rc.dispatched_at IS NULL
                    )
                )
            ORDER BY r.updated_at ASC, r.created_at ASC
            FOR UPDATE SKIP LOCKED
            LIMIT 1
        ),
        claimed AS (
            UPDATE runs AS r
            SET status = 'running'::run_status,
                coordinator_id = $1,
                coordinator_leased_until = CASE
                    WHEN candidate.previous_status = 'pending'::run_status
                    THEN now() + ($2::int * interval '1 second')
                    ELSE r.coordinator_leased_until
                END,
                coordinator_heartbeat_at = now(),
                started_at = COALESCE(r.started_at, now()),
                dispatched_at = COALESCE(r.dispatched_at, now()),
                updated_at = now()
            FROM candidate
            WHERE r.id = candidate.id
            RETURNING r.id, r.run_key, candidate.previous_status
        ),
        selected_chunks AS (
            SELECT rc.run_id, rc.run_shard, rc.id
            FROM run_chunks rc
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
            (SELECT COUNT(*)::bigint FROM chunk_events) AS chunk_events_enqueued,
            (SELECT COUNT(*)::bigint FROM marked_chunks) AS chunks_marked_dispatched,
            (SELECT COUNT(*)::bigint FROM started_event) AS run_started_events_enqueued
        FROM claimed
        "#,
    )
    .bind(coordinator_id)
    .bind(lease_seconds)
    .bind(chunk_window_size)
    .fetch_optional(db)
    .await?;

    Ok(dispatched)
}
